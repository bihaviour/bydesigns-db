//! The `scale-to-zero` lifecycle scenario (spec 09 **Experiment 5** — cold read;
//! spec 15 — lifecycle scenarios & the serverless-efficiency report). Unlike the
//! steady-state experiments and request-mix scenarios, this one measures the
//! *cold path*: it drives `query → idle past the controller's reaper → query`,
//! repeatedly, so each cycle pays a real cold start (fence acquire + WAL replay)
//! and a first "cold read" against the freshly-warmed instance.
//!
//! It is **controller-driven and in-process**: the scenario owns a
//! [`twill_controller::Controller`] so it can both drive the lifecycle and
//! *pull* the [`ControllerStats`](controller::ControllerStats) snapshot at the
//! run boundaries — the metric source settled in the #53 observability design
//! (Decision 2: pull, never scrape or push). The per-run deltas of those
//! cumulative counters become the report: the cold-boot percentile distribution
//! (via the shared HDR histogram) plus the controller-sourced lifecycle figures
//! and the derived serverless-efficiency numbers (utilization,
//! compute-seconds/query), emitted under the settled `twill_*` vocabulary.
//!
//! A deployed pgwire server runs its own controller out of the bench's reach, so
//! `--transport pgwire` / `--server` is rejected here; the scenario runs against
//! any embedded backend URL (`file://` for a smoke run, an object store on a real
//! host for the spec-09 Exp 5 tail).

use crate::hist::Histogram;
use crate::{git_sha, run_tag, url_scheme, BenchError, Lifecycle, Opts, Report, Transport};
use controller::{Controller, ControllerConfig};
use engine::Connection;
use std::time::{Duration, Instant};

/// The scenario's own table; dropped + reseeded each run so the cold-read row
/// count is exact regardless of any residue in a reused durable database.
const TABLE: &str = "bench_cold";

pub(crate) fn run_scale_to_zero(opts: &Opts) -> Result<Report, BenchError> {
    // The scenario owns the lifecycle in-process; a deployed server has its own
    // controller the bench cannot drive, so reject the pgwire/server forms.
    if opts.transport == Transport::Pgwire || opts.server.is_some() {
        return Err(BenchError::Config(
            "scale-to-zero is controller-driven (in-process embedded); \
             drop --transport pgwire / --server"
                .into(),
        ));
    }
    let url = opts.url.clone();

    // A reaper fast enough to tear an idle instance down within the run; spec 09
    // Exp 5 uses a long (~10 min) idle window on a real deployment, set here via
    // `--idle-ms` so a smoke run completes in seconds.
    let reap = (opts.idle / 4).max(Duration::from_millis(5));
    let cfg = ControllerConfig {
        idle_timeout: opts.idle,
        reap_interval: reap,
        max_concurrent_warms: 16,
        keep_warm: false,
    };
    let ctrl =
        Controller::new(cfg).map_err(|e| BenchError::Connection(format!("controller: {e}")))?;

    // Seed over a plain connection (not the controller) so each cold start has
    // real WAL to replay — the cold-read payload — then drop it so it does not
    // pin the instance warm. The controller stays unaware until the first cycle.
    let tag = run_tag();
    seed(&url, tag, opts.rows)?;

    let start = ctrl.stats();
    let mut hist = Histogram::new();
    let mut queries = 0u64;
    // Backend page reads observed across the cold reads — the storage-side
    // numerator of the serverless-efficiency report (`storage_reads_per_query`).
    // Each cold start gets a fresh storage with zeroed counters, so the per-read
    // snapshot is exactly that instance's reads; summing across cycles totals the
    // run. `0` on `file://` (the in-memory store serves the read), nonzero on an
    // object store with a cold cache.
    let mut page_reads = 0u64;
    let run_start = Instant::now();

    for c in 0..opts.cycles {
        // Cold before timing: cycle 0 is already cold (never started); later
        // cycles wait for the reaper to have scaled the previous one to zero.
        wait_until_cold(&ctrl, &url, opts.idle, reap)?;

        // The cold path: cold-start the instance (fence acquire + WAL replay)
        // and run the first read against it (spec 09 Exp 5 "cold read").
        let t0 = Instant::now();
        let lease = ctrl
            .start(&url)
            .map_err(|e| BenchError::Run(format!("cycle {c}: cold start: {e}")))?;
        let (got, reads) = cold_read(&url)?;
        let cold_us = t0.elapsed().as_micros() as u64;
        if got != opts.rows {
            return Err(BenchError::Run(format!(
                "cycle {c}: cold read saw {got} rows, expected {} \
                 (durable state lost across scale-to-zero)",
                opts.rows
            )));
        }
        hist.record(cold_us);
        page_reads += reads;
        queries += 1;
        drop(lease); // release so the reaper can idle it out → scale to zero
    }

    // Let the final instance scale to zero so its teardown is counted and its
    // compute time settled, then pull the closing snapshot.
    wait_until_cold(&ctrl, &url, opts.idle, reap)?;
    let elapsed = run_start.elapsed();
    let end = ctrl.stats();

    let lifecycle = Lifecycle {
        cold_starts: end.cold_starts.saturating_sub(start.cold_starts),
        warm_starts: end.warm_starts.saturating_sub(start.warm_starts),
        scale_to_zero: end
            .scale_to_zero_events
            .saturating_sub(start.scale_to_zero_events),
        peak_workers: end.peak_workers,
        compute_active_us: end
            .compute_active_us
            .saturating_sub(start.compute_active_us),
        compute_idle_us: end.compute_idle_us.saturating_sub(start.compute_idle_us),
        admission_wait_us: end
            .admission_wait_us
            .saturating_sub(start.admission_wait_us),
        lease_renews: end
            .lease_renew_total
            .saturating_sub(start.lease_renew_total),
        page_reads,
        queries,
    };

    let cold_boots = hist.count();
    Ok(Report {
        experiment: "scale-to-zero",
        label: opts.label.clone(),
        transport: opts.transport.name(),
        url_scheme: url_scheme(&opts.url),
        writers: 1,
        duration_s: elapsed.as_secs_f64(),
        commits: cold_boots,
        conflicts: 0,
        failures: 0,
        throughput: cold_boots as f64 / elapsed.as_secs_f64().max(f64::MIN_POSITIVE),
        hist,
        git_sha: git_sha(),
        json_only: opts.json,
        correctness: None,
        lifecycle: Some(lifecycle),
        soak: None,
        burst: None,
        mix_realized: None,
    })
}

/// Drop + recreate the table and insert `rows` keyed by the run tag, in one
/// transaction. DDL is autocommit (engine rule); the inserts batch under an
/// explicit transaction so seeding is one durable append, not `rows` of them.
fn seed(url: &str, tag: u128, rows: u64) -> Result<(), BenchError> {
    let mut conn =
        Connection::open(url).map_err(|e| BenchError::Connection(format!("open {url}: {e}")))?;
    // A prior run may have left the table; reset it so the row count is exact.
    let _ = conn.exec(&format!("DROP TABLE {TABLE}"));
    conn.exec(&format!(
        "CREATE TABLE {TABLE} (k TEXT PRIMARY KEY, v INTEGER)"
    ))
    .map_err(|e| BenchError::Run(format!("create table: {e}")))?;
    conn.exec("BEGIN")
        .map_err(|e| BenchError::Run(format!("begin: {e}")))?;
    for i in 0..rows {
        conn.exec(&format!(
            "INSERT INTO {TABLE} (k, v) VALUES ('{tag}-{i}', {i})"
        ))
        .map_err(|e| BenchError::Run(format!("seed insert {i}: {e}")))?;
    }
    conn.exec("COMMIT")
        .map_err(|e| BenchError::Run(format!("commit: {e}")))?;
    Ok(())
}

/// Open a fresh connection (sharing the just-warmed instance via the engine's
/// registry — no second cold start) and count the seeded rows, exercising the
/// read path on the cold instance. Returns the row count and the backend page
/// reads the read incurred, pulled from the engine's `EngineStats` snapshot
/// (the storage-side input to the serverless-efficiency report). Dropping the
/// connection lets the instance idle out.
fn cold_read(url: &str) -> Result<(u64, u64), BenchError> {
    let mut conn =
        Connection::open(url).map_err(|e| BenchError::Connection(format!("open {url}: {e}")))?;
    let rs = conn
        .query(&format!("SELECT k FROM {TABLE}"))
        .map_err(|e| BenchError::Run(format!("cold read: {e}")))?;
    let page_reads = conn.stats().storage.page_reads;
    Ok((rs.rows.len() as u64, page_reads))
}

/// Block until the controller has scaled `url` to zero (status `Cold` or never
/// started). Bounded so a stuck reaper fails the run rather than hanging.
fn wait_until_cold(
    ctrl: &Controller,
    url: &str,
    idle: Duration,
    reap: Duration,
) -> Result<(), BenchError> {
    use controller::LifecycleState;
    // Teardown needs idle_timeout plus a couple reaper passes; budget generously.
    let deadline = Instant::now() + idle * 4 + reap * 4 + Duration::from_secs(1);
    loop {
        match ctrl.status(url) {
            None | Some(LifecycleState::Cold) => return Ok(()),
            _ => {}
        }
        if Instant::now() >= deadline {
            return Err(BenchError::Run(format!(
                "instance did not scale to zero within the idle window ({idle:?})"
            )));
        }
        std::thread::sleep(reap);
    }
}
