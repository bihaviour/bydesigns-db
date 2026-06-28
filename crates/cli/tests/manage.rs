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

// ---- Milestone 2: branches, db reset, schema dump, serve ----------------

#[test]
fn branch_create_list_delete() {
    let dir = scratch("branch");
    let url = db_url(&dir);
    manage::run("sql", &args(&[&url, "CREATE TABLE t (a integer)"])).unwrap();
    manage::run("sql", &args(&[&url, "INSERT INTO t VALUES (1)"])).unwrap();

    // No branches on a fresh database.
    let empty = manage::run("branch", &args(&["list", &url])).unwrap();
    assert!(empty.contains("no branches"), "got: {empty}");

    // Create one — reported with its id and a #branch= address.
    let created = manage::run("branch", &args(&["create", &url, "feature"])).unwrap();
    assert!(created.contains("created branch 1"), "got: {created}");
    assert!(created.contains("#branch=1"), "got: {created}");

    // List shows it.
    let list = manage::run("branch", &args(&["list", &url])).unwrap();
    assert!(list.contains("#branch=1"), "got: {list}");

    // Delete it; the namespace is empty again.
    let del = manage::run("branch", &args(&["delete", &url, "1"])).unwrap();
    assert!(del.contains("deleted branch 1"), "got: {del}");
    let after = manage::run("branch", &args(&["list", &url])).unwrap();
    assert!(after.contains("no branches"), "got: {after}");

    // A non-numeric branch id is a usage error, not a panic.
    let bad = manage::run("branch", &args(&["delete", &url, "nope"])).unwrap_err();
    assert!(bad.contains("must be a number"), "got: {bad}");

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn branch_writes_are_isolated_from_base() {
    let dir = scratch("branchiso");
    let url = db_url(&dir);
    manage::run("sql", &args(&[&url, "CREATE TABLE t (a integer)"])).unwrap();
    manage::run("sql", &args(&[&url, "INSERT INTO t VALUES (1)"])).unwrap();
    manage::run("branch", &args(&["create", &url, "b"])).unwrap();

    // Write through the branch address; the base must not see it.
    let branch_url = format!("{url}#branch=1");
    manage::run("sql", &args(&[&branch_url, "INSERT INTO t VALUES (2)"])).unwrap();

    let on_branch = manage::run(
        "sql",
        &args(&[&branch_url, "SELECT count(*) AS c FROM t", "--json"]),
    )
    .unwrap();
    assert_eq!(on_branch, "[{\"c\":2}]");
    let on_base = manage::run(
        "sql",
        &args(&[&url, "SELECT count(*) AS c FROM t", "--json"]),
    )
    .unwrap();
    assert_eq!(on_base, "[{\"c\":1}]", "base must be untouched");

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn migrate_up_branch_previews_without_touching_base() {
    let dir = scratch("migbranch");
    let url = db_url(&dir);
    let mdir = dir.join("migrations");
    fs::create_dir_all(&mdir).unwrap();
    fs::write(
        mdir.join("0001_init.sql"),
        "CREATE TABLE t (id integer primary key);",
    )
    .unwrap();
    let mdir_s = mdir.to_str().unwrap();

    let out = manage::run(
        "migrate",
        &args(&["up", &url, "--dir", mdir_s, "--branch", "preview"]),
    )
    .unwrap();
    assert!(out.contains("applied 0001_init"), "got: {out}");
    assert!(out.contains("previewed on branch 1"), "got: {out}");

    // The base is untouched — the migration applied only to the branch.
    let base_tables = manage::run("tables", &args(&[&url])).unwrap();
    assert!(
        base_tables.contains("no tables"),
        "base must be untouched: {base_tables}"
    );
    let branch_tables = manage::run("tables", &args(&[&format!("{url}#branch=1")])).unwrap();
    assert!(
        branch_tables.contains("t ("),
        "branch has it: {branch_tables}"
    );

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn db_reset_is_safe_by_default_and_rebuilds() {
    let dir = scratch("dbreset");
    let url = db_url(&dir);
    let mdir = dir.join("migrations");
    fs::create_dir_all(&mdir).unwrap();
    fs::write(
        mdir.join("0001_init.sql"),
        "CREATE TABLE t (id integer primary key, n integer);",
    )
    .unwrap();
    let mdir_s = mdir.to_str().unwrap();

    manage::run("migrate", &args(&["up", &url, "--dir", mdir_s])).unwrap();
    manage::run("sql", &args(&[&url, "CREATE TABLE stray (x integer)"])).unwrap();

    // A non-empty database refuses to reset without --force.
    let refused = manage::run("db", &args(&["reset", &url, "--dir", mdir_s])).unwrap_err();
    assert!(refused.contains("--force"), "got: {refused}");

    // With --force + --seed: drop everything, re-migrate, re-seed.
    let seed = dir.join("seed.sql");
    fs::write(&seed, "INSERT INTO t VALUES (1, 10);").unwrap();
    let out = manage::run(
        "db",
        &args(&[
            "reset",
            &url,
            "--dir",
            mdir_s,
            "--seed",
            seed.to_str().unwrap(),
            "--force",
        ]),
    )
    .unwrap();
    assert!(out.contains("dropped"), "got: {out}");
    assert!(out.contains("1 migration(s) applied"), "got: {out}");
    assert!(out.contains("seeded"), "got: {out}");

    // The stray table is gone; t exists with exactly the seeded row.
    let tables = manage::run("tables", &args(&[&url])).unwrap();
    assert!(!tables.contains("stray"), "stray dropped: {tables}");
    let json = manage::run("sql", &args(&[&url, "SELECT n FROM t", "--json"])).unwrap();
    assert_eq!(json, "[{\"n\":10}]");

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn schema_dump_reconstructs_reparseable_ddl() {
    let dir = scratch("schemadump");
    let url = db_url(&dir);
    manage::run(
        "sql",
        &args(&[
            &url,
            "CREATE TABLE authors (id integer primary key, name text NOT NULL, \
             email text UNIQUE, country text DEFAULT 'US')",
        ]),
    )
    .unwrap();
    manage::run(
        "sql",
        &args(&[
            &url,
            "CREATE TABLE books (id integer primary key, title text NOT NULL, \
             author_id integer REFERENCES authors(id), embedding vector(3))",
        ]),
    )
    .unwrap();

    let dump = manage::run("schema", &args(&["dump", &url])).unwrap();
    assert!(dump.contains("CREATE TABLE authors ("), "got: {dump}");
    assert!(dump.contains("id integer PRIMARY KEY"), "pk inline: {dump}");
    assert!(dump.contains("name text NOT NULL"), "not null: {dump}");
    assert!(dump.contains("email text UNIQUE"), "unique: {dump}");
    assert!(
        dump.contains("country text DEFAULT 'US'"),
        "default: {dump}"
    );
    assert!(dump.contains("embedding vector(3)"), "vector type: {dump}");
    assert!(
        dump.contains("FOREIGN KEY (author_id) REFERENCES authors (id)"),
        "fk: {dump}"
    );

    // The dump re-parses: replaying it into a fresh database reproduces the shape.
    let dir2 = scratch("schemadump2");
    let url2 = db_url(&dir2);
    for stmt in dump.split(';') {
        let s = stmt.trim();
        if !s.is_empty() {
            manage::run("sql", &args(&[&url2, s])).unwrap();
        }
    }
    let tables = manage::run("tables", &args(&[&url2])).unwrap();
    assert!(tables.contains("authors"), "got: {tables}");
    assert!(tables.contains("books"), "got: {tables}");

    fs::remove_dir_all(&dir).ok();
    fs::remove_dir_all(&dir2).ok();
}

#[test]
fn serve_validates_transport_without_binding() {
    // An embedded url + an explicit listen resolve cleanly.
    let (listen, url) =
        manage::serve_check(&args(&["file://./x.db", "--listen", "127.0.0.1:6000"])).unwrap();
    assert_eq!(listen, "127.0.0.1:6000");
    assert_eq!(url, "file://./x.db");

    // The listen address defaults when omitted.
    let (listen, _) = manage::serve_check(&args(&["file://./x.db"])).unwrap();
    assert_eq!(listen, "127.0.0.1:5433");

    // postgres:// and #branch= addresses are rejected before any bind.
    let pg = manage::serve_check(&args(&["postgres://h/db"])).unwrap_err();
    assert!(pg.contains("embedded"), "got: {pg}");
    let br = manage::serve_check(&args(&["file://./x.db#branch=1"])).unwrap_err();
    assert!(br.contains("#branch="), "got: {br}");
}

#[test]
fn transport_rejects_unknown_schemes() {
    // An unknown scheme is rejected outright (never silently defaulted).
    let bad = manage::run("tables", &args(&["mysql://x"])).unwrap_err();
    assert!(bad.contains("scheme"), "got: {bad}");

    // Branching is an embedded/storage-seam operation with no wire form, so a
    // live postgres:// deployment is refused with guidance (Milestone 3).
    let pg = manage::run("branch", &args(&["create", "postgres://localhost/db"])).unwrap_err();
    assert!(
        pg.contains("postgres://") && pg.contains("embedded"),
        "got: {pg}"
    );
}

// ---- Milestone 3: the postgres:// (pgwire) transport ----------------------

/// Spin up an in-process `engine-server` over the `file://` database at `url` and
/// return the `postgres://` address the management commands connect to. The
/// server is the sole writer; the CLI is just a wire client (the single-writer-
/// safe path the spec mandates for a live deployment).
fn start_pg_server(url: &str) -> String {
    use std::net::TcpListener;
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let backend = url.to_string();
    std::thread::spawn(move || {
        let _ = twill_server::serve_listener(listener, &backend);
    });
    format!("postgres://twilldb@127.0.0.1:{}/app", addr.port())
}

#[test]
fn postgres_transport_runs_sql_and_reflects_catalog() {
    let dir = scratch("pg-sql");
    let pg = start_pg_server(&db_url(&dir));

    // DDL + DML over the wire, against the server's database.
    manage::run(
        "sql",
        &args(&[
            &pg,
            "CREATE TABLE authors (id integer PRIMARY KEY, name text NOT NULL, \
             email text UNIQUE, country text DEFAULT 'US')",
        ]),
    )
    .unwrap();
    manage::run(
        "sql",
        &args(&[
            &pg,
            "CREATE TABLE books (id integer PRIMARY KEY, author_id integer REFERENCES authors(id))",
        ]),
    )
    .unwrap();
    let ins = manage::run(
        "sql",
        &args(&[&pg, "INSERT INTO authors (id, name) VALUES (1, 'Ada')"]),
    )
    .unwrap();
    assert!(ins.contains("1 row(s) affected"), "got: {ins}");

    let sel = manage::run("sql", &args(&[&pg, "SELECT name FROM authors"])).unwrap();
    assert!(
        sel.contains("Ada") && sel.contains("(1 row(s))"),
        "got: {sel}"
    );

    // Inspect commands reflect the catalog over the `twill.catalog` surface.
    let tables = manage::run("tables", &args(&[&pg])).unwrap();
    assert!(
        tables.contains("authors") && tables.contains("books"),
        "got: {tables}"
    );

    let desc = manage::run("describe", &args(&[&pg, "books"])).unwrap();
    assert!(desc.contains("author_id"), "columns missing: {desc}");
    assert!(
        desc.contains("authors") && desc.contains("Foreign keys"),
        "FK missing: {desc}"
    );

    // `schema dump` reconstructs reparseable DDL purely from the wire reflection.
    let dump = manage::run("schema", &args(&["dump", &pg])).unwrap();
    assert!(dump.contains("CREATE TABLE authors"), "got: {dump}");
    assert!(dump.contains("name text NOT NULL"), "got: {dump}");
    // UNIQUE / DEFAULT round-trip over the wire reflection too (twill.catalog).
    assert!(dump.contains("email text UNIQUE"), "wire unique: {dump}");
    assert!(
        dump.contains("country text DEFAULT 'US'"),
        "wire default: {dump}"
    );
    assert!(
        dump.contains("FOREIGN KEY (author_id) REFERENCES authors (id)"),
        "got: {dump}"
    );

    // `gen types` reflects the same catalog into TypeScript.
    let ts = manage::run("gen", &args(&["types", &pg])).unwrap();
    assert!(ts.contains("export interface Authors"), "got: {ts}");
    assert!(ts.contains("name: string;"), "got: {ts}");

    // `stats` parses the `twill.stats` rows back into the same rendered block.
    let stats = manage::run("stats", &args(&[&pg])).unwrap();
    assert!(stats.contains("commits"), "got: {stats}");
    assert!(stats.contains("storage.wal_appends"), "got: {stats}");

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn postgres_transport_applies_migrations() {
    let dir = scratch("pg-migrate");
    let mig_dir = dir.join("migrations");
    fs::create_dir_all(&mig_dir).unwrap();
    fs::write(
        mig_dir.join("20260101000000_init.sql"),
        "CREATE TABLE widgets (id integer PRIMARY KEY, label text);",
    )
    .unwrap();
    let mig = mig_dir.to_str().unwrap();
    let pg = start_pg_server(&db_url(&dir));

    // Apply pending migrations over the wire, then confirm idempotency + status.
    let up = migrate(&["up", &pg, "--dir", mig]).unwrap();
    assert!(up.contains("applied 20260101000000_init"), "got: {up}");
    let again = migrate(&["up", &pg, "--dir", mig]).unwrap();
    assert!(again.contains("up to date"), "got: {again}");
    let status = migrate(&["status", &pg, "--dir", mig]).unwrap();
    assert!(
        status.contains("[applied] 20260101000000_init"),
        "got: {status}"
    );

    // The migrated table is visible over the same wire transport.
    let tables = manage::run("tables", &args(&[&pg])).unwrap();
    assert!(tables.contains("widgets"), "got: {tables}");

    fs::remove_dir_all(&dir).ok();
}
