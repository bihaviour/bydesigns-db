//! PostgREST data-path rewriting (issue #27). PostgREST wraps every read in a
//! fixed template the engine cannot run directly:
//!
//! ```sql
//! WITH pgrst_source AS ( SELECT "public"."t".* FROM "public"."t" <where/order/limit> )
//! SELECT null::bigint AS total_result_set, pg_catalog.count(_postgrest_t) AS page_total,
//!        coalesce(json_agg(_postgrest_t), '[]') AS body, ...
//! FROM ( SELECT * FROM pgrst_source ) _postgrest_t
//! ```
//!
//! Rather than teach the engine CTEs + subqueries + whole-row `json_agg`, the
//! server recognizes this template, extracts the inner `SELECT … FROM <table>
//! <where/order/limit>` (stripping schema/table qualifiers), runs *that* on the
//! engine, and assembles the `body` (a JSON array) + `page_total` itself. This is
//! composition glue (spec 12), kept out of the engine.

use engine::Value;

/// The six columns PostgREST decodes from a data-path result, with their OIDs
/// (`int8`, `int8`, `json`, `text`, `text`, `text`).
pub const BODY_COLUMNS: [&str; 6] = [
    "total_result_set",
    "page_total",
    "body",
    "response_headers",
    "response_status",
    "response_inserted",
];
pub const BODY_OIDS: [i32; 6] = [20, 20, 114, 25, 25, 25];

/// Is this PostgREST's read data-path query?
pub fn is_read(sql: &str) -> bool {
    let s = sql.to_ascii_lowercase();
    s.contains("pgrst_source") && s.contains("json_agg(_postgrest_t)")
}

/// Extract and de-qualify the inner read query so the engine can run it.
/// Returns `None` if the template is not the expected read shape (e.g. a write
/// whose source is an INSERT/UPDATE/DELETE — not yet supported).
pub fn rewrite_read(sql: &str) -> Option<String> {
    let inner = inner_cte(sql)?;
    let trimmed = inner.trim_start();
    if !trimmed[..trimmed.len().min(6)].eq_ignore_ascii_case("select") {
        return None; // a writing CTE — handled elsewhere when implemented
    }
    Some(strip_qualifiers(&inner))
}

/// The text inside `pgrst_source AS ( … )`, matched by balancing parentheses.
fn inner_cte(sql: &str) -> Option<String> {
    let lower = sql.to_ascii_lowercase();
    let marker = "pgrst_source as (";
    let start = lower.find(marker)? + marker.len();
    let bytes = sql.as_bytes();
    let mut depth = 1;
    let mut i = start;
    while i < bytes.len() {
        match bytes[i] {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(sql[start..i].to_string());
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// Drop schema/table qualifiers and quoting from identifier chains: `"a"."b"."c"`
/// → `c`, `"a"."b".*` → `*`, `"a"."b"` → `b`. Numbers, strings, operators and
/// `$n` placeholders pass through unchanged. (Identifiers in this template are
/// plain `[A-Za-z_][A-Za-z0-9_]*`, so unquoting is a simple `"` removal.)
fn strip_qualifiers(inner: &str) -> String {
    let s = inner.replace('"', "");
    let b = s.as_bytes();
    let mut out = String::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c == b'_' || c.is_ascii_alphabetic() {
            let start = i;
            while i < b.len() && (b[i] == b'_' || b[i].is_ascii_alphanumeric()) {
                i += 1;
            }
            // A trailing dot makes this a qualifier (schema/table) we drop; the
            // chain's final segment (the next ident, or `*`) is what we keep.
            if i < b.len() && b[i] == b'.' {
                i += 1; // skip the dot, drop this segment
            } else {
                out.push_str(&s[start..i]);
            }
        } else {
            out.push(c as char);
            i += 1;
        }
    }
    out
}

/// Assemble the single data-path response row from the inner query's result:
/// `body` is the rows as a JSON array; `page_total` is the row count.
pub fn body_row(columns: &[String], rows: &[Vec<Value>]) -> Vec<Value> {
    vec![
        Value::Null,                              // total_result_set (null::bigint)
        Value::Int(rows.len() as i64),            // page_total
        Value::Text(rows_to_json(columns, rows)), // body (json)
        Value::Null,                              // response_headers
        Value::Null,                              // response_status
        Value::Text(String::new()),               // response_inserted
    ]
}

/// Encode a result set as a JSON array of objects (PostgREST's `body`).
fn rows_to_json(columns: &[String], rows: &[Vec<Value>]) -> String {
    let mut out = String::from("[");
    for (r, row) in rows.iter().enumerate() {
        if r > 0 {
            out.push(',');
        }
        out.push('{');
        for (c, name) in columns.iter().enumerate() {
            if c > 0 {
                out.push(',');
            }
            out.push_str(&json_string(name));
            out.push(':');
            out.push_str(&value_json(row.get(c).unwrap_or(&Value::Null)));
        }
        out.push('}');
    }
    out.push(']');
    out
}

fn value_json(v: &Value) -> String {
    match v {
        Value::Null => "null".to_string(),
        Value::Int(i) => i.to_string(),
        Value::Real(r) if r.is_finite() => format!("{r}"),
        Value::Real(_) => "null".to_string(),
        Value::Text(s) => json_string(s),
        Value::Blob(b) => json_string(&base64(b)),
        Value::Vector(_) => "null".to_string(),
    }
}

fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Minimal standard base64 (no padding dependency on engine internals).
fn base64(bytes: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        out.push(T[(n >> 18 & 63) as usize] as char);
        out.push(T[(n >> 12 & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            T[(n >> 6 & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            T[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrites_read_template() {
        let sql = "WITH pgrst_source AS ( SELECT \"public\".\"books\".* FROM \"public\".\"books\" \
                   WHERE \"public\".\"books\".\"price\" > $1 ORDER BY \"public\".\"books\".\"price\" ASC ) \
                   SELECT coalesce(json_agg(_postgrest_t), '[]') AS body FROM ( SELECT * FROM pgrst_source ) _postgrest_t";
        assert!(is_read(sql));
        let engine_sql = rewrite_read(sql).unwrap();
        assert_eq!(
            engine_sql.split_whitespace().collect::<Vec<_>>().join(" "),
            "SELECT * FROM books WHERE price > $1 ORDER BY price ASC"
        );
    }

    #[test]
    fn body_row_builds_json_array() {
        let cols = vec!["id".to_string(), "title".to_string()];
        let rows = vec![
            vec![Value::Int(1), Value::Text("A".into())],
            vec![Value::Int(2), Value::Null],
        ];
        let row = body_row(&cols, &rows);
        assert!(matches!(row[1], Value::Int(2))); // page_total
        let Value::Text(body) = &row[2] else {
            panic!("body must be json text")
        };
        assert_eq!(body, r#"[{"id":1,"title":"A"},{"id":2,"title":null}]"#);
    }
}
