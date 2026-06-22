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
    p.push(format!("twill-eng-{tag}-{}-{n}.db", std::process::id()));
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
fn transaction_keywords_and_modes() {
    let p = db_path("txnkw");
    let mut db = Connection::open(&url_for(&p)).unwrap();
    db.exec("CREATE TABLE t (id INTEGER PRIMARY KEY)").unwrap();

    // BEGIN with a full transaction-mode list (PostgREST's exact form) commits.
    db.exec("BEGIN ISOLATION LEVEL READ COMMITTED READ ONLY")
        .unwrap();
    db.exec("INSERT INTO t VALUES (1)").unwrap();
    db.exec("COMMIT").unwrap();
    assert_eq!(db.query("SELECT id FROM t").unwrap().rows.len(), 1);

    // ABORT is a ROLLBACK synonym — the insert is discarded.
    db.exec("BEGIN").unwrap();
    db.exec("INSERT INTO t VALUES (2)").unwrap();
    db.exec("ABORT").unwrap();
    assert_eq!(db.query("SELECT id FROM t").unwrap().rows.len(), 1);

    // END is a COMMIT synonym (START TRANSACTION + modes also parse).
    db.exec("START TRANSACTION ISOLATION LEVEL SERIALIZABLE")
        .unwrap();
    db.exec("INSERT INTO t VALUES (3)").unwrap();
    db.exec("END").unwrap();
    assert_eq!(db.query("SELECT id FROM t").unwrap().rows.len(), 2);

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

#[test]
fn type_casts_coerce_values() {
    let p = db_path("cast");
    let mut db = Connection::open(&url_for(&p)).unwrap();

    // text -> int, real -> int (rounds), int -> text
    let rs = db
        .query("SELECT '42'::int AS a, 1.7::integer AS b, 5::text AS c")
        .unwrap();
    assert_eq!(cell(&rs, 0, 0).as_deref(), Some("42"));
    assert_eq!(cell(&rs, 0, 1).as_deref(), Some("2"));
    assert_eq!(cell(&rs, 0, 2).as_deref(), Some("5"));

    // multi-word / parameterized / schema-qualified / array type spellings parse
    let rs = db
        .query("SELECT 3::double precision AS a, '7'::varchar(10) AS b, 9::pg_catalog.int8 AS c")
        .unwrap();
    assert_eq!(cell(&rs, 0, 0).as_deref(), Some("3.0")); // real renders with a fraction

    assert_eq!(cell(&rs, 0, 1).as_deref(), Some("7"));
    assert_eq!(cell(&rs, 0, 2).as_deref(), Some("9"));

    // cast binds tighter than unary minus: -1::int == -(1::int)
    let rs = db.query("SELECT -1::int AS a").unwrap();
    assert_eq!(cell(&rs, 0, 0).as_deref(), Some("-1"));

    // NULL casts to NULL; bad numeric text is a SQL error
    let rs = db.query("SELECT NULL::int AS a").unwrap();
    assert_eq!(cell(&rs, 0, 0), None);
    let err = db.query("SELECT 'nope'::int").unwrap_err();
    assert_eq!(err.status, EngineStatus::ErrSql);

    // cast applies in a WHERE filter against stored rows
    db.exec("CREATE TABLE t (id INTEGER PRIMARY KEY, tag TEXT)")
        .unwrap();
    db.exec("INSERT INTO t VALUES (1, '10'), (2, '20')")
        .unwrap();
    let rs = db.query("SELECT id FROM t WHERE tag::int > 15").unwrap();
    assert_eq!(rs.rows.len(), 1);
    assert_eq!(cell(&rs, 0, 0).as_deref(), Some("2"));

    // aggregate with a trailing cast: `count(*)::int` (Bun.sql / PostgREST)
    let rs = db.query("SELECT count(*)::int AS n FROM t").unwrap();
    assert_eq!(rs.columns[0], "n");
    assert_eq!(cell(&rs, 0, 0).as_deref(), Some("2"));

    let _ = fs::remove_file(&p);
}

#[test]
fn group_by_having_and_order() {
    let p = db_path("groupby");
    let mut db = Connection::open(&url_for(&p)).unwrap();
    db.exec("CREATE TABLE sales (id INTEGER PRIMARY KEY, region TEXT, amount INTEGER)")
        .unwrap();
    db.exec(
        "INSERT INTO sales VALUES (1,'west',10),(2,'west',30),(3,'east',5),(4,'east',5),(5,'north',100)",
    )
    .unwrap();

    // GROUP BY with aggregate + grouped column, ordered by the aggregate.
    let rs = db
        .query(
            "SELECT region, sum(amount) AS total, count(*) AS n \
             FROM sales GROUP BY region ORDER BY total DESC",
        )
        .unwrap();
    assert_eq!(rs.columns, vec!["region", "total", "n"]);
    assert_eq!(rs.rows.len(), 3);
    assert_eq!(cell(&rs, 0, 0).as_deref(), Some("north"));
    assert_eq!(cell(&rs, 0, 1).as_deref(), Some("100"));
    assert_eq!(cell(&rs, 1, 0).as_deref(), Some("west"));
    assert_eq!(cell(&rs, 1, 1).as_deref(), Some("40"));

    // HAVING filters whole groups.
    let rs = db
        .query("SELECT region FROM sales GROUP BY region HAVING sum(amount) > 20 ORDER BY region")
        .unwrap();
    assert_eq!(rs.rows.len(), 2);
    assert_eq!(cell(&rs, 0, 0).as_deref(), Some("north"));
    assert_eq!(cell(&rs, 1, 0).as_deref(), Some("west"));

    let _ = fs::remove_file(&p);
}

#[test]
fn limit_offset_paginates() {
    let p = db_path("offset");
    let mut db = Connection::open(&url_for(&p)).unwrap();
    db.exec("CREATE TABLE t (id INTEGER PRIMARY KEY)").unwrap();
    db.exec("INSERT INTO t VALUES (1),(2),(3),(4),(5)").unwrap();

    // OFFSET then LIMIT, in PostgREST's pagination shape.
    let rs = db
        .query("SELECT id FROM t ORDER BY id LIMIT 2 OFFSET 1")
        .unwrap();
    assert_eq!(rs.rows.len(), 2);
    assert_eq!(cell(&rs, 0, 0).as_deref(), Some("2"));
    assert_eq!(cell(&rs, 1, 0).as_deref(), Some("3"));

    // OFFSET past the end yields nothing; LIMIT ALL keeps everything.
    let rs = db.query("SELECT id FROM t ORDER BY id OFFSET 10").unwrap();
    assert_eq!(rs.rows.len(), 0);
    let rs = db
        .query("SELECT id FROM t ORDER BY id LIMIT ALL OFFSET 3")
        .unwrap();
    assert_eq!(rs.rows.len(), 2);

    let _ = fs::remove_file(&p);
}

#[test]
fn scalar_functions_and_json() {
    let p = db_path("funcs");
    let mut db = Connection::open(&url_for(&p)).unwrap();

    // coalesce / nullif / string + numeric helpers (no FROM).
    let rs = db
        .query(
            "SELECT coalesce(NULL, 'x') AS a, nullif(1, 1) AS b, upper('hi') AS c, \
             length('abc') AS d, abs(-4) AS e, round(2.6) AS f, concat('a', 'b', 'c') AS g",
        )
        .unwrap();
    assert_eq!(cell(&rs, 0, 0).as_deref(), Some("x"));
    assert_eq!(cell(&rs, 0, 1), None); // nullif(1,1) -> NULL
    assert_eq!(cell(&rs, 0, 2).as_deref(), Some("HI"));
    assert_eq!(cell(&rs, 0, 3).as_deref(), Some("3"));
    assert_eq!(cell(&rs, 0, 4).as_deref(), Some("4"));
    assert_eq!(cell(&rs, 0, 5).as_deref(), Some("3"));
    assert_eq!(cell(&rs, 0, 6).as_deref(), Some("abc"));

    // json_build_object produces JSON text.
    let rs = db
        .query("SELECT json_build_object('k', 1, 'name', 'Ada') AS j")
        .unwrap();
    assert_eq!(cell(&rs, 0, 0).as_deref(), Some(r#"{"k":1,"name":"Ada"}"#));

    // json_agg aggregates a column into a JSON array; coalesce covers the empty
    // set (the PostgREST result-wrapping shape).
    db.exec("CREATE TABLE books (id INTEGER PRIMARY KEY, title TEXT)")
        .unwrap();
    db.exec("INSERT INTO books VALUES (1,'A'),(2,'B')").unwrap();
    let rs = db
        .query("SELECT coalesce(json_agg(title), '[]') AS titles FROM books")
        .unwrap();
    assert_eq!(cell(&rs, 0, 0).as_deref(), Some(r#"["A","B"]"#));

    let rs = db
        .query("SELECT coalesce(json_agg(title), '[]') AS titles FROM books WHERE id > 99")
        .unwrap();
    assert_eq!(cell(&rs, 0, 0).as_deref(), Some("[]"));

    // unknown function is a SQL error, not a silent NULL.
    let err = db.query("SELECT bogus_fn(1)").unwrap_err();
    assert_eq!(err.status, EngineStatus::ErrSql);

    let _ = fs::remove_file(&p);
}

#[test]
fn foreign_keys_are_tracked_and_persist() {
    let p = db_path("fk");
    let url = url_for(&p);
    {
        let mut db = Connection::open(&url).unwrap();
        db.exec("CREATE TABLE authors (id INTEGER PRIMARY KEY, name TEXT)")
            .unwrap();
        // Inline column REFERENCES with an explicit referenced column.
        db.exec(
            "CREATE TABLE books (id INTEGER PRIMARY KEY, title TEXT, \
             author_id INTEGER REFERENCES authors (id))",
        )
        .unwrap();
        // Table-level FOREIGN KEY whose referenced column defaults to the PK,
        // plus a named constraint.
        db.exec(
            "CREATE TABLE reviews (id INTEGER PRIMARY KEY, book INTEGER, \
             CONSTRAINT reviews_book_fk FOREIGN KEY (book) REFERENCES books)",
        )
        .unwrap();

        let cat = db.catalog();
        let books = cat.iter().find(|t| t.name == "books").unwrap();
        assert_eq!(books.foreign_keys.len(), 1);
        let fk = &books.foreign_keys[0];
        assert_eq!(fk.name, "books_author_id_fkey"); // synthesized
        assert_eq!(fk.columns, vec!["author_id".to_string()]);
        assert_eq!(fk.foreign_table, "authors");
        assert_eq!(fk.foreign_columns, vec!["id".to_string()]);

        let reviews = cat.iter().find(|t| t.name == "reviews").unwrap();
        let rfk = &reviews.foreign_keys[0];
        assert_eq!(rfk.name, "reviews_book_fk"); // declared
        assert_eq!(rfk.foreign_table, "books");
        assert_eq!(rfk.foreign_columns, vec!["id".to_string()]); // defaulted to PK
    }

    // Reopen: the FK metadata is rebuilt purely from the durable WAL.
    let db = Connection::open(&url).unwrap();
    let cat = db.catalog();
    let books = cat.iter().find(|t| t.name == "books").unwrap();
    assert_eq!(books.foreign_keys.len(), 1);
    assert_eq!(books.foreign_keys[0].foreign_table, "authors");
    let reviews = cat.iter().find(|t| t.name == "reviews").unwrap();
    assert_eq!(
        reviews.foreign_keys[0].foreign_columns,
        vec!["id".to_string()]
    );

    let _ = fs::remove_file(&p);
}
