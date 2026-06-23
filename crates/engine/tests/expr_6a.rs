//! Stage 6A — expression & single-table completeness: `||`, `CASE`,
//! `CAST(x AS t)`, `IN`/`BETWEEN`, `NULLS FIRST/LAST`, `LIKE … ESCAPE`/`ILIKE`,
//! the conditional/null function group, `RETURNING`, upsert (`ON CONFLICT` /
//! `OR IGNORE`/`OR REPLACE`), and `INSERT … SELECT`. Each test would fail
//! without its feature (spec 16 §6A; testing.md).

use engine::{Connection, EngineStatus, ResultSet};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

fn db_path(tag: &str) -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let mut p = std::env::temp_dir();
    p.push(format!("twill-6a-{tag}-{}-{n}.db", std::process::id()));
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
fn string_concat() {
    let (mut db, p) = open("concat");
    let rs = db
        .query("SELECT 'a' || 'b' || 'c' AS s, 1 || '-' || 2 AS n, 'x' || NULL AS z")
        .unwrap();
    assert_eq!(cell(&rs, 0, 0).as_deref(), Some("abc"));
    assert_eq!(cell(&rs, 0, 1).as_deref(), Some("1-2"));
    assert_eq!(cell(&rs, 0, 2), None, "|| propagates NULL");

    // `||` binds looser than `+` (so `1 + 2 || 3` is `(1+2) || 3` => '33').
    let rs = db.query("SELECT 1 + 2 || 3 AS s").unwrap();
    assert_eq!(cell(&rs, 0, 0).as_deref(), Some("33"));
    let _ = fs::remove_file(&p);
}

#[test]
fn case_searched_and_simple() {
    let (mut db, p) = open("case");
    db.exec("CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER)")
        .unwrap();
    db.exec("INSERT INTO t VALUES (1, 5), (2, 15), (3, 25)")
        .unwrap();

    // Searched CASE.
    let rs = db
        .query(
            "SELECT id, CASE WHEN n < 10 THEN 'lo' WHEN n < 20 THEN 'mid' ELSE 'hi' END AS bucket \
             FROM t ORDER BY id",
        )
        .unwrap();
    assert_eq!(cell(&rs, 0, 1).as_deref(), Some("lo"));
    assert_eq!(cell(&rs, 1, 1).as_deref(), Some("mid"));
    assert_eq!(cell(&rs, 2, 1).as_deref(), Some("hi"));

    // Simple CASE with no ELSE → NULL when nothing matches.
    let rs = db
        .query("SELECT CASE 2 WHEN 1 THEN 'one' WHEN 2 THEN 'two' END AS w")
        .unwrap();
    assert_eq!(cell(&rs, 0, 0).as_deref(), Some("two"));
    let rs = db
        .query("SELECT CASE 9 WHEN 1 THEN 'one' END AS w")
        .unwrap();
    assert_eq!(cell(&rs, 0, 0), None);
    let _ = fs::remove_file(&p);
}

#[test]
fn cast_as_function_form() {
    let (mut db, p) = open("castfn");
    // CAST(x AS t) is equivalent to x::t.
    let rs = db
        .query("SELECT CAST('42' AS INTEGER) AS a, CAST(3 AS double precision) AS b")
        .unwrap();
    assert_eq!(cell(&rs, 0, 0).as_deref(), Some("42"));
    assert_eq!(cell(&rs, 0, 1).as_deref(), Some("3.0"));
    let _ = fs::remove_file(&p);
}

#[test]
fn in_list_and_between() {
    let (mut db, p) = open("inbetween");
    db.exec("CREATE TABLE t (id INTEGER PRIMARY KEY, tag TEXT)")
        .unwrap();
    db.exec("INSERT INTO t VALUES (1,'a'),(2,'b'),(3,'c'),(4,'d')")
        .unwrap();

    let rs = db
        .query("SELECT id FROM t WHERE id IN (1, 3) ORDER BY id")
        .unwrap();
    assert_eq!(rs.rows.len(), 2);
    assert_eq!(cell(&rs, 0, 0).as_deref(), Some("1"));
    assert_eq!(cell(&rs, 1, 0).as_deref(), Some("3"));

    let rs = db
        .query("SELECT id FROM t WHERE tag NOT IN ('a','b') ORDER BY id")
        .unwrap();
    assert_eq!(rs.rows.len(), 2);
    assert_eq!(cell(&rs, 0, 0).as_deref(), Some("3"));

    let rs = db
        .query("SELECT id FROM t WHERE id BETWEEN 2 AND 3 ORDER BY id")
        .unwrap();
    assert_eq!(rs.rows.len(), 2);
    assert_eq!(cell(&rs, 0, 0).as_deref(), Some("2"));

    let rs = db
        .query("SELECT id FROM t WHERE id NOT BETWEEN 2 AND 3 ORDER BY id")
        .unwrap();
    assert_eq!(rs.rows.len(), 2);
    assert_eq!(cell(&rs, 0, 0).as_deref(), Some("1"));
    assert_eq!(cell(&rs, 1, 0).as_deref(), Some("4"));

    // IN with no NULL match but a NULL element → NULL (filtered out).
    let rs = db.query("SELECT 5 IN (1, NULL) AS r").unwrap();
    assert_eq!(cell(&rs, 0, 0), None);
    let _ = fs::remove_file(&p);
}

#[test]
fn nulls_first_last_ordering() {
    let (mut db, p) = open("nulls");
    db.exec("CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER)")
        .unwrap();
    db.exec("INSERT INTO t VALUES (1, 10), (2, NULL), (3, 20)")
        .unwrap();

    // Explicit NULLS LAST on ascending order.
    let rs = db
        .query("SELECT id FROM t ORDER BY n ASC NULLS LAST")
        .unwrap();
    assert_eq!(cell(&rs, 0, 0).as_deref(), Some("1"));
    assert_eq!(cell(&rs, 1, 0).as_deref(), Some("3"));
    assert_eq!(cell(&rs, 2, 0).as_deref(), Some("2"), "NULL sorts last");

    // Explicit NULLS FIRST on descending order.
    let rs = db
        .query("SELECT id FROM t ORDER BY n DESC NULLS FIRST")
        .unwrap();
    assert_eq!(cell(&rs, 0, 0).as_deref(), Some("2"), "NULL sorts first");
    let _ = fs::remove_file(&p);
}

#[test]
fn like_escape_and_ilike() {
    let (mut db, p) = open("like");
    db.exec("CREATE TABLE t (id INTEGER PRIMARY KEY, s TEXT)")
        .unwrap();
    db.exec("INSERT INTO t VALUES (1,'100%'),(2,'1000'),(3,'abc')")
        .unwrap();

    // ESCAPE makes the `%` after `\` a literal percent sign.
    let rs = db
        .query(r"SELECT id FROM t WHERE s LIKE '100\%' ESCAPE '\'")
        .unwrap();
    assert_eq!(rs.rows.len(), 1);
    assert_eq!(cell(&rs, 0, 0).as_deref(), Some("1"));

    // ILIKE parses and matches (currently case-insensitive like LIKE).
    let rs = db.query("SELECT id FROM t WHERE s ILIKE 'ABC'").unwrap();
    assert_eq!(rs.rows.len(), 1);
    assert_eq!(cell(&rs, 0, 0).as_deref(), Some("3"));
    let _ = fs::remove_file(&p);
}

#[test]
fn conditional_and_null_functions() {
    let (mut db, p) = open("condfn");
    let rs = db
        .query(
            "SELECT ifnull(NULL, 7) AS a, iif(1 < 2, 'y', 'n') AS b, \
             greatest(3, 9, 1) AS c, least(3, NULL, 1) AS d",
        )
        .unwrap();
    assert_eq!(cell(&rs, 0, 0).as_deref(), Some("7"));
    assert_eq!(cell(&rs, 0, 1).as_deref(), Some("y"));
    assert_eq!(cell(&rs, 0, 2).as_deref(), Some("9"));
    assert_eq!(cell(&rs, 0, 3).as_deref(), Some("1"), "least skips NULL");
    let _ = fs::remove_file(&p);
}

#[test]
fn returning_on_insert_update_delete() {
    let (mut db, p) = open("returning");
    db.exec("CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER)")
        .unwrap();

    // INSERT ... RETURNING projects the inserted rows.
    let rs = db
        .query("INSERT INTO t VALUES (1, 10), (2, 20) RETURNING id, n * 2 AS dbl")
        .unwrap();
    assert_eq!(rs.columns, vec!["id", "dbl"]);
    assert_eq!(rs.rows.len(), 2);
    assert_eq!(cell(&rs, 0, 0).as_deref(), Some("1"));
    assert_eq!(cell(&rs, 0, 1).as_deref(), Some("20"));

    // UPDATE ... RETURNING * projects the new row.
    let rs = db
        .query("UPDATE t SET n = 99 WHERE id = 1 RETURNING *")
        .unwrap();
    assert_eq!(rs.rows.len(), 1);
    assert_eq!(cell(&rs, 0, 1).as_deref(), Some("99"));

    // DELETE ... RETURNING projects the deleted (old) row.
    let rs = db
        .query("DELETE FROM t WHERE id = 2 RETURNING id, n")
        .unwrap();
    assert_eq!(rs.rows.len(), 1);
    assert_eq!(cell(&rs, 0, 0).as_deref(), Some("2"));
    assert_eq!(cell(&rs, 0, 1).as_deref(), Some("20"));

    let rs = db.query("SELECT COUNT(*) FROM t").unwrap();
    assert_eq!(cell(&rs, 0, 0).as_deref(), Some("1"));
    let _ = fs::remove_file(&p);
}

#[test]
fn upsert_on_conflict_do_nothing_and_update() {
    let (mut db, p) = open("upsert");
    db.exec("CREATE TABLE kv (k TEXT PRIMARY KEY, v INTEGER)")
        .unwrap();
    db.exec("INSERT INTO kv VALUES ('a', 1)").unwrap();

    // DO NOTHING leaves the existing row untouched and counts no change.
    db.exec("INSERT INTO kv VALUES ('a', 999) ON CONFLICT (k) DO NOTHING")
        .unwrap();
    assert_eq!(db.last_changes, 0);
    let rs = db.query("SELECT v FROM kv WHERE k = 'a'").unwrap();
    assert_eq!(cell(&rs, 0, 0).as_deref(), Some("1"));

    // DO UPDATE with `excluded` referencing the proposed row.
    db.exec("INSERT INTO kv VALUES ('a', 5) ON CONFLICT (k) DO UPDATE SET v = excluded.v + 100")
        .unwrap();
    let rs = db.query("SELECT v FROM kv WHERE k = 'a'").unwrap();
    assert_eq!(cell(&rs, 0, 0).as_deref(), Some("105"));

    // A non-conflicting upsert is a plain insert.
    db.exec("INSERT INTO kv VALUES ('b', 2) ON CONFLICT (k) DO UPDATE SET v = excluded.v")
        .unwrap();
    let rs = db.query("SELECT COUNT(*) FROM kv").unwrap();
    assert_eq!(cell(&rs, 0, 0).as_deref(), Some("2"));
    let _ = fs::remove_file(&p);
}

#[test]
fn sqlite_or_ignore_and_or_replace() {
    let (mut db, p) = open("orclause");
    db.exec("CREATE TABLE kv (k TEXT PRIMARY KEY, v INTEGER)")
        .unwrap();
    db.exec("INSERT INTO kv VALUES ('a', 1)").unwrap();

    // OR IGNORE no longer silently falls through to a PK failure — it skips.
    db.exec("INSERT OR IGNORE INTO kv VALUES ('a', 2)").unwrap();
    let rs = db.query("SELECT v FROM kv WHERE k = 'a'").unwrap();
    assert_eq!(cell(&rs, 0, 0).as_deref(), Some("1"));

    // OR REPLACE swaps the whole row.
    db.exec("INSERT OR REPLACE INTO kv VALUES ('a', 7)")
        .unwrap();
    let rs = db.query("SELECT v FROM kv WHERE k = 'a'").unwrap();
    assert_eq!(cell(&rs, 0, 0).as_deref(), Some("7"));

    // A plain duplicate insert is still a hard constraint error.
    let err = db.exec("INSERT INTO kv VALUES ('a', 9)").unwrap_err();
    assert_eq!(err.status, EngineStatus::ErrConstraint);
    let _ = fs::remove_file(&p);
}

#[test]
fn insert_select_copies_rows() {
    let (mut db, p) = open("inssel");
    db.exec("CREATE TABLE src (id INTEGER PRIMARY KEY, n INTEGER)")
        .unwrap();
    db.exec("CREATE TABLE dst (id INTEGER PRIMARY KEY, n INTEGER)")
        .unwrap();
    db.exec("INSERT INTO src VALUES (1, 10), (2, 20), (3, 30)")
        .unwrap();

    db.exec("INSERT INTO dst SELECT id, n * 2 FROM src WHERE n >= 20")
        .unwrap();
    assert_eq!(db.last_changes, 2);
    let rs = db.query("SELECT id, n FROM dst ORDER BY id").unwrap();
    assert_eq!(rs.rows.len(), 2);
    assert_eq!(cell(&rs, 0, 0).as_deref(), Some("2"));
    assert_eq!(cell(&rs, 0, 1).as_deref(), Some("40"));
    assert_eq!(cell(&rs, 1, 1).as_deref(), Some("60"));

    // Column-targeted INSERT ... SELECT.
    db.exec("INSERT INTO dst (id, n) SELECT id, n FROM src WHERE id = 1")
        .unwrap();
    let rs = db.query("SELECT COUNT(*) FROM dst").unwrap();
    assert_eq!(cell(&rs, 0, 0).as_deref(), Some("3"));
    let _ = fs::remove_file(&p);
}

#[test]
fn returning_persists_and_replays() {
    // The upsert/replace writes survive a reopen (durable WAL), proving the new
    // write shapes emit the ordinary WAL batch.
    let p = db_path("replay");
    let url = format!("file://{}", p.display());
    {
        let mut db = Connection::open(&url).unwrap();
        db.exec("CREATE TABLE kv (k TEXT PRIMARY KEY, v INTEGER)")
            .unwrap();
        db.exec("INSERT INTO kv VALUES ('a', 1)").unwrap();
        db.exec("INSERT OR REPLACE INTO kv VALUES ('a', 42)")
            .unwrap();
        db.exec("INSERT INTO kv VALUES ('b', 2) ON CONFLICT (k) DO NOTHING")
            .unwrap();
    }
    let mut db = Connection::open(&url).unwrap();
    let rs = db.query("SELECT k, v FROM kv ORDER BY k").unwrap();
    assert_eq!(rs.rows.len(), 2);
    assert_eq!(cell(&rs, 0, 1).as_deref(), Some("42"));
    assert_eq!(cell(&rs, 1, 1).as_deref(), Some("2"));
    let _ = fs::remove_file(&p);
}
