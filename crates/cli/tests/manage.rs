//! Behaviour tests for the `twilldb` management half (spec 19), built only with
//! the `manage` feature. Each command is driven through the test-facing
//! `manage::run` / `manage::shell_str` wrappers against a temp `file://`
//! database, so output is asserted without spawning a process or capturing
//! stdout. Migration apply / idempotency / drift are covered end-to-end.

use std::fs;
use std::path::PathBuf;

use twilldb_cli::manage;

/// A unique scratch directory under the OS temp dir for one test.
fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "twilldb-manage-{tag}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// A `file://` URL for a fresh database file under `dir`.
fn db_url(dir: &std::path::Path) -> String {
    format!("file://{}", dir.join("test.db").display())
}

fn args(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| s.to_string()).collect()
}

/// Run `migrate <sub> …`, returning its output.
fn migrate(parts: &[&str]) -> Result<String, String> {
    manage::run("migrate", &args(parts))
}

#[test]
fn sql_runs_ddl_dml_and_select() {
    let dir = scratch("sql");
    let url = db_url(&dir);

    manage::run("sql", &args(&[&url, "CREATE TABLE t (a integer, b text)"])).unwrap();
    let ins = manage::run("sql", &args(&[&url, "INSERT INTO t VALUES (1, 'x')"])).unwrap();
    assert!(ins.contains("1 row(s) affected"), "got: {ins}");

    let table = manage::run("sql", &args(&[&url, "SELECT a, b FROM t"])).unwrap();
    assert!(table.contains(" a "), "header missing: {table}");
    assert!(table.contains(" x "), "value missing: {table}");
    assert!(table.contains("(1 row(s))"), "footer missing: {table}");

    let json = manage::run("sql", &args(&[&url, "SELECT a, b FROM t", "--json"])).unwrap();
    assert_eq!(json, "[{\"a\":1,\"b\":\"x\"}]");

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn tables_and_describe_reflect_schema() {
    let dir = scratch("describe");
    let url = db_url(&dir);
    manage::run(
        "sql",
        &args(&[
            &url,
            "CREATE TABLE books (id integer primary key, title text NOT NULL, author_id integer)",
        ]),
    )
    .unwrap();

    let tables = manage::run("tables", &args(&[&url])).unwrap();
    assert!(tables.contains("books"), "got: {tables}");

    let desc = manage::run("describe", &args(&[&url, "books"])).unwrap();
    assert!(desc.contains("title"), "columns missing: {desc}");
    assert!(desc.contains("NOT NULL"), "nullability missing: {desc}");
    assert!(desc.contains("PK"), "primary key missing: {desc}");

    // A missing table is a runtime error, not a panic.
    let err = manage::run("describe", &args(&[&url, "nope"])).unwrap_err();
    assert!(err.contains("no such table"), "got: {err}");

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn gen_types_maps_storage_classes() {
    let dir = scratch("gentypes");
    let url = db_url(&dir);
    manage::run(
        "sql",
        &args(&[
            &url,
            "CREATE TABLE items (id integer NOT NULL, embedding vector(3), tag text NOT NULL)",
        ]),
    )
    .unwrap();

    let ts = manage::run("gen", &args(&["types", &url])).unwrap();
    assert!(ts.contains("export interface Items {"), "got: {ts}");
    assert!(ts.contains("id: number;"), "integer mapping: {ts}");
    assert!(
        ts.contains("embedding: number[] | null;"),
        "vector + nullable mapping: {ts}"
    );
    assert!(ts.contains("tag: string;"), "text NOT NULL mapping: {ts}");

    // `--out` writes the file and reports it.
    let out = dir.join("types.ts");
    let msg = manage::run(
        "gen",
        &args(&["types", &url, "--out", out.to_str().unwrap()]),
    )
    .unwrap();
    assert!(msg.contains("wrote"));
    let written = fs::read_to_string(&out).unwrap();
    assert!(written.contains("export interface Items"));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn migrate_new_writes_timestamped_file() {
    let dir = scratch("mignew");
    let mdir = dir.join("migrations");
    let mdir_s = mdir.to_str().unwrap();

    let msg = migrate(&["new", "add users", "--dir", mdir_s]).unwrap();
    assert!(msg.contains("created"), "got: {msg}");

    let entries: Vec<_> = fs::read_dir(&mdir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(entries.len(), 1);
    let name = &entries[0];
    // <14-digit timestamp>_add_users.sql — spaces slugified to underscores.
    assert!(name.ends_with("_add_users.sql"), "got: {name}");
    let stamp = name.split('_').next().unwrap();
    assert_eq!(stamp.len(), 14, "timestamp prefix: {name}");
    assert!(stamp.chars().all(|c| c.is_ascii_digit()));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn migrate_up_applies_pending_and_is_idempotent() {
    let dir = scratch("migup");
    let url = db_url(&dir);
    let mdir = dir.join("migrations");
    fs::create_dir_all(&mdir).unwrap();
    // 0001: DDL + DML (autocommit path). 0002: DML-only (transactional path).
    fs::write(
        mdir.join("0001_init.sql"),
        "CREATE TABLE t (id integer primary key, n integer);\nINSERT INTO t VALUES (1, 10);",
    )
    .unwrap();
    fs::write(
        mdir.join("0002_more.sql"),
        "INSERT INTO t VALUES (2, 20);\nINSERT INTO t VALUES (3, 30);",
    )
    .unwrap();
    let mdir_s = mdir.to_str().unwrap();

    let first = migrate(&["up", &url, "--dir", mdir_s]).unwrap();
    assert!(first.contains("applied 0001_init"), "got: {first}");
    assert!(first.contains("applied 0002_more"), "got: {first}");
    assert!(first.contains("2 migration(s) applied"), "got: {first}");

    // Data landed.
    let json = manage::run(
        "sql",
        &args(&[&url, "SELECT count(*) AS c FROM t", "--json"]),
    )
    .unwrap();
    assert_eq!(json, "[{\"c\":3}]");

    // Re-running is a no-op.
    let second = migrate(&["up", &url, "--dir", mdir_s]).unwrap();
    assert!(second.contains("up to date"), "got: {second}");

    // Status reports both applied.
    let status = migrate(&["status", &url, "--dir", mdir_s]).unwrap();
    assert!(status.contains("[applied] 0001_init"), "got: {status}");
    assert!(status.contains("[applied] 0002_more"), "got: {status}");

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn migrate_detects_checksum_drift() {
    let dir = scratch("migdrift");
    let url = db_url(&dir);
    let mdir = dir.join("migrations");
    fs::create_dir_all(&mdir).unwrap();
    let file = mdir.join("0001_init.sql");
    fs::write(&file, "CREATE TABLE t (id integer);").unwrap();
    let mdir_s = mdir.to_str().unwrap();

    migrate(&["up", &url, "--dir", mdir_s]).unwrap();

    // Edit the already-applied file — drift.
    fs::write(&file, "CREATE TABLE t (id integer, extra text);").unwrap();

    let status = migrate(&["status", &url, "--dir", mdir_s]).unwrap();
    assert!(
        status.contains("DRIFT"),
        "status should flag drift: {status}"
    );

    let up = migrate(&["up", &url, "--dir", mdir_s]).unwrap();
    assert!(up.contains("checksum drift"), "up should warn: {up}");

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn migrate_up_stops_on_failure() {
    let dir = scratch("migfail");
    let url = db_url(&dir);
    let mdir = dir.join("migrations");
    fs::create_dir_all(&mdir).unwrap();
    fs::write(mdir.join("0001_ok.sql"), "CREATE TABLE t (id integer);").unwrap();
    // References a column that does not exist → the statement fails.
    fs::write(
        mdir.join("0002_bad.sql"),
        "INSERT INTO t (nope) VALUES (1);",
    )
    .unwrap();
    let mdir_s = mdir.to_str().unwrap();

    let err = migrate(&["up", &url, "--dir", mdir_s]).unwrap_err();
    assert!(
        err.contains("0002_bad"),
        "names the failing migration: {err}"
    );

    // 0001 stayed applied; 0002 is still pending.
    let status = migrate(&["status", &url, "--dir", mdir_s]).unwrap();
    assert!(status.contains("[applied] 0001_ok"), "got: {status}");
    assert!(status.contains("[pending] 0002_bad"), "got: {status}");

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn seed_runs_a_script() {
    let dir = scratch("seed");
    let url = db_url(&dir);
    manage::run("sql", &args(&[&url, "CREATE TABLE t (a integer)"])).unwrap();
    let seedfile = dir.join("seed.sql");
    fs::write(
        &seedfile,
        "INSERT INTO t VALUES (1);\nINSERT INTO t VALUES (2);",
    )
    .unwrap();

    let msg = manage::run("seed", &args(&[&url, seedfile.to_str().unwrap()])).unwrap();
    assert!(msg.contains("2 statement"), "got: {msg}");

    let json = manage::run(
        "sql",
        &args(&[&url, "SELECT count(*) AS c FROM t", "--json"]),
    )
    .unwrap();
    assert_eq!(json, "[{\"c\":2}]");

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn stats_reports_counters() {
    let dir = scratch("stats");
    let url = db_url(&dir);
    manage::run("sql", &args(&[&url, "CREATE TABLE t (a integer)"])).unwrap();
    manage::run("sql", &args(&[&url, "INSERT INTO t VALUES (1)"])).unwrap();

    let stats = manage::run("stats", &args(&[&url])).unwrap();
    assert!(stats.contains("committed_lsn"), "got: {stats}");
    assert!(stats.contains("storage.wal_appends"), "got: {stats}");

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn shell_runs_statements_and_dot_commands() {
    let dir = scratch("shell");
    let url = db_url(&dir);
    manage::run("sql", &args(&[&url, "CREATE TABLE t (a integer, b text)"])).unwrap();

    let input = "INSERT INTO t VALUES (7, 'hi');\n\
                 SELECT a, b FROM t;\n\
                 .tables\n\
                 .quit\n";
    let out = manage::shell_str(&url, input).unwrap();
    assert!(out.contains("1 row(s) affected"), "insert echoed: {out}");
    assert!(out.contains(" hi "), "select rendered: {out}");
    assert!(out.contains("t"), ".tables listed: {out}");

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn transport_rejects_postgres_and_unknown_schemes() {
    // postgres:// is a recognized-but-unimplemented transport (Milestone 3).
    let pg = manage::run("tables", &args(&["postgres://localhost/db"])).unwrap_err();
    assert!(pg.contains("postgres://"), "got: {pg}");
    assert!(pg.contains("Milestone 3"), "got: {pg}");

    // An unknown scheme is rejected outright.
    let bad = manage::run("tables", &args(&["mysql://x"])).unwrap_err();
    assert!(bad.contains("scheme"), "got: {bad}");
}
