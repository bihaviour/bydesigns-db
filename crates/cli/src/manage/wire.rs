//! The `postgres://` transport (spec 19 Milestone 3): drive a *running*
//! `engine-server` over the Postgres wire protocol, so the same management
//! surface that manages a local `file://` database embedded also manages a live
//! deployment — with the server staying the sole writer and the CLI just a
//! client (the single-writer-safe path the spec mandates for live databases).
//!
//! It reuses `twill-bench`'s hand-rolled, dependency-free [`pgclient`] rather than
//! adding a pgwire dependency. The connection-string scheme picks the transport
//! in [`super::open`], exactly as the engine picks its storage backend; this
//! module is what `postgres://` resolves to.
//!
//! ## What rides the wire
//!
//! `sql` / `shell` / `seed` / `migrate up`/`status` / `db reset` are plain SQL and
//! run unchanged. The *inspect* commands (`tables` / `describe` / `gen types` /
//! `schema dump`) need the catalog, which has no SQL form in the engine; the
//! server reflects it as the plain-text `twill.catalog` / `twill.relationships`
//! surface (the same `#53` mechanism behind `SHOW twill.stats`), which this
//! module reads back and reassembles into the engine's [`CatalogTable`] shape so
//! the renderers are transport-agnostic. Branching and `serve` are storage-seam /
//! server-lifecycle operations with no wire form, so they stay embedded-only
//! (rejected for `postgres://` in [`super`]).

use engine::{
    CatalogColumn, CatalogForeignKey, CatalogTable, ColumnType, EngineStats, ResultSet, Value,
};
use twill_bench::pgclient::{ExecError, PgClient, QueryResult};

/// A management connection over pgwire to a running `engine-server`.
pub struct WireConn {
    client: PgClient,
    /// Affected-row count of the last `exec`, mirroring `engine::Connection`'s
    /// `last_changes` so the renderers report `N row(s) affected` over the wire.
    pub last_changes: i64,
}

impl WireConn {
    /// Connect to the server named by a `postgres://` URL and complete the
    /// pgwire startup handshake.
    pub fn connect(url: &str) -> Result<WireConn, String> {
        let addr = host_port(url)?;
        let client = PgClient::connect(&addr).map_err(|e| format!("connecting to {addr}: {e}"))?;
        Ok(WireConn {
            client,
            last_changes: 0,
        })
    }

    /// Run a query, returning an engine [`ResultSet`] rebuilt from the wire's text
    /// cells. The wire is the text protocol, so every cell comes back as
    /// [`Value::Text`] (or [`Value::Null`]); the renderers print text either way.
    pub fn query(&mut self, sql: &str) -> Result<ResultSet, String> {
        let r = self.run(sql)?;
        self.last_changes = r.affected();
        Ok(to_result_set(r))
    }

    /// Run a statement for its effect, recording the affected-row count.
    pub fn exec(&mut self, sql: &str) -> Result<(), String> {
        let r = self.run(sql)?;
        self.last_changes = r.affected();
        Ok(())
    }

    /// Reflect the live catalog over the wire (`twill.catalog` +
    /// `twill.relationships`) and reassemble it into the engine's shape.
    pub fn catalog(&mut self) -> Result<Vec<CatalogTable>, String> {
        let cols = self.run("SHOW twill.catalog")?;
        let rels = self.run("SHOW twill.relationships")?;
        Ok(assemble_catalog(&cols, &rels))
    }

    /// Pull the `twill.stats` observability surface and rebuild an [`EngineStats`]
    /// so the `stats` renderer is shared with the embedded path.
    pub fn stats(&mut self) -> Result<EngineStats, String> {
        let rows = self.run("SHOW twill.stats")?;
        Ok(assemble_stats(&rows))
    }

    /// Send one statement, mapping a wire error to a flat message.
    fn run(&mut self, sql: &str) -> Result<QueryResult, String> {
        self.client.query_full(sql).map_err(|e| match e {
            ExecError::Conflict => {
                "serialization conflict (40001); retry the statement".to_string()
            }
            ExecError::Fatal(m) => m,
        })
    }
}

/// Build an engine [`ResultSet`] from a wire result. Types are reported as
/// `Text` (the wire is the text protocol); a SQL NULL maps to [`Value::Null`].
fn to_result_set(r: QueryResult) -> ResultSet {
    let types = vec![ColumnType::Text; r.columns.len()];
    let rows = r
        .rows
        .into_iter()
        .map(|row| {
            row.into_iter()
                .map(|cell| cell.map(Value::Text).unwrap_or(Value::Null))
                .collect()
        })
        .collect();
    ResultSet {
        columns: r.columns,
        types,
        rows,
    }
}

/// Reassemble [`CatalogTable`]s from the two reflection result sets. Table order
/// follows first appearance in the `twill.catalog` columns view (which the server
/// emits in catalog order).
fn assemble_catalog(cols: &QueryResult, rels: &QueryResult) -> Vec<CatalogTable> {
    let mut tables: Vec<CatalogTable> = Vec::new();
    for (i, row) in cols.rows.iter().enumerate() {
        // (tbl, col, typ, notnull, pk)
        let tbl = cell(row, 0);
        let ty = parse_column_type(&cell(row, 2));
        let column = CatalogColumn {
            name: cell(row, 1),
            pg_type: pg_type_name(ty),
            ty,
            not_null: cell(row, 3) == "1",
            primary_key: cell(row, 4) == "1",
            position: (i + 1) as i32,
        };
        match tables.iter_mut().find(|t| t.name == tbl) {
            Some(t) => t.columns.push(column),
            None => tables.push(CatalogTable {
                name: tbl,
                columns: vec![column],
                foreign_keys: Vec::new(),
            }),
        }
    }
    for row in &rels.rows {
        // (tbl, name, cols, ftable, fcols)
        let tbl = cell(row, 0);
        let fk = CatalogForeignKey {
            name: cell(row, 1),
            columns: split_list(&cell(row, 2)),
            foreign_table: cell(row, 3),
            foreign_columns: split_list(&cell(row, 4)),
        };
        if let Some(t) = tables.iter_mut().find(|t| t.name == tbl) {
            t.foreign_keys.push(fk);
        }
    }
    tables
}

/// Rebuild an [`EngineStats`] from the `(metric, value)` rows of `twill.stats`,
/// mapping each settled metric name back to its field (the inverse of the
/// server's `stats_rows`). Unknown metric names are ignored, so a server that
/// adds a metric does not break an older CLI.
fn assemble_stats(rows: &QueryResult) -> EngineStats {
    let mut s = EngineStats::default();
    for row in &rows.rows {
        let value: u64 = cell(row, 1).parse().unwrap_or(0);
        match cell(row, 0).as_str() {
            "twill_commit_total" => s.commits = value,
            "twill_durable_append_total" => s.durable_appends = value,
            "twill_committed_lsn" => s.committed_lsn = value,
            "twill_write_lane_acquire_total" => s.write_acquires = value,
            "twill_write_handoff_total" => s.write_handoffs = value,
            "twill_write_wait_us_total" => s.write_wait_us_total = value,
            "twill_storage_wal_appends_total" => s.storage.wal_appends = value,
            "twill_storage_wal_bytes_total" => s.storage.wal_bytes = value,
            "twill_storage_page_reads_total" => s.storage.page_reads = value,
            "twill_storage_page_read_bytes_total" => s.storage.page_read_bytes = value,
            "twill_storage_cache_hits_total" => s.storage.cache_hits = value,
            "twill_storage_cache_misses_total" => s.storage.cache_misses = value,
            "twill_storage_fetch_latency_us_total" => s.storage.fetch_latency_us_total = value,
            "twill_storage_fsync_total" => s.storage.fsyncs = value,
            _ => {}
        }
    }
    s
}

/// The cell at `i` as an owned string (`""` for a missing cell or SQL NULL).
fn cell(row: &[Option<String>], i: usize) -> String {
    row.get(i).and_then(|c| c.clone()).unwrap_or_default()
}

/// Split a comma-joined column list (empty string → no columns).
fn split_list(s: &str) -> Vec<String> {
    if s.is_empty() {
        Vec::new()
    } else {
        s.split(',').map(str::to_string).collect()
    }
}

/// Parse the lowercase SQL type keyword the server emits back into an engine
/// storage class (the inverse of the server's `sql_type_name`). An unrecognized
/// keyword falls back to `Text`, the engine's widest storage class.
fn parse_column_type(s: &str) -> ColumnType {
    match s {
        "integer" => ColumnType::Integer,
        "real" => ColumnType::Real,
        "blob" => ColumnType::Blob,
        s if s.starts_with("vector(") && s.ends_with(')') => s[7..s.len() - 1]
            .parse::<u32>()
            .map(ColumnType::Vector)
            .unwrap_or(ColumnType::Text),
        _ => ColumnType::Text,
    }
}

/// A `&'static str` Postgres type name for a storage class — the field
/// [`CatalogColumn::pg_type`] requires. The CLI's catalog consumers key off `ty`,
/// not `pg_type`, so this only needs to be a valid stand-in.
fn pg_type_name(ty: ColumnType) -> &'static str {
    match ty {
        ColumnType::Integer => "integer",
        ColumnType::Real => "real",
        ColumnType::Blob => "bytea",
        // `vector(N)` flattens to text on the pgwire reflection path, matching the
        // engine's own `catalog()`.
        ColumnType::Text | ColumnType::Vector(_) => "text",
    }
}

/// Extract `host:port` from a `postgres://[user[:pass]@]host[:port][/db][?…]` URL,
/// defaulting to the standard Postgres port `5432` when none is given.
fn host_port(url: &str) -> Result<String, String> {
    let rest = url
        .strip_prefix("postgres://")
        .or_else(|| url.strip_prefix("postgresql://"))
        .ok_or_else(|| format!("not a postgres:// url: {url}"))?;
    // Drop any path/query, then any `user[:pass]@` credentials prefix.
    let authority = rest.split(['/', '?']).next().unwrap_or(rest);
    let hostport = authority.rsplit('@').next().unwrap_or(authority);
    if hostport.is_empty() {
        return Err(format!("missing host in {url}"));
    }
    if hostport.contains(':') {
        Ok(hostport.to_string())
    } else {
        Ok(format!("{hostport}:5432"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_port_parses_the_authority() {
        assert_eq!(
            host_port("postgres://localhost:5433/app").unwrap(),
            "localhost:5433"
        );
        assert_eq!(
            host_port("postgres://user@db.example:5432/x").unwrap(),
            "db.example:5432"
        );
        assert_eq!(
            host_port("postgres://user:pw@10.0.0.1:6000").unwrap(),
            "10.0.0.1:6000"
        );
        // No port → the standard Postgres default.
        assert_eq!(
            host_port("postgres://localhost/app").unwrap(),
            "localhost:5432"
        );
        assert_eq!(host_port("postgresql://host").unwrap(), "host:5432");
        assert!(host_port("file:///tmp/x.db").is_err());
    }

    #[test]
    fn column_type_round_trips_through_the_wire_spelling() {
        assert_eq!(parse_column_type("integer"), ColumnType::Integer);
        assert_eq!(parse_column_type("real"), ColumnType::Real);
        assert_eq!(parse_column_type("text"), ColumnType::Text);
        assert_eq!(parse_column_type("blob"), ColumnType::Blob);
        assert_eq!(parse_column_type("vector(384)"), ColumnType::Vector(384));
        // An unknown keyword widens to text rather than failing the reflection.
        assert_eq!(parse_column_type("jsonb"), ColumnType::Text);
    }
}
