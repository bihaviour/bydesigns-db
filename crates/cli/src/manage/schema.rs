//! `schema dump` (spec 19, Milestone 2) — reconstruct `CREATE TABLE` DDL from the
//! live catalog ([`engine::Connection::catalog`], the same `TableSchema` the
//! pgwire server reflects for PostgREST). The output is the schema a fresh
//! database would need to match this one's shape: one `CREATE TABLE` per table
//! with column types, nullability, primary key, and foreign keys.
//!
//! It is *reconstructed*, not a byte-exact replay: the catalog carries column
//! type / nullability / key flags, single-column `UNIQUE` and `DEFAULT`, and FK
//! relationships, so those round-trip (over both the embedded and `postgres://`
//! transports). Table-level / composite `UNIQUE` and `CHECK` constraints are not
//! part of the reflected catalog and so are not emitted (noted in the docs).

use super::{open, positional, CmdError::Runtime, CmdError::Usage, CmdResult};
use engine::{CatalogTable, ColumnType};

/// `twilldb schema dump <url>`.
pub fn cmd_schema(args: &[String]) -> CmdResult {
    match positional(args, 0, "a `schema` subcommand (only `dump` is supported)")? {
        "dump" => {
            let url = positional(args, 1, "<url>")?;
            let mut conn = open(url)?;
            let tables = conn.catalog().map_err(Runtime)?;
            if tables.is_empty() {
                return Ok("-- (no tables)".to_string());
            }
            let ddl: Vec<String> = tables.iter().map(create_table_ddl).collect();
            Ok(ddl.join("\n\n"))
        }
        other => Err(Usage(format!(
            "unknown `schema` subcommand '{other}' (only `dump` is supported)"
        ))),
    }
}

/// One table's `CREATE TABLE` statement, reconstructed from its catalog entry.
fn create_table_ddl(t: &CatalogTable) -> String {
    let mut lines: Vec<String> = Vec::new();
    // How many columns are primary key: a single PK is inlined on the column; a
    // composite PK becomes a table-level `PRIMARY KEY (…)` constraint.
    let pk_cols: Vec<&str> = t
        .columns
        .iter()
        .filter(|c| c.primary_key)
        .map(|c| c.name.as_str())
        .collect();
    let inline_pk = pk_cols.len() == 1;

    for c in &t.columns {
        let mut col = format!("  {} {}", c.name, sql_type(c.ty));
        if c.primary_key && inline_pk {
            col.push_str(" PRIMARY KEY");
        } else if c.not_null {
            // An inlined PRIMARY KEY already implies NOT NULL.
            col.push_str(" NOT NULL");
        }
        // `unique` is reflected only for non-PK columns (a PK is already unique).
        if c.unique {
            col.push_str(" UNIQUE");
        }
        if let Some(default) = &c.default_sql {
            col.push_str(&format!(" DEFAULT {default}"));
        }
        lines.push(col);
    }
    if pk_cols.len() > 1 {
        lines.push(format!("  PRIMARY KEY ({})", pk_cols.join(", ")));
    }
    for fk in &t.foreign_keys {
        lines.push(format!(
            "  FOREIGN KEY ({}) REFERENCES {} ({})",
            fk.columns.join(", "),
            fk.foreign_table,
            fk.foreign_columns.join(", "),
        ));
    }
    format!("CREATE TABLE {} (\n{}\n);", t.name, lines.join(",\n"))
}

/// The SQL type keyword for an engine storage class (the inverse of the parser's
/// type mapping, so the dump re-parses).
fn sql_type(ty: ColumnType) -> String {
    match ty {
        ColumnType::Integer => "integer".to_string(),
        ColumnType::Real => "real".to_string(),
        ColumnType::Text => "text".to_string(),
        ColumnType::Blob => "blob".to_string(),
        ColumnType::Vector(n) => format!("vector({n})"),
    }
}
