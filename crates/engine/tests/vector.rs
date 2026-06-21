//! Phase 5 — in-core vector search (spec 12). Exercises the `vector(N)` type,
//! the distance operators, the HNSW access method and its top-k query, MVCC +
//! index maintenance, copy-on-write branching of the index, and rebuild-from-WAL
//! on restart. All via the Rust `Connection` API (the same paths the C ABI and
//! pgwire server drive).

use engine::{Connection, EngineStatus, ResultSet, Value};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

fn db_path(tag: &str) -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let mut p = std::env::temp_dir();
    p.push(format!("bydesigns-vec-{tag}-{}-{n}.db", std::process::id()));
    let _ = fs::remove_file(&p);
    p
}

fn url_for(p: &std::path::Path) -> String {
    format!("file://{}", p.display())
}

fn cell(rs: &ResultSet, row: usize, col: usize) -> Option<String> {
    rs.rows[row][col].render()
}

/// Column 0 of every row, rendered (the id list of a result, in order).
fn ids(rs: &ResultSet) -> Vec<String> {
    (0..rs.rows.len())
        .map(|r| cell(rs, r, 0).unwrap_or_default())
        .collect()
}

fn seed_items(db: &mut Connection) {
    db.exec("CREATE TABLE items (id INTEGER PRIMARY KEY, embedding VECTOR(3))")
        .unwrap();
    db.exec("INSERT INTO items VALUES (1, [1, 0, 0])").unwrap();
    db.exec("INSERT INTO items VALUES (2, [0, 1, 0])").unwrap();
    db.exec("INSERT INTO items VALUES (3, [0, 0, 1])").unwrap();
    db.exec("INSERT INTO items VALUES (4, [0.9, 0.1, 0])")
        .unwrap();
}

#[test]
fn vector_type_roundtrips_and_validates_dimension() {
    let p = db_path("type");
    let mut db = Connection::open(&url_for(&p)).unwrap();
    db.exec("CREATE TABLE v (id INTEGER PRIMARY KEY, e VECTOR(3))")
        .unwrap();
    db.exec("INSERT INTO v VALUES (1, [1, 2, 3])").unwrap();
    // A '[..]' text literal coerces into the vector column (pgvector-style).
    db.exec("INSERT INTO v VALUES (2, '[4,5,6]')").unwrap();

    let rs = db.query("SELECT e FROM v ORDER BY id").unwrap();
    assert_eq!(cell(&rs, 0, 0).as_deref(), Some("[1,2,3]"));
    assert_eq!(cell(&rs, 1, 0).as_deref(), Some("[4,5,6]"));

    // Wrong dimension is rejected; the row leaves no trace.
    let err = db
        .exec("INSERT INTO v VALUES (3, [1, 2])")
        .expect_err("dimension mismatch must fail");
    assert_eq!(err.status, EngineStatus::ErrConstraint);
    let rs = db.query("SELECT COUNT(*) FROM v").unwrap();
    assert_eq!(cell(&rs, 0, 0).as_deref(), Some("2"));

    // vector(N) with a non-positive dimension is a parse error.
    let err = db
        .exec("CREATE TABLE bad (id INTEGER, e VECTOR(0))")
        .expect_err("vector(0) must be rejected");
    assert_eq!(err.status, EngineStatus::ErrSql);

    let _ = fs::remove_file(&p);
}

#[test]
fn distance_operators_compute_without_an_index() {
    let p = db_path("dist");
    let mut db = Connection::open(&url_for(&p)).unwrap();
    seed_items(&mut db);

    // L2: distance from each row to [1,0,0]; id=1 is exactly 0. (ORDER BY repeats
    // the expression — the engine orders by expressions, not output aliases.)
    let rs = db
        .query(
            "SELECT id, embedding <-> [1,0,0] AS d FROM items \
             ORDER BY embedding <-> [1,0,0]",
        )
        .unwrap();
    assert_eq!(ids(&rs), ["1", "4", "2", "3"]);
    assert_eq!(cell(&rs, 0, 1).as_deref(), Some("0.0"));

    // Cosine: id=1 (identical direction) then id=4 (nearly aligned).
    let rs = db
        .query("SELECT id FROM items ORDER BY embedding <=> [1,0,0]")
        .unwrap();
    assert_eq!(ids(&rs)[0], "1");
    assert_eq!(ids(&rs)[1], "4");

    // A '[..]' text query vector works on the operator too.
    let rs = db
        .query("SELECT id FROM items ORDER BY embedding <-> '[0,1,0]' LIMIT 1")
        .unwrap();
    assert_eq!(ids(&rs), ["2"]);

    // A dimension mismatch on the operator is a SQL error.
    let err = db
        .query("SELECT embedding <-> [1,0] FROM items")
        .expect_err("operator dim mismatch");
    assert_eq!(err.status, EngineStatus::ErrSql);

    let _ = fs::remove_file(&p);
}

#[test]
fn hnsw_index_answers_top_k_and_matches_brute_force() {
    let p = db_path("knn");
    let mut db = Connection::open(&url_for(&p)).unwrap();
    seed_items(&mut db);
    db.exec("CREATE INDEX items_e ON items USING hnsw (embedding) WITH (metric = 'l2')")
        .unwrap();

    // Top-2 nearest to [1,0,0] under L2 → ids 1 then 4, answered by the index.
    let rs = db
        .query("SELECT id FROM items ORDER BY embedding <-> [1,0,0] LIMIT 2")
        .unwrap();
    assert_eq!(ids(&rs), ["1", "4"]);

    // Param-bound query vector takes the index path too.
    let mut stmt = db
        .prepare("SELECT id FROM items ORDER BY embedding <-> ? LIMIT 1")
        .unwrap();
    stmt.bind(1, Value::Vector(vec![0.0, 0.0, 1.0])).unwrap();
    assert!(stmt.step().unwrap());
    assert_eq!(
        stmt.column_value(0).and_then(|v| v.render()).as_deref(),
        Some("3")
    );
    drop(stmt);

    // Dropping the index falls back to a brute-force scan with identical results.
    db.exec("DROP INDEX items_e").unwrap();
    let rs2 = db
        .query("SELECT id FROM items ORDER BY embedding <-> [1,0,0] LIMIT 2")
        .unwrap();
    assert_eq!(ids(&rs2), ["1", "4"]);

    let _ = fs::remove_file(&p);
}

#[test]
fn knn_respects_where_filter_and_mvcc() {
    let p = db_path("knnfilter");
    let mut db = Connection::open(&url_for(&p)).unwrap();
    db.exec("CREATE TABLE m (id INTEGER PRIMARY KEY, kind INTEGER, e VECTOR(2))")
        .unwrap();
    db.exec("CREATE INDEX m_e ON m USING hnsw (e) WITH (metric = 'l2')")
        .unwrap();
    db.exec("INSERT INTO m VALUES (1, 0, [0, 0])").unwrap();
    db.exec("INSERT INTO m VALUES (2, 1, [0.1, 0])").unwrap();
    db.exec("INSERT INTO m VALUES (3, 0, [0.2, 0])").unwrap();

    // Nearest of kind=0 to the origin: id 1 then 3 (id 2 is filtered out).
    let rs = db
        .query("SELECT id FROM m WHERE kind = 0 ORDER BY e <-> [0,0] LIMIT 5")
        .unwrap();
    assert_eq!(ids(&rs), ["1", "3"]);

    // An UPDATE supersedes a row version; the new vector is indexed and the old
    // one becomes invisible. Move id 3 right on top of the origin.
    db.exec("UPDATE m SET e = [0, 0] WHERE id = 3").unwrap();
    let rs = db
        .query("SELECT id FROM m ORDER BY e <-> [0,0] LIMIT 2")
        .unwrap();
    let top = ids(&rs);
    assert!(top.contains(&"1".to_string()) && top.contains(&"3".to_string()));

    // A DELETE makes a row invisible to the KNN scan.
    db.exec("DELETE FROM m WHERE id = 1").unwrap();
    let rs = db
        .query("SELECT id FROM m ORDER BY e <-> [0,0] LIMIT 1")
        .unwrap();
    assert_eq!(ids(&rs), ["3"]);

    let _ = fs::remove_file(&p);
}

#[test]
fn branch_branches_the_vector_index() {
    let p = db_path("branch");
    let mut base = Connection::open(&url_for(&p)).unwrap();
    seed_items(&mut base);
    base.exec("CREATE INDEX items_e ON items USING hnsw (embedding) WITH (metric = 'cosine')")
        .unwrap();

    // Fork a branch; its index inherits the base's vectors.
    let mut br = base.branch("memory-fork").unwrap();
    let rs = br
        .query("SELECT id FROM items ORDER BY embedding <=> [1,0,0] LIMIT 1")
        .unwrap();
    assert_eq!(ids(&rs), ["1"]);

    // Diverge the branch with a vector nearest to a fresh query direction. The
    // query [0.2,0.9,0.2] is closer (cosine) to id 5 than to the exact-axis id 2.
    br.exec("INSERT INTO items VALUES (5, [0.1, 0.95, 0.1])")
        .unwrap();
    let rs = br
        .query("SELECT id FROM items ORDER BY embedding <=> [0.2, 0.9, 0.2] LIMIT 1")
        .unwrap();
    assert_eq!(ids(&rs), ["5"], "branch index sees its diverged vector");

    // The base index is untouched: it never saw id 5, so its nearest is id 2.
    let rs = base
        .query("SELECT id FROM items ORDER BY embedding <=> [0.2, 0.9, 0.2] LIMIT 1")
        .unwrap();
    assert_eq!(
        ids(&rs),
        ["2"],
        "branch write must not leak into the base index"
    );

    drop(br);
    let _ = fs::remove_file(&p);
}

#[test]
fn index_rebuilds_from_wal_on_restart() {
    let p = db_path("restart");
    let url = url_for(&p);
    {
        let mut db = Connection::open(&url).unwrap();
        seed_items(&mut db);
        db.exec("CREATE INDEX items_e ON items USING hnsw (embedding) WITH (metric = 'l2')")
            .unwrap();
        db.exec("INSERT INTO items VALUES (5, [0.05, 0.05, 0.95])")
            .unwrap();
    } // all handles dropped → only the durable WAL remains

    // Reopen: the index is rebuilt purely by replaying the WAL (no side file).
    let mut db = Connection::open(&url).unwrap();
    let rs = db
        .query("SELECT id FROM items ORDER BY embedding <-> [0,0,1] LIMIT 2")
        .unwrap();
    // Nearest to [0,0,1]: id 3 ([0,0,1]) then id 5 ([.05,.05,.95]).
    assert_eq!(ids(&rs), ["3", "5"]);

    let _ = fs::remove_file(&p);
}

#[test]
fn rollback_removes_pending_vectors_from_the_index() {
    let p = db_path("rollback");
    let mut db = Connection::open(&url_for(&p)).unwrap();
    seed_items(&mut db);
    db.exec("CREATE INDEX items_e ON items USING hnsw (embedding) WITH (metric = 'l2')")
        .unwrap();

    db.begin().unwrap();
    db.exec("INSERT INTO items VALUES (9, [0, 1, 0])").unwrap();
    // Read-your-writes: the pending vector is visible inside the txn.
    let during = db.query("SELECT COUNT(*) FROM items WHERE id = 9").unwrap();
    assert_eq!(cell(&during, 0, 0).as_deref(), Some("1"));
    db.rollback().unwrap();

    // After rollback the vector is gone from both the table and the index, so a
    // KNN search never returns it.
    let rs = db
        .query("SELECT id FROM items ORDER BY embedding <-> [0,1,0] LIMIT 4")
        .unwrap();
    assert!(!ids(&rs).contains(&"9".to_string()));
    assert_eq!(ids(&rs)[0], "2");

    let _ = fs::remove_file(&p);
}

#[test]
fn hnsw_finds_exact_nearest_over_a_larger_set() {
    let p = db_path("recall");
    let mut db = Connection::open(&url_for(&p)).unwrap();
    db.exec("CREATE TABLE pts (id INTEGER PRIMARY KEY, e VECTOR(4))")
        .unwrap();
    db.exec("CREATE INDEX pts_e ON pts USING hnsw (e) WITH (metric = 'l2', ef_search = 128)")
        .unwrap();

    // A deterministic spread of 100 points on a lattice; the query coincides with
    // exactly one of them, so its nearest neighbour is unambiguous.
    for i in 0..100i64 {
        let a = (i % 10) as f32;
        let b = (i / 10) as f32;
        let sql = format!("INSERT INTO pts VALUES ({i}, [{a}, {b}, 0, 0])");
        db.exec(&sql).unwrap();
    }
    // Point id 73 sits at (3, 7, 0, 0).
    let rs = db
        .query("SELECT id FROM pts ORDER BY e <-> [3, 7, 0, 0] LIMIT 1")
        .unwrap();
    assert_eq!(ids(&rs), ["73"], "HNSW returns the exact nearest neighbour");

    let _ = fs::remove_file(&p);
}

#[test]
fn create_index_requires_a_vector_column() {
    let p = db_path("nonvec");
    let mut db = Connection::open(&url_for(&p)).unwrap();
    db.exec("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)")
        .unwrap();
    let err = db
        .exec("CREATE INDEX t_name ON t USING hnsw (name)")
        .expect_err("HNSW on a non-vector column must fail");
    assert_eq!(err.status, EngineStatus::ErrSql);

    // An unknown access method is rejected too.
    let err = db
        .exec("CREATE TABLE q (id INTEGER, e VECTOR(2))")
        .map(|_| ())
        .and_then(|_| db.exec("CREATE INDEX q_e ON q USING btree (e)"))
        .expect_err("only HNSW is supported");
    assert_eq!(err.status, EngineStatus::ErrSql);

    let _ = fs::remove_file(&p);
}
