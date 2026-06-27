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
//! a live deployment — it drives a running `engine-server` over the wire (see
//! [`wire`]). The scheme picks the transport, modeled as a [`Conn`] the commands
//! are written against. Unknown schemes are rejected, never defaulted.
//!
//! ## The single-writer caveat
//!
//! Twill is single-writer-per-database (a durable, epoch-fenced lease). When the
//! CLI opens `file://` embedded it *is* the writer, so it must not be pointed at a
//! database an app or server currently holds — manage a live deployment over
//! `postgres://` instead, where the server stays the sole writer and the CLI is
//! just a client. The command help repeats this.
//!
//! The command bodies return `Result<String, String>` (rendered output, or an
//! error message) so the integration tests can drive them against a temp
//! `file://` database without spawning a process or capturing stdout.

mod branch;
mod gentypes;
mod migrate;
mod render;
mod schema;
mod wire;

use crate::exit;
use std::time::{SystemTime, UNIX_EPOCH};

/// A management connection over the transport chosen by the connection-string
/// scheme: [`Embedded`](Conn::Embedded) links the engine in-process (`file://` and
/// the object stores), [`Wire`](Conn::Wire) drives a running `engine-server` over
/// pgwire (`postgres://`, Milestone 3). The commands are written against this
/// enum, so the same surface manages a local database and a live deployment; the
/// renderers never learn which transport produced a result.
pub enum Conn {
    Embedded(engine::Connection),
    Wire(wire::WireConn),
}

impl Conn {
    /// Run a query, returning the engine result set (rebuilt from text cells on
    /// the wire path).
    pub fn query(&mut self, sql: &str) -> Result<engine::ResultSet, String> {
        match self {
            Conn::Embedded(c) => c.query(sql).map_err(|e| e.to_string()),
            Conn::Wire(c) => c.query(sql),
        }
    }

    /// Run a statement for its effect, updating [`last_changes`](Conn::last_changes).
    pub fn exec(&mut self, sql: &str) -> Result<(), String> {
        match self {
            Conn::Embedded(c) => c.exec(sql).map_err(|e| e.to_string()),
            Conn::Wire(c) => c.exec(sql),
        }
    }

    /// Affected-row count of the last `exec`/`query`.
    pub fn last_changes(&self) -> i64 {
        match self {
            Conn::Embedded(c) => c.last_changes,
            Conn::Wire(c) => c.last_changes,
        }
    }

    /// The live catalog — read directly when embedded, reflected over the
    /// `twill.catalog` surface when on the wire.
    pub fn catalog(&mut self) -> Result<Vec<engine::CatalogTable>, String> {
        match self {
            Conn::Embedded(c) => Ok(c.catalog()),
            Conn::Wire(c) => c.catalog(),
        }
    }

    /// The engine + storage observability snapshot.
    pub fn stats(&mut self) -> Result<engine::EngineStats, String> {
        match self {
            Conn::Embedded(c) => Ok(c.stats()),
            Conn::Wire(c) => c.stats(),
        }
    }
}

/// Dispatch a management subcommand. `cmd` is the top-level verb (`sql`,
/// `migrate`, …) and `rest` the arguments after it. Prints output to stdout and
/// errors to stderr, returning the process exit code.
pub fn dispatch(cmd: &str, rest: &[String]) -> i32 {
    // `shell` and `serve` run long / stream to stdio rather than buffering one
    // output string, so they bypass the buffered-output path.
    if cmd == "shell" {
        return cmd_shell(rest);
    }
    if cmd == "serve" {
        return cmd_serve(rest);
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
        "branch" => branch::cmd_branch(rest),
        "schema" => schema::cmd_schema(rest),
        "db" => cmd_db(rest),
        "shell" => Err(Usage(
            "`shell` is interactive; run it from a terminal".into(),
        )),
        "serve" => Err(Usage(
            "`serve` runs a server; run it from a terminal".into(),
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

/// Test-facing wrapper: validate `serve` arguments without binding a listener,
/// returning the resolved `(listen, url)` or a flattened error message.
pub fn serve_check(rest: &[String]) -> Result<(String, String), String> {
    serve_args(rest).map_err(|e| match e {
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

/// Open a database by connection-string scheme (spec 19 transport model):
/// `file://` and the object stores open the engine **embedded** (this process is
/// the writer); `postgres://` drives a running `engine-server` over **pgwire**
/// (Milestone 3), where the server stays the sole writer. Unknown schemes are
/// rejected, never defaulted.
///
/// A `<base-url>#branch=<id>` suffix (Milestone 2) opens a copy-on-write branch
/// instead of the base line — the address `branch create` / `branch list` hand
/// back, so every command can target a branch. Branches are an embedded /
/// storage-seam concept with no wire form, so a branch address requires an
/// embedded base.
fn open(url: &str) -> Result<Conn, CmdError> {
    if url.starts_with("postgres://") {
        return wire::WireConn::connect(url)
            .map(Conn::Wire)
            .map_err(Runtime);
    }
    open_embedded(url).map(Conn::Embedded)
}

/// Open the engine **embedded** at `url`, the in-process transport that links the
/// engine and makes this process the writer. Used directly by the commands that
/// are inherently embedded — branching (a storage-seam operation) and the
/// preview-and-swap fork — which have no `postgres://` form; a wire URL is
/// rejected here with guidance to manage those against the embedded database.
fn open_embedded(url: &str) -> Result<engine::Connection, CmdError> {
    if let Some((base, id)) = url.split_once("#branch=") {
        let id: u64 = id
            .parse()
            .map_err(|_| Usage(format!("branch id in '{url}' must be a number")))?;
        return engine::Connection::open_branch(base, engine::BranchId(id))
            .map_err(|e| Runtime(format!("opening branch {id} of {base}: {e}")));
    }
    if url.starts_with("file://")
        || url.starts_with("s3://")
        || url.starts_with("r2://")
        || url.starts_with("gs://")
    {
        engine::Connection::open(url).map_err(|e| Runtime(format!("opening {url}: {e}")))
    } else if url.starts_with("postgres://") {
        Err(Usage(format!(
            "'{url}' is a live (postgres://) deployment; branching and the \
             preview-and-swap fork are storage-level operations with no wire form. \
             Manage branches against the embedded database (file://)."
        )))
    } else {
        Err(Usage(format!(
            "unrecognized connection-string scheme in '{url}'. \
             Use file:// (embedded) — s3://, r2://, gs:// are also embedded; \
             postgres:// drives a running engine-server over pgwire."
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
    let rs = conn.query(query).map_err(Runtime)?;
    Ok(render::result(&rs, conn.last_changes(), json))
}

/// `twilldb tables <url>` — list tables from the catalog.
fn cmd_tables(args: &[String]) -> CmdResult {
    let url = positional(args, 0, "<url>")?;
    let mut conn = open(url)?;
    let tables = conn.catalog().map_err(Runtime)?;
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
    let mut conn = open(url)?;
    let t = conn
        .catalog()
        .map_err(Runtime)?
        .into_iter()
        .find(|t| t.name.eq_ignore_ascii_case(table))
        .ok_or_else(|| Runtime(format!("no such table: {table}")))?;
    Ok(render::describe(&t))
}

/// `twilldb stats <url>` — engine + storage observability snapshot.
fn cmd_stats(args: &[String]) -> CmdResult {
    let url = positional(args, 0, "<url>")?;
    let mut conn = open(url)?;
    Ok(render::stats(&conn.stats().map_err(Runtime)?))
}

/// `twilldb seed <url> <file.sql>` — run a seed script.
fn cmd_seed(args: &[String]) -> CmdResult {
    let url = positional(args, 0, "<url>")?;
    let file = positional(args, 1, "<file.sql>")?;
    let mut conn = open(url)?;
    apply_seed_file(&mut conn, file)
}

/// Run a `.sql` script statement-by-statement against `conn`. Shared by `seed`
/// and `db reset --seed`.
fn apply_seed_file(conn: &mut Conn, file: &str) -> CmdResult {
    let sql = std::fs::read_to_string(file).map_err(|e| Runtime(format!("reading {file}: {e}")))?;
    let mut n = 0usize;
    for stmt in migrate::split_statements(&sql) {
        conn.exec(&stmt)
            .map_err(|e| Runtime(format!("seed statement #{}: {e}", n + 1)))?;
        n += 1;
    }
    Ok(format!("seeded {file} ({n} statement(s))"))
}

/// `twilldb db reset <url> [--dir d] [--seed f] [--force]` — drop every table,
/// re-apply all migrations, and optionally re-seed: a clean rebuild of the
/// database's shape and fixtures. Destructive, so a non-empty database requires
/// `--force` (spec 19 safe-by-default).
fn cmd_db(args: &[String]) -> CmdResult {
    let sub = positional(args, 0, "a `db` subcommand (only `reset` is supported)")?;
    if sub != "reset" {
        return Err(Usage(format!(
            "unknown `db` subcommand '{sub}' (only `reset` is supported)"
        )));
    }
    // positional 0 is `reset`; the url is positional 1.
    let url = positional(args, 1, "<url>")?;
    let dir = flag_value(args, "--dir").unwrap_or("migrations");
    let mut conn = open(url)?;

    let tables: Vec<String> = conn
        .catalog()
        .map_err(Runtime)?
        .into_iter()
        .map(|t| t.name)
        .collect();
    if !tables.is_empty() && !has_flag(args, "--force") {
        return Err(Usage(format!(
            "refusing to reset a non-empty database ({} table(s)) without --force",
            tables.len()
        )));
    }
    // Drop every table (DDL is autocommit; the engine enforces no FK, so order
    // is irrelevant). The migration tracking table drops too, so `migrate up`
    // re-applies from a clean slate.
    for t in &tables {
        conn.exec(&format!("DROP TABLE IF EXISTS {t}"))
            .map_err(|e| Runtime(format!("dropping {t}: {e}")))?;
    }
    let mut out = format!("dropped {} table(s)\n", tables.len());
    out.push_str(&migrate::up(&mut conn, dir, now_unix())?);
    if let Some(seed) = flag_value(args, "--seed") {
        out.push('\n');
        out.push_str(&apply_seed_file(&mut conn, seed)?);
    }
    Ok(out)
}

/// `twilldb serve <url> [--listen HOST:PORT]` — run the engine behind a
/// Postgres-wire listener (Phase 3's `engine-server`, composed in-process).
/// Blocks until the server exits. Returns the process exit code directly.
fn cmd_serve(args: &[String]) -> i32 {
    let (listen, url) = match serve_args(args) {
        Ok(v) => v,
        Err(e) => return finish(Err(e)),
    };
    eprintln!("twilldb serve: listening on {listen}, serving {url}");
    match twill_server::run(&listen, &url) {
        Ok(()) => exit::OK,
        Err(e) => {
            eprintln!("error: serve: {e}");
            exit::ERROR
        }
    }
}

/// Parse and validate `serve` arguments *without* binding, so the logic is unit
/// testable (the bind/block is the only untested part). Returns `(listen, url)`.
/// The transport must be embedded — the server itself opens the base as the sole
/// writer — so `postgres://` / an unknown scheme / a `#branch=` address is
/// rejected.
fn serve_args(args: &[String]) -> Result<(String, String), CmdError> {
    let url = positional(args, 0, "<url>")?;
    if url.contains("#branch=") {
        return Err(Usage(
            "serve runs the base database; pass the base url, not a #branch= address".into(),
        ));
    }
    let embedded = ["file://", "s3://", "r2://", "gs://"]
        .iter()
        .any(|s| url.starts_with(s));
    if !embedded {
        return Err(Usage(format!(
            "serve needs an embedded url (file://, s3://, r2://, gs://); got '{url}'"
        )));
    }
    let listen = flag_value(args, "--listen")
        .unwrap_or("127.0.0.1:5433")
        .to_string();
    Ok((listen, url.to_string()))
}

/// `twilldb gen types <url> [--out <file>]` — TypeScript for `@twilldb/bun`.
fn cmd_gen(args: &[String]) -> CmdResult {
    match positional(args, 0, "a `gen` target (only `types` is supported)")? {
        "types" => {
            let url = positional(args, 1, "<url>")?;
            let mut conn = open(url)?;
            let ts = gentypes::generate(&conn.catalog().map_err(Runtime)?);
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
            if has_flag(&rest, "--branch") {
                return migrate_up_branch(url, dir, &rest);
            }
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

/// `migrate up <url> --branch [name]` (spec 19 preview-and-swap): fork a
/// copy-on-write branch off the base, apply the pending migrations there, and
/// report how to inspect / promote / discard it. Turns Twill's branching into a
/// migration safety feature — verify a risky change on a zero-copy fork before
/// touching the base, which a single-writer, autocommit-DDL engine cannot do
/// transactionally in place.
fn migrate_up_branch(url: &str, dir: &str, args: &[String]) -> CmdResult {
    if url.contains("#branch=") {
        return Err(Usage(
            "migrate up --branch forks from the base; pass the base url, not a #branch= address"
                .into(),
        ));
    }
    // An optional label after `--branch`; anything starting with `--` is the next
    // flag, not the name.
    let label = match flag_value(args, "--branch") {
        Some(v) if !v.starts_with("--") => v,
        _ => "migrate-preview",
    };
    let base = open_embedded(url)?;
    let id = base
        .create_branch(label)
        .map_err(|e| Runtime(e.to_string()))?;
    let branch_url = format!("{url}#branch={}", id.0);
    let mut branch_conn = open(&branch_url)?;
    let applied = migrate::up(&mut branch_conn, dir, now_unix())?;
    Ok(format!(
        "{applied}\n\n\
         previewed on branch {id} (\"{label}\").\n\
         \x20 inspect:  twilldb schema dump {branch_url}\n\
         \x20 promote:  twilldb migrate up {url}   (applies the same migrations to the base)\n\
         \x20 discard:  twilldb branch delete {url} {id}",
        id = id.0
    ))
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
         \x20 twilldb migrate up <url> --branch [n]   preview on a copy-on-write branch\n\
         \x20 twilldb migrate status <url> [--dir d]  applied vs pending + drift\n\
         \x20 twilldb gen types <url> [--out f]       TypeScript types for @twilldb/bun\n\
         \x20 twilldb seed <url> <file.sql>           run a seed script\n\
         \x20 twilldb stats <url>                     engine + storage stats\n\
         \x20 twilldb branch create <url> [name]      fork a copy-on-write branch\n\
         \x20 twilldb branch list <url>               list branches\n\
         \x20 twilldb branch delete <url> <id>        delete a branch\n\
         \x20 twilldb db reset <url> [--seed f] [--force]   drop, re-migrate, re-seed\n\
         \x20 twilldb schema dump <url>               print reconstructed CREATE TABLE DDL\n\
         \x20 twilldb serve <url> [--listen H:P]      run the engine behind pgwire\n\
         \n\
         <url> is a connection string: file:// (and s3://, r2://, gs://) open the\n\
         engine embedded — the CLI is itself the single writer, so point it only at\n\
         a local or stopped database. A branch is addressed as <url>#branch=<id>\n\
         (embedded only). A live deployment is managed over postgres:// (read /\n\
         inspect / migrate over the wire; the server stays the sole writer)."
    );
}
