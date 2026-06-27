//! Canned answers for the connect-time introspection queries real clients issue
//! (spec 07 — "MUST answer the small set of introspection queries that Bun.sql
//! and PostgREST issue … failing this is the most common reason a wire-compatible
//! engine fails to connect").
//!
//! These are *not* engine features; they are protocol-handshake glue. Anything
//! not matched here falls through to the engine ([`Canned::Pass`]).

use engine::Value;

/// The server version string advertised in `ParameterStatus` and `version()`.
/// Clients parse the leading `MAJOR.MINOR`; we report a modern Postgres major.
pub const SERVER_VERSION: &str = "15.0";

/// The numeric form of [`SERVER_VERSION`] (`MAJOR*10000 + MINOR`), which
/// `server_version_num` / `current_setting('server_version_num')` report. This
/// is the first thing PostgREST probes on connect; failing it aborts startup.
pub const SERVER_VERSION_NUM: &str = "150000";

/// The outcome of inspecting a statement before it reaches the engine.
pub enum Canned {
    /// A canned result set (column names + rows) and a CommandComplete tag.
    /// `oids` is either empty (infer per-column from the values) or one explicit
    /// Postgres type OID per column (needed when a column carries pre-encoded
    /// binary, e.g. the schema-cache arrays).
    Rows {
        columns: Vec<String>,
        oids: Vec<i32>,
        rows: Vec<Vec<Value>>,
        tag: String,
    },
    /// A canned CommandComplete tag with no rows (e.g. `SET`).
    Tag(String),
    /// A schema-cache query the session must answer by reflecting the live
    /// catalog (the pure classifier here has no catalog access).
    Reflect(ReflectKind),
    /// The `twill.stats` observability surface (#53 / spec 15): the session must
    /// answer it by pulling the live [`engine::EngineStats`] snapshot (the pure
    /// classifier here has no engine access, exactly like `Reflect`).
    Stats,
    /// Not intercepted — hand the statement to the engine.
    Pass,
}

/// Which PostgREST schema-cache query to reflect from the catalog.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ReflectKind {
    /// The tables/columns/PK query (`pg_relation_is_updatable`).
    Tables,
    /// The foreign-key relationship query (`pg_constraint`, `contype='f'`),
    /// which PostgREST turns into resource-embedding relationships.
    Relationships,
}

/// The advertised value of a GUC / run-time setting, for `SHOW` and
/// `current_setting`. Unknown settings report empty (rather than erroring) for
/// maximum client compatibility.
fn setting_value(name: &str) -> &'static str {
    match name {
        "server_version" => SERVER_VERSION,
        "server_version_num" => SERVER_VERSION_NUM,
        "transaction isolation level"
        | "default_transaction_isolation"
        | "transaction_isolation" => "read committed",
        "transaction_read_only" => "off",
        "server_encoding" | "client_encoding" => "UTF8",
        "standard_conforming_strings" => "on",
        "search_path" => "\"$user\", public",
        "max_index_keys" => "32",
        "integer_datetimes" => "on",
        "datestyle" => "ISO, MDY",
        "timezone" => "UTC",
        _ => "",
    }
}

fn one_row(col: &str, value: &str) -> Canned {
    Canned::Rows {
        columns: vec![col.to_string()],
        oids: vec![],
        rows: vec![vec![Value::Text(value.to_string())]],
        tag: "SELECT 1".to_string(),
    }
}

/// A zero-row result with the given column names — used to satisfy introspection
/// queries that have no rows to return here (role settings, the schema cache
/// before catalog reflection lands). Empty rows carry no data, so the column
/// count is all the client decodes; no per-column binary encoding is needed.
fn empty_rows(columns: &[&str]) -> Canned {
    Canned::Rows {
        columns: columns.iter().map(|c| c.to_string()).collect(),
        oids: vec![],
        rows: vec![],
        tag: "SELECT 0".to_string(),
    }
}

/// Whether `s` (lowercased/trimmed) is a session statement that sets the RLS
/// principal — `SET [LOCAL|SESSION] ROLE …`, `SET … twill.jwt.claims` /
/// `request.jwt.claims` (PostgREST's GUC), `SET … twill.rls.bypass`, or the
/// matching `RESET`. These must reach the engine (identity composed at the
/// boundary, enforced in-core), unlike the other fire-and-forget `SET`s. Matched
/// precisely so an unrelated `SET` (which the engine's lexer may reject) is still
/// a no-op.
pub fn is_rls_session_stmt(s: &str) -> bool {
    let body = match s.strip_prefix("set ").or_else(|| s.strip_prefix("reset ")) {
        Some(b) => b.trim_start(),
        None => return false,
    };
    let body = body
        .strip_prefix("local ")
        .or_else(|| body.strip_prefix("session "))
        .map(str::trim_start)
        .unwrap_or(body);
    // The GUC name may be double-quoted (`"request.jwt.claims"`).
    let body = body.trim_start_matches('"');
    for name in [
        "role",
        "twill.jwt.claims",
        "request.jwt.claims",
        "twill.rls.bypass",
    ] {
        if let Some(after) = body.strip_prefix(name) {
            if after.is_empty()
                || after.starts_with(['"', ' ', '\t', '='])
                || after.starts_with(';')
            {
                return true;
            }
        }
    }
    false
}

/// The CommandComplete tag for a fire-and-forget session-setup command
/// (`SET` / `DISCARD` / `LISTEN` / `UNLISTEN`), or `None` if `s` is not one.
/// Drivers issue these on connect and ignore the reply; the engine has no GUCs
/// or pub/sub, so they are accepted as no-ops. Split out of `intercept` to keep
/// that classifier within the project's lizard complexity gate.
fn session_noop_tag(s: &str) -> Option<&'static str> {
    if s.starts_with("set ") || s == "begin read only" {
        Some("SET")
    } else if s.starts_with("discard ") {
        Some("DISCARD ALL")
    } else if s.starts_with("listen ") {
        Some("LISTEN")
    } else if s.starts_with("unlisten ") {
        Some("UNLISTEN")
    } else {
        None
    }
}

/// Whether `s` (already lowercased/trimmed) is one of the accepted spellings of
/// the `twill.stats` observability surface (#53). Split out of `intercept` to
/// keep that classifier's cyclomatic complexity within the project's lizard gate.
fn is_twill_stats_query(s: &str) -> bool {
    matches!(
        s,
        "show twill.stats"
            | "show twill_stats"
            | "select * from twill.stats"
            | "select * from twill_stats"
            | "table twill.stats"
            | "table twill_stats"
    )
}

/// Format a live [`engine::EngineStats`] snapshot into the `twill.stats` result
/// set: `(metric TEXT, value BIGINT)`, one row per signal under the settled #53
/// vocabulary (spec 15). Pure formatting from a passed-in value — this module
/// never reaches the engine; the session pulls the snapshot and calls this. The
/// server reports the engine + storage families it can source from one process;
/// the compute/scheduler family joins it over a controller-driven deployment.
pub fn stats_rows(stats: &engine::EngineStats) -> Canned {
    let st = &stats.storage;
    let metrics: &[(&str, u64)] = &[
        // commit family
        ("twill_commit_total", stats.commits),
        ("twill_durable_append_total", stats.durable_appends),
        ("twill_committed_lsn", stats.committed_lsn),
        // storage family (pulled through the seam)
        ("twill_storage_wal_appends_total", st.wal_appends),
        ("twill_storage_wal_bytes_total", st.wal_bytes),
        ("twill_storage_page_reads_total", st.page_reads),
        ("twill_storage_page_read_bytes_total", st.page_read_bytes),
        ("twill_storage_cache_hits_total", st.cache_hits),
        ("twill_storage_cache_misses_total", st.cache_misses),
        (
            "twill_storage_fetch_latency_us_total",
            st.fetch_latency_us_total,
        ),
        ("twill_storage_fsync_total", st.fsyncs),
    ];
    let rows = metrics
        .iter()
        .map(|(name, v)| vec![Value::Text(name.to_string()), Value::Int(*v as i64)])
        .collect::<Vec<_>>();
    let n = rows.len();
    Canned::Rows {
        columns: vec!["metric".to_string(), "value".to_string()],
        oids: vec![],
        rows,
        tag: format!("SELECT {n}"),
    }
}

/// Inspect a statement; return a canned answer or [`Canned::Pass`].
pub fn intercept(sql: &str, user: &str, database: &str) -> Canned {
    let s = sql.trim().trim_end_matches(';').trim().to_ascii_lowercase();

    // RLS principal statements (`SET ROLE` / `SET … {twill,request}.jwt.claims` /
    // `SET … twill.rls.bypass` and their `RESET`) must reach the engine so the
    // executor can enforce policies against the session principal (Phase 7).
    // Checked before the fire-and-forget `SET` no-op below.
    if is_rls_session_stmt(&s) {
        return Canned::Pass;
    }

    // Session-setup commands drivers fire and forget (SET / DISCARD / LISTEN /
    // UNLISTEN) — grouped into a helper to keep this classifier within the
    // project's lizard complexity gate.
    if let Some(tag) = session_noop_tag(&s) {
        return Canned::Tag(tag.to_string());
    }

    // `twill.stats` — the read-only observability surface (#53 / spec 15),
    // pulled in-band over pgwire. Recognized as a `SHOW`, a view select, or a
    // bare `TABLE`. Matched before the generic `SHOW <name>` so it is not
    // mistaken for a GUC. The session resolves it against the live engine.
    if is_twill_stats_query(&s) {
        return Canned::Stats;
    }

    // SHOW <name> — one row named after the setting.
    if let Some(rest) = s.strip_prefix("show ") {
        let name = rest.trim();
        return one_row(name, setting_value(name));
    }

    // PostgREST's per-request preamble: `SELECT set_config('search_path', $1,
    // true), set_config('role', $2, true), …`. It sets session GUCs (search_path,
    // role, JWT claims) the engine has no notion of, and ignores the returned
    // values — answer with one row of empty strings, one per set_config call.
    if s.starts_with("select set_config(") {
        let n = s.matches("set_config(").count().max(1);
        return Canned::Rows {
            columns: vec!["set_config".to_string(); n],
            oids: vec![],
            rows: vec![vec![Value::Text(String::new()); n]],
            tag: "SELECT 1".to_string(),
        };
    }

    // PostgREST's schema-cache introspection (a recursive pg_catalog query the
    // engine cannot execute). Reflected from the real catalog elsewhere; for now
    // a placeholder empty result with the exact 9-column shape PostgREST decodes.
    if s.contains("pg_relation_is_updatable") {
        return Canned::Reflect(ReflectKind::Tables);
    }

    // PostgREST loads the timezone list (for the `Prefer: timezone=` header).
    // The engine has no tz catalog → empty list.
    if s.contains("pg_timezone_names") {
        return empty_rows(&["name"]);
    }

    // PostgREST's cast introspection (pg_cast — domain ⇄ json/text coercions).
    // No domains/casts here → empty result.
    if s.contains("pg_cast") || s.contains("castsource") {
        return empty_rows(&["castsource", "casttarget", "castfunc"]);
    }

    // PostgREST's computed-relationships query (set-returning functions that act
    // as relationships). No functions here → empty result.
    if s.contains("computed_rels") || s.contains("all_relations") {
        return empty_rows(&[
            "name",
            "rel_table_schema",
            "rel_table_name",
            "rel_ftable_schema",
            "rel_ftable_name",
            "single_row",
            "is_self",
        ]);
    }

    // PostgREST's functions/RPC introspection (pg_proc). No user functions here
    // → empty result (the /rpc surface is simply empty).
    if s.contains("rettype_is_setof") || s.contains("proargmodes") {
        return empty_rows(&[
            "proc_schema",
            "proc_name",
            "proc_description",
            "args",
            "schema",
            "name",
            "rettype_is_setof",
            "rettype_is_composite",
            "rettype_is_composite_alias",
            "provolatile",
            "hasvariadic",
            "transaction_isolation_level",
            "kvs",
        ]);
    }

    // PostgREST's foreign-key relationship query (pg_constraint, contype='f').
    // Reflected from the engine catalog's foreign keys by the session, which
    // turns each into an embeddable relationship.
    if s.contains("pks_uniques_cols") {
        return Canned::Reflect(ReflectKind::Relationships);
    }

    // PostgREST's view-relationship dependency query (recursive, parses view
    // definitions from pg_rewrite). No views here → empty result.
    if s.contains("pks_fks") || s.contains("column_dependencies") {
        return empty_rows(&[
            "table_schema",
            "table_name",
            "view_schema",
            "view_name",
            "constraint_name",
            "constraint_type",
            "column_dependencies",
        ]);
    }

    // PostgREST's per-role config-settings query reads `pg_db_role_setting`
    // (custom GUCs set via ALTER ROLE). The engine has no such catalog and no
    // role settings, so the correct answer is zero rows — and PostgREST blocks
    // (retries) until this query succeeds, so it must not fall through and fail.
    if s.contains("pg_db_role_setting") {
        return empty_rows(&["key", "value"]);
    }
    // PostgREST's other role-settings query (per-role GUCs via ALTER ROLE,
    // joined against pg_settings). No roles carry settings here → zero rows.
    if s.contains("pg_auth_members") {
        return empty_rows(&["rolname", "iso_lvl", "role_settings"]);
    }

    // PostgREST 14.x's exact startup probe (captured from the real client): one
    // SELECT pulling the numeric version, the version string, and version() in a
    // single row. Answer it as a 3-column row so the schema-cache step proceeds.
    let compact: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    if compact.starts_with(
        "selectcurrent_setting('server_version_num')::integer,\
         current_setting('server_version'),version()",
    ) {
        return Canned::Rows {
            columns: vec![
                "server_version_num".to_string(),
                "server_version".to_string(),
                "version".to_string(),
            ],
            oids: vec![],
            rows: vec![vec![
                Value::Int(SERVER_VERSION_NUM.parse().unwrap_or(150000)),
                Value::Text(SERVER_VERSION.to_string()),
                Value::Text(format!(
                    "PostgreSQL {SERVER_VERSION} (twill-db engine-server)"
                )),
            ]],
            tag: "SELECT 1".to_string(),
        };
    }

    // `current_setting('<name>'[, missing_ok])[::type]` — the GUC accessor
    // PostgREST and Bun.sql probe on connect (notably `server_version_num`).
    // Match whitespace-insensitively and stop at the setting name, so any cast
    // suffix, second argument, or alias is tolerated.
    if let Some(rest) = compact.strip_prefix("selectcurrent_setting('") {
        if let Some(end) = rest.find('\'') {
            return one_row("current_setting", setting_value(&rest[..end]));
        }
    }

    // Scalar introspection functions clients probe on connect.
    match s.as_str() {
        "select version()" => {
            return one_row(
                "version",
                &format!("PostgreSQL {SERVER_VERSION} (twill-db engine-server)"),
            )
        }
        "select current_schema()" | "select current_schema" => {
            return one_row("current_schema", "public")
        }
        "select current_database()" | "select current_database" => {
            return one_row("current_database", database)
        }
        "select current_user" | "select user" | "select current_user()" => {
            return one_row("current_user", user)
        }
        "select pg_backend_pid()" => {
            return Canned::Rows {
                columns: vec!["pg_backend_pid".to_string()],
                oids: vec![],
                rows: vec![vec![Value::Int(std::process::id() as i64)]],
                tag: "SELECT 1".to_string(),
            }
        }
        _ => {}
    }

    Canned::Pass
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scalar(sql: &str) -> Option<String> {
        match intercept(sql, "postgres", "srv") {
            Canned::Rows { rows, .. } => match &rows[0][0] {
                Value::Text(s) => Some(s.clone()),
                Value::Int(i) => Some(i.to_string()),
                _ => None,
            },
            _ => None,
        }
    }

    #[test]
    fn version_probe_is_canned() {
        // The exact PostgREST/Bun.sql startup probe, with the cast + whitespace.
        assert_eq!(
            scalar("SELECT current_setting('server_version_num')::integer").as_deref(),
            Some(SERVER_VERSION_NUM)
        );
        assert_eq!(
            scalar("select  current_setting( 'server_version_num' , true )").as_deref(),
            Some(SERVER_VERSION_NUM)
        );
        assert_eq!(
            scalar("SHOW server_version_num").as_deref(),
            Some(SERVER_VERSION_NUM)
        );
    }

    #[test]
    fn common_settings_resolve() {
        assert_eq!(
            scalar("select current_setting('standard_conforming_strings')").as_deref(),
            Some("on")
        );
        assert_eq!(scalar("SHOW client_encoding").as_deref(), Some("UTF8"));
        // Unknown settings report empty rather than erroring (Pass would 500).
        assert_eq!(
            scalar("select current_setting('nonesuch')").as_deref(),
            Some("")
        );
    }

    #[test]
    fn twill_stats_is_intercepted_and_formatted() {
        // All accepted spellings classify as the stats surface, not a GUC.
        for q in [
            "SHOW twill.stats",
            "show twill_stats",
            "SELECT * FROM twill_stats",
            "TABLE twill.stats",
        ] {
            assert!(
                matches!(intercept(q, "u", "d"), Canned::Stats),
                "{q} should intercept to Stats"
            );
        }
        // A normal SHOW is unaffected (still a one-row GUC answer).
        assert_eq!(scalar("SHOW client_encoding").as_deref(), Some("UTF8"));

        // The formatter renders the (metric, value) rows under the settled names.
        let stats = engine::EngineStats {
            commits: 7,
            durable_appends: 3,
            committed_lsn: 42,
            storage: engine::StorageStats {
                wal_appends: 3,
                fsyncs: 4,
                ..Default::default()
            },
        };
        match stats_rows(&stats) {
            Canned::Rows { columns, rows, .. } => {
                assert_eq!(columns, vec!["metric".to_string(), "value".to_string()]);
                let kv: std::collections::HashMap<String, i64> = rows
                    .iter()
                    .map(|r| match (&r[0], &r[1]) {
                        (Value::Text(k), Value::Int(v)) => (k.clone(), *v),
                        other => panic!("unexpected cell {other:?}"),
                    })
                    .collect();
                assert_eq!(kv["twill_commit_total"], 7);
                assert_eq!(kv["twill_durable_append_total"], 3);
                assert_eq!(kv["twill_committed_lsn"], 42);
                assert_eq!(kv["twill_storage_wal_appends_total"], 3);
                assert_eq!(kv["twill_storage_fsync_total"], 4);
            }
            _ => panic!("expected rows from stats_rows"),
        }
    }

    #[test]
    fn data_queries_still_pass_through() {
        assert!(matches!(
            intercept("SELECT * FROM books", "postgres", "srv"),
            Canned::Pass
        ));
        // A column literally named current_setting must not be intercepted.
        assert!(matches!(
            intercept("SELECT current_setting FROM t", "postgres", "srv"),
            Canned::Pass
        ));
    }

    #[test]
    fn postgrest_startup_probes_are_canned() {
        // The combined 3-column version probe PostgREST 14.x issues on connect.
        match intercept(
            "SELECT current_setting('server_version_num')::integer, \
             current_setting('server_version'), version()",
            "postgres",
            "srv",
        ) {
            Canned::Rows { columns, rows, .. } => {
                assert_eq!(columns.len(), 3);
                assert_eq!(rows[0].len(), 3);
                assert!(matches!(rows[0][0], Value::Int(150000)));
            }
            _ => panic!("version probe must be canned"),
        }

        // LISTEN / UNLISTEN are accepted no-ops (no engine pub/sub).
        assert!(matches!(
            intercept("LISTEN \"pgrst\"", "postgres", "srv"),
            Canned::Tag(t) if t == "LISTEN"
        ));

        // Both role-settings queries resolve to an empty result so PostgREST
        // stops retrying and proceeds to load the schema cache.
        for q in [
            "WITH role_setting AS (SELECT 1 FROM pg_catalog.pg_db_role_setting) SELECT 1",
            "with role_setting as (select 1 from pg_auth_members) select 1",
        ] {
            assert!(matches!(
                intercept(q, "postgres", "srv"),
                Canned::Rows { rows, .. } if rows.is_empty()
            ));
        }
    }

    #[test]
    fn schema_cache_queries_are_intercepted_empty() {
        // Each PostgREST schema-cache query (matched by a marker token) must be
        // intercepted with an empty, correctly-shaped result so the engine never
        // sees the un-runnable pg_catalog SQL and the cache loads. Column counts
        // match what PostgREST decodes.
        // The tables query is reflected from the live catalog by the session,
        // so the pure classifier returns the Reflect marker.
        assert!(matches!(
            intercept(
                "...pg_relation_is_updatable(c.oid::regclass, TRUE) & 8...",
                "postgres",
                "srv"
            ),
            Canned::Reflect(ReflectKind::Tables)
        ));

        // The FK relationship query is likewise reflected from the live catalog.
        assert!(matches!(
            intercept(
                "...JOIN pks_uniques_cols ... contype = 'f'...",
                "postgres",
                "srv"
            ),
            Canned::Reflect(ReflectKind::Relationships)
        ));

        // The remaining schema-cache queries are answered with an empty,
        // correctly-shaped result (no catalog needed).
        let cases = [
            ("...with recursive pks_fks as ... column_dependencies...", 7),
            ("...rettype_is_setof ... proargmodes::text[]...", 13),
            ("...all_relations as ( ... computed_rels", 7),
            ("...from pg_cast c ... castsource...", 3),
            ("SELECT name FROM pg_timezone_names", 1),
        ];
        for (sql, ncols) in cases {
            match intercept(sql, "postgres", "srv") {
                Canned::Rows { columns, rows, .. } => {
                    assert_eq!(columns.len(), ncols, "wrong column count for {sql:?}");
                    assert!(rows.is_empty(), "expected empty rows for {sql:?}");
                }
                _ => panic!("schema-cache query not intercepted: {sql:?}"),
            }
        }
    }
}
