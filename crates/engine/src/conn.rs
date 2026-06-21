//! A connection (engine handle): the transaction state machine plus exec /
//! query / prepared-statement entry points. One connection is single-threaded
//! (spec 02 — one handle, one thread of execution at a time); parallelism comes
//! from opening multiple handles to the shared [`Database`].

use crate::db::{commit_error, Database};
use crate::error::{EngineError, Result};
use crate::exec::{run_delete, run_insert, run_select, run_update, ResultSet};
use crate::sql::{self, Stmt};
use crate::value::Value;
use crate::vector::{IndexDef, IndexParams};
use crate::wal::WalOp;
use bydesigns_storage::{block_on, Lsn, WalRecord};
use std::ffi::CString;
use std::os::raw::c_char;
use std::sync::Arc;

struct Txn {
    /// MVCC snapshot LSN captured at transaction start.
    snapshot: u64,
    /// Whether this connection currently holds the write lane.
    writer: bool,
    /// Buffered WAL ops, flushed durably as one batch at commit.
    wal_ops: Vec<WalOp>,
}

pub struct Connection {
    db: Arc<Database>,
    txn: Option<Txn>,
    /// Last error message as a stable NUL-terminated string for the C ABI.
    last_error: CString,
    pub last_changes: i64,
    pub last_lsn: i64,
}

impl Connection {
    pub fn open(url: &str) -> Result<Connection> {
        Ok(Connection {
            db: Database::open(url)?,
            txn: None,
            last_error: CString::default(),
            last_changes: 0,
            last_lsn: 0,
        })
    }

    pub fn in_transaction(&self) -> bool {
        self.txn.is_some()
    }

    /// Create a copy-on-write branch off this connection's database at its
    /// current committed LSN, returning a new connection bound to the branch.
    /// The branch forks from committed state (not this connection's uncommitted
    /// changes), so it must not be called inside an active transaction. Writes
    /// to the returned connection are isolated from the base and any siblings.
    pub fn branch(&self, name: &str) -> Result<Connection> {
        if self.txn.is_some() {
            return Err(EngineError::txn(
                "cannot branch inside an active transaction",
            ));
        }
        if self.db.is_branch() {
            return Err(EngineError::misuse(
                "branch-of-branch is not supported yet; branch from the base database",
            ));
        }
        let base = self.db.committed_lsn();
        let id = block_on(self.db.storage.create_branch(name, Lsn(base))).map_err(commit_error)?;
        let db = Database::open_branch(self.db.url(), id)?;
        Ok(Connection {
            db,
            txn: None,
            last_error: CString::default(),
            last_changes: 0,
            last_lsn: 0,
        })
    }

    /// Record the last error for retrieval via `engine_last_error`.
    pub fn set_last_error(&mut self, msg: &str) {
        self.last_error = CString::new(msg).unwrap_or_default();
    }

    /// Borrowed pointer to the last error string (valid until the next call).
    pub fn last_error_ptr(&self) -> *const c_char {
        self.last_error.as_ptr()
    }

    /// Snapshot LSN and writer flag for a read.
    fn read_snapshot(&self) -> (u64, bool) {
        match &self.txn {
            Some(t) => (t.snapshot, t.writer),
            None => (self.db.committed_lsn(), false),
        }
    }

    // ---- one-shot entry points ------------------------------------------

    pub fn exec(&mut self, sql: &str) -> Result<()> {
        let (stmt, _n) = sql::parse(sql)?;
        self.run(&stmt, &[])?;
        Ok(())
    }

    pub fn query(&mut self, sql: &str) -> Result<ResultSet> {
        let (stmt, _n) = sql::parse(sql)?;
        self.run(&stmt, &[])
    }

    /// Execute a (possibly parameterized) statement, returning rows for SELECT.
    pub fn run(&mut self, stmt: &Stmt, params: &[Value]) -> Result<ResultSet> {
        match stmt {
            Stmt::Begin => {
                self.begin()?;
                Ok(ResultSet::default())
            }
            Stmt::Commit => {
                self.commit()?;
                Ok(ResultSet::default())
            }
            Stmt::Rollback => {
                self.rollback()?;
                Ok(ResultSet::default())
            }
            Stmt::Select(sel) => {
                let (snapshot, as_writer) = self.read_snapshot();
                let store = self.db.store.read().unwrap();
                let rs = run_select(&store, sel, snapshot, as_writer, params)?;
                self.last_changes = 0;
                Ok(rs)
            }
            Stmt::CreateTable { .. }
            | Stmt::DropTable { .. }
            | Stmt::CreateIndex { .. }
            | Stmt::DropIndex { .. } => {
                self.exec_ddl(stmt)?;
                Ok(ResultSet::default())
            }
            Stmt::Insert { .. } | Stmt::Update { .. } | Stmt::Delete { .. } => {
                self.exec_dml(stmt, params)?;
                Ok(ResultSet::default())
            }
        }
    }

    // ---- transaction control --------------------------------------------

    pub fn begin(&mut self) -> Result<()> {
        if self.txn.is_some() {
            return Err(EngineError::txn("a transaction is already active"));
        }
        self.txn = Some(Txn {
            snapshot: self.db.committed_lsn(),
            writer: false,
            wal_ops: Vec::new(),
        });
        Ok(())
    }

    pub fn commit(&mut self) -> Result<()> {
        let txn = self
            .txn
            .take()
            .ok_or_else(|| EngineError::txn("no active transaction to commit"))?;
        self.finish_commit(txn)
    }

    pub fn rollback(&mut self) -> Result<()> {
        let txn = self
            .txn
            .take()
            .ok_or_else(|| EngineError::txn("no active transaction to roll back"))?;
        if txn.writer {
            self.db.store.write().unwrap().rollback_pending();
            self.db.lane.release();
        }
        Ok(())
    }

    /// Committing → Committed: durably append the WAL batch, then publish the
    /// pending versions at the returned commit LSN. Never acks before durable.
    fn finish_commit(&mut self, txn: Txn) -> Result<()> {
        if txn.writer {
            if !txn.wal_ops.is_empty() {
                let mut records: Vec<WalRecord> =
                    txn.wal_ops.iter().map(|op| op.encode()).collect();
                records.push(WalOp::Commit.encode());

                let commit_lsn =
                    match block_on(self.db.storage.append_wal(&self.db.token, &records)) {
                        Ok(l) => l.0,
                        Err(e) => {
                            // Durability unconfirmed: abort, discard pending, step down.
                            self.db.store.write().unwrap().rollback_pending();
                            self.db.lane.release();
                            return Err(commit_error(e));
                        }
                    };
                self.db.store.write().unwrap().finalize_pending(commit_lsn);
                self.last_lsn = commit_lsn as i64;
            }
            self.db.lane.release();
        }
        Ok(())
    }

    fn ensure_writer(&mut self) {
        if !self.txn.as_ref().unwrap().writer {
            self.db.lane.acquire();
            self.txn.as_mut().unwrap().writer = true;
        }
    }

    // ---- DML ------------------------------------------------------------

    fn exec_dml(&mut self, stmt: &Stmt, params: &[Value]) -> Result<()> {
        let implicit = self.txn.is_none();
        if implicit {
            self.txn = Some(Txn {
                snapshot: self.db.committed_lsn(),
                writer: false,
                wal_ops: Vec::new(),
            });
        }
        self.ensure_writer();
        let snapshot = self.txn.as_ref().unwrap().snapshot;

        let result = {
            let mut store = self.db.store.write().unwrap();
            match stmt {
                Stmt::Insert {
                    table,
                    columns,
                    rows,
                } => run_insert(&mut store, table, columns, rows, params),
                Stmt::Update {
                    table,
                    sets,
                    filter,
                } => run_update(&mut store, table, sets, filter, snapshot, params),
                Stmt::Delete { table, filter } => {
                    run_delete(&mut store, table, filter, snapshot, params)
                }
                _ => unreachable!(),
            }
        };

        match result {
            Ok((wal_ops, changes)) => {
                self.txn.as_mut().unwrap().wal_ops.extend(wal_ops);
                self.last_changes = changes;
                if implicit {
                    let txn = self.txn.take().unwrap();
                    self.finish_commit(txn)?;
                }
                Ok(())
            }
            Err(e) => {
                // The statement is atomic (store untouched on failure). For an
                // implicit (autocommit) txn, tear it down; for an explicit txn,
                // keep prior pending changes and surface the error.
                if implicit {
                    let txn = self.txn.take().unwrap();
                    if txn.writer {
                        self.db.store.write().unwrap().rollback_pending();
                        self.db.lane.release();
                    }
                }
                Err(e)
            }
        }
    }

    // ---- DDL (autocommit only in Phase 1) -------------------------------

    fn exec_ddl(&mut self, stmt: &Stmt) -> Result<()> {
        if self.txn.is_some() {
            return Err(EngineError::txn(
                "DDL is only supported in autocommit (not inside a transaction) in Phase 1",
            ));
        }
        self.db.lane.acquire();
        let res = self.do_ddl(stmt);
        self.db.lane.release();
        self.last_changes = 0;
        if let Ok(Some(lsn)) = &res {
            self.last_lsn = *lsn as i64;
        }
        res.map(|_| ())
    }

    fn do_ddl(&self, stmt: &Stmt) -> Result<Option<u64>> {
        match stmt {
            Stmt::CreateTable {
                name,
                columns,
                if_not_exists,
            } => {
                if self.db.store.read().unwrap().has_table(name) {
                    if *if_not_exists {
                        return Ok(None);
                    }
                    return Err(EngineError::sql(format!("table {name} already exists")));
                }
                let cols: Vec<crate::catalog::Column> = columns
                    .iter()
                    .map(|c| crate::catalog::Column {
                        name: c.name.clone(),
                        ty: c.ty,
                        primary_key: c.primary_key,
                        not_null: c.not_null,
                    })
                    .collect();
                if cols.iter().filter(|c| c.primary_key).count() > 1 {
                    return Err(EngineError::sql(
                        "composite PRIMARY KEY is not supported in Phase 1",
                    ));
                }
                let schema = crate::catalog::TableSchema {
                    name: name.clone(),
                    columns: cols,
                };
                let records = vec![
                    WalOp::CreateTable {
                        schema: schema.clone(),
                    }
                    .encode(),
                    WalOp::Commit.encode(),
                ];
                let commit_lsn = block_on(self.db.storage.append_wal(&self.db.token, &records))
                    .map_err(commit_error)?;
                let mut store = self.db.store.write().unwrap();
                store.insert_table(schema);
                store.committed_lsn = store.committed_lsn.max(commit_lsn.0);
                Ok(Some(commit_lsn.0))
            }
            Stmt::DropTable { name, if_exists } => {
                if !self.db.store.read().unwrap().has_table(name) {
                    if *if_exists {
                        return Ok(None);
                    }
                    return Err(EngineError::sql(format!("no such table: {name}")));
                }
                let records = vec![
                    WalOp::DropTable { name: name.clone() }.encode(),
                    WalOp::Commit.encode(),
                ];
                let commit_lsn = block_on(self.db.storage.append_wal(&self.db.token, &records))
                    .map_err(commit_error)?;
                let mut store = self.db.store.write().unwrap();
                store.drop_table(name);
                store.committed_lsn = store.committed_lsn.max(commit_lsn.0);
                Ok(Some(commit_lsn.0))
            }
            Stmt::CreateIndex {
                name,
                table,
                column,
                params,
                if_not_exists,
            } => self.do_create_index(name, table, column, *params, *if_not_exists),
            Stmt::DropIndex { name, if_exists } => self.do_drop_index(name, *if_exists),
            _ => unreachable!(),
        }
    }

    /// `CREATE INDEX … USING hnsw`: validate the target column is a vector,
    /// durably log the definition, then build the in-memory graph from the
    /// column's current rows (autocommit, like `CREATE TABLE`).
    fn do_create_index(
        &self,
        name: &str,
        table: &str,
        column: &str,
        params: IndexParams,
        if_not_exists: bool,
    ) -> Result<Option<u64>> {
        {
            let store = self.db.store.read().unwrap();
            if store.has_index(name) {
                if if_not_exists {
                    return Ok(None);
                }
                return Err(EngineError::sql(format!("index {name} already exists")));
            }
            let t = store
                .table(table)
                .ok_or_else(|| EngineError::sql(format!("no such table: {table}")))?;
            let col = t
                .schema
                .column_index(column)
                .ok_or_else(|| EngineError::sql(format!("no such column: {table}.{column}")))?;
            if !t.schema.columns[col].ty.is_vector() {
                return Err(EngineError::sql(format!(
                    "HNSW index requires a vector column; {table}.{column} is not a vector"
                )));
            }
        }
        let def = IndexDef {
            name: name.to_string(),
            table: table.to_string(),
            column: column.to_string(),
            params,
        };
        let records = vec![
            WalOp::CreateIndex { def: def.clone() }.encode(),
            WalOp::Commit.encode(),
        ];
        let commit_lsn =
            block_on(self.db.storage.append_wal(&self.db.token, &records)).map_err(commit_error)?;
        let mut store = self.db.store.write().unwrap();
        store.create_index(def);
        store.committed_lsn = store.committed_lsn.max(commit_lsn.0);
        Ok(Some(commit_lsn.0))
    }

    fn do_drop_index(&self, name: &str, if_exists: bool) -> Result<Option<u64>> {
        if !self.db.store.read().unwrap().has_index(name) {
            if if_exists {
                return Ok(None);
            }
            return Err(EngineError::sql(format!("no such index: {name}")));
        }
        let records = vec![
            WalOp::DropIndex {
                name: name.to_string(),
            }
            .encode(),
            WalOp::Commit.encode(),
        ];
        let commit_lsn =
            block_on(self.db.storage.append_wal(&self.db.token, &records)).map_err(commit_error)?;
        let mut store = self.db.store.write().unwrap();
        store.drop_index(name);
        store.committed_lsn = store.committed_lsn.max(commit_lsn.0);
        Ok(Some(commit_lsn.0))
    }

    // ---- prepared statements --------------------------------------------

    pub fn prepare(&mut self, sql: &str) -> Result<Statement> {
        let (stmt, nparams) = sql::parse(sql)?;
        Ok(Statement {
            conn: self as *mut Connection,
            stmt,
            params: vec![Value::Null; nparams],
            nparams,
            result: None,
            row_idx: 0,
            cur: None,
            executed: false,
        })
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        if let Some(txn) = self.txn.take() {
            if txn.writer {
                if let Ok(mut s) = self.db.store.write() {
                    s.rollback_pending();
                }
                self.db.lane.release();
            }
        }
    }
}

/// A prepared statement bound to its owning connection. Re-executable via
/// [`Statement::reset`]; column values are borrowed until the next step/reset.
pub struct Statement {
    conn: *mut Connection,
    stmt: Stmt,
    params: Vec<Value>,
    nparams: usize,
    result: Option<ResultSet>,
    row_idx: usize,
    cur: Option<usize>,
    executed: bool,
}

impl Statement {
    /// Record an error on the owning connection (for `engine_last_error`).
    pub fn record_error(&self, msg: &str) {
        // Safety: the owning handle outlives the statement (caller contract).
        unsafe { (*self.conn).set_last_error(msg) }
    }

    /// Number of `?`/`$n` parameters this statement expects. Used by the
    /// pgwire server to answer `Describe` (ParameterDescription).
    pub fn param_count(&self) -> usize {
        self.nparams
    }

    /// Force execution (idempotent), so column metadata is available before the
    /// first `step` — the pgwire server needs it to emit `RowDescription`.
    pub fn execute(&mut self) -> Result<()> {
        self.ensure_executed()
    }

    pub fn bind(&mut self, idx: usize, value: Value) -> Result<()> {
        if idx < 1 || idx > self.nparams {
            return Err(EngineError::misuse(format!(
                "bind index {idx} out of range (statement has {} parameters)",
                self.nparams
            )));
        }
        self.params[idx - 1] = value;
        Ok(())
    }

    fn ensure_executed(&mut self) -> Result<()> {
        if !self.executed {
            // Safety: the caller keeps the owning handle alive for the lifetime
            // of the statement (spec 02 ownership rules; one thread per handle).
            let conn = unsafe { &mut *self.conn };
            let rs = conn.run(&self.stmt, &self.params)?;
            self.result = Some(rs);
            self.row_idx = 0;
            self.cur = None;
            self.executed = true;
        }
        Ok(())
    }

    /// Advance the cursor. Returns `true` if a row is now current.
    pub fn step(&mut self) -> Result<bool> {
        self.ensure_executed()?;
        let rs = self.result.as_ref().unwrap();
        if self.row_idx < rs.rows.len() {
            self.cur = Some(self.row_idx);
            self.row_idx += 1;
            Ok(true)
        } else {
            self.cur = None;
            Ok(false)
        }
    }

    /// Rows affected by the last execution (for DML statements).
    pub fn changes(&self) -> i64 {
        let conn = unsafe { &*self.conn };
        conn.last_changes
    }

    pub fn reset(&mut self) {
        self.executed = false;
        self.result = None;
        self.row_idx = 0;
        self.cur = None;
    }

    pub fn column_count(&self) -> usize {
        self.result.as_ref().map(|r| r.columns.len()).unwrap_or(0)
    }

    pub fn column_name(&self, col: usize) -> Option<&str> {
        self.result
            .as_ref()
            .and_then(|r| r.columns.get(col))
            .map(|s| s.as_str())
    }

    /// Best-effort declared type of result column `col` (for the pgwire server's
    /// `RowDescription` type OIDs). Available after [`Statement::execute`].
    pub fn column_type(&self, col: usize) -> Option<crate::value::ColumnType> {
        self.result.as_ref().and_then(|r| r.types.get(col).copied())
    }

    pub fn column_value(&self, col: usize) -> Option<&Value> {
        let rs = self.result.as_ref()?;
        let cur = self.cur?;
        rs.rows.get(cur).and_then(|row| row.get(col))
    }
}
