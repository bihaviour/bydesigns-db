//! Stage 6C — the scalar function library: string, math, date/time, UUID, and
//! JSON helpers, plus the replay-determinism contract for non-deterministic
//! functions (now/random/gen_random_uuid evaluate once; the concrete value is
//! what lands in the WAL). Spec 16 §6C; testing.md.

use engine::{Connection, ResultSet};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

fn db_path(tag: &str) -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let mut p = std::env::temp_dir();
    p.push(format!("twill-6c-{tag}-{}-{n}.db", std::process::id()));
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

fn scalar(db: &mut Connection, sql: &str) -> Option<String> {
    let rs = db.query(sql).unwrap();
    cell(&rs, 0, 0)
}

#[test]
fn string_functions() {
    let (mut db, p) = open("str");
    assert_eq!(
        scalar(&mut db, "SELECT substr('hello', 2, 3)").as_deref(),
        Some("ell")
    );
    assert_eq!(
        scalar(&mut db, "SELECT substr('hello', -2)").as_deref(),
        Some("lo")
    );
    assert_eq!(
        scalar(&mut db, "SELECT replace('a-b-c', '-', '_')").as_deref(),
        Some("a_b_c")
    );
    assert_eq!(
        scalar(&mut db, "SELECT instr('hello', 'l')").as_deref(),
        Some("3")
    );
    assert_eq!(
        scalar(&mut db, "SELECT instr('hello', 'z')").as_deref(),
        Some("0")
    );
    assert_eq!(
        scalar(&mut db, "SELECT repeat('ab', 3)").as_deref(),
        Some("ababab")
    );
    assert_eq!(
        scalar(&mut db, "SELECT reverse('abc')").as_deref(),
        Some("cba")
    );
    assert_eq!(
        scalar(&mut db, "SELECT left('hello', 2)").as_deref(),
        Some("he")
    );
    assert_eq!(
        scalar(&mut db, "SELECT right('hello', 2)").as_deref(),
        Some("lo")
    );
    assert_eq!(
        scalar(&mut db, "SELECT lpad('7', 3, '0')").as_deref(),
        Some("007")
    );
    assert_eq!(
        scalar(&mut db, "SELECT rpad('7', 3, '.')").as_deref(),
        Some("7..")
    );
    let _ = fs::remove_file(&p);
}

#[test]
fn math_functions() {
    let (mut db, p) = open("math");
    assert_eq!(scalar(&mut db, "SELECT ceil(2.1)").as_deref(), Some("3.0"));
    assert_eq!(scalar(&mut db, "SELECT floor(2.9)").as_deref(), Some("2.0"));
    assert_eq!(scalar(&mut db, "SELECT sqrt(9)").as_deref(), Some("3.0"));
    assert_eq!(
        scalar(&mut db, "SELECT power(2, 10)").as_deref(),
        Some("1024.0")
    );
    assert_eq!(scalar(&mut db, "SELECT mod(17, 5)").as_deref(), Some("2"));
    assert_eq!(scalar(&mut db, "SELECT sign(-4)").as_deref(), Some("-1"));
    assert_eq!(
        scalar(&mut db, "SELECT round(3.14159, 2)").as_deref(),
        Some("3.14")
    );
    assert_eq!(scalar(&mut db, "SELECT round(2.6)").as_deref(), Some("3"));
    assert_eq!(scalar(&mut db, "SELECT abs(-7)").as_deref(), Some("7"));
    let _ = fs::remove_file(&p);
}

#[test]
fn datetime_functions() {
    let (mut db, p) = open("dt");
    // Fixed reference: 2021-07-04 12:30:45 UTC.
    let ts = "'2021-07-04 12:30:45'";
    assert_eq!(
        scalar(&mut db, &format!("SELECT date({ts})")).as_deref(),
        Some("2021-07-04")
    );
    assert_eq!(
        scalar(&mut db, &format!("SELECT datetime({ts})")).as_deref(),
        Some("2021-07-04 12:30:45")
    );
    assert_eq!(
        scalar(&mut db, &format!("SELECT date_trunc('month', {ts})")).as_deref(),
        Some("2021-07-01 00:00:00")
    );
    assert_eq!(
        scalar(&mut db, &format!("SELECT extract(year FROM {ts})")).as_deref(),
        Some("2021")
    );
    assert_eq!(
        scalar(&mut db, &format!("SELECT extract(dow FROM {ts})")).as_deref(),
        Some("0") // a Sunday
    );
    assert_eq!(
        scalar(&mut db, &format!("SELECT strftime('%Y/%m/%d', {ts})")).as_deref(),
        Some("2021/07/04")
    );
    // now() returns a parseable ISO timestamp.
    let now = scalar(&mut db, "SELECT now()").unwrap();
    assert!(
        now.len() == 19 && now.contains('-') && now.contains(':'),
        "got {now}"
    );
    let _ = fs::remove_file(&p);
}

#[test]
fn uuid_typeof_hex_random() {
    let (mut db, p) = open("misc");
    let u = scalar(&mut db, "SELECT gen_random_uuid()").unwrap();
    assert_eq!(u.len(), 36, "uuid has 8-4-4-4-12 layout: {u}");
    assert_eq!(u.as_bytes()[14], b'4', "version 4 nibble");
    // Two draws differ.
    let u2 = scalar(&mut db, "SELECT gen_random_uuid()").unwrap();
    assert_ne!(u, u2);

    assert_eq!(
        scalar(&mut db, "SELECT typeof(1)").as_deref(),
        Some("integer")
    );
    assert_eq!(
        scalar(&mut db, "SELECT typeof('x')").as_deref(),
        Some("text")
    );
    assert_eq!(
        scalar(&mut db, "SELECT typeof(1.5)").as_deref(),
        Some("real")
    );
    assert_eq!(
        scalar(&mut db, "SELECT typeof(NULL)").as_deref(),
        Some("null")
    );
    assert_eq!(scalar(&mut db, "SELECT hex('AB')").as_deref(), Some("4142"));

    let r = scalar(&mut db, "SELECT random()")
        .unwrap()
        .parse::<f64>()
        .unwrap();
    assert!((0.0..1.0).contains(&r), "random() in [0,1): {r}");
    let _ = fs::remove_file(&p);
}

#[test]
fn json_accessors() {
    let (mut db, p) = open("json");
    db.exec("CREATE TABLE docs (id INTEGER PRIMARY KEY, body TEXT)")
        .unwrap();
    db.exec(r#"INSERT INTO docs VALUES (1, '{"name":"Ada","tags":["a","b"],"age":36}')"#)
        .unwrap();

    // -> returns JSON, ->> returns text.
    let rs = db
        .query("SELECT body ->> 'name' AS name, body -> 'tags' ->> 1 AS tag1 FROM docs")
        .unwrap();
    assert_eq!(cell(&rs, 0, 0).as_deref(), Some("Ada"));
    assert_eq!(cell(&rs, 0, 1).as_deref(), Some("b"));

    // json_extract with an SQLite-style path returns the scalar typed.
    let rs = db
        .query("SELECT json_extract(body, '$.age') AS age FROM docs")
        .unwrap();
    assert_eq!(cell(&rs, 0, 0).as_deref(), Some("36"));

    // Filtering on a JSON field.
    let rs = db
        .query("SELECT id FROM docs WHERE body ->> 'name' = 'Ada'")
        .unwrap();
    assert_eq!(rs.rows.len(), 1);

    // json_array builds an array.
    assert_eq!(
        scalar(&mut db, "SELECT json_array(1, 'two', 3)").as_deref(),
        Some(r#"[1,"two",3]"#)
    );
    let _ = fs::remove_file(&p);
}

#[test]
fn nondeterministic_functions_are_stored_concretely() {
    // The value written by now()/gen_random_uuid() is concrete in the WAL, so a
    // reopen (pure replay) reproduces it exactly — never re-rolled.
    let p = db_path("determinism");
    let url = format!("file://{}", p.display());
    let (stored_ts, stored_uuid);
    {
        let mut db = Connection::open(&url).unwrap();
        db.exec("CREATE TABLE ev (id INTEGER PRIMARY KEY, at TEXT, uid TEXT)")
            .unwrap();
        db.exec("INSERT INTO ev VALUES (1, now(), gen_random_uuid())")
            .unwrap();
        let rs = db.query("SELECT at, uid FROM ev").unwrap();
        stored_ts = cell(&rs, 0, 0).unwrap();
        stored_uuid = cell(&rs, 0, 1).unwrap();
    }
    // Reopen → state rebuilt purely from the durable WAL.
    let mut db = Connection::open(&url).unwrap();
    let rs = db.query("SELECT at, uid FROM ev").unwrap();
    assert_eq!(cell(&rs, 0, 0).as_deref(), Some(stored_ts.as_str()));
    assert_eq!(cell(&rs, 0, 1).as_deref(), Some(stored_uuid.as_str()));
    let _ = fs::remove_file(&p);
}

#[test]
fn null_propagation_and_unknown_function() {
    let (mut db, p) = open("nullfn");
    assert_eq!(scalar(&mut db, "SELECT substr(NULL, 1)"), None);
    assert_eq!(scalar(&mut db, "SELECT sqrt(NULL)"), None);
    assert_eq!(scalar(&mut db, "SELECT date(NULL)"), None);
    // Unknown functions still error rather than silently NULL.
    assert!(db.query("SELECT no_such_fn(1)").is_err());
    let _ = fs::remove_file(&p);
}
