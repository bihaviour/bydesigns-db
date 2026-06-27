//! A connection (engine handle): the transaction state machine plus exec /
//! query / prepared-statement entry points. One connection is single-threaded
//! (spec 02 — one handle, one thread of execution at a time); parallelism comes
//! from opening multiple handles to the shared [`Database`].

use crate::db::{commit_error, Database};
use crate::error::{EngineError, Result};
use crate::exec::{run_delete, run_insert, run_select_tuned, run_update, ResultSet, WriteCtx};
use crate::sql::{self, Stmt};
use crate::value::{ColumnType, Value};
use crate::vector::{IndexDef, IndexParams};
use crate::wal::WalOp;
use std::ffi::CString;
use std::os::raw::c_char;
use std::sync::Arc;
use twill_storage::{block_on, Lsn, WalRecord};

struct Txn {
    /// MVCC snapshot LSN captured at transaction start.
    snapshot: u64,
    /// Whether this connection currently holds the write lane.
    writer: bool,
    /// Buffered WAL ops, flushed durably as one batch at commit.
    wal_ops: Vec<WalOp>,
    /// In-flight writer id tagging this transaction's pending store versions, so
    /// group commit can publish/discard exactly this transaction's changes even
    /// while another commit is in flight (see [`crate::group_commit`]).
    owner: u64,
    /// Savepoint stack (stage 6D): each entry records the writer's pending state
    /// (inserted / deleted vids) when the savepoint was set, so `ROLLBACK TO`
    /// can restore it.
    savepoints: Vec<Savepoint>,
}

struct Savepoint {
    name: String,
    inserted: std::collections::HashSet<u64>,
    deleted: std::collections::HashSet<u64>,
    /// Length of the buffered WAL op list at savepoint time — `ROLLBACK TO`
    /// truncates back to it so rolled-back ops never reach the durable commit.
    wal_len: usize,
}

pub struct Connection {
    db: Arc<Database>,
    txn: Option<Txn>,
    /// Last error message as a stable NUL-terminated string for the C ABI.
    last_error: CString,
    pub last_changes: i64,
    pub last_lsn: i64,
    /// VH-3: per-session HNSW `ef_search` override (`SET twill.vector_ef_search`).
    /// `None` means each index uses its configured default. Pure parameter
    /// binding — no durable or shared state, so it never affects branching or
    /// scale-to-zero.
    vector_ef_search: Option<usize>,
}

/// A reflected table, for wire-protocol catalog introspection ([`Connection::catalog`]).
pub struct CatalogTable {
    pub name: String,
    pub columns: Vec<CatalogColumn>,
    pub foreign_keys: Vec<CatalogForeignKey>,
}

/// A reflected foreign-key relationship: the local columns and the table +
/// columns they reference. The pgwire server reflects these into PostgREST's
/// relationship cache to enable resource embedding.
pub struct CatalogForeignKey {
    pub name: String,
    pub columns: Vec<String>,
    pub foreign_table: String,
    pub foreign_columns: Vec<String>,
}

/// A reflected column: its Postgres type name plus key / nullability flags.
pub struct CatalogColumn {
    pub name: String,
    /// The Postgres type name a client expects (`integer`, `text`, …).
    pub pg_type: &'static str,
    pub not_null: bool,
    pub primary_key: bool,
    /// 1-based ordinal position in the table.
    pub position: i32,
}

/// Build the one-row result a `SHOW <name>` returns (stage 6E). The column is
/// named after the setting; the value is a best-effort default (the snapshot-
/// isolation level for the isolation GUCs, otherwise empty).
fn show_result(name: &str) -> ResultSet {
    let key = name.to_ascii_lowercase();
    let value = match key.as_str() {
        "transaction_isolation" | "default_transaction_isolation" => "repeatable read",
        "transaction isolation level" => "repeatable read",
        _ => "",
    };
    ResultSet {
        columns: vec![if key.is_empty() {
            "setting".to_string()
        } else {
            name.to_string()
        }],
        types: vec![ColumnType::Text],
        rows: vec![vec![Value::Text(value.to_string())]],
    }
}

/// Build the one-row plan text an `EXPLAIN` returns (stage 6E). The engine has a
/// single strategy per statement shape, so this is a terse description.
fn explain_result(stmt: &Stmt) -> ResultSet {
    let plan = match stmt {
        Stmt::Select(_) => "Query plan: scan / nested-loop join over the MVCC snapshot",
        Stmt::Insert { .. } => "Insert",
        Stmt::Update { .. } => "Update (scan + supersede)",
        Stmt::Delete { .. } => "Delete (scan + supersede)",
        _ => "Utility statement",
    };
    ResultSet {
        columns: vec!["QUERY PLAN".to_string()],
        types: vec![ColumnType::Text],
        rows: vec![vec![Value::Text(plan.to_string())]],
    }
}

/// Map an engine storage class to the Postgres type name clients introspect.
fn pg_type_name(ty: ColumnType) -> &'static str {
    match ty {
        ColumnType::Integer => "bigint",
        ColumnType::Real => "double precision",
        ColumnType::Text => "text",
        ColumnType::Blob => "bytea",
        ColumnType::Vector(_) => "text",
    }
}

/// Whether registering view `new_name` with body `query` would create a cycle in
/// the view-dependency graph (direct or transitive self-reference). Every stored
/// view is acyclic by this check, so query-time view expansion always terminates.
fn view_would_cycle(
    store: &crate::store::Store,
    new_name: &str,
    query: &crate::sql::SelectStmt,
) -> bool {
    let mut stack = sql::referenced_relations(query);
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    while let Some(n) = stack.pop() {
        if n.eq_ignore_ascii_case(new_name) {
            return true;
        }
        if !seen.insert(n.to_ascii_lowercase()) {
            continue;
        }
        if let Some(view_query) = store.view(&n) {
            stack.extend(sql::referenced_relations(view_query));
        }
    }
    false
}

impl Connection {
    pub fn open(url: &str) -> Result<Connection> {
        Ok(Connection {
            db: Database::open(url)?,
            txn: None,
            last_error: CString::default(),
            last_changes: 0,
            last_lsn: 0,
            vector_ef_search: None,
        })
    }

    pub fn in_transaction(&self) -> bool {
        self.txn.is_some()
    }

    /// VH-1 observability: how many vector indexes this connection's database
    /// warmed from a page checkpoint on cold open (rather than rebuilding from the
    /// rows). Equivalent result either way; this confirms the page path engaged.
    pub fn vector_pages_loaded(&self) -> u64 {
        self.db.index_pages_loaded()
    }

    /// A read-only [`EngineStats`](crate::EngineStats) snapshot of the shared
    /// database — the engine + storage observability surface (#53 / spec 15),
    /// surfaced over pgwire by the server's `SHOW twill.stats`.
    pub fn stats(&self) -> crate::EngineStats {
        self.db.stats()
    }

    /// Reflect the catalog (tables + columns) for wire-protocol introspection
    /// (e.g. the pgwire server answering a PostgREST schema-cache query). Returns
    /// tables sorted by name, each column carrying its Postgres type name and
    /// key/nullability flags.
    pub fn catalog(&self) -> Vec<CatalogTable> {
        self.db
            .catalog()
            .into_iter()
            .map(|s| CatalogTable {
                name: s.name,
                columns: s
                    .columns
                    .into_iter()
                    .enumerate()
                    .map(|(i, c)| CatalogColumn {
                        name: c.name,
                        pg_type: pg_type_name(c.ty),
                        not_null: c.not_null,
                        primary_key: c.primary_key,
                        position: (i + 1) as i32,
                    })
                    .collect(),
                foreign_keys: s
                    .foreign_keys
                    .into_iter()
                    .map(|fk| CatalogForeignKey {
                        name: fk.name,
                        columns: fk.columns,
                        foreign_table: fk.foreign_table,
                        foreign_columns: fk.foreign_columns,
                    })
                    .collect(),
            })
            .collect()
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
            vector_ef_search: None,
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

    /// Snapshot LSN and, when reading inside our own write transaction, the
    /// owner id so the read sees this transaction's own pending changes. A
    /// read-only transaction or autocommit read sees only committed rows.
    fn read_snapshot(&self) -> (u64, Option<u64>) {
        match &self.txn {
            Some(t) if t.writer => (t.snapshot, Some(t.owner)),
            Some(t) => (t.snapshot, None),
            None => (self.db.committed_lsn(), None),
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
            Stmt::Savepoint(name) => {
                self.savepoint(name)?;
                Ok(ResultSet::default())
            }
            Stmt::ReleaseSavepoint(name) => {
                self.release_savepoint(name)?;
                Ok(ResultSet::default())
            }
            Stmt::RollbackTo(name) => {
                self.rollback_to(name)?;
                Ok(ResultSet::default())
            }
            Stmt::Select(sel) => {
                let (snapshot, writer) = self.read_snapshot();
                let store = self.db.store.read().unwrap();
                let rs =
                    run_select_tuned(&store, sel, snapshot, writer, params, self.vector_ef_search)?;
                self.last_changes = 0;
                Ok(rs)
            }
            Stmt::CreateTable { .. }
            | Stmt::DropTable { .. }
            | Stmt::CreateIndex { .. }
            | Stmt::DropIndex { .. }
            | Stmt::AlterTable { .. }
            | Stmt::CreateView { .. }
            | Stmt::DropView { .. } => {
                self.exec_ddl(stmt)?;
                Ok(ResultSet::default())
            }
            Stmt::Insert { .. } | Stmt::Update { .. } | Stmt::Delete { .. } => {
                self.exec_dml(stmt, params)
            }
            // Accepted-and-ignored session statements (stage 6E).
            Stmt::Noop => Ok(ResultSet::default()),
            // VH-3: the per-session HNSW recall knob.
            Stmt::SetVectorEf(value) => {
                self.vector_ef_search = *value;
                self.last_changes = 0;
                Ok(ResultSet::default())
            }
            Stmt::Show(name) if name.eq_ignore_ascii_case(sql::VECTOR_EF_SEARCH_GUC) => {
                let value = match self.vector_ef_search {
                    Some(n) => n.to_string(),
                    None => String::new(),
                };
                Ok(ResultSet {
                    columns: vec![sql::VECTOR_EF_SEARCH_GUC.to_string()],
                    types: vec![ColumnType::Text],
                    rows: vec![vec![Value::Text(value)]],
                })
            }
            Stmt::Show(name) => Ok(show_result(name)),
            Stmt::Explain(inner) => Ok(explain_result(inner)),
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
            owner: crate::db::next_owner(),
            savepoints: Vec::new(),
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
            self.db.store.write().unwrap().rollback_owner(txn.owner);
            self.db.lane.release();
        }
        Ok(())
    }

    /// `SAVEPOINT name` (stage 6D): record the writer's current pending state so
    /// a later `ROLLBACK TO` can restore it. A redefined name shadows the old one.
    pub fn savepoint(&mut self, name: &str) -> Result<()> {
        let txn = self
            .txn
            .as_mut()
            .ok_or_else(|| EngineError::txn("SAVEPOINT requires an active transaction"))?;
        let (inserted, deleted) = self.db.store.read().unwrap().pending_snapshot(txn.owner);
        let wal_len = txn.wal_ops.len();
        txn.savepoints.push(Savepoint {
            name: name.to_string(),
            inserted,
            deleted,
            wal_len,
        });
        Ok(())
    }

    /// `RELEASE [SAVEPOINT] name`: drop the named savepoint and any set after it
    /// (their effects remain part of the transaction).
    pub fn release_savepoint(&mut self, name: &str) -> Result<()> {
        let txn = self
            .txn
            .as_mut()
            .ok_or_else(|| EngineError::txn("RELEASE requires an active transaction"))?;
        let at = txn
            .savepoints
            .iter()
            .rposition(|s| s.name.eq_ignore_ascii_case(name))
            .ok_or_else(|| EngineError::txn(format!("no such savepoint: {name}")))?;
        txn.savepoints.truncate(at);
        Ok(())
    }

    /// `ROLLBACK TO [SAVEPOINT] name`: undo pending changes since the savepoint
    /// (keeping the savepoint itself, per SQL), discarding any later savepoints.
    pub fn rollback_to(&mut self, name: &str) -> Result<()> {
        let txn = self
            .txn
            .as_mut()
            .ok_or_else(|| EngineError::txn("ROLLBACK TO requires an active transaction"))?;
        let at = txn
            .savepoints
            .iter()
            .rposition(|s| s.name.eq_ignore_ascii_case(name))
            .ok_or_else(|| EngineError::txn(format!("no such savepoint: {name}")))?;
        if txn.writer {
            let sp = &txn.savepoints[at];
            self.db.store.write().unwrap().rollback_to_savepoint(
                txn.owner,
                &sp.inserted,
                &sp.deleted,
            );
        }
        let wal_len = txn.savepoints[at].wal_len;
        txn.wal_ops.truncate(wal_len);
        txn.savepoints.truncate(at + 1);
        Ok(())
    }

    /// Committing → Committed: hand the WAL batch to the group-commit coordinator,
    /// which coalesces it with concurrent commits into one durable append and
    /// returns the commit LSN once durable. Never acks before durable. The
    /// coordinator takes over the write lane (releasing it after enqueue) and, on
    /// failure, discards this transaction's pending versions.
    fn finish_commit(&mut self, txn: Txn) -> Result<()> {
        if txn.writer {
            if !txn.wal_ops.is_empty() {
                let mut records: Vec<WalRecord> =
                    txn.wal_ops.iter().map(|op| op.encode()).collect();
                records.push(WalOp::Commit.encode());
                let commit_lsn = self.db.group_commit.commit(&self.db, txn.owner, records)?;
                self.last_lsn = commit_lsn as i64;
            } else {
                // No durable work to do; just release the lane we hold.
                self.db.lane.release();
            }
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

    fn exec_dml(&mut self, stmt: &Stmt, params: &[Value]) -> Result<ResultSet> {
        let implicit = self.txn.is_none();
        if implicit {
            self.txn = Some(Txn {
                snapshot: self.db.committed_lsn(),
                writer: false,
                wal_ops: Vec::new(),
                owner: crate::db::next_owner(),
                savepoints: Vec::new(),
            });
        }
        self.ensure_writer();
        let wc = WriteCtx {
            snapshot: self.txn.as_ref().unwrap().snapshot,
            owner: self.txn.as_ref().unwrap().owner,
            params,
        };

        let result = {
            let mut store = self.db.store.write().unwrap();
            match stmt {
                Stmt::Insert {
                    table,
                    columns,
                    source,
                    on_conflict,
                    returning,
                } => run_insert(
                    &mut store,
                    table,
                    columns,
                    source,
                    on_conflict,
                    returning.as_deref(),
                    &wc,
                ),
                Stmt::Update {
                    table,
                    sets,
                    from,
                    filter,
                    returning,
                } => run_update(
                    &mut store,
                    table,
                    sets,
                    from.as_ref(),
                    filter,
                    returning.as_deref(),
                    &wc,
                ),
                Stmt::Delete {
                    table,
                    using,
                    filter,
                    returning,
                } => run_delete(
                    &mut store,
                    table,
                    using.as_ref(),
                    filter,
                    returning.as_deref(),
                    &wc,
                ),
                _ => unreachable!(),
            }
        };

        match result {
            Ok(mutation) => {
                self.txn.as_mut().unwrap().wal_ops.extend(mutation.wal);
                self.last_changes = mutation.changes;
                if implicit {
                    let txn = self.txn.take().unwrap();
                    self.finish_commit(txn)?;
                }
                Ok(mutation.result.unwrap_or_default())
            }
            Err(e) => {
                // The statement is atomic (store untouched on failure). For an
                // implicit (autocommit) txn, tear it down; for an explicit txn,
                // keep prior pending changes and surface the error.
                if implicit {
                    let txn = self.txn.take().unwrap();
                    if txn.writer {
                        self.db.store.write().unwrap().rollback_owner(txn.owner);
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
        // DDL is autocommit and appends directly (not via the group-commit
        // coordinator), so drain any in-flight commits first: holding the lane
        // blocks new ones from starting, and quiesce waits for those already
        // queued, giving DDL a consistent, exclusive point to run.
        self.db.group_commit.quiesce();
        let res = self.do_ddl(stmt);
        self.db.lane.release();
        self.last_changes = 0;
        if let Ok(Some(lsn)) = &res {
            self.last_lsn = *lsn as i64;
        }
        res.map(|_| ())
    }

    /// Resolve parsed foreign keys into catalog form: default the referenced
    /// columns to the referenced table's primary key, validate the column counts
    /// agree, and synthesize a `<table>_<cols>_fkey` name when none was declared.
    /// The engine does not enforce referential integrity in this phase; FKs are
    /// metadata for the pgwire server to reflect into PostgREST's schema cache.
    fn resolve_foreign_keys(
        &self,
        table: &str,
        local_columns: &[crate::catalog::Column],
        specs: &[crate::sql::ForeignKeySpec],
    ) -> Result<Vec<crate::catalog::ForeignKey>> {
        let store = self.db.store.read().unwrap();
        // The primary-key column(s) of a referenced table — looked up in the
        // store, or among the columns being created for a self-reference.
        let referenced_pk = |ft: &str| -> Option<Vec<String>> {
            let pk_of = |cols: &[crate::catalog::Column]| -> Vec<String> {
                cols.iter()
                    .filter(|c| c.primary_key)
                    .map(|c| c.name.clone())
                    .collect()
            };
            let pk = if ft.eq_ignore_ascii_case(table) {
                pk_of(local_columns)
            } else {
                pk_of(&store.table(ft)?.schema.columns)
            };
            (!pk.is_empty()).then_some(pk)
        };

        let mut out = Vec::with_capacity(specs.len());
        for fk in specs {
            let foreign_columns = if fk.foreign_columns.is_empty() {
                referenced_pk(&fk.foreign_table).ok_or_else(|| {
                    EngineError::sql(format!(
                        "foreign key on {table} references {} which has no known primary key",
                        fk.foreign_table
                    ))
                })?
            } else {
                fk.foreign_columns.clone()
            };
            if foreign_columns.len() != fk.columns.len() {
                return Err(EngineError::sql(format!(
                    "foreign key on {table}: {} local column(s) reference {} column(s)",
                    fk.columns.len(),
                    foreign_columns.len()
                )));
            }
            let name = fk
                .name
                .clone()
                .unwrap_or_else(|| format!("{table}_{}_fkey", fk.columns.join("_")));
            out.push(crate::catalog::ForeignKey {
                name,
                columns: fk.columns.clone(),
                foreign_table: fk.foreign_table.clone(),
                foreign_columns,
            });
        }
        Ok(out)
    }

    fn do_ddl(&self, stmt: &Stmt) -> Result<Option<u64>> {
        match stmt {
            Stmt::CreateTable {
                name,
                columns,
                foreign_keys,
                primary_key,
                uniques,
                checks,
                if_not_exists,
            } => {
                if self.db.store.read().unwrap().has_table(name) {
                    if *if_not_exists {
                        return Ok(None);
                    }
                    return Err(EngineError::sql(format!("table {name} already exists")));
                }
                let mut cols: Vec<crate::catalog::Column> = columns
                    .iter()
                    .map(|c| crate::catalog::Column {
                        name: c.name.clone(),
                        ty: c.ty,
                        primary_key: c.primary_key,
                        not_null: c.not_null,
                        unique: c.unique,
                        autoincrement: c.autoincrement,
                        default_sql: c.default_sql.clone(),
                    })
                    .collect();
                // Fold a table-level PRIMARY KEY (cols) into the columns (a column
                // listed there is primary-key and implicitly NOT NULL).
                for pk in primary_key {
                    let idx = cols
                        .iter()
                        .position(|c| c.name.eq_ignore_ascii_case(pk))
                        .ok_or_else(|| {
                            EngineError::sql(format!("PRIMARY KEY references unknown column {pk}"))
                        })?;
                    cols[idx].primary_key = true;
                    cols[idx].not_null = true;
                }
                let fks = self.resolve_foreign_keys(name, &cols, foreign_keys)?;
                let schema = crate::catalog::TableSchema {
                    name: name.clone(),
                    columns: cols,
                    foreign_keys: fks,
                    checks: checks.clone(),
                    uniques: uniques.clone(),
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
            Stmt::AlterTable { table, action } => self.do_alter(table, action),
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
            Stmt::CreateView {
                name,
                query,
                sql,
                or_replace,
                if_not_exists,
            } => self.do_create_view(name, query, sql, *or_replace, *if_not_exists),
            Stmt::DropView { name, if_exists } => self.do_drop_view(name, *if_exists),
            _ => unreachable!(),
        }
    }

    /// `CREATE [OR REPLACE] VIEW … AS <select>` (deferred 6B item): reject a name
    /// that collides with a table or (without `OR REPLACE`) an existing view and
    /// any definition cycle, durably log the statement text, then register the
    /// parsed body. Autocommit, like `CREATE TABLE`.
    fn do_create_view(
        &self,
        name: &str,
        query: &crate::sql::SelectStmt,
        sql: &str,
        or_replace: bool,
        if_not_exists: bool,
    ) -> Result<Option<u64>> {
        {
            let store = self.db.store.read().unwrap();
            if store.has_table(name) {
                return Err(EngineError::sql(format!(
                    "cannot create view {name}: a table with that name already exists"
                )));
            }
            if store.has_view(name) && !or_replace {
                if if_not_exists {
                    return Ok(None);
                }
                return Err(EngineError::sql(format!("view {name} already exists")));
            }
            if view_would_cycle(&store, name, query) {
                return Err(EngineError::sql(format!(
                    "cannot create view {name}: the definition is recursive (unsupported)"
                )));
            }
        }
        let records = vec![
            WalOp::CreateView {
                name: name.to_string(),
                sql: sql.to_string(),
            }
            .encode(),
            WalOp::Commit.encode(),
        ];
        let commit_lsn =
            block_on(self.db.storage.append_wal(&self.db.token, &records)).map_err(commit_error)?;
        let mut store = self.db.store.write().unwrap();
        store.insert_view(name.to_string(), query.clone());
        store.committed_lsn = store.committed_lsn.max(commit_lsn.0);
        Ok(Some(commit_lsn.0))
    }

    fn do_drop_view(&self, name: &str, if_exists: bool) -> Result<Option<u64>> {
        if !self.db.store.read().unwrap().has_view(name) {
            if if_exists {
                return Ok(None);
            }
            return Err(EngineError::sql(format!("no such view: {name}")));
        }
        let records = vec![
            WalOp::DropView {
                name: name.to_string(),
            }
            .encode(),
            WalOp::Commit.encode(),
        ];
        let commit_lsn =
            block_on(self.db.storage.append_wal(&self.db.token, &records)).map_err(commit_error)?;
        let mut store = self.db.store.write().unwrap();
        store.drop_view(name);
        store.committed_lsn = store.committed_lsn.max(commit_lsn.0);
        Ok(Some(commit_lsn.0))
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
        {
            let mut store = self.db.store.write().unwrap();
            store.create_index(def);
            store.committed_lsn = store.committed_lsn.max(commit_lsn.0);
        }
        // VH-1: checkpoint the freshly built graph as pages so a cold reopen can
        // load it without replaying every vector through HNSW insertion. The lane
        // is held (autocommit DDL), so this is an exclusive, consistent point.
        self.db.checkpoint_vector_index(name, commit_lsn.0);
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

    /// `ALTER TABLE` (stage 6D): durably log the schema mutation, then apply it
    /// to the in-memory store (reshaping row versions for column add/drop).
    fn do_alter(&self, table: &str, action: &crate::sql::AlterAction) -> Result<Option<u64>> {
        use crate::sql::AlterAction;
        if !self.db.store.read().unwrap().has_table(table) {
            return Err(EngineError::sql(format!("no such table: {table}")));
        }
        let op = match action {
            AlterAction::AddColumn(spec) => {
                if self
                    .db
                    .store
                    .read()
                    .unwrap()
                    .table(table)
                    .is_some_and(|t| t.schema.column_index(&spec.name).is_some())
                {
                    return Err(EngineError::sql(format!(
                        "column {} already exists in {table}",
                        spec.name
                    )));
                }
                WalOp::AlterAddColumn {
                    table: table.to_string(),
                    column: crate::catalog::Column {
                        name: spec.name.clone(),
                        ty: spec.ty,
                        // A freshly added column cannot be the primary key.
                        primary_key: false,
                        not_null: spec.not_null,
                        unique: spec.unique,
                        autoincrement: spec.autoincrement,
                        default_sql: spec.default_sql.clone(),
                    },
                }
            }
            AlterAction::DropColumn { name, if_exists } => {
                let exists = self
                    .db
                    .store
                    .read()
                    .unwrap()
                    .table(table)
                    .is_some_and(|t| t.schema.column_index(name).is_some());
                if !exists {
                    if *if_exists {
                        return Ok(None);
                    }
                    return Err(EngineError::sql(format!("no such column: {table}.{name}")));
                }
                WalOp::AlterDropColumn {
                    table: table.to_string(),
                    column: name.clone(),
                }
            }
            AlterAction::RenameColumn { from, to } => WalOp::AlterRenameColumn {
                table: table.to_string(),
                from: from.clone(),
                to: to.clone(),
            },
            AlterAction::RenameTable { to } => WalOp::AlterRenameTable {
                table: table.to_string(),
                to: to.clone(),
            },
        };
        let records = vec![op.encode(), WalOp::Commit.encode()];
        let commit_lsn =
            block_on(self.db.storage.append_wal(&self.db.token, &records)).map_err(commit_error)?;
        let mut store = self.db.store.write().unwrap();
        crate::db::apply_alter(&mut store, &op);
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
                    s.rollback_owner(txn.owner);
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
