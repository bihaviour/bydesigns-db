//! Stage 6D — constraints, schema evolution & savepoints: DEFAULT, CHECK,
//! secondary/composite UNIQUE, composite PRIMARY KEY, AUTOINCREMENT/SERIAL,
//! ALTER TABLE, and SAVEPOINT/ROLLBACK TO/RELEASE. Spec 16 §6D.

use engine::{Connection, EngineStatus, ResultSet};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

fn db_path(tag: &str) -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let mut p = std::env::temp_dir();
    p.push(format!("twill-6d-{tag}-{}-{n}.db", std::process::id()));
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
fn default_values() {
    let (mut db, p) = open("default");
    db.exec(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, status TEXT DEFAULT 'new', n INTEGER DEFAULT 1 + 1)",
    )
    .unwrap();
    // Omitted columns take their defaults.
    db.exec("INSERT INTO t (id) VALUES (1)").unwrap();
    // The DEFAULT keyword as a value also takes the default.
    db.exec("INSERT INTO t (id, status) VALUES (2, DEFAULT)")
        .unwrap();
    let rs = db.query("SELECT id, status, n FROM t ORDER BY id").unwrap();
    assert_eq!(cell(&rs, 0, 1).as_deref(), Some("new"));
    assert_eq!(cell(&rs, 0, 2).as_deref(), Some("2"));
    assert_eq!(cell(&rs, 1, 1).as_deref(), Some("new"));
    let _ = fs::remove_file(&p);
}

#[test]
fn check_constraints() {
    let (mut db, p) = open("check");
    db.exec(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, age INTEGER CHECK (age >= 0), \
         CONSTRAINT pos CHECK (age < 200))",
    )
    .unwrap();
    db.exec("INSERT INTO t VALUES (1, 30)").unwrap();
    let err = db.exec("INSERT INTO t VALUES (2, -5)").unwrap_err();
    assert_eq!(err.status, EngineStatus::ErrConstraint);
    let err = db.exec("INSERT INTO t VALUES (3, 999)").unwrap_err();
    assert_eq!(err.status, EngineStatus::ErrConstraint);
    // CHECK also applies on UPDATE.
    let err = db.exec("UPDATE t SET age = -1 WHERE id = 1").unwrap_err();
    assert_eq!(err.status, EngineStatus::ErrConstraint);
    assert_eq!(db.query("SELECT count(*) FROM t").unwrap().rows.len(), 1);
    let _ = fs::remove_file(&p);
}

#[test]
fn autoincrement_and_serial() {
    let (mut db, p) = open("autoinc");
    db.exec("CREATE TABLE a (id INTEGER PRIMARY KEY AUTOINCREMENT, v TEXT)")
        .unwrap();
    db.exec("INSERT INTO a (v) VALUES ('x'), ('y')").unwrap();
    db.exec("INSERT INTO a (id, v) VALUES (10, 'explicit')")
        .unwrap();
    db.exec("INSERT INTO a (v) VALUES ('after')").unwrap();
    let rs = db.query("SELECT id FROM a ORDER BY id").unwrap();
    assert_eq!(cell(&rs, 0, 0).as_deref(), Some("1"));
    assert_eq!(cell(&rs, 1, 0).as_deref(), Some("2"));
    assert_eq!(cell(&rs, 2, 0).as_deref(), Some("10"));
    // The counter advances past the explicit 10.
    assert_eq!(cell(&rs, 3, 0).as_deref(), Some("11"));

    // SERIAL is an autoincrement integer too.
    db.exec("CREATE TABLE s (id SERIAL PRIMARY KEY, v TEXT)")
        .unwrap();
    db.exec("INSERT INTO s (v) VALUES ('a'), ('b')").unwrap();
    let rs = db.query("SELECT id FROM s ORDER BY id").unwrap();
    assert_eq!(cell(&rs, 0, 0).as_deref(), Some("1"));
    assert_eq!(cell(&rs, 1, 0).as_deref(), Some("2"));
    let _ = fs::remove_file(&p);
}

#[test]
fn secondary_unique() {
    let (mut db, p) = open("unique");
    db.exec("CREATE TABLE u (id INTEGER PRIMARY KEY, email TEXT UNIQUE)")
        .unwrap();
    db.exec("INSERT INTO u VALUES (1, 'a@x'), (2, 'b@x')")
        .unwrap();
    let err = db.exec("INSERT INTO u VALUES (3, 'a@x')").unwrap_err();
    assert_eq!(err.status, EngineStatus::ErrConstraint);
    // Multiple NULLs are allowed (NULLs are distinct).
    db.exec("INSERT INTO u VALUES (4, NULL), (5, NULL)")
        .unwrap();
    // UPDATE into a duplicate also fails.
    let err = db
        .exec("UPDATE u SET email = 'b@x' WHERE id = 1")
        .unwrap_err();
    assert_eq!(err.status, EngineStatus::ErrConstraint);
    let _ = fs::remove_file(&p);
}

#[test]
fn composite_primary_key() {
    let (mut db, p) = open("composite");
    db.exec("CREATE TABLE m (a INTEGER, b INTEGER, v TEXT, PRIMARY KEY (a, b))")
        .unwrap();
    db.exec("INSERT INTO m VALUES (1, 1, 'x'), (1, 2, 'y'), (2, 1, 'z')")
        .unwrap();
    // Same (a,b) tuple clashes; a differing component is fine.
    let err = db.exec("INSERT INTO m VALUES (1, 1, 'dup')").unwrap_err();
    assert_eq!(err.status, EngineStatus::ErrConstraint);
    db.exec("INSERT INTO m VALUES (2, 2, 'ok')").unwrap();
    assert_eq!(
        cell(&db.query("SELECT count(*) FROM m").unwrap(), 0, 0).as_deref(),
        Some("4")
    );
    let _ = fs::remove_file(&p);
}

#[test]
fn alter_table_add_drop_rename() {
    let (mut db, p) = open("alter");
    db.exec("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)")
        .unwrap();
    db.exec("INSERT INTO t VALUES (1, 'a')").unwrap();

    // ADD COLUMN backfills existing rows with the default.
    db.exec("ALTER TABLE t ADD COLUMN score INTEGER DEFAULT 0")
        .unwrap();
    let rs = db.query("SELECT id, score FROM t").unwrap();
    assert_eq!(cell(&rs, 0, 1).as_deref(), Some("0"));
    db.exec("INSERT INTO t (id, name, score) VALUES (2, 'b', 5)")
        .unwrap();

    // RENAME COLUMN.
    db.exec("ALTER TABLE t RENAME COLUMN name TO label")
        .unwrap();
    let rs = db.query("SELECT label FROM t ORDER BY id").unwrap();
    assert_eq!(cell(&rs, 0, 0).as_deref(), Some("a"));

    // DROP COLUMN.
    db.exec("ALTER TABLE t DROP COLUMN score").unwrap();
    assert!(db.query("SELECT score FROM t").is_err());

    // RENAME TO.
    db.exec("ALTER TABLE t RENAME TO renamed").unwrap();
    let rs = db.query("SELECT id FROM renamed ORDER BY id").unwrap();
    assert_eq!(rs.rows.len(), 2);
    let _ = fs::remove_file(&p);
}

#[test]
fn savepoints() {
    let (mut db, p) = open("savepoint");
    db.exec("CREATE TABLE t (id INTEGER PRIMARY KEY)").unwrap();
    db.exec("INSERT INTO t VALUES (1)").unwrap();

    db.exec("BEGIN").unwrap();
    db.exec("INSERT INTO t VALUES (2)").unwrap();
    db.exec("SAVEPOINT sp1").unwrap();
    db.exec("INSERT INTO t VALUES (3)").unwrap();
    db.exec("INSERT INTO t VALUES (4)").unwrap();
    // Read-your-writes: all four visible.
    assert_eq!(
        cell(&db.query("SELECT count(*) FROM t").unwrap(), 0, 0).as_deref(),
        Some("4")
    );
    // Roll back to the savepoint: 3 and 4 vanish, 2 stays.
    db.exec("ROLLBACK TO sp1").unwrap();
    assert_eq!(
        cell(&db.query("SELECT count(*) FROM t").unwrap(), 0, 0).as_deref(),
        Some("2")
    );
    // Continue and commit; the rolled-back rows never persist.
    db.exec("INSERT INTO t VALUES (5)").unwrap();
    db.exec("COMMIT").unwrap();
    let rs = db.query("SELECT id FROM t ORDER BY id").unwrap();
    assert_eq!(rs.rows.len(), 3);
    assert_eq!(cell(&rs, 0, 0).as_deref(), Some("1"));
    assert_eq!(cell(&rs, 1, 0).as_deref(), Some("2"));
    assert_eq!(cell(&rs, 2, 0).as_deref(), Some("5"));
    let _ = fs::remove_file(&p);
}

#[test]
fn constraints_persist_across_restart() {
    // Constraints + autoincrement survive a reopen (rebuilt from the durable WAL).
    let p = db_path("persist");
    let url = format!("file://{}", p.display());
    {
        let mut db = Connection::open(&url).unwrap();
        db.exec(
            "CREATE TABLE t (id INTEGER PRIMARY KEY AUTOINCREMENT, \
             email TEXT UNIQUE, status TEXT DEFAULT 'new', age INTEGER CHECK (age >= 0))",
        )
        .unwrap();
        db.exec("INSERT INTO t (email, age) VALUES ('a@x', 1), ('b@x', 2)")
            .unwrap();
    }
    let mut db = Connection::open(&url).unwrap();
    // Default still applies, autoincrement counter resumes at 3, UNIQUE/CHECK hold.
    db.exec("INSERT INTO t (email, age) VALUES ('c@x', 3)")
        .unwrap();
    let rs = db.query("SELECT id, status FROM t ORDER BY id").unwrap();
    assert_eq!(cell(&rs, 2, 0).as_deref(), Some("3"));
    assert_eq!(cell(&rs, 2, 1).as_deref(), Some("new"));
    let err = db
        .exec("INSERT INTO t (email, age) VALUES ('a@x', 9)")
        .unwrap_err();
    assert_eq!(err.status, EngineStatus::ErrConstraint);
    let err = db
        .exec("INSERT INTO t (email, age) VALUES ('d@x', -1)")
        .unwrap_err();
    assert_eq!(err.status, EngineStatus::ErrConstraint);
    let _ = fs::remove_file(&p);
}
