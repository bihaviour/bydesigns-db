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
    Rows {
        columns: Vec<String>,
        rows: Vec<Vec<Value>>,
        tag: String,
    },
    /// A canned CommandComplete tag with no rows (e.g. `SET`).
    Tag(String),
    /// Not intercepted — hand the statement to the engine.
    Pass,
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
        rows: vec![vec![Value::Text(value.to_string())]],
        tag: "SELECT 1".to_string(),
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

    // PostgREST's per-role config-settings query reads `pg_db_role_setting`
    // (custom GUCs set via ALTER ROLE). The engine has no such catalog and no
    // role settings, so the correct answer is zero rows — and PostgREST blocks
    // (retries) until this query succeeds, so it must not fall through and fail.
    if s.contains("pg_db_role_setting") {
        return Canned::Rows {
            columns: vec!["key".to_string(), "value".to_string()],
            rows: vec![],
            tag: "SELECT 0".to_string(),
        };
    }
    // PostgREST's other role-settings query (per-role GUCs via ALTER ROLE,
    // joined against pg_settings). No roles carry settings here → zero rows.
    if s.contains("pg_auth_members") {
        return Canned::Rows {
            columns: vec![
                "rolname".to_string(),
                "iso_lvl".to_string(),
                "role_settings".to_string(),
            ],
            rows: vec![],
            tag: "SELECT 0".to_string(),
        };
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
}
