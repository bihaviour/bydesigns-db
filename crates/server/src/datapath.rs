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

/// The columns PostgREST decodes from a *write* data-path result (return=minimal):
/// total_result_set, page_total, header text[], body, response_*.
pub const WRITE_COLUMNS: [&str; 7] = [
    "total_result_set",
    "page_total",
    "header",
    "body",
    "response_headers",
    "response_status",
    "response_inserted",
];
pub const WRITE_OIDS: [i32; 7] = [25, 20, 1009, 25, 25, 25, 25];

/// Is this PostgREST's read data-path query?
pub fn is_read(sql: &str) -> bool {
    let s = sql.to_ascii_lowercase();
    s.contains("pgrst_source") && s.contains("json_agg(_postgrest_t)")
}

/// A parsed PostgREST INSERT (POST): the target table and its column list. The
/// row values arrive separately as the JSON body parameter.
pub struct InsertPlan {
    pub table: String,
    pub columns: Vec<String>,
}

/// Recognize PostgREST's INSERT template and extract the target table + columns:
/// `WITH pgrst_source AS (INSERT INTO "public"."t"("a","b") SELECT … RETURNING …)`.
pub fn parse_insert(sql: &str) -> Option<InsertPlan> {
    let inner = inner_cte(sql)?;
    let lower = inner.to_ascii_lowercase();
    let pos = lower.find("insert into")?;
    let after = inner[pos + "insert into".len()..].trim_start();
    // table name (qualified, quoted), up to '('
    let paren = after.find('(')?;
    let table = last_ident(after[..paren].trim());
    // column list between the first matched parens
    let rest = &after[paren + 1..];
    let close = rest.find(')')?;
    let columns: Vec<String> = rest[..close]
        .split(',')
        .map(|c| unquote(c.trim()))
        .filter(|c| !c.is_empty())
        .collect();
    if table.is_empty() || columns.is_empty() {
        return None;
    }
    Some(InsertPlan { table, columns })
}

/// The single response row for a write (return=minimal): page_total = row count.
pub fn write_body_row(count: i64) -> Vec<Value> {
    vec![
        Value::Text(String::new()),             // total_result_set
        Value::Int(count),                      // page_total
        Value::Blob(empty_text_array_binary()), // header text[] (binary)
        Value::Text(String::new()),             // body
        Value::Null,                            // response_headers
        Value::Null,                            // response_status
        Value::Text(String::new()),             // response_inserted
    ]
}

/// Binary wire form of an empty `text[]` (`{}`): ndim=0, flags=0, elem_oid=text.
/// hasql decodes results in binary, so the `header` column must carry this — not
/// the `{}` text literal, which would fail the binary array decoder.
fn empty_text_array_binary() -> Vec<u8> {
    let mut b = Vec::with_capacity(12);
    b.extend_from_slice(&0i32.to_be_bytes()); // ndim
    b.extend_from_slice(&0i32.to_be_bytes()); // flags
    b.extend_from_slice(&25i32.to_be_bytes()); // text element oid
    b
}

/// The last dotted, optionally-quoted identifier segment: `"public"."t"` → `t`.
fn last_ident(s: &str) -> String {
    unquote(s.rsplit('.').next().unwrap_or(s).trim())
}

fn unquote(s: &str) -> String {
    s.trim().trim_matches('"').to_string()
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

/// Extract insert rows from a PostgREST JSON body (one object, or an array of
/// objects) as engine values in `columns` order. A missing key is NULL. Returns
/// an error string on malformed JSON.
pub fn json_rows(body: &str, columns: &[String]) -> Result<Vec<Vec<Value>>, String> {
    let json = JsonParser::new(body).parse()?;
    let objects = match json {
        Json::Array(items) => items,
        obj @ Json::Object(_) => vec![obj],
        _ => return Err("request body must be a JSON object or array".to_string()),
    };
    let mut rows = Vec::with_capacity(objects.len());
    for obj in objects {
        let Json::Object(fields) = obj else {
            return Err("each element must be a JSON object".to_string());
        };
        let row = columns
            .iter()
            .map(|col| {
                fields
                    .iter()
                    .find(|(k, _)| k == col)
                    .map(|(_, v)| v.to_value())
                    .unwrap_or(Value::Null)
            })
            .collect();
        rows.push(row);
    }
    Ok(rows)
}

/// A minimal JSON value (enough for PostgREST request bodies).
enum Json {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Array(Vec<Json>),
    Object(Vec<(String, Json)>),
}

impl Json {
    /// Coerce a JSON scalar to an engine value (the engine re-coerces to the
    /// column's type on insert). Composite JSON is stored as its text form.
    fn to_value(&self) -> Value {
        match self {
            Json::Null => Value::Null,
            Json::Bool(b) => Value::Int(*b as i64),
            Json::Num(n) if n.fract() == 0.0 && n.abs() < 9e18 => Value::Int(*n as i64),
            Json::Num(n) => Value::Real(*n),
            Json::Str(s) => Value::Text(s.clone()),
            Json::Array(_) | Json::Object(_) => Value::Text(String::new()),
        }
    }
}

/// A tiny recursive-descent JSON parser (no external crate, matching the project
/// style). Sufficient for request bodies: objects, arrays, strings, numbers,
/// booleans, null.
struct JsonParser<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> JsonParser<'a> {
    fn new(s: &'a str) -> Self {
        JsonParser {
            b: s.as_bytes(),
            i: 0,
        }
    }

    fn parse(&mut self) -> Result<Json, String> {
        self.ws();
        let v = self.value()?;
        self.ws();
        if self.i != self.b.len() {
            return Err("trailing JSON content".to_string());
        }
        Ok(v)
    }

    fn ws(&mut self) {
        while self.i < self.b.len() && self.b[self.i].is_ascii_whitespace() {
            self.i += 1;
        }
    }

    fn value(&mut self) -> Result<Json, String> {
        self.ws();
        match self.b.get(self.i) {
            Some(b'{') => self.object(),
            Some(b'[') => self.array(),
            Some(b'"') => Ok(Json::Str(self.string()?)),
            Some(b't') => self.literal("true", Json::Bool(true)),
            Some(b'f') => self.literal("false", Json::Bool(false)),
            Some(b'n') => self.literal("null", Json::Null),
            Some(_) => self.number(),
            None => Err("unexpected end of JSON".to_string()),
        }
    }

    fn literal(&mut self, word: &str, val: Json) -> Result<Json, String> {
        if self.b[self.i..].starts_with(word.as_bytes()) {
            self.i += word.len();
            Ok(val)
        } else {
            Err(format!("invalid JSON literal, expected {word}"))
        }
    }

    fn object(&mut self) -> Result<Json, String> {
        self.i += 1; // {
        let mut fields = Vec::new();
        self.ws();
        if self.b.get(self.i) == Some(&b'}') {
            self.i += 1;
            return Ok(Json::Object(fields));
        }
        loop {
            self.ws();
            let key = self.string()?;
            self.ws();
            if self.b.get(self.i) != Some(&b':') {
                return Err("expected ':' in JSON object".to_string());
            }
            self.i += 1;
            let val = self.value()?;
            fields.push((key, val));
            self.ws();
            match self.b.get(self.i) {
                Some(b',') => self.i += 1,
                Some(b'}') => {
                    self.i += 1;
                    return Ok(Json::Object(fields));
                }
                _ => return Err("expected ',' or '}' in JSON object".to_string()),
            }
        }
    }

    fn array(&mut self) -> Result<Json, String> {
        self.i += 1; // [
        let mut items = Vec::new();
        self.ws();
        if self.b.get(self.i) == Some(&b']') {
            self.i += 1;
            return Ok(Json::Array(items));
        }
        loop {
            items.push(self.value()?);
            self.ws();
            match self.b.get(self.i) {
                Some(b',') => self.i += 1,
                Some(b']') => {
                    self.i += 1;
                    return Ok(Json::Array(items));
                }
                _ => return Err("expected ',' or ']' in JSON array".to_string()),
            }
        }
    }

    fn string(&mut self) -> Result<String, String> {
        if self.b.get(self.i) != Some(&b'"') {
            return Err("expected JSON string".to_string());
        }
        self.i += 1;
        let mut buf: Vec<u8> = Vec::new();
        while let Some(&c) = self.b.get(self.i) {
            match c {
                b'"' => {
                    self.i += 1;
                    return String::from_utf8(buf).map_err(|_| "invalid UTF-8 in JSON".to_string());
                }
                b'\\' => {
                    self.i += 1;
                    let mut ch = [0u8; 4];
                    let s: &str = match self.b.get(self.i) {
                        Some(b'"') => "\"",
                        Some(b'\\') => "\\",
                        Some(b'/') => "/",
                        Some(b'n') => "\n",
                        Some(b't') => "\t",
                        Some(b'r') => "\r",
                        Some(b'b') => "\u{8}",
                        Some(b'f') => "\u{c}",
                        Some(b'u') => {
                            let hex = self.b.get(self.i + 1..self.i + 5).ok_or("bad \\u")?;
                            let code = u32::from_str_radix(
                                std::str::from_utf8(hex).map_err(|_| "bad \\u")?,
                                16,
                            )
                            .map_err(|_| "bad \\u")?;
                            self.i += 4;
                            char::from_u32(code)
                                .unwrap_or('\u{fffd}')
                                .encode_utf8(&mut ch)
                        }
                        _ => return Err("bad JSON escape".to_string()),
                    };
                    buf.extend_from_slice(s.as_bytes());
                    self.i += 1;
                }
                _ => {
                    // Raw byte (UTF-8 multibyte sequences copy through verbatim).
                    buf.push(c);
                    self.i += 1;
                }
            }
        }
        Err("unterminated JSON string".to_string())
    }

    fn number(&mut self) -> Result<Json, String> {
        let start = self.i;
        while let Some(&c) = self.b.get(self.i) {
            if c.is_ascii_digit() || matches!(c, b'-' | b'+' | b'.' | b'e' | b'E') {
                self.i += 1;
            } else {
                break;
            }
        }
        let text = std::str::from_utf8(&self.b[start..self.i]).map_err(|_| "bad number")?;
        text.parse::<f64>()
            .map(Json::Num)
            .map_err(|_| format!("invalid JSON number {text:?}"))
    }
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

    #[test]
    fn parses_insert_template() {
        let sql = "WITH pgrst_source AS (INSERT INTO \"public\".\"authors\"(\"id\", \"name\") \
                   SELECT \"pgrst_body\".\"id\", \"pgrst_body\".\"name\" FROM (SELECT $1 AS json_data) \
                   pgrst_payload RETURNING 1) SELECT '' AS total_result_set FROM (SELECT * FROM pgrst_source) _postgrest_t";
        let plan = parse_insert(sql).expect("insert template");
        assert_eq!(plan.table, "authors");
        assert_eq!(plan.columns, vec!["id".to_string(), "name".to_string()]);
        assert!(parse_insert("SELECT 1").is_none());
    }

    #[test]
    fn json_rows_extracts_values_in_column_order() {
        let cols = vec!["id".to_string(), "name".to_string()];
        // Single object.
        let rows = json_rows(r#"{"name":"Cy","id":3}"#, &cols).unwrap();
        assert_eq!(rows.len(), 1);
        assert!(matches!(rows[0][0], Value::Int(3)));
        assert!(matches!(&rows[0][1], Value::Text(s) if s == "Cy"));
        // Array of objects; a missing key is NULL.
        let rows = json_rows(r#"[{"id":1,"name":"A"},{"id":2}]"#, &cols).unwrap();
        assert_eq!(rows.len(), 2);
        assert!(matches!(rows[1][1], Value::Null));
        // Malformed JSON is an error, not a panic.
        assert!(json_rows("{bad", &cols).is_err());
    }
}
