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
    /// Not intercepted — hand the statement to the engine.
    Pass,
}

/// Which PostgREST schema-cache query to reflect from the catalog.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ReflectKind {
    /// The tables/columns/PK query (`pg_relation_is_updatable`).
    Tables,
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

/// Inspect a statement; return a canned answer or [`Canned::Pass`].
pub fn intercept(sql: &str, user: &str, database: &str) -> Canned {
    let s = sql.trim().trim_end_matches(';').trim().to_ascii_lowercase();

    // Session-setup commands drivers fire and forget.
    if s.starts_with("set ") || s == "begin read only" {
        return Canned::Tag("SET".to_string());
    }
    if s.starts_with("discard ") {
        return Canned::Tag("DISCARD ALL".to_string());
    }
    // LISTEN / UNLISTEN — PostgREST opens a `pgrst` channel for schema-cache
    // reload notifications. The engine has no pub/sub; accept the command as a
    // no-op (the client simply never receives async notifications).
    if s.starts_with("listen ") {
        return Canned::Tag("LISTEN".to_string());
    }
    if s.starts_with("unlisten ") {
        return Canned::Tag("UNLISTEN".to_string());
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
    // The engine does not yet track FK constraints → no relationships (so no
    // resource embedding until FK metadata lands).
    if s.contains("pks_uniques_cols") {
        return empty_rows(&[
            "table_schema",
            "table_name",
            "foreign_table_schema",
            "foreign_table_name",
            "is_self",
            "constraint_name",
            "cols_and_fcols",
            "one_to_one",
        ]);
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

        // The remaining schema-cache queries are answered with an empty,
        // correctly-shaped result (no catalog needed).
        let cases = [
            ("...JOIN pks_uniques_cols ... contype = 'f'...", 8),
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
