//! Per-connection session: the protocol state machine that marshals wire
//! messages onto the *unmodified* engine [`Connection`] and back (spec 07 — "the
//! listener only marshals messages to and from the engine"). One session = one
//! engine handle = one thread; parallelism comes from multiple handles sharing
//! the process-global `Database`, exactly as in the embedded path.

use crate::datapath;
use crate::introspect::{self, Canned, SERVER_VERSION};
use crate::protocol::{read_message, read_startup, Frontend, Out, Startup};
use crate::reflect;
use crate::types::{
    column_type_oid, decode_param, encode_value, infer_column_oid, type_len, OID_TEXT,
};
use engine::{Connection, Value};
use std::collections::HashMap;
use std::io::{self, Read, Write};

/// A parsed (prepared) statement, keyed by name for the connection's lifetime.
struct Prepared {
    /// SQL with Postgres `$n` placeholders rewritten to the engine's `?`.
    sql: String,
    /// Parameter number ($n) for each `?` occurrence, in text order — lets us
    /// bind wire parameters to the engine's positional placeholders even when
    /// `$n` are out of order or repeated.
    order: Vec<usize>,
    /// Highest `$n` seen — the parameter count reported by `Describe`.
    nparams: usize,
    param_oids: Vec<i32>,
    /// How to shape the engine result before sending it (e.g. wrap a PostgREST
    /// read into its `body`/`page_total` response).
    postprocess: Postprocess,
}

/// Post-processing applied to a prepared statement's engine result.
enum Postprocess {
    /// Send the engine result as-is.
    None,
    /// Wrap the rows into PostgREST's data-path response (a single row whose
    /// `body` column is the JSON array of the rows).
    PgrstRead,
    /// PostgREST INSERT (POST): build engine INSERTs from the JSON body param.
    PgrstInsert(datapath::InsertPlan),
    /// PostgREST UPDATE (PATCH): SET values from the JSON body param, filtered by
    /// the de-qualified WHERE clause carrying the remaining `$n` parameters.
    PgrstUpdate(datapath::UpdatePlan),
    /// PostgREST DELETE: the de-qualified WHERE clause + its `$n` parameters.
    PgrstDelete(datapath::DeletePlan),
}

/// A bound portal and its (lazily) materialized result.
struct Portal {
    stmt: String,
    params: Vec<Value>,
    result_formats: Vec<i16>,
    materialized: Option<Mat>,
}

/// A materialized result: column metadata + rows (for SELECT / canned) or just a
/// command tag (for DML / DDL / transaction control). Computed exactly once so
/// `Describe` + `Execute` on the same portal never double-run a statement.
struct Mat {
    columns: Vec<String>,
    /// Type OID per column (from the engine's declared types, or inferred for
    /// canned results). Parallel to `columns`.
    oids: Vec<i32>,
    rows: Vec<Vec<Value>>,
    tag: String,
    has_rows: bool,
}

struct Session {
    conn: Box<Connection>,
    user: String,
    database: String,
    stmts: HashMap<String, Prepared>,
    portals: HashMap<String, Portal>,
    /// In the extended protocol, an error makes the server skip messages until
    /// the next `Sync` (spec 07 — "on error mid-batch, discard until Sync").
    skip_until_sync: bool,
    /// Whether an explicit transaction has entered the failed state.
    failed_txn: bool,
}

/// Serve one client connection to completion. Drives startup/auth, then the
/// command loop, until the client disconnects or sends `Terminate`.
pub fn serve(mut stream: impl Read + Write, db_url: &str) -> io::Result<()> {
    let debug = std::env::var_os("TWILL_WIRE_DEBUG").is_some();
    // Startup: handle an optional SSL/GSS probe (declined), then read params.
    let params = loop {
        match read_startup(&mut stream)? {
            Startup::SslRequest | Startup::GssRequest => {
                if debug {
                    eprintln!("wire: SSL/GSS probe -> declining with 'N'");
                }
                stream.write_all(b"N")?; // no in-process TLS; client may proceed cleartext
                stream.flush()?;
            }
            Startup::Cancel => return Ok(()), // no out-of-band cancel yet
            Startup::Params(p) => {
                if debug {
                    eprintln!("wire: startup params {p:?}");
                }
                break p;
            }
        }
    };

    let user = param(&params, "user").unwrap_or_else(|| "postgres".to_string());
    let database = param(&params, "database").unwrap_or_else(|| user.clone());

    let conn = match Connection::open(db_url) {
        Ok(c) => Box::new(c),
        Err(e) => {
            let mut out = Out::new();
            out.error("FATAL", "08006", &format!("cannot open database: {e}"));
            out.flush_to(&mut stream)?;
            return Ok(());
        }
    };

    // Trust auth: announce readiness and the parameters drivers expect.
    let mut out = Out::new();
    out.auth_ok();
    out.parameter_status("server_version", SERVER_VERSION);
    out.parameter_status("server_encoding", "UTF8");
    out.parameter_status("client_encoding", "UTF8");
    out.parameter_status("DateStyle", "ISO, MDY");
    out.parameter_status("standard_conforming_strings", "on");
    out.parameter_status(
        "application_name",
        &param(&params, "application_name").unwrap_or_default(),
    );
    out.backend_key_data(std::process::id() as i32, 0);
    let mut session = Session {
        conn,
        user,
        database,
        stmts: HashMap::new(),
        portals: HashMap::new(),
        skip_until_sync: false,
        failed_txn: false,
    };
    out.ready_for_query(session.status());
    out.flush_to(&mut stream)?;

    // Command loop.
    while let Some(msg) = read_message(&mut stream)? {
        let mut out = Out::new();
        if debug {
            eprintln!("wire: msg {}", msg_name(&msg));
        }
        match msg {
            Frontend::Query(sql) => {
                session.simple_query(&mut out, &sql);
                out.ready_for_query(session.status());
            }
            Frontend::Parse {
                name,
                sql,
                param_oids,
            } => session.parse(&mut out, name, sql, param_oids),
            Frontend::Bind {
                portal,
                stmt,
                param_formats,
                params,
                result_formats,
            } => session.bind(
                &mut out,
                portal,
                stmt,
                param_formats,
                params,
                result_formats,
            ),
            Frontend::Describe { kind, name } => session.describe(&mut out, kind, name),
            Frontend::Execute { portal, max_rows } => session.execute(&mut out, portal, max_rows),
            Frontend::Close { kind, name } => session.close(&mut out, kind, name),
            Frontend::Sync => {
                session.skip_until_sync = false;
                out.ready_for_query(session.status());
            }
            Frontend::Flush => {}
            Frontend::Terminate => break,
            Frontend::Unsupported(tag) => {
                session.fail(
                    &mut out,
                    "0A000",
                    &format!("unsupported protocol message '{}'", tag as char),
                );
            }
        }
        out.flush_to(&mut stream)?;
    }
    Ok(())
}

impl Session {
    /// Transaction-status byte for `ReadyForQuery`.
    fn status(&self) -> u8 {
        if !self.conn.in_transaction() {
            b'I'
        } else if self.failed_txn {
            b'E'
        } else {
            b'T'
        }
    }

    /// Record an error, and (in the extended protocol) enter skip-until-Sync.
    fn fail(&mut self, out: &mut Out, code: &str, message: &str) {
        out.error("ERROR", code, message);
        if self.conn.in_transaction() {
            self.failed_txn = true;
        }
        self.skip_until_sync = true;
    }

    // ---- simple query protocol ------------------------------------------

    fn simple_query(&mut self, out: &mut Out, sql: &str) {
        log_sql("simple", sql);
        let stmts = split_statements(sql);
        if stmts.is_empty() {
            out.empty_query_response();
            return;
        }
        for stmt in stmts {
            if self.run_simple_one(out, &stmt).is_err() {
                break; // abort the rest of the batch on error
            }
            if !self.conn.in_transaction() {
                self.failed_txn = false;
            }
        }
    }

    /// Inspect a statement, resolving a schema-cache reflection against the live
    /// catalog (the pure `introspect::intercept` cannot reach the engine).
    fn inspect(&self, sql: &str) -> Canned {
        match introspect::intercept(sql, &self.user, &self.database) {
            Canned::Reflect(kind) => reflect::reflect(kind, &self.conn.catalog()),
            other => other,
        }
    }

    fn run_simple_one(&mut self, out: &mut Out, sql: &str) -> Result<(), ()> {
        match self.inspect(sql) {
            Canned::Rows {
                columns,
                oids,
                rows,
                tag,
            } => {
                let oids = resolve_oids(oids, &columns, &rows);
                self.send_rows(out, &columns, &oids, &rows, &[]);
                out.command_complete(&tag);
                Ok(())
            }
            Canned::Tag(tag) => {
                out.command_complete(&tag);
                Ok(())
            }
            Canned::Reflect(_) => unreachable!("inspect() resolves Reflect"),
            Canned::Pass => match self.conn.query(sql) {
                Ok(rs) => {
                    if !rs.columns.is_empty() {
                        let oids = engine_oids(&rs);
                        self.send_rows(out, &rs.columns, &oids, &rs.rows, &[]);
                        out.command_complete(&format!("SELECT {}", rs.rows.len()));
                    } else {
                        let tag = command_tag(&classify(sql), 0, self.conn.last_changes);
                        out.command_complete(&tag);
                    }
                    Ok(())
                }
                Err(e) => {
                    let (code, msg) = describe_error(&e.to_string());
                    out.error("ERROR", code, &msg);
                    if self.conn.in_transaction() {
                        self.failed_txn = true;
                    }
                    Err(())
                }
            },
        }
    }

    // ---- extended query protocol ----------------------------------------

    fn parse(&mut self, out: &mut Out, name: String, sql: String, param_oids: Vec<i32>) {
        if self.skip_until_sync {
            return;
        }
        log_sql("parse", &sql);
        // PostgREST write (POST/PATCH/DELETE): nothing to prepare on the engine —
        // the row values / filters arrive as parameters and are applied at execute.
        if let Some(write) = datapath::parse_write(&sql) {
            let nparams = datapath::max_param(&sql).max(1);
            let postprocess = match write {
                datapath::Write::Insert(p) => Postprocess::PgrstInsert(p),
                datapath::Write::Update(p) => Postprocess::PgrstUpdate(p),
                datapath::Write::Delete(p) => Postprocess::PgrstDelete(p),
            };
            self.stmts.insert(
                name,
                Prepared {
                    sql,
                    order: vec![],
                    nparams,
                    param_oids,
                    postprocess,
                },
            );
            out.parse_complete();
            return;
        }
        // PostgREST read template: can't run on the engine as-is — rewrite it to
        // the inner SELECT and flag the result for body/page_total wrapping.
        let (engine_sql, postprocess) =
            match (datapath::is_read(&sql), datapath::rewrite_read(&sql)) {
                (true, Some(inner)) => (inner, Postprocess::PgrstRead),
                _ => (sql.clone(), Postprocess::None),
            };
        // Intercepted (introspection) statements never reach the engine parser.
        let to_engine = matches!(postprocess, Postprocess::PgrstRead)
            || matches!(
                introspect::intercept(&sql, &self.user, &self.database),
                Canned::Pass
            );
        let prepared = if to_engine {
            let (rewritten, order) = rewrite_placeholders(&engine_sql);
            if let Err(e) = self.conn.prepare(&rewritten) {
                let (code, msg) = describe_error(&e.to_string());
                return self.fail(out, code, &msg);
            }
            let nparams = order.iter().copied().max().unwrap_or(0);
            Prepared {
                sql: rewritten,
                order,
                nparams,
                param_oids,
                postprocess,
            }
        } else {
            Prepared {
                sql,
                order: vec![],
                nparams: 0,
                param_oids,
                postprocess: Postprocess::None,
            }
        };
        self.stmts.insert(name, prepared);
        out.parse_complete();
    }

    fn bind(
        &mut self,
        out: &mut Out,
        portal: String,
        stmt: String,
        param_formats: Vec<i16>,
        params: Vec<Option<Vec<u8>>>,
        result_formats: Vec<i16>,
    ) {
        if self.skip_until_sync {
            return;
        }
        if !self.stmts.contains_key(&stmt) {
            return self.fail(
                out,
                "26000",
                &format!("unknown prepared statement '{stmt}'"),
            );
        }
        let values: Vec<Value> = params
            .iter()
            .enumerate()
            .map(|(i, raw)| decode_param(raw, fmt_at(&param_formats, i)))
            .collect();
        self.portals.insert(
            portal,
            Portal {
                stmt,
                params: values,
                result_formats,
                materialized: None,
            },
        );
        out.bind_complete();
    }

    fn describe(&mut self, out: &mut Out, kind: u8, name: String) {
        if self.skip_until_sync {
            return;
        }
        if kind == b'S' {
            // Statement describe: ParameterDescription, then RowDescription for a
            // row-returning statement (Bun.sql relies on this) or NoData otherwise.
            // The column shape does not depend on parameter *values*, so a SELECT
            // is dummy-run with NULL parameters to learn its columns; a DML
            // statement is never run here (that would be a side effect).
            let Some(prep) = self.stmts.get(&name) else {
                return self.fail(out, "26000", &format!("unknown statement '{name}'"));
            };
            let oids = param_oids_for(prep);
            let sql = prep.sql.clone();
            let order_len = prep.order.len();
            out.parameter_description(&oids);

            // A PostgREST read/write reports a fixed response column shape,
            // independent of the inner query's columns.
            match &prep.postprocess {
                Postprocess::PgrstRead => {
                    let cols: Vec<String> = datapath::BODY_COLUMNS
                        .iter()
                        .map(|c| c.to_string())
                        .collect();
                    out.row_description(&fields(&cols, &datapath::BODY_OIDS, &[]));
                    return;
                }
                Postprocess::PgrstInsert(_)
                | Postprocess::PgrstUpdate(_)
                | Postprocess::PgrstDelete(_) => {
                    let cols: Vec<String> = datapath::WRITE_COLUMNS
                        .iter()
                        .map(|c| c.to_string())
                        .collect();
                    out.row_description(&fields(&cols, &datapath::WRITE_OIDS, &[]));
                    return;
                }
                Postprocess::None => {}
            }

            match self.inspect(&sql) {
                Canned::Rows {
                    columns,
                    oids,
                    rows,
                    ..
                } => {
                    let oids = resolve_oids(oids, &columns, &rows);
                    out.row_description(&fields(&columns, &oids, &[]));
                }
                Canned::Tag(_) => out.no_data(),
                Canned::Reflect(_) => unreachable!("inspect() resolves Reflect"),
                Canned::Pass if is_row_returning(&sql) => match self.dummy_columns(&sql, order_len)
                {
                    Ok(Some((columns, oids))) => out.row_description(&fields(&columns, &oids, &[])),
                    Ok(None) => out.no_data(),
                    Err((code, msg)) => return self.fail(out, &code, &msg),
                },
                Canned::Pass => out.no_data(),
            }
            return;
        }
        // Portal describe.
        match self.materialize(&name) {
            Ok(()) => {
                let p = self.portals.get(&name).unwrap();
                let m = p.materialized.as_ref().unwrap();
                if m.has_rows {
                    out.row_description(&fields(&m.columns, &m.oids, &p.result_formats));
                } else {
                    out.no_data();
                }
            }
            Err((code, msg)) => self.fail(out, &code, &msg),
        }
    }

    fn execute(&mut self, out: &mut Out, portal: String, _max_rows: i32) {
        if self.skip_until_sync {
            return;
        }
        if let Err((code, msg)) = self.materialize(&portal) {
            return self.fail(out, &code, &msg);
        }
        let p = self.portals.get(&portal).unwrap();
        let m = p.materialized.as_ref().unwrap();
        if m.has_rows {
            for row in &m.rows {
                let cols: Vec<Option<Vec<u8>>> = row
                    .iter()
                    .enumerate()
                    .map(|(c, v)| encode_value(v, fmt_at(&p.result_formats, c)))
                    .collect();
                out.data_row(&cols);
            }
        }
        out.command_complete(&m.tag);
        if !self.conn.in_transaction() {
            self.failed_txn = false;
        }
        // Free the cursor; a re-Execute re-materializes.
        if let Some(p) = self.portals.get_mut(&portal) {
            p.materialized = None;
        }
    }

    fn close(&mut self, out: &mut Out, kind: u8, name: String) {
        if kind == b'S' {
            self.stmts.remove(&name);
        } else {
            self.portals.remove(&name);
        }
        out.close_complete();
    }

    /// Run a portal's statement exactly once, caching the result on the portal.
    fn materialize(&mut self, portal: &str) -> Result<(), (String, String)> {
        let Some(p) = self.portals.get(portal) else {
            return Err(("34000".into(), format!("unknown portal '{portal}'")));
        };
        if p.materialized.is_some() {
            return Ok(());
        }
        let prep = self.stmts.get(&p.stmt).ok_or_else(|| {
            (
                "26000".to_string(),
                format!("unknown statement '{}'", p.stmt),
            )
        })?;
        let sql = prep.sql.clone();
        let order = prep.order.clone();
        let params = p.params.clone();
        // Extract what the postprocess paths need without holding the borrow.
        let insert_plan = match &prep.postprocess {
            Postprocess::PgrstInsert(plan) => Some((plan.table.clone(), plan.columns.clone())),
            _ => None,
        };
        let update_plan = match &prep.postprocess {
            Postprocess::PgrstUpdate(plan) => Some((
                plan.table.clone(),
                plan.set_columns.clone(),
                plan.where_clause.clone(),
            )),
            _ => None,
        };
        let delete_plan = match &prep.postprocess {
            Postprocess::PgrstDelete(plan) => Some((plan.table.clone(), plan.where_clause.clone())),
            _ => None,
        };
        let is_read = matches!(prep.postprocess, Postprocess::PgrstRead);

        // A PostgREST read: run the (rewritten) inner SELECT on the engine, then
        // wrap its rows into the body/page_total response shape.
        if is_read {
            let inner = self.run_engine(&sql, &params, &order)?;
            let mat = Mat {
                columns: datapath::BODY_COLUMNS
                    .iter()
                    .map(|c| c.to_string())
                    .collect(),
                oids: datapath::BODY_OIDS.to_vec(),
                rows: vec![datapath::body_row(&inner.columns, &inner.rows)],
                tag: "SELECT 1".to_string(),
                has_rows: true,
            };
            self.portals.get_mut(portal).unwrap().materialized = Some(mat);
            return Ok(());
        }

        // A PostgREST INSERT: parse the JSON body parameter into rows and run an
        // engine INSERT per row; report the count as page_total.
        if let Some((table, columns)) = insert_plan {
            let count = self.run_pgrst_insert(&table, &columns, &params)?;
            self.set_write_result(portal, count);
            return Ok(());
        }

        // A PostgREST UPDATE (PATCH): SET values from the JSON body, filtered by
        // the de-qualified WHERE; report the affected-row count as page_total.
        if let Some((table, columns, where_clause)) = update_plan {
            let count = self.run_pgrst_update(&table, &columns, &where_clause, &params)?;
            self.set_write_result(portal, count);
            return Ok(());
        }

        // A PostgREST DELETE: run the de-qualified DELETE with its `$n` filters.
        if let Some((table, where_clause)) = delete_plan {
            let count = self.run_pgrst_delete(&table, &where_clause, &params)?;
            self.set_write_result(portal, count);
            return Ok(());
        }

        let mat = match self.inspect(&sql) {
            Canned::Rows {
                columns,
                oids,
                rows,
                tag,
            } => Mat {
                oids: resolve_oids(oids, &columns, &rows),
                columns,
                rows,
                tag,
                has_rows: true,
            },
            Canned::Tag(tag) => Mat {
                columns: vec![],
                oids: vec![],
                rows: vec![],
                tag,
                has_rows: false,
            },
            Canned::Reflect(_) => unreachable!("inspect() resolves Reflect"),
            Canned::Pass => self.run_engine(&sql, &params, &order)?,
        };
        self.portals.get_mut(portal).unwrap().materialized = Some(mat);
        Ok(())
    }

    /// Cache a write's response (return=minimal): a single row whose `page_total`
    /// is the affected-row count, in PostgREST's write column shape.
    fn set_write_result(&mut self, portal: &str, count: i64) {
        let mat = Mat {
            columns: datapath::WRITE_COLUMNS
                .iter()
                .map(|c| c.to_string())
                .collect(),
            oids: datapath::WRITE_OIDS.to_vec(),
            rows: vec![datapath::write_body_row(count)],
            tag: "SELECT 1".to_string(),
            has_rows: true,
        };
        self.portals.get_mut(portal).unwrap().materialized = Some(mat);
    }

    /// Execute a PostgREST UPDATE (PATCH): the SET values are the JSON body
    /// parameter (`$1`); the WHERE clause carries the remaining `$n` filters.
    /// One engine UPDATE binds the SET values first, then the filter parameters.
    fn run_pgrst_update(
        &mut self,
        table: &str,
        columns: &[String],
        where_clause: &str,
        params: &[Value],
    ) -> Result<i64, (String, String)> {
        let body = body_text(params)?;
        let row = datapath::json_rows(&body, columns)
            .map_err(|e| ("22023".to_string(), e))?
            .into_iter()
            .next()
            .unwrap_or_default();
        let assignments = columns
            .iter()
            .map(|c| format!("{c} = ?"))
            .collect::<Vec<_>>()
            .join(", ");
        let engine_sql = if where_clause.is_empty() {
            format!("UPDATE {table} SET {assignments}")
        } else {
            format!("UPDATE {table} SET {assignments} WHERE {where_clause}")
        };
        // The SET `?`s pass through unchanged; only the WHERE `$n` are rewritten.
        let (rewritten, where_order) = rewrite_placeholders(&engine_sql);
        let mut st = self
            .conn
            .prepare(&rewritten)
            .map_err(|e| split_err(&e.to_string()))?;
        let mut pos = 1;
        for v in row {
            st.bind(pos, v).map_err(|e| split_err(&e.to_string()))?;
            pos += 1;
        }
        for &num in &where_order {
            let v = params.get(num - 1).cloned().unwrap_or(Value::Null);
            st.bind(pos, v).map_err(|e| split_err(&e.to_string()))?;
            pos += 1;
        }
        st.execute().map_err(|e| split_err(&e.to_string()))?;
        Ok(self.conn.last_changes)
    }

    /// Execute a PostgREST DELETE: the de-qualified WHERE clause carries the
    /// `$n` filter parameters, bound positionally.
    fn run_pgrst_delete(
        &mut self,
        table: &str,
        where_clause: &str,
        params: &[Value],
    ) -> Result<i64, (String, String)> {
        let engine_sql = if where_clause.is_empty() {
            format!("DELETE FROM {table}")
        } else {
            format!("DELETE FROM {table} WHERE {where_clause}")
        };
        let (rewritten, order) = rewrite_placeholders(&engine_sql);
        let mut st = self
            .conn
            .prepare(&rewritten)
            .map_err(|e| split_err(&e.to_string()))?;
        for (j, &num) in order.iter().enumerate() {
            let v = params.get(num - 1).cloned().unwrap_or(Value::Null);
            st.bind(j + 1, v).map_err(|e| split_err(&e.to_string()))?;
        }
        st.execute().map_err(|e| split_err(&e.to_string()))?;
        Ok(self.conn.last_changes)
    }

    /// Execute a PostgREST INSERT: the row data is the JSON body parameter
    /// (`$1`); parse it into rows and run one engine INSERT per row. Returns the
    /// number of rows inserted (PostgREST's `page_total`).
    fn run_pgrst_insert(
        &mut self,
        table: &str,
        columns: &[String],
        params: &[Value],
    ) -> Result<i64, (String, String)> {
        let body = body_text(params)?;
        let rows = datapath::json_rows(&body, columns).map_err(|e| ("22023".to_string(), e))?;
        let placeholders = vec!["?"; columns.len()].join(", ");
        let sql = format!(
            "INSERT INTO {} ({}) VALUES ({})",
            table,
            columns.join(", "),
            placeholders
        );
        let mut count = 0i64;
        for row in rows {
            let mut st = self
                .conn
                .prepare(&sql)
                .map_err(|e| split_err(&e.to_string()))?;
            for (i, v) in row.into_iter().enumerate() {
                st.bind(i + 1, v).map_err(|e| split_err(&e.to_string()))?;
            }
            st.execute().map_err(|e| split_err(&e.to_string()))?;
            count += 1;
        }
        Ok(count)
    }

    /// Execute a parameterized statement on the engine and collect its result.
    /// `order[j]` is the wire parameter number ($n) bound to the j-th engine `?`.
    fn run_engine(
        &mut self,
        sql: &str,
        params: &[Value],
        order: &[usize],
    ) -> Result<Mat, (String, String)> {
        let mut st = self
            .conn
            .prepare(sql)
            .map_err(|e| split_err(&e.to_string()))?;
        for (j, &num) in order.iter().enumerate() {
            let v = params.get(num - 1).cloned().unwrap_or(Value::Null);
            st.bind(j + 1, v).map_err(|e| split_err(&e.to_string()))?;
        }
        st.execute().map_err(|e| split_err(&e.to_string()))?;

        let ncols = st.column_count();
        if ncols > 0 {
            let columns: Vec<String> = (0..ncols)
                .map(|c| st.column_name(c).unwrap_or("").to_string())
                .collect();
            let oids: Vec<i32> = (0..ncols)
                .map(|c| st.column_type(c).map(column_type_oid).unwrap_or(OID_TEXT))
                .collect();
            let mut rows = Vec::new();
            while st.step().map_err(|e| split_err(&e.to_string()))? {
                let row: Vec<Value> = (0..ncols)
                    .map(|c| st.column_value(c).cloned().unwrap_or(Value::Null))
                    .collect();
                rows.push(row);
            }
            let tag = format!("SELECT {}", rows.len());
            Ok(Mat {
                columns,
                oids,
                rows,
                tag,
                has_rows: true,
            })
        } else {
            let changes = self.conn.last_changes;
            Ok(Mat {
                columns: vec![],
                oids: vec![],
                rows: vec![],
                tag: command_tag(&classify(sql), 0, changes),
                has_rows: false,
            })
        }
    }

    /// Run a row-returning statement with NULL parameters purely to learn its
    /// column names (for `Describe` of a statement). Safe only for SELECT-like
    /// statements, which the caller guarantees via [`is_row_returning`].
    #[allow(clippy::type_complexity)]
    fn dummy_columns(
        &mut self,
        sql: &str,
        order_len: usize,
    ) -> Result<Option<(Vec<String>, Vec<i32>)>, (String, String)> {
        let mut st = self
            .conn
            .prepare(sql)
            .map_err(|e| split_err(&e.to_string()))?;
        for j in 0..order_len {
            st.bind(j + 1, Value::Null)
                .map_err(|e| split_err(&e.to_string()))?;
        }
        st.execute().map_err(|e| split_err(&e.to_string()))?;
        let nc = st.column_count();
        if nc > 0 {
            let columns = (0..nc)
                .map(|c| st.column_name(c).unwrap_or("").to_string())
                .collect();
            let oids = (0..nc)
                .map(|c| st.column_type(c).map(column_type_oid).unwrap_or(OID_TEXT))
                .collect();
            Ok(Some((columns, oids)))
        } else {
            Ok(None)
        }
    }

    // ---- shared result encoding -----------------------------------------

    fn send_rows(
        &self,
        out: &mut Out,
        columns: &[String],
        oids: &[i32],
        rows: &[Vec<Value>],
        formats: &[i16],
    ) {
        out.row_description(&fields(columns, oids, formats));
        for row in rows {
            let cols: Vec<Option<Vec<u8>>> = row
                .iter()
                .enumerate()
                .map(|(c, v)| encode_value(v, fmt_at(formats, c)))
                .collect();
            out.data_row(&cols);
        }
    }
}

// ---- SQL capture (corpus collection) --------------------------------------

/// Where captured SQL is written. Configured once from `TWILL_LOG_SQL`:
/// unset/empty → off; `1`/`true`/`stderr`/`-` → stderr; anything else → a file
/// path appended to. This is a debugging/diagnostic aid for building a
/// wire-client SQL corpus (e.g. PostgREST); it logs statement *text* only,
/// never bound parameter values.
enum SqlSink {
    Off,
    Stderr,
    File(std::sync::Mutex<std::fs::File>),
}

fn sql_sink() -> &'static SqlSink {
    static SINK: std::sync::OnceLock<SqlSink> = std::sync::OnceLock::new();
    SINK.get_or_init(|| match std::env::var("TWILL_LOG_SQL") {
        Ok(v) if !v.trim().is_empty() => {
            let v = v.trim();
            if matches!(v, "1" | "true" | "stderr" | "-") {
                SqlSink::Stderr
            } else {
                match std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(v)
                {
                    Ok(f) => SqlSink::File(std::sync::Mutex::new(f)),
                    Err(e) => {
                        eprintln!("[twill] TWILL_LOG_SQL: cannot open {v:?}: {e}; using stderr");
                        SqlSink::Stderr
                    }
                }
            }
        }
        _ => SqlSink::Off,
    })
}

/// Record one received statement (preserving its formatting) with a record
/// separator so multi-line client SQL stays readable and greppable.
fn log_sql(tag: &str, sql: &str) {
    let record = format!("-- [{tag}]\n{}\n", sql.trim_end());
    match sql_sink() {
        SqlSink::Off => {}
        SqlSink::Stderr => eprint!("{record}"),
        SqlSink::File(m) => {
            if let Ok(mut f) = m.lock() {
                let _ = f.write_all(record.as_bytes());
                let _ = f.flush();
            }
        }
    }
}

// ---- helpers --------------------------------------------------------------

fn msg_name(m: &Frontend) -> String {
    match m {
        Frontend::Query(s) => format!("Query({s:?})"),
        Frontend::Parse { sql, .. } => format!("Parse({sql:?})"),
        Frontend::Bind { .. } => "Bind".into(),
        Frontend::Describe { kind, .. } => format!("Describe({})", *kind as char),
        Frontend::Execute { .. } => "Execute".into(),
        Frontend::Sync => "Sync".into(),
        Frontend::Flush => "Flush".into(),
        Frontend::Close { .. } => "Close".into(),
        Frontend::Terminate => "Terminate".into(),
        Frontend::Unsupported(t) => format!("Unsupported({})", *t as char),
    }
}

fn param(params: &[(String, String)], key: &str) -> Option<String> {
    params
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.clone())
}

/// Result/parameter format code at index `i`: empty => all text; len 1 => that
/// code for all columns; otherwise per-index.
fn fmt_at(formats: &[i16], i: usize) -> i16 {
    match formats.len() {
        0 => 0,
        1 => formats[0],
        _ => *formats.get(i).unwrap_or(&0),
    }
}

fn param_oids_for(p: &Prepared) -> Vec<i32> {
    if p.param_oids.len() == p.nparams {
        p.param_oids.clone()
    } else {
        vec![crate::types::OID_TEXT; p.nparams]
    }
}

/// Whether a statement returns rows (so a statement-`Describe` should emit a
/// `RowDescription` rather than `NoData`).
fn is_row_returning(sql: &str) -> bool {
    matches!(
        classify(sql).as_str(),
        "SELECT" | "VALUES" | "WITH" | "SHOW"
    )
}

/// Build a `RowDescription` field list: `(name, type_oid, type_len, format)`.
fn fields(columns: &[String], oids: &[i32], formats: &[i16]) -> Vec<(String, i32, i16, i16)> {
    columns
        .iter()
        .enumerate()
        .map(|(c, name)| {
            let oid = oids.get(c).copied().unwrap_or(OID_TEXT);
            (name.clone(), oid, type_len(oid), fmt_at(formats, c))
        })
        .collect()
}

/// OIDs from the engine's declared column types (accurate even for 0 rows).
fn engine_oids(rs: &engine::ResultSet) -> Vec<i32> {
    (0..rs.columns.len())
        .map(|c| {
            rs.types
                .get(c)
                .map(|t| column_type_oid(*t))
                .unwrap_or(OID_TEXT)
        })
        .collect()
}

/// OIDs inferred from canned-result values (introspection answers have no
/// engine-declared types).
fn infer_oids(columns: &[String], rows: &[Vec<Value>]) -> Vec<i32> {
    (0..columns.len())
        .map(|c| infer_column_oid(rows, c))
        .collect()
}

/// Use a canned result's explicit per-column OIDs when given (a reflected query
/// carrying pre-encoded binary), otherwise infer them from the values.
fn resolve_oids(explicit: Vec<i32>, columns: &[String], rows: &[Vec<Value>]) -> Vec<i32> {
    if explicit.is_empty() {
        infer_oids(columns, rows)
    } else {
        explicit
    }
}

/// Leading SQL keyword, uppercased — used to build the CommandComplete tag.
fn classify(sql: &str) -> String {
    sql.trim_start()
        .chars()
        .take_while(|c| c.is_ascii_alphabetic())
        .collect::<String>()
        .to_ascii_uppercase()
}

fn command_tag(class: &str, rows: usize, changes: i64) -> String {
    match class {
        "SELECT" | "VALUES" | "SHOW" => format!("SELECT {rows}"),
        "INSERT" => format!("INSERT 0 {changes}"),
        "UPDATE" => format!("UPDATE {changes}"),
        "DELETE" => format!("DELETE {changes}"),
        "CREATE" => "CREATE TABLE".to_string(),
        "DROP" => "DROP TABLE".to_string(),
        "BEGIN" | "START" => "BEGIN".to_string(),
        "COMMIT" | "END" => "COMMIT".to_string(),
        "ROLLBACK" | "ABORT" => "ROLLBACK".to_string(),
        "" => "SELECT 0".to_string(),
        other => other.to_string(),
    }
}

/// Map an engine error message to a SQLSTATE + message. A fenced/lost writer
/// gets a defined code (spec 07 MUST); everything else is a syntax/usage error.
fn describe_error(msg: &str) -> (&'static str, String) {
    let lower = msg.to_ascii_lowercase();
    let code =
        if lower.contains("conflict") || lower.contains("fenced") || lower.contains("durability") {
            // serialization_failure: first-committer-wins conflict or a lost-writer
            // step-down. A retry-able class — clients (pgbench --max-tries) may retry.
            "40001"
        } else if lower.contains("transaction") {
            "25000" // invalid_transaction_state
        } else {
            "42601" // syntax_error / generic statement failure
        };
    (code, msg.to_string())
}

fn split_err(msg: &str) -> (String, String) {
    let (c, m) = describe_error(msg);
    (c.to_string(), m)
}

/// The JSON request body for a PostgREST write — always the first parameter
/// (`$1`), sent as text or bytes by the driver.
fn body_text(params: &[Value]) -> Result<String, (String, String)> {
    match params.first() {
        Some(Value::Text(s)) => Ok(s.clone()),
        Some(Value::Blob(b)) => Ok(String::from_utf8_lossy(b).into_owned()),
        _ => Err(("22023".into(), "missing JSON request body".into())),
    }
}

/// Rewrite Postgres `$n` placeholders to the engine's positional `?`, returning
/// the rewritten SQL and, for each `?` in text order, the `$n` number it came
/// from. `$` inside single/double-quoted literals is left untouched.
fn rewrite_placeholders(sql: &str) -> (String, Vec<usize>) {
    let mut out = String::with_capacity(sql.len());
    let mut order = Vec::new();
    let mut in_single = false;
    let mut in_double = false;
    let bytes = sql.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let ch = bytes[i] as char;
        match ch {
            '\'' if !in_double => {
                in_single = !in_single;
                out.push(ch);
                i += 1;
            }
            '"' if !in_single => {
                in_double = !in_double;
                out.push(ch);
                i += 1;
            }
            '$' if !in_single
                && !in_double
                && i + 1 < bytes.len()
                && bytes[i + 1].is_ascii_digit() =>
            {
                let mut j = i + 1;
                let mut num = 0usize;
                while j < bytes.len() && bytes[j].is_ascii_digit() {
                    num = num * 10 + (bytes[j] - b'0') as usize;
                    j += 1;
                }
                out.push('?');
                order.push(num);
                i = j;
            }
            _ => {
                out.push(ch);
                i += 1;
            }
        }
    }
    (out, order)
}

/// Split a simple-query string into individual statements on `;`, respecting
/// single- and double-quoted literals. Trailing whitespace-only fragments drop.
fn split_statements(sql: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_single = false;
    let mut in_double = false;
    for ch in sql.chars() {
        match ch {
            '\'' if !in_double => {
                in_single = !in_single;
                cur.push(ch);
            }
            '"' if !in_single => {
                in_double = !in_double;
                cur.push(ch);
            }
            ';' if !in_single && !in_double => {
                if !cur.trim().is_empty() {
                    out.push(cur.trim().to_string());
                }
                cur.clear();
            }
            _ => cur.push(ch),
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim().to_string());
    }
    out
}
