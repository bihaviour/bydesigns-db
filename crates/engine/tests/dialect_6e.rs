//! Stage 6E — dialect shims: `$n` / `:name` placeholders, backtick identifier
//! quoting, the LIKE (case-sensitive) vs ILIKE (case-insensitive) split, and the
//! accept-and-no-op session statements (SET / PRAGMA / VACUUM), SHOW, EXPLAIN.
//! Spec 16 §6E.

use engine::{Connection, ResultSet, Value};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

fn db_path(tag: &str) -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let mut p = std::env::temp_dir();
    p.push(format!("twill-6e-{tag}-{}-{n}.db", std::process::id()));
    let _ = fs::remove_file(&p);
    p
}

fn open(tag: &str) -> (Connection, PathBuf) {
    let p = db_path(tag);
    let db = Connection::open(&format!("file://{}", p.display())).unwrap();
    (db, p)
}

fn cell(rs: &ResultSet, row: usize, col: usize) -> Option<String> {
    rs.rows[row][col].render()
}

#[test]
fn numbered_and_named_placeholders() {
    let (mut db, p) = open("params");
    db.exec("CREATE TABLE t (id INTEGER PRIMARY KEY, a TEXT, b TEXT)")
        .unwrap();

    // Postgres `$n`: a repeated number reuses the same value.
    let mut ins = db
        .prepare("INSERT INTO t (id, a, b) VALUES ($1, $2, $2)")
        .unwrap();
    ins.bind(1, Value::Int(1)).unwrap();
    ins.bind(2, Value::Text("dup".into())).unwrap();
    let _ = ins.step().unwrap();
    drop(ins);
    let rs = db.query("SELECT a, b FROM t WHERE id = 1").unwrap();
    assert_eq!(cell(&rs, 0, 0).as_deref(), Some("dup"));
    assert_eq!(cell(&rs, 0, 1).as_deref(), Some("dup"));

    // SQLite `:name`: distinct names get sequential slots in first-seen order.
    let mut sel = db
        .prepare("SELECT id FROM t WHERE a = :val AND id = :n")
        .unwrap();
    sel.bind(1, Value::Text("dup".into())).unwrap(); // :val
    sel.bind(2, Value::Int(1)).unwrap(); // :n
    assert!(sel.step().unwrap());
    assert_eq!(
        sel.column_value(0).and_then(|v| v.render()).as_deref(),
        Some("1")
    );
    let _ = fs::remove_file(&p);
}

#[test]
fn backtick_identifiers() {
    let (mut db, p) = open("backtick");
    db.exec("CREATE TABLE `my table` (`id` INTEGER PRIMARY KEY, `select` TEXT)")
        .unwrap();
    db.exec("INSERT INTO `my table` (`id`, `select`) VALUES (1, 'kw')")
        .unwrap();
    let rs = db.query("SELECT `select` FROM `my table`").unwrap();
    assert_eq!(cell(&rs, 0, 0).as_deref(), Some("kw"));
    let _ = fs::remove_file(&p);
}

#[test]
fn like_is_case_sensitive_ilike_is_not() {
    let (mut db, p) = open("like");
    db.exec("CREATE TABLE t (id INTEGER PRIMARY KEY, s TEXT)")
        .unwrap();
    db.exec("INSERT INTO t VALUES (1, 'Hello'), (2, 'hello')")
        .unwrap();

    // LIKE matches case exactly.
    let rs = db.query("SELECT id FROM t WHERE s LIKE 'Hello'").unwrap();
    assert_eq!(rs.rows.len(), 1);
    assert_eq!(cell(&rs, 0, 0).as_deref(), Some("1"));

    // ILIKE folds case.
    let rs = db
        .query("SELECT id FROM t WHERE s ILIKE 'hello' ORDER BY id")
        .unwrap();
    assert_eq!(rs.rows.len(), 2);
    let _ = fs::remove_file(&p);
}

#[test]
fn session_statements_are_accepted() {
    let (mut db, p) = open("session");
    // SET / PRAGMA / VACUUM / ANALYZE / SET TRANSACTION are accepted no-ops.
    db.exec("SET search_path TO public").unwrap();
    db.exec("SET TIME ZONE 'UTC'").unwrap();
    db.exec("SET TRANSACTION ISOLATION LEVEL SERIALIZABLE")
        .unwrap();
    db.exec("PRAGMA foreign_keys = ON").unwrap();
    db.exec("VACUUM").unwrap();
    db.exec("ANALYZE").unwrap();

    // SHOW returns a one-row result.
    let rs = db.query("SHOW transaction_isolation").unwrap();
    assert_eq!(rs.rows.len(), 1);
    assert_eq!(cell(&rs, 0, 0).as_deref(), Some("repeatable read"));

    // The connection still works for real SQL afterwards.
    db.exec("CREATE TABLE t (id INTEGER PRIMARY KEY)").unwrap();
    db.exec("INSERT INTO t VALUES (1)").unwrap();
    assert_eq!(
        cell(&db.query("SELECT count(*) FROM t").unwrap(), 0, 0).as_deref(),
        Some("1")
    );
    let _ = fs::remove_file(&p);
}

#[test]
fn explain_returns_a_plan() {
    let (mut db, p) = open("explain");
    db.exec("CREATE TABLE t (id INTEGER PRIMARY KEY)").unwrap();
    let rs = db.query("EXPLAIN SELECT * FROM t").unwrap();
    assert_eq!(rs.columns, vec!["QUERY PLAN"]);
    assert_eq!(rs.rows.len(), 1);
    assert!(cell(&rs, 0, 0).unwrap().to_lowercase().contains("scan"));

    // EXPLAIN ANALYZE and EXPLAIN (options) parse too.
    assert!(db.query("EXPLAIN ANALYZE SELECT 1").is_ok());
    assert!(db.query("EXPLAIN (FORMAT TEXT) SELECT 1").is_ok());
    let _ = fs::remove_file(&p);
}
