//! MVCC table store. Every row version is stamped with the LSN of the
//! transaction that created it and, on delete/update, the LSN that superseded
//! it. Readers see a consistent snapshot LSN; they never block writers and are
//! never blocked by them (spec 02 — snapshot isolation).
//!
//! There is a single writer per database (serialized by the write lane in
//! [`crate::db`]), so every pending (uncommitted) version belongs to the current
//! writer — which keeps visibility and rollback rules simple.

use crate::catalog::TableSchema;
use crate::value::Value;
use std::collections::HashMap;

/// Stamp marking a version created or deleted by the in-flight writer; not yet
/// durable, never visible to other connections' snapshots.
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
}

impl RowVersion {
    /// Visible to a reader at snapshot `s`: created at-or-before `s` and not yet
    /// deleted as of `s`.
    pub fn visible_to_reader(&self, s: u64) -> bool {
        self.create_lsn <= s && (self.delete_lsn == 0 || self.delete_lsn > s)
    }

    /// Visible to the in-flight writer (snapshot `s`): committed-and-visible OR
    /// our own pending insert, and not deleted by a committed delete or our own
    /// pending delete.
    pub fn visible_to_writer(&self, s: u64) -> bool {
        let created = self.create_lsn == PENDING || self.create_lsn <= s;
        let deleted = self.delete_lsn == PENDING || (self.delete_lsn != 0 && self.delete_lsn <= s);
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
}

#[derive(Default)]
pub struct Store {
    /// Keyed by lowercased table name; `schema.name` keeps the original casing.
    tables: HashMap<String, Table>,
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

    pub fn insert_table(&mut self, schema: TableSchema) {
        let k = Self::key(&schema.name);
        self.tables.insert(k, Table::new(schema));
    }

    pub fn drop_table(&mut self, name: &str) -> bool {
        self.tables.remove(&Self::key(name)).is_some()
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

    /// Publish all pending versions at `commit_lsn` (Committing → Committed).
    pub fn finalize_pending(&mut self, commit_lsn: u64) {
        for t in self.tables.values_mut() {
            for r in &mut t.rows {
                if r.create_lsn == PENDING {
                    r.create_lsn = commit_lsn;
                }
                if r.delete_lsn == PENDING {
                    r.delete_lsn = commit_lsn;
                }
            }
        }
        self.committed_lsn = commit_lsn;
    }

    /// Discard every pending version (whole-transaction rollback). Single-writer
    /// invariant: all remaining pending stamps belong to the aborting txn.
    pub fn rollback_pending(&mut self) {
        for t in self.tables.values_mut() {
            t.rows.retain(|r| r.create_lsn != PENDING);
            for r in &mut t.rows {
                if r.delete_lsn == PENDING {
                    r.delete_lsn = 0;
                }
            }
        }
    }
}
