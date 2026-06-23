//! Correctness workload profiles (spec 15 "Data-correctness validation"):
//! contended workloads that drive the engine hard and then **assert an ACID
//! invariant over the result**. A fast run that loses an acked write or breaks a
//! balance is a *failure*, not a fast success — these profiles set
//! [`Report::correctness`], which the CLI turns into [`exit::CORRECTNESS`](crate::exit::CORRECTNESS)
//! (exit code 2) when the invariant is violated, however good the latency was.
//!
//! Both profiles are fixed-work (each writer does `--ops` operations) so the
//! expected result is known exactly:
//!
//!   * **counter** — N writers each increment one shared row `--ops` times; the
//!     final value MUST equal `writers × ops` (zero lost updates). This
//!     generalizes `exp3` from "count the conflicts" to "prove the total".
//!   * **bank-transfer** — N writers move random amounts between two accounts in
//!     atomic transactions; the summed balance MUST equal the seeded total
//!     (value is conserved — no torn or partial transfer).
//!
//! Conflicts are retried (the contended path never gives up), so a violation can
//! only come from a real isolation/durability bug, not from losing a race.

use crate::hist::Histogram;
use crate::workload::Rng;
use crate::{
    resolve_target, run_nonce, url_scheme, BenchError, Correctness, Opts, Outcome, Report, Target,
    Writer, TABLE_COUNTER,
};
use std::time::Instant;

const TABLE_ACCOUNTS: &str = "bench_accounts";
/// Seeded balance per account — large enough that random transfers never drive a
/// balance negative within a run, so the only invariant under test is conservation.
const ACCOUNT_START: i64 = 1_000_000;

/// `counter`: N writers each do `--ops` increments of one row; assert the final
/// value equals `writers × ops` (no lost update).
pub(crate) fn run_counter(opts: &Opts) -> Result<Report, BenchError> {
    let target = resolve_target(opts)?;

    let mut setup = target.open()?;
    ddl(
        &mut setup,
        &format!("CREATE TABLE IF NOT EXISTS {TABLE_COUNTER} (id INTEGER PRIMARY KEY, n INTEGER)"),
    )?;
    // Reset the counter to a known zero so the assertion is exact across reruns.
    reset_row(
        &mut setup,
        &format!("UPDATE {TABLE_COUNTER} SET n = 0 WHERE id = 1"),
        &format!("INSERT INTO {TABLE_COUNTER} VALUES (1, 0)"),
    )?;

    let ops = opts.ops;
    let (tallies, elapsed) = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..opts.writers)
            .map(|_| {
                let target = target.clone();
                scope.spawn(move || {
                    fixed_work(&target, ops, |conn| {
                        single_stmt(
                            conn,
                            &format!("UPDATE {TABLE_COUNTER} SET n = n + 1 WHERE id = 1"),
                        )
                    })
                })
            })
            .collect();
        let start = Instant::now();
        let tallies: Vec<Result<WorkTally, BenchError>> =
            handles.into_iter().map(|h| h.join().unwrap()).collect();
        (tallies, start.elapsed())
    });

    let (hist, conflicts) = merge_tallies(tallies)?;

    // Assert: the durable counter equals exactly the work performed.
    let expected = (opts.writers as i64) * (ops as i64);
    let got = setup
        .query_i64(&format!("SELECT n FROM {TABLE_COUNTER} WHERE id = 1"))
        .map_err(BenchError::Run)?;
    let correctness = Correctness {
        name: "no-lost-update",
        passed: got == expected,
        detail: format!("expected {expected}, got {got}"),
    };

    Ok(report(
        opts,
        "counter",
        elapsed,
        &hist,
        conflicts,
        correctness,
    ))
}

/// `bank-transfer`: concurrent atomic transfers between two accounts; assert the
/// summed balance is conserved (ACID — no torn transfer leaks or destroys value).
pub(crate) fn run_bank_transfer(opts: &Opts) -> Result<Report, BenchError> {
    let target = resolve_target(opts)?;
    let nonce = run_nonce();

    let mut setup = target.open()?;
    ddl(
        &mut setup,
        &format!(
            "CREATE TABLE IF NOT EXISTS {TABLE_ACCOUNTS} (id INTEGER PRIMARY KEY, bal INTEGER)"
        ),
    )?;
    for id in [1, 2] {
        reset_row(
            &mut setup,
            &format!("UPDATE {TABLE_ACCOUNTS} SET bal = {ACCOUNT_START} WHERE id = {id}"),
            &format!("INSERT INTO {TABLE_ACCOUNTS} VALUES ({id}, {ACCOUNT_START})"),
        )?;
    }
    let total_before = ACCOUNT_START * 2;

    let ops = opts.ops;
    let (tallies, elapsed) = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..opts.writers)
            .map(|w| {
                let target = target.clone();
                let seed = nonce as u64 ^ ((w as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15));
                scope.spawn(move || {
                    let mut rng = Rng::new(seed);
                    fixed_work(&target, ops, move |conn| transfer(conn, &mut rng))
                })
            })
            .collect();
        let start = Instant::now();
        let tallies: Vec<Result<WorkTally, BenchError>> =
            handles.into_iter().map(|h| h.join().unwrap()).collect();
        (tallies, start.elapsed())
    });

    let (hist, conflicts) = merge_tallies(tallies)?;

    // Assert: total value is conserved across all the concurrent transfers.
    let bal1 = setup
        .query_i64(&format!("SELECT bal FROM {TABLE_ACCOUNTS} WHERE id = 1"))
        .map_err(BenchError::Run)?;
    let bal2 = setup
        .query_i64(&format!("SELECT bal FROM {TABLE_ACCOUNTS} WHERE id = 2"))
        .map_err(BenchError::Run)?;
    let correctness = Correctness {
        name: "balance-conserved",
        passed: bal1 + bal2 == total_before,
        detail: format!("sum expected {total_before}, got {}", bal1 + bal2),
    };

    Ok(report(
        opts,
        "bank-transfer",
        elapsed,
        &hist,
        conflicts,
        correctness,
    ))
}

// ---- the shared fixed-work driver ---------------------------------------

/// One writer's tally over its fixed-work window.
struct WorkTally {
    conflicts: u64,
    hist: Histogram,
}

/// The outcome of one unit of work: it either committed (`Ok`), hit a retry-able
/// conflict (`Conflict`, the driver loops), or failed fatally.
enum Unit {
    Ok,
    Conflict,
    Fatal(String),
}

/// Drive `ops` units of work, retrying conflicts, recording per-completed-unit
/// latency. `step` produces one unit's outcome against the writer's connection.
fn fixed_work<F>(target: &Target, ops: u64, mut step: F) -> Result<WorkTally, BenchError>
where
    F: FnMut(&mut Writer) -> Unit,
{
    let mut conn = target.open()?;
    let mut hist = Histogram::new();
    let mut conflicts = 0u64;
    for _ in 0..ops {
        let t0 = Instant::now();
        loop {
            match step(&mut conn) {
                Unit::Ok => break,
                Unit::Conflict => {
                    conflicts += 1;
                    continue;
                }
                Unit::Fatal(m) => return Err(BenchError::Run(m)),
            }
        }
        hist.record(t0.elapsed().as_micros() as u64);
    }
    Ok(WorkTally { conflicts, hist })
}

/// Run one autocommit statement, mapping its outcome to a [`Unit`].
fn single_stmt(conn: &mut Writer, sql: &str) -> Unit {
    match conn.exec(sql) {
        Outcome::Ok => Unit::Ok,
        Outcome::Conflict => Unit::Conflict,
        Outcome::Fatal(m) => Unit::Fatal(m),
    }
}

/// One atomic transfer: move a random amount between the two accounts inside an
/// explicit transaction. Any conflict rolls the whole transfer back and retries,
/// so the two-row update is all-or-nothing (the invariant the profile asserts).
fn transfer(conn: &mut Writer, rng: &mut Rng) -> Unit {
    let amount = 1 + rng.below(100); // small vs ACCOUNT_START, so never negative
    let (src, dst) = if rng.below(2) == 0 { (1, 2) } else { (2, 1) };

    // BEGIN can only fail fatally (no conflict possible yet).
    if let Outcome::Fatal(m) = conn.exec("BEGIN") {
        return Unit::Fatal(m);
    }
    for sql in [
        format!("UPDATE {TABLE_ACCOUNTS} SET bal = bal - {amount} WHERE id = {src}"),
        format!("UPDATE {TABLE_ACCOUNTS} SET bal = bal + {amount} WHERE id = {dst}"),
    ] {
        match conn.exec(&sql) {
            Outcome::Ok => {}
            Outcome::Conflict => {
                // Roll back the partial transfer and signal a retry.
                let _ = conn.exec("ROLLBACK");
                return Unit::Conflict;
            }
            Outcome::Fatal(m) => {
                let _ = conn.exec("ROLLBACK");
                return Unit::Fatal(m);
            }
        }
    }
    match conn.exec("COMMIT") {
        Outcome::Ok => Unit::Ok,
        Outcome::Conflict => {
            let _ = conn.exec("ROLLBACK");
            Unit::Conflict
        }
        Outcome::Fatal(m) => {
            let _ = conn.exec("ROLLBACK");
            Unit::Fatal(m)
        }
    }
}

// ---- small helpers ------------------------------------------------------

/// Run a DDL statement, tolerating an "already exists" outcome.
fn ddl(w: &mut Writer, sql: &str) -> Result<(), BenchError> {
    match w.exec(sql) {
        Outcome::Ok | Outcome::Conflict => Ok(()),
        Outcome::Fatal(m) => Err(BenchError::Run(format!("ddl `{sql}`: {m}"))),
    }
}

/// Force a row to a known seed value: try the `update`, and if it matched nothing
/// (first run), `insert` it. Either path leaves the row at the seed.
fn reset_row(w: &mut Writer, update: &str, insert: &str) -> Result<(), BenchError> {
    if let Outcome::Fatal(m) = w.exec(update) {
        return Err(BenchError::Run(format!("seed reset: {m}")));
    }
    // Insert the row if it didn't exist yet; a duplicate-key error just means the
    // UPDATE above already set it, which is fine.
    let _ = w.exec(insert);
    Ok(())
}

fn merge_tallies(
    tallies: Vec<Result<WorkTally, BenchError>>,
) -> Result<(Histogram, u64), BenchError> {
    let mut hist = Histogram::new();
    let mut conflicts = 0u64;
    for t in tallies {
        let t = t?;
        hist.merge(&t.hist);
        conflicts += t.conflicts;
    }
    Ok((hist, conflicts))
}

#[allow(clippy::too_many_arguments)]
fn report(
    opts: &Opts,
    name: &'static str,
    elapsed: std::time::Duration,
    hist: &Histogram,
    conflicts: u64,
    correctness: Correctness,
) -> Report {
    let ops = hist.count();
    Report {
        experiment: name,
        label: opts.label.clone(),
        transport: opts.transport.name(),
        url_scheme: url_scheme(&opts.url),
        writers: opts.writers,
        duration_s: elapsed.as_secs_f64(),
        commits: ops,
        conflicts,
        failures: 0,
        throughput: ops as f64 / elapsed.as_secs_f64().max(f64::MIN_POSITIVE),
        hist: hist.clone(),
        git_sha: crate::git_sha(),
        json_only: opts.json,
        correctness: Some(correctness),
    }
}
