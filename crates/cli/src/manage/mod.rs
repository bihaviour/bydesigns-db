//! `twilldb` management half (spec 19) — the database *management* commands,
//! compiled only with the `manage` feature. Where the scaffolder (spec 18) only
//! *writes files*, every command here *talks to a real database*: it links
//! `twill-engine` and opens a [`Connection`](engine::Connection) by the
//! connection-string scheme, exactly like the engine chooses its backend.
//!
//! ## Transport
//!
//! `file://` / `s3://` / `r2://` / `gs://` open the engine **embedded** (in this
//! process, function-call latency). `postgres://` is the **pgwire** transport for
//! a live deployment — reserved for Milestone 3 and rejected with guidance for
//! now. Unknown schemes are rejected, never defaulted.
//!
//! ## The single-writer caveat
//!
//! Twill is single-writer-per-database (a durable, epoch-fenced lease). When the
//! CLI opens `file://` embedded it *is* the writer, so it must not be pointed at a
//! database an app or server currently holds — manage a live deployment over
//! `postgres://` instead (Milestone 3). The command help repeats this.
//!
//! The command bodies return `Result<String, String>` (rendered output, or an
//! error message) so the integration tests can drive them against a temp
//! `file://` database without spawning a process or capturing stdout.

mod gentypes;
mod migrate;
mod render;

use crate::exit;
use std::time::{SystemTime, UNIX_EPOCH};

/// Dispatch a management subcommand. `cmd` is the top-level verb (`sql`,
/// `migrate`, …) and `rest` the arguments after it. Prints output to stdout and
/// errors to stderr, returning the process exit code.
pub fn dispatch(cmd: &str, rest: &[String]) -> i32 {
    // `shell` streams to stdout/stdin rather than buffering one output string.
    if cmd == "shell" {
        return cmd_shell(rest);
    }
    finish(run_core(cmd, rest))
}

/// Run a non-streaming management command, returning its rendered output. Shared
/// by [`dispatch`] (which prints it) and [`run`] (the test-facing wrapper).
fn run_core(cmd: &str, rest: &[String]) -> CmdResult {
    match cmd {
        "sql" => cmd_sql(rest),
        "tables" => cmd_tables(rest),
        "describe" => cmd_describe(rest),
        "migrate" => cmd_migrate(rest),
        "gen" => cmd_gen(rest),
        "seed" => cmd_seed(rest),
        "stats" => cmd_stats(rest),
        "shell" => Err(Usage(
            "`shell` is interactive; run it from a terminal".into(),
        )),
        other => Err(Usage(format!("unknown management subcommand '{other}'"))),
    }
}

/// Test-facing wrapper: run a non-streaming command and return its output (or a
/// flattened error message) as a string, so the integration tests can assert on
/// it without spawning a process or capturing stdout.
pub fn run(cmd: &str, rest: &[String]) -> Result<String, String> {
    run_core(cmd, rest).map_err(|e| match e {
        Usage(m) | Runtime(m) => m,
    })
}

/// Test-facing wrapper for the REPL: drive [`render::shell`] with an in-memory
/// input string, returning everything it wrote.
pub fn shell_str(url: &str, input: &str) -> Result<String, String> {
    let mut conn = open(url).map_err(|e| match e {
        Usage(m) | Runtime(m) => m,
    })?;
    let mut reader = std::io::Cursor::new(input.as_bytes());
    let mut out: Vec<u8> = Vec::new();
    render::shell(&mut conn, &mut reader, &mut out, false)?;
    String::from_utf8(out).map_err(|e| e.to_string())
}

/// A command outcome: a string to print on success, or an error. `Usage`
/// separates "bad flags / arguments" (exit 2) from runtime failures (exit 1).
type CmdResult = Result<String, CmdError>;

enum CmdError {
    /// Bad flags / arguments — prints the message and the management help.
    Usage(String),
    /// A runtime failure (I/O, SQL error, open failure).
    Runtime(String),
}
use CmdError::{Runtime, Usage};

impl From<String> for CmdError {
    fn from(s: String) -> Self {
        Runtime(s)
    }
}

fn finish(result: CmdResult) -> i32 {
    match result {
        Ok(out) => {
            if !out.is_empty() {
                println!("{out}");
            }
            exit::OK
        }
        Err(Usage(msg)) => {
            eprintln!("error: {msg}\n");
            print_manage_help();
            exit::USAGE
        }
        Err(Runtime(msg)) => {
            eprintln!("error: {msg}");
            exit::ERROR
        }
    }
}

// ---- transport ----------------------------------------------------------

/// Open a database by connection-string scheme (spec 19 transport model). Only
/// the embedded schemes are wired up in Milestone 1; `postgres://` is the
/// Milestone-3 pgwire transport and is rejected with guidance.
fn open(url: &str) -> Result<engine::Connection, CmdError> {
    if url.starts_with("file://")
        || url.starts_with("s3://")
        || url.starts_with("r2://")
        || url.starts_with("gs://")
    {
        engine::Connection::open(url).map_err(|e| Runtime(format!("opening {url}: {e}")))
    } else if url.starts_with("postgres://") {
        Err(Runtime(
            "the postgres:// transport is not implemented yet (spec 19 Milestone 3).\n\
             Manage a live deployment over the wire is coming; for now manage a \
             local or stopped database embedded via file://."
                .to_string(),
        ))
    } else {
        Err(Usage(format!(
            "unrecognized connection-string scheme in '{url}'. \
             Use file:// (embedded) — s3://, r2://, gs:// are also embedded; \
             postgres:// (live, Milestone 3) is not wired up yet."
        )))
    }
}

/// First positional that is not a flag, or a usage error naming what was wanted.
fn positional<'a>(args: &'a [String], n: usize, want: &str) -> Result<&'a str, CmdError> {
    args.iter()
        .filter(|a| !a.starts_with("--"))
        .nth(n)
        .map(String::as_str)
        .ok_or_else(|| Usage(format!("missing {want}")))
}

/// Value of a `--flag <value>` option, if present.
fn flag_value<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .map(String::as_str)
}

/// Whether a boolean `--flag` is present.
fn has_flag(args: &[String], flag: &str) -> bool {
    args.iter().any(|a| a == flag)
}

// ---- commands -----------------------------------------------------------

/// `twilldb sql <url> "<query>" [--json]` — run one statement/query.
fn cmd_sql(args: &[String]) -> CmdResult {
    let url = positional(args, 0, "<url>")?;
    let query = positional(args, 1, "a SQL statement")?;
    let json = has_flag(args, "--json");
    let mut conn = open(url)?;
    let rs = conn.query(query).map_err(|e| Runtime(e.to_string()))?;
    Ok(render::result(&rs, conn.last_changes, json))
}

/// `twilldb tables <url>` — list tables from the catalog.
fn cmd_tables(args: &[String]) -> CmdResult {
    let url = positional(args, 0, "<url>")?;
    let conn = open(url)?;
    let tables = conn.catalog();
    if tables.is_empty() {
        return Ok("(no tables)".to_string());
    }
    let mut out = String::new();
    for t in &tables {
        out.push_str(&format!("{} ({} columns)\n", t.name, t.columns.len()));
    }
    Ok(out.trim_end().to_string())
}

/// `twilldb describe <url> <table>` — columns, PK, FKs of one table.
fn cmd_describe(args: &[String]) -> CmdResult {
    let url = positional(args, 0, "<url>")?;
    let table = positional(args, 1, "<table>")?;
    let conn = open(url)?;
    let t = conn
        .catalog()
        .into_iter()
        .find(|t| t.name.eq_ignore_ascii_case(table))
        .ok_or_else(|| Runtime(format!("no such table: {table}")))?;
    Ok(render::describe(&t))
}

/// `twilldb stats <url>` — engine + storage observability snapshot.
fn cmd_stats(args: &[String]) -> CmdResult {
    let url = positional(args, 0, "<url>")?;
    let conn = open(url)?;
    Ok(render::stats(&conn.stats()))
}

/// `twilldb seed <url> <file.sql>` — run a seed script.
fn cmd_seed(args: &[String]) -> CmdResult {
    let url = positional(args, 0, "<url>")?;
    let file = positional(args, 1, "<file.sql>")?;
    let sql = std::fs::read_to_string(file).map_err(|e| Runtime(format!("reading {file}: {e}")))?;
    let mut conn = open(url)?;
    let mut n = 0usize;
    for stmt in migrate::split_statements(&sql) {
        conn.exec(&stmt)
            .map_err(|e| Runtime(format!("seed statement #{}: {e}", n + 1)))?;
        n += 1;
    }
    Ok(format!("seeded {file} ({n} statement(s))"))
}

/// `twilldb gen types <url> [--out <file>]` — TypeScript for `@twilldb/bun`.
fn cmd_gen(args: &[String]) -> CmdResult {
    match positional(args, 0, "a `gen` target (only `types` is supported)")? {
        "types" => {
            let url = positional(args, 1, "<url>")?;
            let conn = open(url)?;
            let ts = gentypes::generate(&conn.catalog());
            if let Some(out) = flag_value(args, "--out") {
                std::fs::write(out, &ts).map_err(|e| Runtime(format!("writing {out}: {e}")))?;
                Ok(format!("wrote {out}"))
            } else {
                // Already a complete file; print it verbatim (no trailing blank).
                Ok(ts.trim_end().to_string())
            }
        }
        other => Err(Usage(format!(
            "unknown `gen` target '{other}' (only `types` is supported)"
        ))),
    }
}

/// `twilldb migrate <new|up|status> …`.
fn cmd_migrate(args: &[String]) -> CmdResult {
    let sub = positional(args, 0, "a migrate subcommand (new|up|status)")?;
    let rest: Vec<String> = args.iter().skip(1).cloned().collect();
    match sub {
        "new" => {
            let name = positional(&rest, 0, "a migration name")?;
            let dir = flag_value(&rest, "--dir").unwrap_or("migrations");
            migrate::new(dir, name, now_unix()).map_err(Into::into)
        }
        "up" => {
            let url = positional(&rest, 0, "<url>")?;
            let dir = flag_value(&rest, "--dir").unwrap_or("migrations");
            let mut conn = open(url)?;
            migrate::up(&mut conn, dir, now_unix()).map_err(Into::into)
        }
        "status" => {
            let url = positional(&rest, 0, "<url>")?;
            let dir = flag_value(&rest, "--dir").unwrap_or("migrations");
            let mut conn = open(url)?;
            migrate::status(&mut conn, dir).map_err(Into::into)
        }
        other => Err(Usage(format!(
            "unknown migrate subcommand '{other}' (new|up|status)"
        ))),
    }
}

/// `twilldb shell <url>` — interactive REPL over stdin/stdout. Returns the exit
/// code directly (it streams rather than buffering a single output string).
fn cmd_shell(args: &[String]) -> i32 {
    let url = match positional(args, 0, "<url>") {
        Ok(u) => u,
        Err(e) => return finish(Err(e)),
    };
    let mut conn = match open(url) {
        Ok(c) => c,
        Err(e) => return finish(Err(e)),
    };
    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    let mut out = std::io::stdout();
    match render::shell(&mut conn, &mut input, &mut out, true) {
        Ok(()) => exit::OK,
        Err(e) => {
            eprintln!("error: {e}");
            exit::ERROR
        }
    }
}

// ---- time ---------------------------------------------------------------

/// Seconds since the Unix epoch (UTC). Clamped at 0 if the clock is before 1970.
fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Break a Unix timestamp into UTC `(year, month, day, hour, min, sec)` using
/// Howard Hinnant's `civil_from_days` algorithm — dependency-free, like the rest
/// of the workspace. Used for the migration filename prefix and `applied_at`.
pub(crate) fn unix_to_utc(secs: i64) -> (i64, u32, u32, u32, u32, u32) {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (hour, min, sec) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let month = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32; // [1, 12]
    let year = if month <= 2 { y + 1 } else { y };
    (year, month, day, hour as u32, min as u32, sec as u32)
}

/// `YYYYMMDDHHMMSS` — the migration filename timestamp prefix.
pub(crate) fn stamp_compact(secs: i64) -> String {
    let (y, mo, d, h, mi, s) = unix_to_utc(secs);
    format!("{y:04}{mo:02}{d:02}{h:02}{mi:02}{s:02}")
}

/// `YYYY-MM-DDTHH:MM:SSZ` — the ISO-8601 `applied_at` stamp.
pub(crate) fn stamp_iso(secs: i64) -> String {
    let (y, mo, d, h, mi, s) = unix_to_utc(secs);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

/// Management-half help, appended to the scaffolder help and shown on a usage
/// error. Kept here so the command surface lives next to its implementation.
pub fn print_manage_help() {
    eprintln!(
        "management commands (this build links the engine):\n\
         \x20 twilldb sql <url> \"<query>\" [--json]   run one statement/query\n\
         \x20 twilldb shell <url>                     interactive SQL REPL\n\
         \x20 twilldb tables <url>                    list tables\n\
         \x20 twilldb describe <url> <table>          show columns, PK, FKs\n\
         \x20 twilldb migrate new <name> [--dir d]    create a timestamped migration\n\
         \x20 twilldb migrate up <url> [--dir d]      apply pending migrations\n\
         \x20 twilldb migrate status <url> [--dir d]  applied vs pending + drift\n\
         \x20 twilldb gen types <url> [--out f]       TypeScript types for @twilldb/bun\n\
         \x20 twilldb seed <url> <file.sql>           run a seed script\n\
         \x20 twilldb stats <url>                     engine + storage stats\n\
         \n\
         <url> is a connection string: file:// (and s3://, r2://, gs://) open the\n\
         engine embedded — the CLI is itself the single writer, so point it only at\n\
         a local or stopped database. A live deployment is managed over postgres://\n\
         (Milestone 3, not wired up yet)."
    );
}
