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

/// A parsed PostgREST UPDATE (PATCH): the target table, the columns being set
/// (their values arrive in the JSON body parameter), and the de-qualified WHERE
/// clause (still carrying its `$n` filter placeholders).
pub struct UpdatePlan {
    pub table: String,
    pub set_columns: Vec<String>,
    pub where_clause: String,
}

/// A parsed PostgREST DELETE: the target table and the de-qualified WHERE clause.
pub struct DeletePlan {
    pub table: String,
    pub where_clause: String,
}

/// A recognized PostgREST write (POST/PATCH/DELETE) data-path statement.
pub enum Write {
    Insert(InsertPlan),
    Update(UpdatePlan),
    Delete(DeletePlan),
}

/// Recognize any PostgREST write template (the `pgrst_source` CTE wrapping an
/// INSERT / UPDATE / DELETE) and extract the engine-runnable plan.
pub fn parse_write(sql: &str) -> Option<Write> {
    if !sql.to_ascii_lowercase().contains("pgrst_source as (") {
        return None;
    }
    parse_insert(sql)
        .map(Write::Insert)
        .or_else(|| parse_update(sql).map(Write::Update))
        .or_else(|| parse_delete(sql).map(Write::Delete))
}

/// Recognize PostgREST's UPDATE template:
/// `WITH pgrst_source AS (UPDATE "public"."t" SET "c" = "pgrst_body"."c"
///  FROM (SELECT $1 AS json_data) … WHERE "public"."t"."id" = $2 RETURNING …)`.
/// Only the SET *column names* are taken (their values come from the JSON body);
/// the WHERE is de-qualified for the engine, keeping its `$n` filter parameters.
pub fn parse_update(sql: &str) -> Option<UpdatePlan> {
    let inner = inner_cte(sql)?;
    let lower = inner.to_ascii_lowercase();
    if !lower.trim_start().starts_with("update ") {
        return None;
    }
    let upd = lower.find("update ")? + "update ".len();
    let set_kw = lower[upd..].find(" set ")? + upd;
    let table = last_ident(inner[upd..set_kw].trim());
    let set_start = set_kw + " set ".len();
    // The SET list ends at the FROM that introduces PostgREST's payload subquery.
    let from_rel = lower[set_start..]
        .find(" from (select")
        .or_else(|| lower[set_start..].find(" from ("))?
        + set_start;
    let set_columns: Vec<String> = inner[set_start..from_rel]
        .split(',')
        .filter_map(|a| a.split('=').next())
        .map(|c| last_ident(c.trim()))
        .filter(|c| !c.is_empty())
        .collect();
    let where_clause = extract_where(&inner, &lower, set_start).unwrap_or_default();
    if table.is_empty() || set_columns.is_empty() {
        return None;
    }
    Some(UpdatePlan {
        table,
        set_columns,
        where_clause,
    })
}

/// Recognize PostgREST's DELETE template:
/// `WITH pgrst_source AS (DELETE FROM "public"."t" WHERE … RETURNING …)`.
pub fn parse_delete(sql: &str) -> Option<DeletePlan> {
    let inner = inner_cte(sql)?;
    let lower = inner.to_ascii_lowercase();
    let trimmed = lower.trim_start();
    if !trimmed.starts_with("delete from") {
        return None;
    }
    let df = lower.find("delete from")? + "delete from".len();
    let table_end = lower[df..]
        .find(" where ")
        .or_else(|| lower[df..].find(" returning"))
        .map(|p| p + df)
        .unwrap_or(inner.len());
    let table = last_ident(inner[df..table_end].trim());
    let where_clause = extract_where(&inner, &lower, df).unwrap_or_default();
    if table.is_empty() {
        return None;
    }
    Some(DeletePlan {
        table,
        where_clause,
    })
}

/// The de-qualified WHERE clause (without the keyword) between ` where ` and the
/// trailing ` returning`/end, searching from `from`. `None` if there is no WHERE.
fn extract_where(inner: &str, lower: &str, from: usize) -> Option<String> {
    let wpos = lower[from..].find(" where ")? + from + " where ".len();
    let end = lower[wpos..]
        .find(" returning")
        .map(|p| p + wpos)
        .unwrap_or(inner.len());
    Some(strip_qualifiers(inner[wpos..end].trim()))
}

/// The highest `$n` placeholder in a template — the wire parameter count the
/// server must report at `Describe` (e.g. body `$1` + filter `$2` ⇒ 2).
pub fn max_param(sql: &str) -> usize {
    let b = sql.as_bytes();
    let (mut max, mut i) = (0usize, 0usize);
    while i < b.len() {
        if b[i] == b'$' && b.get(i + 1).is_some_and(u8::is_ascii_digit) {
            let mut j = i + 1;
            let mut n = 0usize;
            while j < b.len() && b[j].is_ascii_digit() {
                n = n * 10 + (b[j] - b'0') as usize;
                j += 1;
            }
            max = max.max(n);
            i = j;
        } else {
            i += 1;
        }
    }
    max
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
/// whose source is an INSERT/UPDATE/DELETE — not yet supported, or an embedding
/// read with a `LEFT JOIN LATERAL`, which [`parse_embed`] handles instead).
pub fn rewrite_read(sql: &str) -> Option<String> {
    let inner = inner_cte(sql)?;
    let trimmed = inner.trim_start();
    if !trimmed[..trimmed.len().min(6)].eq_ignore_ascii_case("select") {
        return None; // a writing CTE — handled elsewhere when implemented
    }
    if inner.to_ascii_lowercase().contains("left join lateral") {
        return None; // an embedding read — see parse_embed
    }
    Some(strip_qualifiers(&inner))
}

// ---- resource embedding (FK-based) ----------------------------------------
//
// PostgREST renders `?select=col,rel(col)` as a `pgrst_source` CTE whose source
// is the base table LEFT JOIN LATERAL'd onto each embedded relation, with the
// embed materialized by `row_to_json` (to-one) or `json_agg` (to-many). The
// engine runs neither LATERAL nor those JSON aggregates, so the server decomposes
// the template into plain per-relation engine queries and assembles the nested
// JSON itself (a nested-loop join in composition glue — spec 12, out of the core).
//
// Forward (many-to-one) `/books?select=title,authors(name)`:
//   SELECT "public"."books"."title",
//          row_to_json("books_authors_1".*)::jsonb AS "authors"
//   FROM "public"."books"
//   LEFT JOIN LATERAL ( SELECT "authors_1"."name"
//                       FROM "public"."authors" AS "authors_1"
//                       WHERE "authors_1"."id" = "public"."books"."author_id" )
//        AS "books_authors_1" ON TRUE
//
// Reverse (one-to-many) `/authors?select=name,books(title)`:
//   SELECT "public"."authors"."name",
//          COALESCE("authors_books_1"."authors_books_1", '[]') AS "books"
//   FROM "public"."authors"
//   LEFT JOIN LATERAL ( SELECT json_agg("authors_books_1")::jsonb AS "authors_books_1"
//                       FROM (SELECT "books_1"."title"
//                             FROM "public"."books" AS "books_1"
//                             WHERE "books_1"."author_id" = "public"."authors"."id")
//                            AS "authors_books_1" )
//        AS "authors_books_1" ON TRUE

/// One embedded relation pulled from a `LEFT JOIN LATERAL` block.
#[derive(Clone, Debug, PartialEq)]
pub struct Embed {
    /// JSON key the embed is emitted under (PostgREST's `AS "<key>"`).
    pub key: String,
    pub rel_table: String,
    pub rel_columns: Vec<String>,
    /// Correlation: `<rel_table>.<rel_col> = <base_table>.<base_col>`.
    pub rel_col: String,
    pub base_col: String,
    /// to-many (`json_agg` ⇒ JSON array) vs to-one (`row_to_json` ⇒ object|null).
    pub to_many: bool,
}

/// One item in an embedding select list: either a plain base-table column or an
/// embedded relation. Order is preserved so the assembled object matches the
/// requested `select=` order.
#[derive(Clone, Debug, PartialEq)]
pub enum EmbedItem {
    /// A base-table column emitted as `key` (the engine column to read is `column`).
    Column {
        key: String,
        column: String,
    },
    Relation(Embed),
}

/// A parsed PostgREST embedding read: the base table, its select items (columns +
/// embeds in order), and any de-qualified base WHERE/ORDER/LIMIT tail (carrying
/// its `$n` filter placeholders).
#[derive(Clone)]
pub struct EmbedRead {
    pub base_table: String,
    pub items: Vec<EmbedItem>,
    pub base_tail: String,
}

/// Recognize PostgREST's FK-embedding read template and decompose it. Returns
/// `None` for any non-embedding read (no `LEFT JOIN LATERAL`) so the plain read
/// path keeps handling those.
pub fn parse_embed(sql: &str) -> Option<EmbedRead> {
    let inner = inner_cte(sql)?;
    let lower = inner.to_ascii_lowercase();
    if !lower.contains("left join lateral") {
        return None;
    }
    let sel = lower.find("select")? + "select".len();
    let from = find_kw_depth0(&lower, " from ", sel)?;
    let select_list = inner[sel..from].to_string();

    // Base table: the qualified identifier right after the depth-0 FROM.
    let after_from = from + " from ".len();
    let lead = inner[after_from..].len() - inner[after_from..].trim_start().len();
    let tok_start = after_from + lead;
    let tok_end = inner[tok_start..]
        .find(char::is_whitespace)
        .map(|p| p + tok_start)
        .unwrap_or(inner.len());
    let base_table = last_ident(inner[tok_start..tok_end].trim());
    if base_table.is_empty() {
        return None;
    }

    // Everything after the base table is the LATERAL blocks + the base tail.
    let region = &inner[tok_end..];
    let embeds = parse_lateral_blocks(region);
    if embeds.is_empty() {
        return None;
    }
    let region_lower = region.to_ascii_lowercase();
    let base_tail = region_lower
        .rfind(" on true")
        .map(|p| strip_qualifiers(region[p + " on true".len()..].trim()))
        .unwrap_or_default();

    // Stitch the select list to the parsed blocks, preserving order.
    let mut items = Vec::new();
    for raw in split_depth0_commas(&select_list) {
        let item = raw.trim();
        if item.is_empty() {
            continue;
        }
        let first = first_quoted(item);
        match first.as_deref() {
            // A base column is the only item qualified by the `public` schema.
            Some("public") => {
                let expr = item_expr(item);
                let column = last_ident(expr.trim());
                let key = item_alias(item).unwrap_or_else(|| column.clone());
                if column.is_empty() {
                    return None;
                }
                items.push(EmbedItem::Column { key, column });
            }
            // Otherwise the first quoted token is the LATERAL alias this embed
            // references; bind it to that block and emit under its `AS` key.
            Some(alias) => {
                let (_, embed) = embeds.iter().find(|(a, _)| a == alias)?;
                let key = item_alias(item)?;
                items.push(EmbedItem::Relation(Embed {
                    key,
                    ..embed.clone()
                }));
            }
            None => return None,
        }
    }
    if items.is_empty() {
        return None;
    }
    Some(EmbedRead {
        base_table,
        items,
        base_tail,
    })
}

/// Parse each `LEFT JOIN LATERAL ( … ) AS "<alias>" ON TRUE` block into its
/// embedded relation, keyed by the lateral alias the select list references.
fn parse_lateral_blocks(region: &str) -> Vec<(String, Embed)> {
    let lower = region.to_ascii_lowercase();
    let marker = "left join lateral (";
    let mut out = Vec::new();
    let mut cursor = 0;
    while let Some(rel) = lower[cursor..].find(marker) {
        let open = cursor + rel + marker.len();
        let Some(close) = matching_paren(region, open) else {
            break;
        };
        let block = &region[open..close];
        // ` AS "<alias>" ON TRUE` follows the closing paren.
        let after = &region[close + 1..];
        let alias = first_quoted(after).unwrap_or_default();
        cursor = close + 1;
        if let Some(embed) = parse_lateral_body(block) {
            out.push((alias, embed));
        }
    }
    out
}

/// Pull the embedded relation out of one LATERAL block body. The correlated
/// `SELECT … FROM "public"."<rel>" AS "<a>" WHERE "<a>"."<rc>" = "public"."<base>"."<bc>"`
/// is the same in both shapes; `json_agg` marks the to-many case.
fn parse_lateral_body(block: &str) -> Option<Embed> {
    let lower = block.to_ascii_lowercase();
    let to_many = lower.contains("json_agg(");

    // The real relation is the one qualified by `public`; its FROM gives the
    // table and its alias, and the SELECT just before it gives the columns.
    let from_pub = lower.find("from \"public\".")?;
    let after = &block[from_pub + "from ".len()..];
    let tbl_end = after.find(char::is_whitespace).unwrap_or(after.len());
    let rel_table = last_ident(after[..tbl_end].trim());

    let sel = lower[..from_pub].rfind("select ")? + "select ".len();
    let cols_text = &block[sel..from_pub];
    let rel_columns: Vec<String> = split_depth0_commas(cols_text)
        .into_iter()
        .map(|c| last_ident(c.trim()))
        .filter(|c| !c.is_empty())
        .collect();
    if rel_table.is_empty() || rel_columns.is_empty() {
        return None;
    }

    // Correlation predicate `<lhs> = <rhs>` after WHERE: lhs is the relation
    // side, rhs the base side (qualified by `public`).
    let wpos = lower.find(" where ")? + " where ".len();
    let cond = &block[wpos..];
    let eq = cond.find('=')?;
    let rel_col = last_ident(read_ident_chain(cond[..eq].trim()));
    let base_col = last_ident(read_ident_chain(cond[eq + 1..].trim()));
    if rel_col.is_empty() || base_col.is_empty() {
        return None;
    }

    Some(Embed {
        key: String::new(), // filled in from the select list's AS
        rel_table,
        rel_columns,
        rel_col,
        base_col,
        to_many,
    })
}

/// Find `kw` (already lowercase) in `lower` at parenthesis depth 0, from `start`.
fn find_kw_depth0(lower: &str, kw: &str, start: usize) -> Option<usize> {
    let b = lower.as_bytes();
    let mut depth = 0i32;
    let mut i = start;
    while i < b.len() {
        match b[i] {
            b'(' => depth += 1,
            b')' => depth -= 1,
            _ if depth == 0 && lower[i..].starts_with(kw) => return Some(i),
            _ => {}
        }
        i += 1;
    }
    None
}

/// Split a select list on commas at parenthesis depth 0 (so `coalesce(a, b)`
/// stays one item).
fn split_depth0_commas(s: &str) -> Vec<&str> {
    let b = s.as_bytes();
    let mut depth = 0i32;
    let mut parts = Vec::new();
    let mut start = 0;
    for (i, &c) in b.iter().enumerate() {
        match c {
            b'(' => depth += 1,
            b')' => depth -= 1,
            b',' if depth == 0 => {
                parts.push(&s[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(&s[start..]);
    parts
}

/// Index of the `)` matching the `(` whose content begins at `open`.
fn matching_paren(s: &str, open: usize) -> Option<usize> {
    let b = s.as_bytes();
    let mut depth = 1i32;
    let mut i = open;
    while i < b.len() {
        match b[i] {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// The first `"quoted"` identifier in `s`, unquoted (or `None`).
fn first_quoted(s: &str) -> Option<String> {
    let start = s.find('"')? + 1;
    let end = s[start..].find('"')? + start;
    Some(s[start..end].to_string())
}

/// A select-item's expression, with any trailing `AS "<alias>"` removed.
fn item_expr(item: &str) -> &str {
    let lower = item.to_ascii_lowercase();
    match find_kw_depth0(&lower, " as ", 0) {
        Some(p) => &item[..p],
        None => item,
    }
}

/// A select-item's `AS "<alias>"` key, if present.
fn item_alias(item: &str) -> Option<String> {
    let lower = item.to_ascii_lowercase();
    let p = find_kw_depth0(&lower, " as ", 0)? + " as ".len();
    Some(unquote(item[p..].trim()))
}

/// Read a leading identifier chain (`"a"."b"."c"`, alnum/`_`/`"`/`.`), stopping
/// at the first character outside that set.
fn read_ident_chain(s: &str) -> &str {
    let end = s
        .find(|c: char| !(c.is_alphanumeric() || matches!(c, '_' | '"' | '.')))
        .unwrap_or(s.len());
    &s[..end]
}

/// JSON-encode a base scalar value (delegates to the body encoder).
pub fn json_value(v: &Value) -> String {
    value_json(v)
}

/// JSON-encode a string as an object key / string value.
pub fn json_key(s: &str) -> String {
    json_string(s)
}

/// One result row as a JSON object (used for a to-one embed's single match).
pub fn json_object(columns: &[String], row: &[Value]) -> String {
    let mut out = String::from("{");
    for (c, name) in columns.iter().enumerate() {
        if c > 0 {
            out.push(',');
        }
        out.push_str(&json_string(name));
        out.push(':');
        out.push_str(&value_json(row.get(c).unwrap_or(&Value::Null)));
    }
    out.push('}');
    out
}

/// A result set as a JSON array of objects (used for a to-many embed).
pub fn json_array(columns: &[String], rows: &[Vec<Value>]) -> String {
    rows_to_json(columns, rows)
}

/// Wrap a pre-assembled JSON `body` array into the data-path response row.
pub fn body_row_json(body: String, count: i64) -> Vec<Value> {
    vec![
        Value::Null,                // total_result_set (null::bigint)
        Value::Int(count),          // page_total
        Value::Text(body),          // body (json)
        Value::Null,                // response_headers
        Value::Null,                // response_status
        Value::Text(String::new()), // response_inserted
    ]
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
    fn parses_update_template() {
        let sql = "WITH pgrst_source AS (UPDATE \"public\".\"authors\" SET \"name\" = \"pgrst_body\".\"name\" \
                   FROM (SELECT $1 AS json_data) pgrst_payload, LATERAL (SELECT \"name\" FROM \
                   json_to_record(pgrst_payload.json_data) AS _(\"name\" text) ) pgrst_body  WHERE  \
                   \"public\".\"authors\".\"id\" = $2 RETURNING 1) SELECT '' AS total_result_set FROM \
                   (SELECT * FROM pgrst_source) _postgrest_t";
        let Some(Write::Update(plan)) = parse_write(sql) else {
            panic!("expected an update plan");
        };
        assert_eq!(plan.table, "authors");
        assert_eq!(plan.set_columns, vec!["name".to_string()]);
        assert_eq!(plan.where_clause, "id = $2");
        assert_eq!(max_param(sql), 2);
    }

    #[test]
    fn parses_delete_template() {
        let sql = "WITH pgrst_source AS (DELETE FROM \"public\".\"authors\"  WHERE  \
                   \"public\".\"authors\".\"id\" = $1 RETURNING 1) SELECT '' AS total_result_set FROM \
                   (SELECT * FROM pgrst_source) _postgrest_t";
        let Some(Write::Delete(plan)) = parse_write(sql) else {
            panic!("expected a delete plan");
        };
        assert_eq!(plan.table, "authors");
        assert_eq!(plan.where_clause, "id = $1");
        assert_eq!(max_param(sql), 1);
        assert!(parse_write("SELECT 1").is_none());
    }

    // The two embedding templates below are the *verbatim* SQL PostgREST 14.13
    // emitted into the capture (local-e2e/postgrest-corpus-embedding.log) for
    // `/books?select=title,authors(name)` and `/authors?select=name,books(title)`.

    #[test]
    fn parses_many_to_one_embed() {
        let sql = "WITH pgrst_source AS ( SELECT \"public\".\"books\".\"title\", \
                   row_to_json(\"books_authors_1\".*)::jsonb AS \"authors\" FROM \"public\".\"books\" \
                   LEFT JOIN LATERAL ( SELECT \"authors_1\".\"name\" FROM \"public\".\"authors\" AS \
                   \"authors_1\" WHERE \"authors_1\".\"id\" = \"public\".\"books\".\"author_id\"    ) \
                   AS \"books_authors_1\" ON TRUE    )  SELECT null::bigint AS total_result_set, \
                   coalesce(json_agg(_postgrest_t), '[]') AS body FROM ( SELECT * FROM pgrst_source ) _postgrest_t";
        assert!(is_read(sql));
        assert!(rewrite_read(sql).is_none()); // embedding is not a plain read
        let plan = parse_embed(sql).expect("embed template");
        assert_eq!(plan.base_table, "books");
        assert_eq!(plan.base_tail, "");
        assert_eq!(plan.items.len(), 2);
        match &plan.items[0] {
            EmbedItem::Column { key, column } => {
                assert_eq!(key, "title");
                assert_eq!(column, "title");
            }
            _ => panic!("first item should be a base column"),
        }
        match &plan.items[1] {
            EmbedItem::Relation(e) => {
                assert_eq!(e.key, "authors");
                assert_eq!(e.rel_table, "authors");
                assert_eq!(e.rel_columns, vec!["name".to_string()]);
                assert_eq!(e.rel_col, "id");
                assert_eq!(e.base_col, "author_id");
                assert!(!e.to_many, "books -> authors is many-to-one");
            }
            _ => panic!("second item should be an embed"),
        }
    }

    #[test]
    fn parses_one_to_many_embed() {
        let sql = "WITH pgrst_source AS ( SELECT \"public\".\"authors\".\"name\", \
                   COALESCE( \"authors_books_1\".\"authors_books_1\", '[]') AS \"books\" FROM \
                   \"public\".\"authors\" LEFT JOIN LATERAL ( SELECT json_agg(\"authors_books_1\")::jsonb \
                   AS \"authors_books_1\" FROM (SELECT \"books_1\".\"title\" FROM \"public\".\"books\" AS \
                   \"books_1\" WHERE \"books_1\".\"author_id\" = \"public\".\"authors\".\"id\"    ) AS \
                   \"authors_books_1\" ) AS \"authors_books_1\" ON TRUE    )  SELECT \
                   coalesce(json_agg(_postgrest_t), '[]') AS body FROM ( SELECT * FROM pgrst_source ) _postgrest_t";
        let plan = parse_embed(sql).expect("embed template");
        assert_eq!(plan.base_table, "authors");
        assert_eq!(plan.items.len(), 2);
        match &plan.items[0] {
            EmbedItem::Column { key, column } => {
                assert_eq!(key, "name");
                assert_eq!(column, "name");
            }
            _ => panic!("first item should be a base column"),
        }
        match &plan.items[1] {
            EmbedItem::Relation(e) => {
                assert_eq!(e.key, "books");
                assert_eq!(e.rel_table, "books");
                assert_eq!(e.rel_columns, vec!["title".to_string()]);
                assert_eq!(e.rel_col, "author_id");
                assert_eq!(e.base_col, "id");
                assert!(e.to_many, "authors -> books is one-to-many");
            }
            _ => panic!("second item should be an embed"),
        }
    }

    #[test]
    fn plain_read_is_not_an_embed() {
        let sql = "WITH pgrst_source AS ( SELECT \"public\".\"books\".* FROM \"public\".\"books\" ) \
                   SELECT coalesce(json_agg(_postgrest_t), '[]') AS body FROM ( SELECT * FROM pgrst_source ) _postgrest_t";
        assert!(parse_embed(sql).is_none());
        assert!(rewrite_read(sql).is_some());
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
