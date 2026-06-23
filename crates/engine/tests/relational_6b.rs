//! Stage 6B — multi-table & aggregation: joins (all kinds), qualified names,
//! DISTINCT, set operations, derived tables, non-recursive CTEs, non-correlated
//! subqueries, and grouped aggregation (group_concat / COUNT(DISTINCT)). Each
//! exercises the relational executor over the MVCC snapshot. Spec 16 §6B.

use engine::{Connection, ResultSet};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

fn db_path(tag: &str) -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let mut p = std::env::temp_dir();
    p.push(format!("twill-6b-{tag}-{}-{n}.db", std::process::id()));
    let _ = fs::remove_file(&p);
    p
}

fn cell(rs: &ResultSet, row: usize, col: usize) -> Option<String> {
    rs.rows[row][col].render()
}

fn col(rs: &ResultSet, c: usize) -> Vec<Option<String>> {
    rs.rows.iter().map(|r| r[c].render()).collect()
}

/// A schema of authors + books used by several join tests.
fn library() -> (Connection, PathBuf) {
    let p = db_path("lib");
    let mut db = Connection::open(&format!("file://{}", p.display())).unwrap();
    db.exec("CREATE TABLE authors (id INTEGER PRIMARY KEY, name TEXT)")
        .unwrap();
    db.exec("CREATE TABLE books (id INTEGER PRIMARY KEY, author_id INTEGER, title TEXT)")
        .unwrap();
    db.exec("INSERT INTO authors VALUES (1,'Ada'),(2,'Bel'),(3,'Cy')")
        .unwrap();
    db.exec("INSERT INTO books VALUES (10,1,'A1'),(11,1,'A2'),(12,2,'B1')")
        .unwrap();
    (db, p)
}

#[test]
fn inner_join_with_qualified_names() {
    let (mut db, p) = library();
    let rs = db
        .query(
            "SELECT a.name, b.title FROM authors a JOIN books b ON a.id = b.author_id \
             ORDER BY b.id",
        )
        .unwrap();
    assert_eq!(rs.columns, vec!["name", "title"]);
    assert_eq!(rs.rows.len(), 3);
    assert_eq!(cell(&rs, 0, 0).as_deref(), Some("Ada"));
    assert_eq!(cell(&rs, 0, 1).as_deref(), Some("A1"));
    assert_eq!(cell(&rs, 2, 0).as_deref(), Some("Bel"));
    let _ = fs::remove_file(&p);
}

#[test]
fn left_join_null_extends_unmatched() {
    let (mut db, p) = library();
    // Cy (id 3) has no books → a LEFT JOIN row with NULL title.
    let rs = db
        .query(
            "SELECT a.name, b.title FROM authors a LEFT JOIN books b ON a.id = b.author_id \
             ORDER BY a.id, b.id",
        )
        .unwrap();
    assert_eq!(rs.rows.len(), 4);
    let names = col(&rs, 0);
    assert_eq!(names.last().unwrap().as_deref(), Some("Cy"));
    assert_eq!(rs.rows[3][1].render(), None, "Cy's title is NULL");
    let _ = fs::remove_file(&p);
}

#[test]
fn right_and_full_join() {
    let (mut db, p) = library();
    // Add a book with no matching author.
    db.exec("INSERT INTO books VALUES (13, 99, 'Orphan')")
        .unwrap();

    // RIGHT JOIN: every book, author may be NULL.
    let rs = db
        .query(
            "SELECT a.name, b.title FROM authors a RIGHT JOIN books b ON a.id = b.author_id \
             ORDER BY b.id",
        )
        .unwrap();
    assert_eq!(rs.rows.len(), 4);
    assert_eq!(rs.rows[3][0].render(), None, "Orphan book has no author");
    assert_eq!(cell(&rs, 3, 1).as_deref(), Some("Orphan"));

    // FULL JOIN: Cy (no books) and the Orphan book (no author) both appear.
    let rs = db
        .query("SELECT a.name, b.title FROM authors a FULL JOIN books b ON a.id = b.author_id")
        .unwrap();
    assert_eq!(rs.rows.len(), 5);
    let _ = fs::remove_file(&p);
}

#[test]
fn cross_join_and_comma_join() {
    let p = db_path("cross");
    let mut db = Connection::open(&format!("file://{}", p.display())).unwrap();
    db.exec("CREATE TABLE a (x INTEGER)").unwrap();
    db.exec("CREATE TABLE b (y INTEGER)").unwrap();
    db.exec("INSERT INTO a VALUES (1),(2)").unwrap();
    db.exec("INSERT INTO b VALUES (10),(20),(30)").unwrap();

    let rs = db.query("SELECT x, y FROM a CROSS JOIN b").unwrap();
    assert_eq!(rs.rows.len(), 6);
    // Comma join with a WHERE acts as an inner join.
    let rs = db
        .query("SELECT x, y FROM a, b WHERE x = 1 ORDER BY y")
        .unwrap();
    assert_eq!(rs.rows.len(), 3);
    assert_eq!(cell(&rs, 0, 1).as_deref(), Some("10"));
    let _ = fs::remove_file(&p);
}

#[test]
fn join_with_using() {
    let p = db_path("using");
    let mut db = Connection::open(&format!("file://{}", p.display())).unwrap();
    db.exec("CREATE TABLE l (id INTEGER, a TEXT)").unwrap();
    db.exec("CREATE TABLE r (id INTEGER, b TEXT)").unwrap();
    db.exec("INSERT INTO l VALUES (1,'x'),(2,'y')").unwrap();
    db.exec("INSERT INTO r VALUES (1,'p'),(3,'q')").unwrap();
    let rs = db
        .query("SELECT a, b FROM l JOIN r USING (id) ORDER BY a")
        .unwrap();
    assert_eq!(rs.rows.len(), 1);
    assert_eq!(cell(&rs, 0, 0).as_deref(), Some("x"));
    assert_eq!(cell(&rs, 0, 1).as_deref(), Some("p"));
    let _ = fs::remove_file(&p);
}

#[test]
fn grouped_aggregation_over_join() {
    let (mut db, p) = library();
    // Books per author, only authors with books.
    let rs = db
        .query(
            "SELECT a.name, count(*) AS n FROM authors a JOIN books b ON a.id = b.author_id \
             GROUP BY a.name ORDER BY n DESC, a.name",
        )
        .unwrap();
    assert_eq!(rs.rows.len(), 2);
    assert_eq!(cell(&rs, 0, 0).as_deref(), Some("Ada"));
    assert_eq!(cell(&rs, 0, 1).as_deref(), Some("2"));
    assert_eq!(cell(&rs, 1, 0).as_deref(), Some("Bel"));
    let _ = fs::remove_file(&p);
}

#[test]
fn distinct_dedups() {
    let p = db_path("distinct");
    let mut db = Connection::open(&format!("file://{}", p.display())).unwrap();
    db.exec("CREATE TABLE t (g TEXT)").unwrap();
    db.exec("INSERT INTO t VALUES ('a'),('a'),('b'),('b'),('b'),('c')")
        .unwrap();
    let rs = db.query("SELECT DISTINCT g FROM t ORDER BY g").unwrap();
    assert_eq!(
        col(&rs, 0),
        vec![Some("a".into()), Some("b".into()), Some("c".into())]
    );

    // COUNT(DISTINCT g)
    let rs = db.query("SELECT count(DISTINCT g) AS d FROM t").unwrap();
    assert_eq!(cell(&rs, 0, 0).as_deref(), Some("3"));
    let _ = fs::remove_file(&p);
}

#[test]
fn set_operations() {
    let p = db_path("setops");
    let mut db = Connection::open(&format!("file://{}", p.display())).unwrap();
    db.exec("CREATE TABLE a (n INTEGER)").unwrap();
    db.exec("CREATE TABLE b (n INTEGER)").unwrap();
    db.exec("INSERT INTO a VALUES (1),(2),(3)").unwrap();
    db.exec("INSERT INTO b VALUES (3),(4)").unwrap();

    let rs = db
        .query("SELECT n FROM a UNION SELECT n FROM b ORDER BY n")
        .unwrap();
    assert_eq!(
        col(&rs, 0),
        vec![
            Some("1".into()),
            Some("2".into()),
            Some("3".into()),
            Some("4".into())
        ]
    );

    let rs = db
        .query("SELECT n FROM a UNION ALL SELECT n FROM b")
        .unwrap();
    assert_eq!(rs.rows.len(), 5);

    let rs = db
        .query("SELECT n FROM a INTERSECT SELECT n FROM b")
        .unwrap();
    assert_eq!(col(&rs, 0), vec![Some("3".into())]);

    let rs = db
        .query("SELECT n FROM a EXCEPT SELECT n FROM b ORDER BY n")
        .unwrap();
    assert_eq!(col(&rs, 0), vec![Some("1".into()), Some("2".into())]);
    let _ = fs::remove_file(&p);
}

#[test]
fn derived_table_and_cte() {
    let (mut db, p) = library();
    // Derived table.
    let rs = db
        .query("SELECT cnt FROM (SELECT author_id, count(*) AS cnt FROM books GROUP BY author_id) s ORDER BY cnt DESC")
        .unwrap();
    assert_eq!(cell(&rs, 0, 0).as_deref(), Some("2"));

    // CTE joined back to a base table.
    let rs = db
        .query(
            "WITH counts AS (SELECT author_id, count(*) AS n FROM books GROUP BY author_id) \
             SELECT a.name, c.n FROM authors a JOIN counts c ON a.id = c.author_id \
             ORDER BY c.n DESC, a.name",
        )
        .unwrap();
    assert_eq!(cell(&rs, 0, 0).as_deref(), Some("Ada"));
    assert_eq!(cell(&rs, 0, 1).as_deref(), Some("2"));
    let _ = fs::remove_file(&p);
}

#[test]
fn subqueries_scalar_in_exists() {
    let (mut db, p) = library();
    // Scalar subquery in the projection.
    let rs = db
        .query("SELECT (SELECT count(*) FROM books) AS total")
        .unwrap();
    assert_eq!(cell(&rs, 0, 0).as_deref(), Some("3"));

    // IN (subquery): authors that have at least one book.
    let rs = db
        .query("SELECT name FROM authors WHERE id IN (SELECT author_id FROM books) ORDER BY name")
        .unwrap();
    assert_eq!(col(&rs, 0), vec![Some("Ada".into()), Some("Bel".into())]);

    // NOT IN: authors with no books.
    let rs = db
        .query("SELECT name FROM authors WHERE id NOT IN (SELECT author_id FROM books)")
        .unwrap();
    assert_eq!(col(&rs, 0), vec![Some("Cy".into())]);

    // EXISTS (non-correlated) is true here.
    let rs = db
        .query("SELECT name FROM authors WHERE EXISTS (SELECT 1 FROM books WHERE author_id = 2) ORDER BY name")
        .unwrap();
    assert_eq!(rs.rows.len(), 3);
    let _ = fs::remove_file(&p);
}

#[test]
fn group_concat_aggregate() {
    let (mut db, p) = library();
    let rs = db
        .query(
            "SELECT a.name, group_concat(b.title, '; ') AS titles \
             FROM authors a JOIN books b ON a.id = b.author_id \
             GROUP BY a.name ORDER BY a.name",
        )
        .unwrap();
    assert_eq!(cell(&rs, 0, 0).as_deref(), Some("Ada"));
    assert_eq!(cell(&rs, 0, 1).as_deref(), Some("A1; A2"));
    let _ = fs::remove_file(&p);
}

#[test]
fn join_respects_mvcc_snapshot() {
    let (mut db, p) = library();
    let url = format!("file://{}", p.display());
    let mut reader = Connection::open(&url).unwrap();
    reader.begin().unwrap();
    let before = reader
        .query("SELECT count(*) FROM authors a JOIN books b ON a.id = b.author_id")
        .unwrap();
    assert_eq!(cell(&before, 0, 0).as_deref(), Some("3"));

    // A concurrent committed insert of a new book.
    db.exec("INSERT INTO books VALUES (20, 3, 'C1')").unwrap();

    // The reader's snapshot does not see it across the join.
    let during = reader
        .query("SELECT count(*) FROM authors a JOIN books b ON a.id = b.author_id")
        .unwrap();
    assert_eq!(cell(&during, 0, 0).as_deref(), Some("3"));
    reader.commit().unwrap();
    let after = reader
        .query("SELECT count(*) FROM authors a JOIN books b ON a.id = b.author_id")
        .unwrap();
    assert_eq!(cell(&after, 0, 0).as_deref(), Some("4"));
    let _ = fs::remove_file(&p);
}
