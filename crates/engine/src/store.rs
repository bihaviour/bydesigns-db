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

#[derive(Clone, Debug)]
pub struct Table {
    pub schema: TableSchema,
    pub rows: Vec<RowVersion>,
    pub next_vid: u64,
}

impl Table {
    fn new(schema: TableSchema) -> Table {
        Table {
            schema,
            rows: Vec::new(),
            next_vid: 1,
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
    /// graph is filled by [`Store::rebuild_indexes`] once all rows are present).
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

    /// Fill one index's graph from every row version of its table that carries a
    /// vector (MVCC visibility is resolved later, at query time, against the vids).
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
        let entries: Vec<(u64, Vec<f32>)> = t
            .rows
            .iter()
            .filter_map(|r| {
                r.values
                    .get(col)
                    .and_then(Value::as_vector)
                    .map(|v| (r.vid, v.to_vec()))
            })
            .collect();
        if let Some(ix) = self.indexes.get_mut(&key) {
            for (vid, vec) in entries {
                ix.insert(vid, vec);
            }
        }
    }

    /// Rebuild every index graph from final table state. Called once after WAL
    /// replay — the cold-start "warm" of the vector index (spec 12 §scale-to-zero).
    pub fn rebuild_indexes(&mut self) {
        let names: Vec<String> = self.indexes.keys().cloned().collect();
        for n in &names {
            if let Some(ix) = self.indexes.get_mut(n) {
                let def = ix.def.clone();
                *ix = VectorIndex::new(def);
            }
            self.populate_index(n);
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

    // ---- commit / rollback ----------------------------------------------

    /// Publish one in-flight writer's pending versions at `commit_lsn`
    /// (Committing → Committed). Only versions tagged with `owner` are stamped,
    /// leaving any concurrently in-flight transaction's pending versions
    /// untouched. The caller advances [`Store::committed_lsn`] once the whole
    /// group-commit batch is durable (see [`crate::group_commit`]), so reader
    /// visibility moves forward atomically for the batch.
    pub fn finalize_owner(&mut self, owner: u64, commit_lsn: u64) {
        for t in self.tables.values_mut() {
            for r in &mut t.rows {
                if r.owner != owner {
                    continue;
                }
                if r.create_lsn == PENDING {
                    r.create_lsn = commit_lsn;
                }
                if r.delete_lsn == PENDING {
                    r.delete_lsn = commit_lsn;
                }
                r.owner = 0;
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
