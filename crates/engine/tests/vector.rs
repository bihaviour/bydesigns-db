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
    p.push(format!("twill-vec-{tag}-{}-{n}.db", std::process::id()));
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

/// VH-3 (#99): `ef_search` is tunable per session via `SET twill.vector_ef_search`,
/// reflected by `SHOW`, cleared by `RESET`, and honored by the KNN path.
#[test]
fn ef_search_is_tunable_per_session() {
    let p = db_path("eftune");
    let mut db = Connection::open(&url_for(&p)).unwrap();
    db.exec("CREATE TABLE pts (id INTEGER PRIMARY KEY, e VECTOR(4))")
        .unwrap();
    db.exec("CREATE INDEX pts_e ON pts USING hnsw (e) WITH (metric = 'l2', ef_search = 16)")
        .unwrap();
    for i in 0..100i64 {
        let a = (i % 10) as f32;
        let b = (i / 10) as f32;
        db.exec(&format!("INSERT INTO pts VALUES ({i}, [{a}, {b}, 0, 0])"))
            .unwrap();
    }

    // Default: SHOW reports the unset (empty) session value.
    let rs = db.query("SHOW twill.vector_ef_search").unwrap();
    assert_eq!(cell(&rs, 0, 0).as_deref(), Some(""));

    // A session override is recorded and reflected.
    db.exec("SET twill.vector_ef_search = 200").unwrap();
    let rs = db.query("SHOW twill.vector_ef_search").unwrap();
    assert_eq!(cell(&rs, 0, 0).as_deref(), Some("200"));

    // The wider search still answers correctly (id 73 sits at (3,7,0,0)).
    let rs = db
        .query("SELECT id FROM pts ORDER BY e <-> [3, 7, 0, 0] LIMIT 1")
        .unwrap();
    assert_eq!(ids(&rs), ["73"]);

    // `TO` syntax and a tiny width are also accepted.
    db.exec("SET twill.vector_ef_search TO 1").unwrap();
    let rs = db.query("SHOW twill.vector_ef_search").unwrap();
    assert_eq!(cell(&rs, 0, 0).as_deref(), Some("1"));

    // RESET clears the override back to the index default.
    db.exec("RESET twill.vector_ef_search").unwrap();
    let rs = db.query("SHOW twill.vector_ef_search").unwrap();
    assert_eq!(cell(&rs, 0, 0).as_deref(), Some(""));

    let _ = fs::remove_file(&p);
}

/// VH-2 (#98): under heavy delete churn the index auto-compacts (no manual step)
/// and top-k recall against the live set stays exact.
#[test]
fn index_recall_holds_under_delete_churn() {
    let p = db_path("churn");
    let mut db = Connection::open(&url_for(&p)).unwrap();
    db.exec("CREATE TABLE pts (id INTEGER PRIMARY KEY, e VECTOR(4))")
        .unwrap();
    db.exec("CREATE INDEX pts_e ON pts USING hnsw (e) WITH (metric = 'l2')")
        .unwrap();

    // 500 lattice points (25×20), inserted in one transaction (one fsync).
    db.begin().unwrap();
    for i in 0..500i64 {
        let a = (i % 25) as f32;
        let b = (i / 25) as f32;
        db.exec(&format!("INSERT INTO pts VALUES ({i}, [{a}, {b}, 0, 0])"))
            .unwrap();
    }
    db.commit().unwrap();

    // Delete 70% (ids 0..350). Live rows are ids 350..500. Compaction triggers
    // automatically inside commit — no VACUUM, no rebuild call.
    db.begin().unwrap();
    db.exec("DELETE FROM pts WHERE id < 350").unwrap();
    db.commit().unwrap();

    // Every live point's own coordinates resolve to exactly itself (top-1 recall
    // on the live set), proving the post-churn graph still navigates correctly.
    for id in [350i64, 401, 437, 472, 499] {
        let a = (id % 25) as f32;
        let b = (id / 25) as f32;
        let rs = db
            .query(&format!(
                "SELECT id FROM pts ORDER BY e <-> [{a},{b},0,0] LIMIT 1"
            ))
            .unwrap();
        assert_eq!(ids(&rs), [id.to_string()], "nearest to live id {id}");
    }

    // A deleted point's slot now resolves to a live neighbour, never the dead row.
    let rs = db
        .query("SELECT id FROM pts ORDER BY e <-> [0,0,0,0] LIMIT 3")
        .unwrap();
    for got in ids(&rs) {
        assert!(got.parse::<i64>().unwrap() >= 350, "no dead row returned");
    }

    let _ = fs::remove_file(&p);
}

/// VH-2 (#98): an MVCC snapshot taken before a compacting rebuild still sees the
/// rows the compaction dropped — the KNN path falls back to a brute-force scan
/// below the compaction floor, preserving snapshot isolation.
#[test]
fn old_snapshot_survives_compaction() {
    let p = db_path("churnmvcc");
    let url = url_for(&p);
    let mut writer = Connection::open(&url).unwrap();
    let mut reader = Connection::open(&url).unwrap();

    writer
        .exec("CREATE TABLE pts (id INTEGER PRIMARY KEY, e VECTOR(4))")
        .unwrap();
    writer
        .exec("CREATE INDEX pts_e ON pts USING hnsw (e) WITH (metric = 'l2')")
        .unwrap();
    writer.begin().unwrap();
    for i in 0..500i64 {
        let a = (i % 25) as f32;
        let b = (i / 25) as f32;
        writer
            .exec(&format!("INSERT INTO pts VALUES ({i}, [{a}, {b}, 0, 0])"))
            .unwrap();
    }
    writer.commit().unwrap();

    // The reader captures a snapshot that can still see id 100 (at (0,4,0,0)).
    reader.begin().unwrap();
    let rs = reader
        .query("SELECT id FROM pts ORDER BY e <-> [0,4,0,0] LIMIT 1")
        .unwrap();
    assert_eq!(ids(&rs), ["100"]);

    // The writer deletes 70% (including id 100) → the index compacts past the
    // reader's snapshot.
    writer.begin().unwrap();
    writer.exec("DELETE FROM pts WHERE id < 350").unwrap();
    writer.commit().unwrap();

    // The reader's older snapshot still sees id 100 (brute-force fallback below the
    // compaction floor keeps snapshot isolation intact).
    let rs = reader
        .query("SELECT id FROM pts ORDER BY e <-> [0,4,0,0] LIMIT 1")
        .unwrap();
    assert_eq!(ids(&rs), ["100"], "old snapshot still sees the dropped row");
    reader.rollback().unwrap();

    // A fresh read at the head no longer sees id 100 (it was deleted).
    let rs = reader
        .query("SELECT id FROM pts ORDER BY e <-> [0,4,0,0] LIMIT 1")
        .unwrap();
    assert_ne!(ids(&rs), ["100"], "head no longer sees the deleted row");

    let _ = fs::remove_file(&p);
}

/// VH-1 (#97): a freshly built index is checkpointed as pages, and a cold reopen
/// loads the graph from those pages (through `get_page`) rather than rebuilding it
/// — while a write after the checkpoint correctly falls back to the rebuild path.
#[test]
fn index_warms_from_page_checkpoint() {
    let p = db_path("pagewarm");
    let url = url_for(&p);
    {
        let mut db = Connection::open(&url).unwrap();
        seed_items(&mut db);
        db.exec("CREATE INDEX items_e ON items USING hnsw (embedding) WITH (metric = 'l2')")
            .unwrap();
    } // checkpoint reflects the head; no writes follow it.

    // Cold reopen adopts the page checkpoint (no rebuild from rows).
    let mut db = Connection::open(&url).unwrap();
    assert_eq!(
        db.vector_pages_loaded(),
        1,
        "index warmed from its page checkpoint"
    );
    let rs = db
        .query("SELECT id FROM items ORDER BY embedding <-> [1,0,0] LIMIT 2")
        .unwrap();
    assert_eq!(ids(&rs), ["1", "4"]);
    drop(db);

    // A write after the checkpoint makes it stale; the next cold reopen rebuilds
    // from the WAL rows instead (still correct).
    {
        let mut db = Connection::open(&url).unwrap();
        db.exec("INSERT INTO items VALUES (7, [0.02, 0.02, 0.98])")
            .unwrap();
    }
    let mut db = Connection::open(&url).unwrap();
    assert_eq!(
        db.vector_pages_loaded(),
        0,
        "stale checkpoint is rejected; index rebuilt from rows"
    );
    let rs = db
        .query("SELECT id FROM items ORDER BY embedding <-> [0.02,0.02,0.98] LIMIT 1")
        .unwrap();
    assert_eq!(ids(&rs), ["7"]);

    let _ = fs::remove_file(&p);
}

/// VH-3 (#99): the documented recall/latency curve. Builds an HNSW index and an
/// index-free twin over the same vectors, then sweeps `ef_search` measuring
/// recall@10 (overlap with the brute-force ground truth) and per-query latency.
/// `#[ignore]`d (slow, and the numbers are environment-relative) — run with
/// `cargo test -p twill-engine --test vector recall_curve -- --ignored --nocapture`
/// to regenerate the table in `pages/specs/12-capabilities.html`.
#[test]
#[ignore]
fn recall_curve() {
    use std::time::Instant;

    const N: usize = 2000;
    const DIM: usize = 16;
    const QUERIES: usize = 100;
    const K: usize = 10;

    // Deterministic SplitMix64 vector generator (no rand dependency).
    let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut next = || -> f32 {
        state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        (z >> 40) as f32 / (1u64 << 24) as f32
    };
    let vec_lit = |v: &[f32]| {
        let parts: Vec<String> = v.iter().map(|x| format!("{x:.5}")).collect();
        format!("[{}]", parts.join(","))
    };

    let p = db_path("recallcurve");
    let mut db = Connection::open(&url_for(&p)).unwrap();
    db.exec("CREATE TABLE idx (id INTEGER PRIMARY KEY, e VECTOR(16))")
        .unwrap();
    db.exec("CREATE TABLE noidx (id INTEGER PRIMARY KEY, e VECTOR(16))")
        .unwrap();
    // A deliberately sparse graph (small m / ef_construction) so the recall knob
    // has visible headroom — a denser graph saturates recall at every ef_search.
    db.exec("CREATE INDEX idx_e ON idx USING hnsw (e) WITH (metric = 'l2', m = 5, ef_construction = 20)")
        .unwrap();

    let mut vectors = Vec::with_capacity(N);
    db.begin().unwrap();
    for i in 0..N {
        let v: Vec<f32> = (0..DIM).map(|_| next()).collect();
        let lit = vec_lit(&v);
        db.exec(&format!("INSERT INTO idx VALUES ({i}, {lit})"))
            .unwrap();
        db.exec(&format!("INSERT INTO noidx VALUES ({i}, {lit})"))
            .unwrap();
        vectors.push(v);
    }
    db.commit().unwrap();

    let queries: Vec<Vec<f32>> = (0..QUERIES)
        .map(|_| (0..DIM).map(|_| next()).collect())
        .collect();

    // Brute-force ground truth from the index-free twin.
    let truth: Vec<Vec<String>> = queries
        .iter()
        .map(|q| {
            let rs = db
                .query(&format!(
                    "SELECT id FROM noidx ORDER BY e <-> {} LIMIT {K}",
                    vec_lit(q)
                ))
                .unwrap();
            ids(&rs)
        })
        .collect();

    println!("\nef_search  recall@{K}   mean_latency_us");
    for ef in [8usize, 16, 32, 64, 128, 256] {
        db.exec(&format!("SET twill.vector_ef_search = {ef}"))
            .unwrap();
        let mut hits = 0usize;
        let start = Instant::now();
        for (qi, q) in queries.iter().enumerate() {
            let rs = db
                .query(&format!(
                    "SELECT id FROM idx ORDER BY e <-> {} LIMIT {K}",
                    vec_lit(q)
                ))
                .unwrap();
            let got = ids(&rs);
            hits += got.iter().filter(|g| truth[qi].contains(g)).count();
        }
        let elapsed = start.elapsed();
        let recall = hits as f64 / (QUERIES * K) as f64;
        let mean_us = elapsed.as_micros() as f64 / QUERIES as f64;
        println!("{ef:>8}   {recall:>7.3}   {mean_us:>14.1}");
    }

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
