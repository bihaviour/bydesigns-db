//! Server-mode (pgwire) driver tests (spec 09 — "applies to both the embedded
//! FFI and server pgwire paths"; issue #6 / #29).
//!
//! These pin the offline-testable half of the server-mode benchmark: the
//! in-crate Postgres-wire client ([`twill_bench::pgclient`]) driving an
//! *in-process* `engine-server` listener. They prove the same properties the
//! embedded driver relies on hold over the wire — DML/DDL round-trips, a scalar
//! read-back, and (critically) that a first-committer/first-toucher conflict
//! surfaces as a retry-able `40001` the driver classifies and retries, so a
//! contended counter driven entirely over pgwire never loses an update. The
//! latency *numbers* are still measured on a real host (or with `pgbench`); only
//! correctness/classification is universal and lives here.

use std::net::TcpListener;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Barrier;
use std::thread;
use twill_bench::pgclient::{ExecError, PgClient};

fn unique_url() -> String {
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let mut p = std::env::temp_dir();
    p.push(format!("twill-bench-pg-{}-{n}.db", std::process::id()));
    let _ = std::fs::remove_file(&p);
    format!("file://{}", p.display())
}

/// Bind an ephemeral port, serve `url` on a detached thread, return the address.
fn start_server(url: String) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    thread::spawn(move || {
        let _ = twill_server::serve_listener(listener, &url);
    });
    addr
}

#[test]
fn pgwire_client_round_trips_dml_and_scalar() {
    let addr = start_server(unique_url());
    let mut c = PgClient::connect(&addr).unwrap();

    c.exec("CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)")
        .unwrap();
    c.exec("INSERT INTO t (id, v) VALUES (1, 10), (2, 20), (3, 30)")
        .unwrap();
    assert_eq!(c.query_scalar_i64("SELECT COUNT(*) FROM t").unwrap(), 3);
    c.exec("UPDATE t SET v = v + 5 WHERE id = 2").unwrap();
    assert_eq!(
        c.query_scalar_i64("SELECT v FROM t WHERE id = 2").unwrap(),
        25
    );
    c.exec("DELETE FROM t WHERE id = 3").unwrap();
    assert_eq!(c.query_scalar_i64("SELECT COUNT(*) FROM t").unwrap(), 2);
}

#[test]
fn pgwire_classifies_fatal_vs_conflict() {
    let addr = start_server(unique_url());
    let mut c = PgClient::connect(&addr).unwrap();

    // A syntax error is fatal (not a retry-able conflict), and the connection
    // stays usable afterward (ReadyForQuery resets the session).
    match c.exec("this is not valid sql") {
        Err(ExecError::Fatal(_)) => {}
        other => panic!("expected fatal error, got {other:?}"),
    }
    c.exec("CREATE TABLE ok (id INTEGER PRIMARY KEY)").unwrap();
    c.exec("INSERT INTO ok VALUES (1)").unwrap();
    assert_eq!(c.query_scalar_i64("SELECT COUNT(*) FROM ok").unwrap(), 1);
}

#[test]
fn pgwire_contended_counter_never_loses_an_update() {
    // The exp3 shape over the wire: many writers hammer one row through pgwire.
    // Conflicts come back as 40001, the driver retries, and the final count is
    // exact — proving the wire path drives the engine's group-commit/conflict
    // machinery exactly as the embedded path does (cf. tests/group_commit.rs).
    const WRITERS: usize = 6;
    const PER_WRITER: usize = 40;

    let addr = start_server(unique_url());
    {
        let mut setup = PgClient::connect(&addr).unwrap();
        setup
            .exec("CREATE TABLE c (id INTEGER PRIMARY KEY, n INTEGER)")
            .unwrap();
        setup.exec("INSERT INTO c VALUES (1, 0)").unwrap();
    }

    let barrier = Barrier::new(WRITERS);
    thread::scope(|s| {
        for _ in 0..WRITERS {
            let addr = &addr;
            let barrier = &barrier;
            s.spawn(move || {
                let mut c = PgClient::connect(addr).unwrap();
                barrier.wait();
                for _ in 0..PER_WRITER {
                    loop {
                        match c.exec("UPDATE c SET n = n + 1 WHERE id = 1") {
                            Ok(()) => break,
                            Err(ExecError::Conflict) => continue,
                            Err(e) => panic!("unexpected error: {e:?}"),
                        }
                    }
                }
            });
        }
    });

    let mut check = PgClient::connect(&addr).unwrap();
    assert_eq!(
        check
            .query_scalar_i64("SELECT n FROM c WHERE id = 1")
            .unwrap(),
        (WRITERS * PER_WRITER) as i64,
        "no update may be lost over the pgwire path"
    );
}

#[test]
fn pgwire_transaction_mode_pooling_preserves_correctness() {
    // Pooler shape (issue #20). Serverless clients open a *burst* of short-lived
    // connections; a transaction-mode pooler (PgBouncer/pgcat) absorbs that burst
    // by multiplexing the many client transactions onto a small, stable set of
    // backend connections, returning a backend to the pool at each transaction
    // boundary. The engine therefore never sees the raw client churn — it sees a
    // few long-lived backends, each carrying a stream of transactions.
    //
    // This test models exactly that backend side: a small fixed pool of backend
    // connections, with many independent transactions (BEGIN / write a distinct
    // row / COMMIT) driven across them concurrently. It pins the two properties
    // #20 requires of the engine under a pooler, without the external pooler in
    // the loop:
    //
    //   * the bounded backend set carries the whole transaction load — every
    //     transaction is accepted and completes (no overload), and
    //   * transaction-mode multiplexing preserves correctness — across the full
    //     burst every committed row is durable, none lost or duplicated.
    //
    // The pooler binary itself (and the raw client-churn it absorbs in front of
    // the engine) is verified manually against this same listener — see
    // `deploy/pooler/README.md` and `pages/docs/connection-pooling.html`.
    const BACKENDS: usize = 4; // the pooler's stable backend pool (transaction mode)
    const TXNS_PER_BACKEND: usize = 80; // many client transactions multiplexed on

    let addr = start_server(unique_url());
    {
        let mut setup = PgClient::connect(&addr).unwrap();
        setup
            .exec("CREATE TABLE c (id INTEGER PRIMARY KEY, backend INTEGER)")
            .unwrap();
    }

    let barrier = Barrier::new(BACKENDS);
    thread::scope(|s| {
        for b in 0..BACKENDS {
            let addr = &addr;
            let barrier = &barrier;
            s.spawn(move || {
                // One persistent backend connection, as a pooler would hold.
                let mut c = PgClient::connect(addr).expect("listener accepts the backend");
                barrier.wait(); // all backends carry their load together
                for i in 0..TXNS_PER_BACKEND {
                    let id = b * TXNS_PER_BACKEND + i; // a distinct row per transaction
                    c.exec("BEGIN").unwrap();
                    c.exec(&format!("INSERT INTO c (id, backend) VALUES ({id}, {b})"))
                        .unwrap();
                    c.exec("COMMIT").unwrap(); // backend returns to the pool here
                }
            });
        }
    });

    let mut check = PgClient::connect(&addr).unwrap();
    assert_eq!(
        check.query_scalar_i64("SELECT COUNT(*) FROM c").unwrap(),
        (BACKENDS * TXNS_PER_BACKEND) as i64,
        "every transaction multiplexed through the backend pool must survive"
    );
}
