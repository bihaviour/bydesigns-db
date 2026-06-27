//! Output rendering for the management commands: a result set as an aligned
//! text table or as JSON, a `describe` block, a `stats` block, and the `shell`
//! REPL loop. All hand-rolled — no `serde`, no table crate — to keep the
//! management build as dependency-light as the scaffolder.

use engine::{CatalogTable, ColumnType, Connection, EngineStats, ResultSet, Value};
use std::io::{BufRead, Write};

/// Render a statement result. A query with columns prints a table (or JSON with
/// `json`); a row-count-only result (INSERT/UPDATE/DELETE without RETURNING)
/// prints the affected-row count instead.
pub fn result(rs: &ResultSet, changes: i64, json: bool) -> String {
    if rs.columns.is_empty() {
        // No projection: a DML/DDL/utility statement. Report what it changed.
        return if json {
            format!("{{\"changes\":{changes}}}")
        } else {
            format!("OK ({changes} row(s) affected)")
        };
    }
    if json {
        json_rows(rs)
    } else {
        table(rs)
    }
}

/// Render the cell at a value, matching the engine's text forms: NULL shows as
/// `NULL` in a table (and `null` in JSON); a blob is base64; a vector is `[…]`.
fn cell_text(v: &Value) -> String {
    match v.render() {
        Some(s) => s,
        None => "NULL".to_string(),
    }
}

/// An aligned, bordered text table (psql-ish, simplified).
fn table(rs: &ResultSet) -> String {
    let cols = &rs.columns;
    let mut widths: Vec<usize> = cols.iter().map(|c| c.chars().count()).collect();
    let cells: Vec<Vec<String>> = rs
        .rows
        .iter()
        .map(|row| {
            row.iter()
                .enumerate()
                .map(|(i, v)| {
                    let s = cell_text(v);
                    if let Some(w) = widths.get_mut(i) {
                        *w = (*w).max(s.chars().count());
                    }
                    s
                })
                .collect()
        })
        .collect();

    let sep = || -> String {
        let mut s = String::from("+");
        for w in &widths {
            s.push_str(&"-".repeat(w + 2));
            s.push('+');
        }
        s
    };
    let row_line = |row: &[String]| -> String {
        let mut s = String::from("|");
        for (i, w) in widths.iter().enumerate() {
            let cell = row.get(i).map(String::as_str).unwrap_or("");
            s.push_str(&format!(" {cell:<width$} |", width = w));
        }
        s
    };

    let mut out = String::new();
    out.push_str(&sep());
    out.push('\n');
    out.push_str(&row_line(cols));
    out.push('\n');
    out.push_str(&sep());
    out.push('\n');
    for row in &cells {
        out.push_str(&row_line(row));
        out.push('\n');
    }
    out.push_str(&sep());
    out.push('\n');
    out.push_str(&format!("({} row(s))", rs.rows.len()));
    out
}

/// Render a result set as a JSON array of objects (one per row).
fn json_rows(rs: &ResultSet) -> String {
    let mut out = String::from("[");
    for (r, row) in rs.rows.iter().enumerate() {
        if r > 0 {
            out.push(',');
        }
        out.push('{');
        for (c, col) in rs.columns.iter().enumerate() {
            if c > 0 {
                out.push(',');
            }
            out.push_str(&json_string(col));
            out.push(':');
            out.push_str(&json_value(row.get(c).unwrap_or(&Value::Null)));
        }
        out.push('}');
    }
    out.push(']');
    out
}

/// A single value as a JSON literal: numbers bare, text/blob quoted, a vector as
/// a JSON number array, NULL as `null`.
fn json_value(v: &Value) -> String {
    match v {
        Value::Null => "null".to_string(),
        Value::Int(i) => i.to_string(),
        Value::Real(r) => {
            if r.is_finite() {
                // Reuse the engine's compact real form, but JSON has no `1.0`
                // requirement — a finite f64 prints as a valid JSON number.
                format!("{r}")
            } else {
                "null".to_string()
            }
        }
        Value::Text(s) => json_string(s),
        Value::Blob(_) => json_string(&cell_text(v)),
        Value::Vector(xs) => {
            let parts: Vec<String> = xs.iter().map(|x| format!("{x}")).collect();
            format!("[{}]", parts.join(","))
        }
    }
}

/// JSON-escape and quote a string.
fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
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

/// The TypeScript-ish type name a column maps to, for `describe` display.
fn type_label(ty: ColumnType) -> String {
    match ty {
        ColumnType::Integer => "integer".to_string(),
        ColumnType::Real => "real".to_string(),
        ColumnType::Text => "text".to_string(),
        ColumnType::Blob => "blob".to_string(),
        ColumnType::Vector(n) => format!("vector({n})"),
    }
}

/// Render `describe <table>`: columns with type / nullability / key flags, then
/// foreign keys.
pub fn describe(t: &CatalogTable) -> String {
    let mut out = format!("Table \"{}\"\n", t.name);
    let mut rs = ResultSet {
        columns: vec![
            "column".into(),
            "type".into(),
            "nullable".into(),
            "key".into(),
        ],
        types: vec![ColumnType::Text; 4],
        rows: Vec::new(),
    };
    for c in &t.columns {
        let key = if c.primary_key { "PK" } else { "" };
        rs.rows.push(vec![
            Value::Text(c.name.clone()),
            Value::Text(type_label(c.ty)),
            Value::Text(if c.not_null { "NOT NULL" } else { "" }.to_string()),
            Value::Text(key.to_string()),
        ]);
    }
    out.push_str(&table(&rs));
    if !t.foreign_keys.is_empty() {
        out.push_str("\nForeign keys:\n");
        for fk in &t.foreign_keys {
            out.push_str(&format!(
                "  {} ({}) -> {} ({})\n",
                fk.name,
                fk.columns.join(", "),
                fk.foreign_table,
                fk.foreign_columns.join(", "),
            ));
        }
        out = out.trim_end().to_string();
    }
    out
}

/// Render an [`EngineStats`] snapshot as aligned `key: value` lines.
pub fn stats(s: &EngineStats) -> String {
    let st = &s.storage;
    let lines = [
        ("commits", s.commits),
        ("durable_appends", s.durable_appends),
        ("committed_lsn", s.committed_lsn),
        ("write_acquires", s.write_acquires),
        ("write_handoffs", s.write_handoffs),
        ("write_wait_us_total", s.write_wait_us_total),
        ("storage.wal_appends", st.wal_appends),
        ("storage.wal_bytes", st.wal_bytes),
        ("storage.page_reads", st.page_reads),
        ("storage.page_read_bytes", st.page_read_bytes),
        ("storage.cache_hits", st.cache_hits),
        ("storage.cache_misses", st.cache_misses),
        ("storage.fetch_latency_us_total", st.fetch_latency_us_total),
        ("storage.fsyncs", st.fsyncs),
    ];
    let width = lines.iter().map(|(k, _)| k.len()).max().unwrap_or(0);
    let mut out = String::new();
    for (k, v) in lines {
        out.push_str(&format!("{k:<width$} : {v}\n"));
    }
    out.trim_end().to_string()
}

/// The interactive REPL. Reads from `input`, writes prompts/results to `out`.
/// Accumulates lines until a statement ends with `;` (or is a `.dot` command),
/// then runs it. Returns on EOF, `.quit`, or `.exit`. Parameterized over the I/O
/// so the integration tests drive it with in-memory buffers.
pub fn shell<R: BufRead, W: Write>(
    conn: &mut Connection,
    input: &mut R,
    out: &mut W,
    interactive: bool,
) -> Result<(), String> {
    let map = |e: std::io::Error| e.to_string();
    if interactive {
        writeln!(
            out,
            "twilldb shell — end statements with ; · .help for commands · .quit to exit"
        )
        .map_err(map)?;
    }
    let mut buf = String::new();
    loop {
        if interactive {
            let prompt = if buf.is_empty() {
                "twilldb> "
            } else {
                "    ...> "
            };
            write!(out, "{prompt}").map_err(map)?;
            out.flush().map_err(map)?;
        }
        let mut line = String::new();
        let n = input.read_line(&mut line).map_err(map)?;
        if n == 0 {
            break; // EOF
        }
        let trimmed = line.trim();
        // A dot-command is only recognized at the start of a fresh statement.
        if buf.trim().is_empty() && trimmed.starts_with('.') {
            match dot_command(conn, trimmed, out)? {
                DotOutcome::Quit => break,
                DotOutcome::Continue => {}
            }
            buf.clear();
            continue;
        }
        buf.push_str(&line);
        if !trimmed.ends_with(';') {
            continue; // keep reading a multi-line statement
        }
        let stmt = buf.trim().trim_end_matches(';').trim().to_string();
        buf.clear();
        if stmt.is_empty() {
            continue;
        }
        match conn.query(&stmt) {
            Ok(rs) => {
                writeln!(out, "{}", result(&rs, conn.last_changes, false)).map_err(map)?;
            }
            Err(e) => writeln!(out, "error: {e}").map_err(map)?,
        }
    }
    Ok(())
}

enum DotOutcome {
    Continue,
    Quit,
}

/// Handle a `.dot` REPL command.
fn dot_command<W: Write>(
    conn: &mut Connection,
    cmd: &str,
    out: &mut W,
) -> Result<DotOutcome, String> {
    let map = |e: std::io::Error| e.to_string();
    let mut parts = cmd.split_whitespace();
    match parts.next() {
        Some(".quit") | Some(".exit") => return Ok(DotOutcome::Quit),
        Some(".help") => {
            writeln!(
                out,
                ".tables          list tables\n\
                 .schema [table]  describe one or all tables\n\
                 .help            this help\n\
                 .quit / .exit    leave the shell"
            )
            .map_err(map)?;
        }
        Some(".tables") => {
            for t in conn.catalog() {
                writeln!(out, "{}", t.name).map_err(map)?;
            }
        }
        Some(".schema") => {
            let want = parts.next();
            let catalog = conn.catalog();
            let mut any = false;
            for t in &catalog {
                if want
                    .map(|w| w.eq_ignore_ascii_case(&t.name))
                    .unwrap_or(true)
                {
                    writeln!(out, "{}", describe(t)).map_err(map)?;
                    any = true;
                }
            }
            if !any {
                writeln!(out, "no matching table").map_err(map)?;
            }
        }
        Some(other) => {
            writeln!(out, "unknown command: {other} (.help for the list)").map_err(map)?;
        }
        None => {}
    }
    Ok(DotOutcome::Continue)
}
