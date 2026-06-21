//! Hot-row contention behaviour (spec 10; issue #6 / #31). These are the
//! correctness gates for the layered hot-row policy, and they run *here* — no
//! real object store, no network — because correctness is universal and offline:
//!
//! * **Layer 1 (correct-but-slow by default)** — a naive contended
//!   `UPDATE counter SET n = n + 1` issued concurrently by many writers MUST
//!   never lose an update. This engine enforces snapshot isolation with
//!   first-committer-wins (`exec.rs::check_no_conflict`): a writer whose row was
//!   modified by a concurrent committed transaction is *rejected* with
//!   `ErrConflict` and retries, rather than silently overwriting. Either way the
//!   read-modify-write is serialized and the total is exact — correctness is
//!   universal, the cost is the retry/serialize latency on the outlier (spec 10
//!   §Premise, §Recommendation Layer 1).
//! * **Layer 2 (Helper A — sharded counter)** — splitting one hot row into N
//!   sub-counter rows turns "same row" into "different rows" (the green
//!   quadrant): writers touch distinct rows so they rarely conflict, while the
//!   summed total stays exactly correct (spec 10 §Helper A).
//!
//! The performance side of the story (the Exp-3 contention wall and the N-DB
//! sharding curve) is measured against a real network object store, not asserted
//! here; these tests pin only the correctness invariants the policy rests on.

use engine::{Connection, EngineStatus, Value};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;

/// Execute `sql` on `conn`, retrying the first-committer-wins conflict that
/// contended same-row writers provoke under snapshot isolation. The retry — not a
/// silent overwrite — is how correctness is preserved: the loser re-reads the
/// freshly committed value and reapplies its read-modify-write.
fn exec_retrying(conn: &mut Connection, sql: &str) {
    loop {
        match conn.exec(sql) {
            Ok(()) => return,
            Err(e) if e.status == EngineStatus::ErrConflict => continue,
            Err(e) => panic!("unexpected error: {e:?}"),
        }
    }
}

fn db_path(tag: &str) -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let mut p = std::env::temp_dir();
    p.push(format!("twill-hotrow-{tag}-{}-{n}.db", std::process::id()));
    let _ = fs::remove_file(&p);
    p
}

fn url_for(p: &std::path::Path) -> String {
    format!("file://{}", p.display())
}

fn scalar_i64(conn: &mut Connection, sql: &str) -> i64 {
    let rs = conn.query(sql).unwrap();
    match &rs.rows[0][0] {
        Value::Int(n) => *n,
        other => panic!("expected integer scalar, got {other:?}"),
    }
}

#[test]
fn naive_contended_increment_stays_correct() {
    // The case spec 10 promises is "correct, just slow": many writers hammering
    // one row with a read-modify-write. Snapshot isolation rejects the loser of
    // each race with ErrConflict; retrying re-reads the committed value, so the
    // total MUST be exact — no lost update is reachable.
    const WRITERS: usize = 8;
    const PER_WRITER: usize = 50;

    let p = db_path("naive");
    let url = url_for(&p);
    let mut setup = Connection::open(&url).unwrap();
    setup
        .exec("CREATE TABLE counter (id INTEGER PRIMARY KEY, n INTEGER)")
        .unwrap();
    setup.exec("INSERT INTO counter VALUES (1, 0)").unwrap();

    thread::scope(|s| {
        for _ in 0..WRITERS {
            let url = url.clone();
            s.spawn(move || {
                // Each writer is its own handle to the same shared database.
                let mut conn = Connection::open(&url).unwrap();
                for _ in 0..PER_WRITER {
                    exec_retrying(&mut conn, "UPDATE counter SET n = n + 1 WHERE id = 1");
                }
            });
        }
    });

    let total = scalar_i64(&mut setup, "SELECT n FROM counter WHERE id = 1");
    assert_eq!(
        total,
        (WRITERS * PER_WRITER) as i64,
        "lost update: contended increments must serialize to the exact total",
    );

    let _ = fs::remove_file(&p);
}

#[test]
fn sharded_counter_recovers_parallelism_and_stays_correct() {
    // Helper A: one logical counter spread over N sub-counter rows. Writers touch
    // *different* rows (the green quadrant), and the logical total is the SUM over
    // the shards — which MUST still equal the exact number of increments.
    const SHARDS: i64 = 16;
    const WRITERS: usize = 8;
    const PER_WRITER: usize = 50;

    let p = db_path("sharded");
    let url = url_for(&p);
    let mut setup = Connection::open(&url).unwrap();
    setup
        .exec(
            "CREATE TABLE counter_shards (counter_id TEXT, shard INTEGER, n INTEGER, \
             PRIMARY KEY (counter_id, shard))",
        )
        .unwrap();
    for shard in 0..SHARDS {
        exec_retrying(
            &mut setup,
            &format!(
                "INSERT INTO counter_shards (counter_id, shard, n) VALUES ('video:42', {shard}, 0)"
            ),
        );
    }

    thread::scope(|s| {
        for w in 0..WRITERS {
            let url = url.clone();
            s.spawn(move || {
                let mut conn = Connection::open(&url).unwrap();
                for i in 0..PER_WRITER {
                    // Deterministic shard spread (no random() in the SQL subset):
                    // each write lands on a distinct sub-counter row.
                    let shard = ((w * PER_WRITER + i) as i64) % SHARDS;
                    exec_retrying(
                        &mut conn,
                        &format!(
                            "UPDATE counter_shards SET n = n + 1 \
                             WHERE counter_id = 'video:42' AND shard = {shard}"
                        ),
                    );
                }
            });
        }
    });

    let total = scalar_i64(
        &mut setup,
        "SELECT SUM(n) FROM counter_shards WHERE counter_id = 'video:42'",
    );
    assert_eq!(
        total,
        (WRITERS * PER_WRITER) as i64,
        "sharded counter must aggregate to the exact total on read",
    );

    let _ = fs::remove_file(&p);
}
