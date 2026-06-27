//! MVCC table store. Every row version is stamped with the LSN of the
//! transaction that created it and, on delete/update, the LSN that superseded
//! it. Readers see a consistent snapshot LSN; they never block writers and are
//! never blocked by them (spec 02 — snapshot isolation).
//!
//! Store *mutation* is single-writer (serialized by the write lane in
//! [`crate::db`]), but with group commit several committed-but-not-yet-durable
//! transactions can have pending versions in flight at once — each waiting on
//! the same batched durable append. A pending version is therefore tagged with
//! its in-flight writer's [`RowVersion::owner`] so the right transaction's
//! versions are published (or discarded) when its commit resolves, and so a
//! writer sees only its own pending changes (snapshot isolation across the
//! in-flight set). `owner == 0` means fully committed (no pending stamp).

use crate::catalog::TableSchema;
use crate::sql::SelectStmt;
use crate::value::Value;
use crate::vector::{IndexDef, VectorIndex};
use std::collections::HashMap;

/// Stamp marking a version created or deleted by an in-flight writer; not yet
/// durable, never visible to other connections' snapshots. The owning writer is
/// recorded in [`RowVersion::owner`].
pub const PENDING: u64 = u64::MAX;

#[derive(Clone, Debug)]
pub struct RowVersion {
    /// Physical version id — unique per version, never reused.
    pub vid: u64,
    pub values: Vec<Value>,
    /// Commit LSN that created this version, or [`PENDING`].
    pub create_lsn: u64,
    /// `0` = live; otherwise the commit LSN that deleted it, or [`PENDING`].
    pub delete_lsn: u64,
    /// In-flight writer that owns this version's [`PENDING`] stamp(s), or `0`
    /// when fully committed. A row can carry at most one pending stamp at a time
    /// (a concurrent writer touching it conflicts at DML time), so a single
    /// owner covers both a pending create and a pending delete.
    pub owner: u64,
}

impl RowVersion {
    /// Visible to a reader at snapshot `s`: created at-or-before `s` and not yet
    /// deleted as of `s`. Pending stamps ([`PENDING`]) are never `<= s`, so a
    /// reader never sees another transaction's uncommitted create or delete.
    pub fn visible_to_reader(&self, s: u64) -> bool {
        self.create_lsn <= s && (self.delete_lsn == 0 || self.delete_lsn > s)
    }

    /// Visible to the in-flight writer `me` at snapshot `s`: committed-and-visible
    /// OR *my own* pending insert, and not deleted by a committed delete or *my
    /// own* pending delete. Another in-flight writer's pending create is invisible
    /// to me, and its pending delete does not hide an otherwise-visible row.
    pub fn visible_to_writer(&self, s: u64, me: u64) -> bool {
        let created = if self.create_lsn == PENDING {
            self.owner == me
        } else {
            self.create_lsn <= s
        };
        let deleted = if self.delete_lsn == PENDING {
            self.owner == me
        } else {
            self.delete_lsn != 0 && self.delete_lsn <= s
        };
        created && !deleted
    }
}

/// VH-2: whether a row version is superseded by a *committed* delete as of the
/// index head — the predicate that classifies a vector index node as head-deleted.
/// A pending delete ([`PENDING`]) does not count: it is not yet visible to anyone
/// but its own in-flight writer.
fn is_dead_at_head(r: &RowVersion) -> bool {
    r.delete_lsn != 0 && r.delete_lsn != PENDING
}

#[derive(Clone, Debug)]
pub struct Table {
    pub schema: TableSchema,
    pub rows: Vec<RowVersion>,
    pub next_vid: u64,
    /// Next value an `AUTOINCREMENT`/`SERIAL` column will be assigned (max seen
    /// + 1; rebuilt from the rows on replay). Stage 6D.
    pub next_autoinc: i64,
}

impl Table {
    fn new(schema: TableSchema) -> Table {
        Table {
            schema,
            rows: Vec::new(),
            next_vid: 1,
            next_autoinc: 1,
        }
    }

    pub fn alloc_vid(&mut self) -> u64 {
        let v = self.next_vid;
        self.next_vid += 1;
        v
    }

    pub fn version_mut(&mut self, vid: u64) -> Option<&mut RowVersion> {
        self.rows.iter_mut().find(|r| r.vid == vid)
    }

    pub fn version(&self, vid: u64) -> Option<&RowVersion> {
        self.rows.iter().find(|r| r.vid == vid)
    }
}

#[derive(Default)]
pub struct Store {
    /// Keyed by lowercased table name; `schema.name` keeps the original casing.
    tables: HashMap<String, Table>,
    /// HNSW vector indexes, keyed by lowercased index name (spec 12). Derived
    /// structures over a table's vector column — rebuilt from the rows on replay,
    /// so they branch and scale-to-zero with the database (see `vector.rs`).
    indexes: HashMap<String, VectorIndex>,
    /// Views, keyed by lowercased name → the parsed query each stands for. A view
    /// resolves as a derived table when referenced in a `FROM` (deferred 6B item).
    /// Like an index, it is a catalog fact rebuilt from the WAL on replay.
    views: HashMap<String, SelectStmt>,
    /// Highest committed transaction boundary published to readers.
    pub committed_lsn: u64,
}

impl Store {
    fn key(name: &str) -> String {
        name.to_ascii_lowercase()
    }

    pub fn table(&self, name: &str) -> Option<&Table> {
        self.tables.get(&Self::key(name))
    }

    pub fn table_mut(&mut self, name: &str) -> Option<&mut Table> {
        self.tables.get_mut(&Self::key(name))
    }

    pub fn has_table(&self, name: &str) -> bool {
        self.tables.contains_key(&Self::key(name))
    }

    /// Every table's schema, sorted by name — the basis for catalog reflection.
    pub fn table_schemas(&self) -> Vec<TableSchema> {
        let mut v: Vec<TableSchema> = self.tables.values().map(|t| t.schema.clone()).collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
    }

    pub fn insert_table(&mut self, schema: TableSchema) {
        let k = Self::key(&schema.name);
        self.tables.insert(k, Table::new(schema));
    }

    pub fn drop_table(&mut self, name: &str) -> bool {
        let existed = self.tables.remove(&Self::key(name)).is_some();
        // A table's indexes go with it.
        self.indexes
            .retain(|_, ix| !ix.def.table.eq_ignore_ascii_case(name));
        existed
    }

    // ---- views (deferred 6B item) ---------------------------------------

    pub fn has_view(&self, name: &str) -> bool {
        self.views.contains_key(&Self::key(name))
    }

    /// The parsed body of view `name`, if it exists.
    pub fn view(&self, name: &str) -> Option<&SelectStmt> {
        self.views.get(&Self::key(name))
    }

    /// Register (or replace) a view with its already-parsed body.
    pub fn insert_view(&mut self, name: String, query: SelectStmt) {
        self.views.insert(Self::key(&name), query);
    }

    pub fn drop_view(&mut self, name: &str) -> bool {
        self.views.remove(&Self::key(name)).is_some()
    }

    // ---- vector indexes (spec 12) ---------------------------------------

    pub fn has_index(&self, name: &str) -> bool {
        self.indexes.contains_key(&Self::key(name))
    }

    /// Whether `table` has any vector index (lets the write path skip index
    /// maintenance entirely when there is none).
    pub fn table_has_index(&self, table: &str) -> bool {
        self.indexes
            .values()
            .any(|ix| ix.def.table.eq_ignore_ascii_case(table))
    }

    /// Register an index definition with an empty graph (replay path — the
    /// graph is filled by [`Store::rebuild_one_index`] once all rows are present).
    pub fn register_index(&mut self, def: IndexDef) {
        self.indexes
            .insert(Self::key(&def.name), VectorIndex::new(def));
    }

    /// Create and immediately populate an index from the table's current rows
    /// (live DDL path).
    pub fn create_index(&mut self, def: IndexDef) {
        let name = def.name.clone();
        self.register_index(def);
        self.populate_index(&name);
    }

    pub fn drop_index(&mut self, name: &str) -> bool {
        self.indexes.remove(&Self::key(name)).is_some()
    }

    /// The HNSW index on `(table, column)`, if any — the planner's lookup for
    /// pushing a nearest-neighbour query into the access method.
    pub fn index_for(&self, table: &str, column: &str) -> Option<&VectorIndex> {
        self.indexes.values().find(|ix| {
            ix.def.table.eq_ignore_ascii_case(table) && ix.def.column.eq_ignore_ascii_case(column)
        })
    }

    /// Names (lowercased keys) of every registered index — the warm path iterates
    /// these to load or rebuild each graph after replay (VH-1).
    pub fn index_names(&self) -> Vec<String> {
        self.indexes.keys().cloned().collect()
    }

    /// The definition of index `name`, if registered.
    pub fn index_def(&self, name: &str) -> Option<IndexDef> {
        self.indexes.get(&Self::key(name)).map(|ix| ix.def.clone())
    }

    /// VH-1: adopt an externally reconstructed graph (loaded from page frames on a
    /// cold open) in place of the registered index of the same name.
    pub fn adopt_index(&mut self, ix: VectorIndex) {
        self.indexes.insert(Self::key(&ix.def.name), ix);
    }

    /// VH-1: rebuild a single index from the recovered rows (the warm-from-WAL
    /// fallback when no current page checkpoint exists), compacting it if churn
    /// already crossed the threshold.
    pub fn rebuild_one_index(&mut self, name: &str) {
        let key = Self::key(name);
        if let Some(ix) = self.indexes.get_mut(&key) {
            let def = ix.def.clone();
            *ix = VectorIndex::new(def);
        }
        self.populate_index(&key);
        if self
            .indexes
            .get(&key)
            .is_some_and(VectorIndex::needs_maintenance)
        {
            let floor = self.committed_lsn;
            self.compact_index(&key, floor);
        }
    }

    /// VH-1: serialize index `name`'s graph into page frames stamped with the
    /// committed LSN they reflect, for a checkpoint through `put_page`. `None` if
    /// the index is absent or its graph cannot be paged (see
    /// [`VectorIndex::to_page_frames`]).
    pub fn index_page_frames(&self, name: &str, reflected_lsn: u64) -> Option<Vec<Vec<u8>>> {
        self.indexes
            .get(&Self::key(name))?
            .to_page_frames(reflected_lsn)
    }

    /// Fill one index's graph from every row version of its table that carries a
    /// vector (MVCC visibility is resolved later, at query time, against the vids).
    /// A version already superseded by a committed delete at the index head is
    /// added but immediately marked head-deleted (VH-2), so the over-fetch sizing
    /// and compaction trigger see accurate churn after a cold rebuild.
    fn populate_index(&mut self, name: &str) {
        let key = Self::key(name);
        let Some(ix) = self.indexes.get(&key) else {
            return;
        };
        let table = Self::key(&ix.def.table);
        let column = ix.def.column.clone();
        let Some(t) = self.tables.get(&table) else {
            return;
        };
        let Some(col) = t.schema.column_index(&column) else {
            return;
        };
        let entries: Vec<(u64, Vec<f32>, bool)> = t
            .rows
            .iter()
            .filter_map(|r| {
                r.values
                    .get(col)
                    .and_then(Value::as_vector)
                    .map(|v| (r.vid, v.to_vec(), is_dead_at_head(r)))
            })
            .collect();
        if let Some(ix) = self.indexes.get_mut(&key) {
            for (vid, vec, dead) in entries {
                ix.insert(vid, vec);
                if dead {
                    ix.mark_dead(vid);
                }
            }
        }
    }

    /// VH-2: rebuild one index's graph from only the rows that are *not* deleted at
    /// the head (committed deletes are dropped; live and still-pending versions are
    /// kept), stamping `floor_lsn` as the index's compaction floor. The dropped
    /// rows are still retained by the row store, so a reader whose snapshot predates
    /// `floor_lsn` is routed to a brute-force scan by `exec::knn_select` — keeping
    /// snapshot isolation intact while the head graph stays compact.
    fn compact_index(&mut self, name: &str, floor_lsn: u64) {
        let key = Self::key(name);
        let Some(ix) = self.indexes.get(&key) else {
            return;
        };
        let def = ix.def.clone();
        let table = Self::key(&def.table);
        let Some(t) = self.tables.get(&table) else {
            return;
        };
        let Some(col) = t.schema.column_index(&def.column) else {
            return;
        };
        let kept: Vec<(u64, Vec<f32>)> = t
            .rows
            .iter()
            .filter(|r| !is_dead_at_head(r))
            .filter_map(|r| {
                r.values
                    .get(col)
                    .and_then(Value::as_vector)
                    .map(|v| (r.vid, v.to_vec()))
            })
            .collect();
        let mut fresh = VectorIndex::new(def);
        for (vid, vec) in kept {
            fresh.insert(vid, vec);
        }
        fresh.set_rebuild_floor(floor_lsn.max(ix.rebuild_floor()));
        self.indexes.insert(key, fresh);
    }

    /// VH-2: reflect a batch of committed deletes on `table` into its vector
    /// indexes — mark the vids head-deleted, then compact any index whose churn
    /// crossed the threshold (at `commit_lsn`, the new compaction floor).
    fn apply_index_deletes(&mut self, table: &str, vids: &[u64], commit_lsn: u64) {
        let names: Vec<String> = self
            .indexes
            .iter()
            .filter(|(_, ix)| ix.def.table.eq_ignore_ascii_case(table))
            .map(|(k, _)| k.clone())
            .collect();
        for name in names {
            if let Some(ix) = self.indexes.get_mut(&name) {
                for &vid in vids {
                    ix.mark_dead(vid);
                }
            }
            if self
                .indexes
                .get(&name)
                .is_some_and(VectorIndex::needs_maintenance)
            {
                self.compact_index(&name, commit_lsn);
            }
        }
    }

    /// Maintain every index on `table` after a row version `(vid, values)` is
    /// inserted (live insert/update path).
    pub fn index_row_inserted(&mut self, table: &str, vid: u64, values: &[Value]) {
        let Some(t) = self.tables.get(&Self::key(table)) else {
            return;
        };
        let schema = &t.schema;
        let mut adds: Vec<(String, Vec<f32>)> = Vec::new();
        for (k, ix) in &self.indexes {
            if !ix.def.table.eq_ignore_ascii_case(table) {
                continue;
            }
            if let Some(col) = schema.column_index(&ix.def.column) {
                if let Some(v) = values.get(col).and_then(Value::as_vector) {
                    adds.push((k.clone(), v.to_vec()));
                }
            }
        }
        for (k, v) in adds {
            if let Some(ix) = self.indexes.get_mut(&k) {
                ix.insert(vid, v);
            }
        }
    }

    // ---- recovery (replay) ----------------------------------------------

    /// Apply a single decoded WAL op at `commit_lsn` during recovery replay.
    pub fn replay_create(&mut self, schema: TableSchema) {
        self.insert_table(schema);
    }
    pub fn replay_drop(&mut self, name: &str) {
        self.drop_table(name);
    }
    pub fn replay_create_view(&mut self, name: String, query: SelectStmt) {
        self.insert_view(name, query);
    }
    pub fn replay_drop_view(&mut self, name: &str) {
        self.drop_view(name);
    }
    pub fn replay_insert(&mut self, table: &str, vid: u64, values: Vec<Value>, commit_lsn: u64) {
        if let Some(t) = self.table_mut(table) {
            t.rows.push(RowVersion {
                vid,
                values,
                create_lsn: commit_lsn,
                delete_lsn: 0,
                owner: 0,
            });
            t.next_vid = t.next_vid.max(vid + 1);
        }
    }
    pub fn replay_delete(&mut self, table: &str, vid: u64, commit_lsn: u64) {
        if let Some(t) = self.table_mut(table) {
            if let Some(v) = t.version_mut(vid) {
                v.delete_lsn = commit_lsn;
            }
        }
    }

    // ---- schema evolution (stage 6D) ------------------------------------

    /// Append a column to a table's schema, extending every existing row version
    /// with `fill` (its default, or NULL). Used by both the live DDL path and
    /// replay.
    pub fn add_column(&mut self, table: &str, column: crate::catalog::Column, fill: Value) {
        if let Some(t) = self.table_mut(table) {
            t.schema.columns.push(column);
            for r in &mut t.rows {
                r.values.push(fill.clone());
            }
        }
    }

    /// Drop a column by name, removing it from the schema and every row version.
    pub fn drop_column(&mut self, table: &str, name: &str) {
        if let Some(t) = self.table_mut(table) {
            if let Some(idx) = t.schema.column_index(name) {
                t.schema.columns.remove(idx);
                for r in &mut t.rows {
                    if idx < r.values.len() {
                        r.values.remove(idx);
                    }
                }
            }
        }
    }

    /// Rename a column in the schema (rows are positional, so unchanged).
    pub fn rename_column(&mut self, table: &str, from: &str, to: &str) {
        if let Some(t) = self.table_mut(table) {
            if let Some(idx) = t.schema.column_index(from) {
                t.schema.columns[idx].name = to.to_string();
            }
        }
    }

    /// Rename a table, re-keying it in the registry and updating `schema.name`.
    pub fn rename_table(&mut self, from: &str, to: &str) {
        if let Some(mut t) = self.tables.remove(&Self::key(from)) {
            t.schema.name = to.to_string();
            // Indexes/FKs keep referring to the old table name by string; the
            // common ALTER … RENAME case is rare enough to leave them as-is.
            self.tables.insert(Self::key(to), t);
        }
    }

    /// Rebuild every table's autoincrement counter from its rows (replay path):
    /// the next value is one past the largest existing value in the column.
    pub fn rebuild_autoinc(&mut self) {
        for t in self.tables.values_mut() {
            let Some(col) = t.schema.columns.iter().position(|c| c.autoincrement) else {
                continue;
            };
            let mut max = 0i64;
            for r in &t.rows {
                if let Some(Value::Int(i)) = r.values.get(col) {
                    max = max.max(*i);
                }
            }
            t.next_autoinc = max + 1;
        }
    }

    // ---- commit / rollback ----------------------------------------------

    /// Publish one in-flight writer's pending versions at `commit_lsn`
    /// (Committing → Committed). Only versions tagged with `owner` are stamped,
    /// leaving any concurrently in-flight transaction's pending versions
    /// untouched. The caller advances [`Store::committed_lsn`] once the whole
    /// group-commit batch is durable (see [`crate::group_commit`]), so reader
    /// visibility moves forward atomically for the batch.
    pub fn finalize_owner(&mut self, owner: u64, commit_lsn: u64) {
        // Collect the vids this owner just committed-deleted, per table, so the
        // vector indexes can be updated after the version stamps are published.
        let mut deleted_by_table: Vec<(String, Vec<u64>)> = Vec::new();
        for (key, t) in self.tables.iter_mut() {
            let mut deleted: Vec<u64> = Vec::new();
            for r in &mut t.rows {
                if r.owner != owner {
                    continue;
                }
                if r.create_lsn == PENDING {
                    r.create_lsn = commit_lsn;
                }
                if r.delete_lsn == PENDING {
                    r.delete_lsn = commit_lsn;
                    deleted.push(r.vid);
                }
                r.owner = 0;
            }
            if !deleted.is_empty() {
                deleted_by_table.push((key.clone(), deleted));
            }
        }
        // VH-2: a committed delete makes its index node head-deleted; under enough
        // churn this triggers a compacting rebuild (single-writer path, so the
        // occasional rebuild cost is paid here, off the read path).
        for (table, vids) in deleted_by_table {
            self.apply_index_deletes(&table, &vids, commit_lsn);
        }
    }

    /// Snapshot one writer's pending state for a SAVEPOINT (stage 6D): the vids
    /// it has pending-inserted and pending-deleted right now.
    pub fn pending_snapshot(
        &self,
        owner: u64,
    ) -> (
        std::collections::HashSet<u64>,
        std::collections::HashSet<u64>,
    ) {
        let mut inserted = std::collections::HashSet::new();
        let mut deleted = std::collections::HashSet::new();
        for t in self.tables.values() {
            for r in &t.rows {
                if r.owner != owner {
                    continue;
                }
                if r.create_lsn == PENDING {
                    inserted.insert(r.vid);
                }
                if r.delete_lsn == PENDING {
                    deleted.insert(r.vid);
                }
            }
        }
        (inserted, deleted)
    }

    /// Roll one writer's pending state back to a saved snapshot (`ROLLBACK TO`):
    /// remove pending inserts created since the savepoint, and clear pending
    /// deletes applied since it — restoring the exact pending set at savepoint
    /// time. Any insert removed here is also tombstoned out of the indexes.
    pub fn rollback_to_savepoint(
        &mut self,
        owner: u64,
        inserted: &std::collections::HashSet<u64>,
        deleted: &std::collections::HashSet<u64>,
    ) {
        let mut discarded: Vec<u64> = Vec::new();
        for t in self.tables.values_mut() {
            for r in &t.rows {
                if r.owner == owner && r.create_lsn == PENDING && !inserted.contains(&r.vid) {
                    discarded.push(r.vid);
                }
            }
            t.rows.retain(|r| {
                !(r.owner == owner && r.create_lsn == PENDING && !inserted.contains(&r.vid))
            });
            for r in &mut t.rows {
                if r.owner == owner && r.delete_lsn == PENDING && !deleted.contains(&r.vid) {
                    r.delete_lsn = 0;
                    // The version reverts to committed-live; clear its owner only
                    // if it carries no remaining pending create.
                    if r.create_lsn != PENDING {
                        r.owner = 0;
                    }
                }
            }
        }
        for ix in self.indexes.values_mut() {
            for &vid in &discarded {
                ix.remove(vid);
            }
        }
    }

    /// Discard one in-flight writer's pending versions (whole-transaction
    /// rollback): remove its pending inserts and clear its pending deletes,
    /// leaving any other concurrently in-flight transaction's versions intact.
    /// Any pending insert removed here is also tombstoned out of the vector
    /// indexes.
    pub fn rollback_owner(&mut self, owner: u64) {
        let mut discarded: Vec<u64> = Vec::new();
        for t in self.tables.values_mut() {
            for r in &t.rows {
                if r.owner == owner && r.create_lsn == PENDING {
                    discarded.push(r.vid);
                }
            }
            t.rows
                .retain(|r| !(r.owner == owner && r.create_lsn == PENDING));
            for r in &mut t.rows {
                if r.owner == owner && r.delete_lsn == PENDING {
                    r.delete_lsn = 0;
                    r.owner = 0;
                }
            }
        }
        if discarded.is_empty() {
            return;
        }
        for ix in self.indexes.values_mut() {
            for &vid in &discarded {
                ix.remove(vid);
            }
        }
    }
}
