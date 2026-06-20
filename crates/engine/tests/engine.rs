//! Engine correctness via the Rust API: DDL/DML/queries, file persistence
//! across restart, MVCC snapshot isolation, transactions, prepared statements,
//! and constraint enforcement (the Phase-1 exit criteria, spec 13 §Phase 1).

use engine::{Connection, EngineStatus, ResultSet, Value};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

fn db_path(tag: &str) -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let mut p = std::env::temp_dir();
    p.push(format!("bydesigns-eng-{tag}-{}-{n}.db", std::process::id()));
    let _ = fs::remove_file(&p);
    p
}

fn url_for(p: &std::path::Path) -> String {
    format!("file://{}", p.display())
}

fn cell(rs: &ResultSet, row: usize, col: usize) -> Option<String> {
    rs.rows[row][col].render()
}

#[test]
fn ddl_dml_and_select() {
    let p = db_path("ddl");
    let mut db = Connection::open(&url_for(&p)).unwrap();

    db.exec("CREATE TABLE notes (id INTEGER PRIMARY KEY, body TEXT, weight REAL)")
        .unwrap();
    db.exec("INSERT INTO notes (id, body, weight) VALUES (1, 'hello', 1.5)")
        .unwrap();
    db.exec("INSERT INTO notes (id, body, weight) VALUES (2, 'world', 2.0), (3, 'again', 3.0)")
        .unwrap();
    assert_eq!(db.last_changes, 2);

    let rs = db.query("SELECT id, body FROM notes WHERE id = 2").unwrap();
    assert_eq!(rs.rows.len(), 1);
    assert_eq!(cell(&rs, 0, 0).as_deref(), Some("2"));
    assert_eq!(cell(&rs, 0, 1).as_deref(), Some("world"));

    // UPDATE + DELETE
    db.exec("UPDATE notes SET body = 'HELLO' WHERE id = 1")
        .unwrap();
    assert_eq!(db.last_changes, 1);
    db.exec("DELETE FROM notes WHERE id = 3").unwrap();
    assert_eq!(db.last_changes, 1);

    let rs = db
        .query("SELECT id, body FROM notes ORDER BY id DESC")
        .unwrap();
    assert_eq!(rs.rows.len(), 2);
    assert_eq!(cell(&rs, 0, 0).as_deref(), Some("2"));
    assert_eq!(cell(&rs, 1, 1).as_deref(), Some("HELLO"));

    let rs = db.query("SELECT COUNT(*) AS n FROM notes").unwrap();
    assert_eq!(rs.columns[0], "n");
    assert_eq!(cell(&rs, 0, 0).as_deref(), Some("2"));

    let rs = db.query("SELECT 1 + 2 AS s, 'x' AS lit").unwrap();
    assert_eq!(cell(&rs, 0, 0).as_deref(), Some("3"));
    assert_eq!(cell(&rs, 0, 1).as_deref(), Some("x"));

    let _ = fs::remove_file(&p);
}

#[test]
fn persists_across_restart() {
    let p = db_path("persist");
    let url = url_for(&p);
    {
        let mut db = Connection::open(&url).unwrap();
        db.exec("CREATE TABLE kv (k TEXT PRIMARY KEY, v INTEGER)")
            .unwrap();
        db.exec("INSERT INTO kv VALUES ('a', 1), ('b', 2)").unwrap();
        db.exec("UPDATE kv SET v = 20 WHERE k = 'b'").unwrap();
        let lsn = db.last_lsn;
        assert!(lsn > 0, "commit should advance the LSN");
    } // all handles dropped → Database dropped → only the .db file remains

    // Reopen: state is rebuilt purely from the durable WAL.
    let mut db = Connection::open(&url).unwrap();
    let rs = db.query("SELECT k, v FROM kv ORDER BY k").unwrap();
    assert_eq!(rs.rows.len(), 2);
    assert_eq!(cell(&rs, 0, 0).as_deref(), Some("a"));
    assert_eq!(cell(&rs, 0, 1).as_deref(), Some("1"));
    assert_eq!(cell(&rs, 1, 0).as_deref(), Some("b"));
    assert_eq!(cell(&rs, 1, 1).as_deref(), Some("20"));

    let _ = fs::remove_file(&p);
}

#[test]
fn mvcc_snapshot_isolation() {
    let p = db_path("mvcc");
    let url = url_for(&p);
    let mut setup = Connection::open(&url).unwrap();
    setup
        .exec("CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER)")
        .unwrap();
    setup.exec("INSERT INTO t VALUES (1, 100)").unwrap();

    // Two handles to the same database share MVCC state in-process.
    let mut reader = Connection::open(&url).unwrap();
    let mut writer = Connection::open(&url).unwrap();

    reader.begin().unwrap(); // capture a stable snapshot
    let before = reader.query("SELECT COUNT(*) FROM t").unwrap();
    assert_eq!(cell(&before, 0, 0).as_deref(), Some("1"));

    // A concurrent committed write by another handle.
    writer.exec("INSERT INTO t VALUES (2, 200)").unwrap();

    // The reader still sees its consistent snapshot (not the new row).
    let during = reader.query("SELECT COUNT(*) FROM t").unwrap();
    assert_eq!(
        cell(&during, 0, 0).as_deref(),
        Some("1"),
        "reader must not see a write committed after its snapshot"
    );

    reader.commit().unwrap(); // end the snapshot
    let after = reader.query("SELECT COUNT(*) FROM t").unwrap();
    assert_eq!(
        cell(&after, 0, 0).as_deref(),
        Some("2"),
        "a fresh snapshot sees the committed write"
    );

    let _ = fs::remove_file(&p);
}

#[test]
fn transaction_rollback_discards_writes() {
    let p = db_path("rollback");
    let mut db = Connection::open(&url_for(&p)).unwrap();
    db.exec("CREATE TABLE t (id INTEGER PRIMARY KEY)").unwrap();
    db.exec("INSERT INTO t VALUES (1)").unwrap();

    db.begin().unwrap();
    db.exec("INSERT INTO t VALUES (2)").unwrap();
    // read-your-writes within the transaction
    let mid = db.query("SELECT COUNT(*) FROM t").unwrap();
    assert_eq!(cell(&mid, 0, 0).as_deref(), Some("2"));
    db.rollback().unwrap();

    let after = db.query("SELECT COUNT(*) FROM t").unwrap();
    assert_eq!(cell(&after, 0, 0).as_deref(), Some("1"));

    let _ = fs::remove_file(&p);
}

#[test]
fn prepared_statements_with_params() {
    let p = db_path("prep");
    let mut db = Connection::open(&url_for(&p)).unwrap();
    db.exec("CREATE TABLE u (id INTEGER PRIMARY KEY, name TEXT)")
        .unwrap();

    let mut ins = db
        .prepare("INSERT INTO u (id, name) VALUES (?, ?)")
        .unwrap();
    for (id, name) in [(1, "ada"), (2, "bel"), (3, "cyn")] {
        ins.bind(1, Value::Int(id)).unwrap();
        ins.bind(2, Value::Text(name.to_string())).unwrap();
        let _ = ins.step().unwrap(); // executes the INSERT
        assert_eq!(ins.changes(), 1);
        ins.reset();
    }
    drop(ins);

    let mut sel = db.prepare("SELECT name FROM u WHERE id = ?").unwrap();
    sel.bind(1, Value::Int(2)).unwrap();
    assert!(sel.step().unwrap());
    assert_eq!(
        sel.column_value(0).and_then(|v| v.render()).as_deref(),
        Some("bel")
    );
    assert!(!sel.step().unwrap()); // no more rows
    drop(sel);

    let rs = db.query("SELECT COUNT(*) FROM u").unwrap();
    assert_eq!(cell(&rs, 0, 0).as_deref(), Some("3"));

    let _ = fs::remove_file(&p);
}

#[test]
fn primary_key_uniqueness_is_enforced() {
    let p = db_path("pk");
    let mut db = Connection::open(&url_for(&p)).unwrap();
    db.exec("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)")
        .unwrap();
    db.exec("INSERT INTO t VALUES (1, 'a')").unwrap();

    let err = db.exec("INSERT INTO t VALUES (1, 'dup')").unwrap_err();
    assert_eq!(err.status, EngineStatus::ErrConstraint);

    // NOT NULL on the primary key.
    let err = db.exec("INSERT INTO t (v) VALUES ('no-id')").unwrap_err();
    assert_eq!(err.status, EngineStatus::ErrConstraint);

    // The failed inserts left no trace.
    let rs = db.query("SELECT COUNT(*) FROM t").unwrap();
    assert_eq!(cell(&rs, 0, 0).as_deref(), Some("1"));

    let _ = fs::remove_file(&p);
}

#[test]
fn aggregates_and_arithmetic() {
    let p = db_path("agg");
    let mut db = Connection::open(&url_for(&p)).unwrap();
    db.exec("CREATE TABLE m (id INTEGER PRIMARY KEY, x INTEGER)")
        .unwrap();
    db.exec("INSERT INTO m VALUES (1, 10), (2, 20), (3, 30)")
        .unwrap();

    let rs = db
        .query("SELECT COUNT(*) AS c, SUM(x) AS s, MIN(x) AS lo, MAX(x) AS hi, AVG(x) AS a FROM m")
        .unwrap();
    assert_eq!(cell(&rs, 0, 0).as_deref(), Some("3"));
    assert_eq!(cell(&rs, 0, 1).as_deref(), Some("60"));
    assert_eq!(cell(&rs, 0, 2).as_deref(), Some("10"));
    assert_eq!(cell(&rs, 0, 3).as_deref(), Some("30"));
    assert_eq!(cell(&rs, 0, 4).as_deref(), Some("20.0"));

    let rs = db
        .query("SELECT x FROM m WHERE x >= 20 ORDER BY x")
        .unwrap();
    assert_eq!(rs.rows.len(), 2);
    assert_eq!(cell(&rs, 0, 0).as_deref(), Some("20"));

    let _ = fs::remove_file(&p);
}

#[test]
fn parse_errors_are_sql_status() {
    let p = db_path("parseerr");
    let mut db = Connection::open(&url_for(&p)).unwrap();
    let err = db.exec("SELEKT oops").unwrap_err();
    assert_eq!(err.status, EngineStatus::ErrSql);
    let _ = fs::remove_file(&p);
}
