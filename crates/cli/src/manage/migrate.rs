//! Migrations (spec 19): an ordered directory of immutable `*.sql` files plus a
//! `_twilldb_migrations` tracking table in the database. The model is close to
//! Supabase / dbmate / golang-migrate so it is familiar.
//!
//! ## The DDL-autocommit constraint
//!
//! Twill runs `CREATE`/`ALTER`/`DROP` in autocommit; DDL inside an explicit
//! transaction is rejected. So a migration file is the unit of *ordering*, not
//! always of *atomicity*: a DML-only file is applied inside one transaction, but
//! a file containing DDL runs statement-by-statement in autocommit. (The
//! preview-and-swap mitigation — `migrate up --branch` — is Milestone 2.)

use engine::Connection;
use std::path::Path;

use super::{stamp_compact, stamp_iso};

const TRACKING_TABLE: &str = "_twilldb_migrations";

/// A migration file discovered on disk: its `version` (timestamp prefix), its
/// `name` (the rest of the stem), the full path, and the file contents.
struct MigrationFile {
    version: String,
    name: String,
    path: std::path::PathBuf,
    body: String,
}

/// `migrate new <name>` — write `migrations/<ts>_<name>.sql` and return a note.
pub fn new(dir: &str, name: &str, now: i64) -> Result<String, String> {
    let slug = slugify(name);
    if slug.is_empty() {
        return Err(format!(
            "migration name '{name}' has no usable characters (use letters/digits)"
        ));
    }
    std::fs::create_dir_all(dir).map_err(|e| format!("creating {dir}/: {e}"))?;
    let filename = format!("{}_{}.sql", stamp_compact(now), slug);
    let path = Path::new(dir).join(&filename);
    if path.exists() {
        return Err(format!("{} already exists", path.display()));
    }
    let template = format!(
        "-- migration: {slug}\n\
         -- created: {}\n\
         -- Statements run in order. A file containing DDL (CREATE/ALTER/DROP)\n\
         -- runs in autocommit; a DML-only file runs inside one transaction.\n\n",
        stamp_iso(now)
    );
    std::fs::write(&path, template).map_err(|e| format!("writing {}: {e}", path.display()))?;
    Ok(format!("created {}", path.display()))
}

/// `migrate up <url>` — apply every pending migration in order, recording each in
/// the tracking table, and warn on checksum drift for already-applied files.
pub fn up(conn: &mut Connection, dir: &str, now: i64) -> Result<String, String> {
    ensure_tracking_table(conn)?;
    let files = load_migrations(dir)?;
    let applied = applied_versions(conn)?;

    let mut out = String::new();
    drift_warnings(&files, &applied, &mut out);

    let pending: Vec<&MigrationFile> = files
        .iter()
        .filter(|f| !applied.iter().any(|(v, _)| v == &f.version))
        .collect();
    if pending.is_empty() {
        out.push_str("nothing to apply (database is up to date)");
        return Ok(out);
    }

    let mut count = 0usize;
    for f in pending {
        apply_one(conn, f, now)
            .map_err(|e| format!("{out}applying migration {}_{}: {e}", f.version, f.name))?;
        out.push_str(&format!("applied {}_{}\n", f.version, f.name));
        count += 1;
    }
    out.push_str(&format!("{count} migration(s) applied"));
    Ok(out)
}

/// `migrate status <url>` — list applied vs pending, flag checksum drift.
pub fn status(conn: &mut Connection, dir: &str) -> Result<String, String> {
    ensure_tracking_table(conn)?;
    let files = load_migrations(dir)?;
    let applied = applied_versions(conn)?;

    let mut out = String::new();
    for f in &files {
        match applied.iter().find(|(v, _)| v == &f.version) {
            Some((_, checksum)) => {
                let drift = *checksum != fnv1a_hex(&f.body);
                out.push_str(&format!(
                    "[applied{}] {}_{}\n",
                    if drift { ", DRIFT" } else { "" },
                    f.version,
                    f.name
                ));
            }
            None => out.push_str(&format!("[pending] {}_{}\n", f.version, f.name)),
        }
    }
    // A version recorded in the table but missing on disk is worth surfacing.
    for (v, _) in &applied {
        if !files.iter().any(|f| &f.version == v) {
            out.push_str(&format!("[applied, file missing] {v}\n"));
        }
    }
    if out.is_empty() {
        out.push_str("(no migrations)");
    }
    Ok(out.trim_end().to_string())
}

// ---- apply --------------------------------------------------------------

/// Apply one migration file: run its statements (DML-only → one transaction;
/// any DDL → autocommit per statement), then record it in the tracking table.
fn apply_one(conn: &mut Connection, f: &MigrationFile, now: i64) -> Result<(), String> {
    let statements = split_statements(&f.body);
    let has_ddl = statements.iter().any(|s| is_ddl(s));

    if has_ddl {
        // DDL cannot run inside an explicit transaction, so run each statement in
        // autocommit. A failure stops here; earlier statements stay applied.
        for (i, s) in statements.iter().enumerate() {
            conn.exec(s)
                .map_err(|e| format!("statement #{}: {e}", i + 1))?;
        }
    } else {
        // DML-only: one atomic transaction.
        conn.exec("BEGIN").map_err(|e| e.to_string())?;
        for (i, s) in statements.iter().enumerate() {
            if let Err(e) = conn.exec(s) {
                let _ = conn.exec("ROLLBACK");
                return Err(format!("statement #{}: {e}", i + 1));
            }
        }
        conn.exec("COMMIT").map_err(|e| e.to_string())?;
    }

    record_applied(conn, &f.version, &f.name, &fnv1a_hex(&f.body), now)
}

/// Insert the tracking row for a freshly applied migration.
fn record_applied(
    conn: &mut Connection,
    version: &str,
    name: &str,
    checksum: &str,
    now: i64,
) -> Result<(), String> {
    let sql = format!(
        "INSERT INTO {TRACKING_TABLE} (version, name, checksum, applied_at) \
         VALUES ('{}', '{}', '{}', '{}')",
        sql_lit(version),
        sql_lit(name),
        sql_lit(checksum),
        sql_lit(&stamp_iso(now)),
    );
    conn.exec(&sql)
        .map_err(|e| format!("recording migration: {e}"))
}

// ---- tracking table -----------------------------------------------------

fn ensure_tracking_table(conn: &mut Connection) -> Result<(), String> {
    let sql = format!(
        "CREATE TABLE IF NOT EXISTS {TRACKING_TABLE} (\
         version TEXT PRIMARY KEY, \
         name TEXT NOT NULL, \
         checksum TEXT NOT NULL, \
         applied_at TEXT NOT NULL)"
    );
    conn.exec(&sql)
        .map_err(|e| format!("creating {TRACKING_TABLE}: {e}"))
}

/// The `(version, checksum)` of every recorded migration.
fn applied_versions(conn: &mut Connection) -> Result<Vec<(String, String)>, String> {
    let rs = conn
        .query(&format!(
            "SELECT version, checksum FROM {TRACKING_TABLE} ORDER BY version"
        ))
        .map_err(|e| format!("reading {TRACKING_TABLE}: {e}"))?;
    Ok(rs
        .rows
        .iter()
        .filter_map(|r| {
            let v = r.first().and_then(|v| v.render())?;
            let c = r.get(1).and_then(|v| v.render())?;
            Some((v, c))
        })
        .collect())
}

/// Append a drift warning for every applied file whose checksum no longer matches.
fn drift_warnings(files: &[MigrationFile], applied: &[(String, String)], out: &mut String) {
    for f in files {
        if let Some((_, checksum)) = applied.iter().find(|(v, _)| v == &f.version) {
            if *checksum != fnv1a_hex(&f.body) {
                out.push_str(&format!(
                    "warning: {}_{} was edited after it was applied (checksum drift)\n",
                    f.version, f.name
                ));
            }
        }
    }
}

// ---- filesystem ---------------------------------------------------------

/// Read and sort the `*.sql` files under `dir`. A missing directory yields an
/// empty list (nothing to apply), not an error.
fn load_migrations(dir: &str) -> Result<Vec<MigrationFile>, String> {
    let path = Path::new(dir);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let mut files = Vec::new();
    for entry in std::fs::read_dir(path).map_err(|e| format!("reading {dir}/: {e}"))? {
        let entry = entry.map_err(|e| e.to_string())?;
        let p = entry.path();
        if p.extension().and_then(|s| s.to_str()) != Some("sql") {
            continue;
        }
        let stem = p
            .file_stem()
            .and_then(|s| s.to_str())
            .ok_or_else(|| format!("non-UTF-8 migration filename: {}", p.display()))?;
        let (version, name) = match stem.split_once('_') {
            Some((v, n)) => (v.to_string(), n.to_string()),
            None => (stem.to_string(), String::new()),
        };
        let body =
            std::fs::read_to_string(&p).map_err(|e| format!("reading {}: {e}", p.display()))?;
        files.push(MigrationFile {
            version,
            name,
            path: p,
            body,
        });
    }
    files.sort_by(|a, b| a.version.cmp(&b.version));
    // Surface duplicate version prefixes — they would make apply order ambiguous.
    for pair in files.windows(2) {
        if pair[0].version == pair[1].version {
            return Err(format!(
                "two migrations share version {}: {} and {}",
                pair[0].version,
                pair[0].path.display(),
                pair[1].path.display()
            ));
        }
    }
    Ok(files)
}

// ---- SQL helpers --------------------------------------------------------

/// Split a SQL script into individual statements on top-level `;`, honoring
/// single/double-quoted strings and `--` / `/* */` comments. Comments are
/// dropped from the emitted statements (they are not needed for execution).
pub fn split_statements(sql: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let bytes = sql.as_bytes();
    let mut i = 0;
    let mut in_single = false;
    let mut in_double = false;
    while i < bytes.len() {
        let c = bytes[i] as char;
        if in_single {
            cur.push(c);
            if c == '\'' {
                in_single = false;
            }
            i += 1;
            continue;
        }
        if in_double {
            cur.push(c);
            if c == '"' {
                in_double = false;
            }
            i += 1;
            continue;
        }
        if c == '-' && bytes.get(i + 1) == Some(&b'-') {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if c == '/' && bytes.get(i + 1) == Some(&b'*') {
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            i += 2;
            continue;
        }
        match c {
            '\'' => {
                in_single = true;
                cur.push(c);
            }
            '"' => {
                in_double = true;
                cur.push(c);
            }
            ';' => {
                let t = cur.trim().to_string();
                if !t.is_empty() {
                    out.push(t);
                }
                cur.clear();
            }
            _ => cur.push(c),
        }
        i += 1;
    }
    let t = cur.trim().to_string();
    if !t.is_empty() {
        out.push(t);
    }
    out
}

/// Whether a statement is DDL (its leading keyword is CREATE/ALTER/DROP), which
/// the engine runs only in autocommit.
fn is_ddl(stmt: &str) -> bool {
    let first = stmt.split_whitespace().next().unwrap_or("");
    matches!(
        first.to_ascii_uppercase().as_str(),
        "CREATE" | "ALTER" | "DROP"
    )
}

/// Escape a single-quoted SQL string literal (double any embedded quote). The
/// values interpolated here (versions, names, ISO stamps, hex checksums) are
/// engine-internal, but escaping keeps the tracking insert robust.
fn sql_lit(s: &str) -> String {
    s.replace('\'', "''")
}

/// Slugify a migration name into a filename-safe stem: lowercase, non-alnum runs
/// collapsed to a single `_`, trimmed.
fn slugify(name: &str) -> String {
    let mut out = String::new();
    let mut prev_us = false;
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            prev_us = false;
        } else if !prev_us {
            out.push('_');
            prev_us = true;
        }
    }
    out.trim_matches('_').to_string()
}

/// FNV-1a 64-bit hex digest — a small, dependency-free content hash for drift
/// detection (collision-resistant enough to catch an accidental edit; not a
/// cryptographic guarantee).
pub fn fnv1a_hex(s: &str) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x00000100000001B3);
    }
    format!("{h:016x}")
}
