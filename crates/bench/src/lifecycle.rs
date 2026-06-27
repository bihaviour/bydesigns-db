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
use crate::{
    analysis, git_sha, run_tag, url_scheme, Archival, BenchError, Lifecycle, Opts, Report,
    Transport,
};
use controller::{Controller, ControllerConfig};
use engine::Connection;
use std::sync::{Arc, Barrier};
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
        archival: crate::Archival::from_opts(opts),
        stall: None,
        sweep: None,
        shard: None,
        herd: None,
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

// ──────────────────────── V-4: thundering-herd cold starts ──────────────────

/// The Exp-5 thundering-herd sweep result (V-4): for each concurrency level, the
/// cold-start latency tail when N clients arrive at once, plus the controller-
/// pulled admission-wait (the warm-admission queue the herd backs up against)
/// and peak resident workers. The reported **knee** is the concurrency at which
/// cold-start latency starts degrading non-linearly — the spin-up saturation
/// point spec 09's Exp-5 extension asks for.
pub struct Herd {
    /// The swept simultaneous-cold-start counts (`1, 2, 4, … --concurrency`).
    pub concurrency: Vec<u64>,
    /// The p99 cold-start latency (µs) at each level — the herd tail.
    pub cold_p99_us: Vec<u64>,
    /// Controller-pulled cumulative warm-admission wait (µs) over the level — the
    /// queue the herd forms against the bounded warm-admission semaphore.
    pub admission_wait_us: Vec<u64>,
    /// Peak databases warming simultaneously at each level (the queue-depth gauge
    /// pulled from `ControllerStats`).
    pub peak_workers: Vec<u64>,
    /// Index of the detected saturation knee, if any.
    pub knee: Option<usize>,
    /// Whether `--gate` was set, so the saturation guard maps to the exit code.
    pub gated: bool,
}

impl Herd {
    /// The concurrency at the detected knee, or `0` if the curve never bent
    /// (cold-start latency stayed linear across the whole sweep — no saturation).
    fn knee_concurrency(&self) -> u64 {
        self.knee
            .and_then(|i| self.concurrency.get(i).copied())
            .unwrap_or(0)
    }

    /// Whether cold-start latency degraded non-linearly across the sweep: the
    /// top-of-sweep p99 is more than 4× the single-cold-start p99. This is the
    /// "knee at low N" red flag — the herd saturated the warm path.
    fn degraded(&self) -> bool {
        match (self.cold_p99_us.first(), self.cold_p99_us.last()) {
            (Some(&first), Some(&last)) if first > 0 => last as f64 > 4.0 * first as f64,
            _ => false,
        }
    }

    /// V-4 gate: the herd saturated (non-linear degradation with a detected
    /// knee). Only fails the run when `--gate` was set.
    pub fn gate_failed(&self) -> bool {
        self.gated && self.degraded() && self.knee.is_some()
    }

    pub fn print_human(&self) {
        println!("── exp5 thundering-herd cold starts (V-4) ──────────");
        for i in 0..self.concurrency.len() {
            println!(
                "herd {:>4}    cold p99={:>8}µs  admission_wait={}µs  peak_workers={}",
                self.concurrency[i],
                self.cold_p99_us[i],
                self.admission_wait_us[i],
                self.peak_workers[i],
            );
        }
        let knee = self.knee_concurrency();
        if knee > 0 {
            println!("knee         saturation at concurrency {knee} (cold-start latency bends up)");
        } else {
            println!("knee         none — cold-start latency stayed linear across the sweep");
        }
        let verdict = if self.degraded() {
            "DEGRADED (top-of-sweep cold p99 > 4× single — cap concurrency / pre-warm)"
        } else {
            "healthy (cold p99 stayed within 4× across the herd)"
        };
        println!("verdict      {verdict}");
    }

    pub fn to_json(&self) -> String {
        let arr = |v: &[u64]| v.iter().map(u64::to_string).collect::<Vec<_>>().join(",");
        format!(
            "{{\"concurrency\":[{}],\"cold_p99_us\":[{}],\"admission_wait_us\":[{}],\
             \"peak_workers\":[{}],\"knee_concurrency\":{},\"degraded\":{}}}",
            arr(&self.concurrency),
            arr(&self.cold_p99_us),
            arr(&self.admission_wait_us),
            arr(&self.peak_workers),
            self.knee_concurrency(),
            self.degraded(),
        )
    }
}

/// A geometric ladder `1, 2, 4, … max` (inclusive), the herd concurrency points.
fn herd_ladder(max: usize) -> Vec<usize> {
    let max = max.max(1);
    let mut v = Vec::new();
    let mut n = 1usize;
    while n < max {
        v.push(n);
        n *= 2;
    }
    v.push(max);
    v
}

/// Run the Exp-5 thundering-herd variant (V-4): fire `N` simultaneous cold boots
/// across `N` synthetic clients (each its own database), sweeping
/// `N = 1, 2, 4, … --concurrency`, and detect the spin-up saturation knee. Like
/// `scale-to-zero` it is controller-driven and in-process (a deployed server runs
/// its own controller the bench cannot drive), so the pgwire/server forms are
/// rejected.
pub(crate) fn run_herd(opts: &Opts) -> Result<Report, BenchError> {
    if opts.transport == Transport::Pgwire || opts.server.is_some() {
        return Err(BenchError::Config(
            "herd is controller-driven (in-process embedded); drop --transport pgwire / --server"
                .into(),
        ));
    }

    let levels = herd_ladder(opts.concurrency);
    let max_n = *levels.last().unwrap_or(&1);

    // Seed every client's database once up front (durable), so each cold start
    // has real WAL to replay — the cold-boot payload.
    let tag = run_tag();
    let urls: Vec<String> = (0..max_n)
        .map(|k| format!("{}-herd{k}", opts.url))
        .collect();
    for url in &urls {
        seed(url, tag, opts.rows)?;
    }

    let reap = (opts.idle / 4).max(Duration::from_millis(5));
    // Cap warm admission below the peak so the herd genuinely queues against the
    // semaphore for the upper levels — that queue is the admission-wait metric.
    let admission_cap = (max_n / 2).max(1);

    let mut concurrency = Vec::new();
    let mut cold_p99_us = Vec::new();
    let mut admission_wait_us = Vec::new();
    let mut peak_workers = Vec::new();

    let run_start = Instant::now();
    for &n in &levels {
        let (p99, wait, peak) = drive_herd(&urls[..n], opts, reap, admission_cap)?;
        concurrency.push(n as u64);
        cold_p99_us.push(p99);
        admission_wait_us.push(wait);
        peak_workers.push(peak);
    }
    let elapsed = run_start.elapsed();

    let xs: Vec<f64> = concurrency.iter().map(|&n| n as f64).collect();
    let ys: Vec<f64> = cold_p99_us.iter().map(|&u| u as f64).collect();
    let knee = analysis::knee_convex(&xs, &ys);

    let herd = Herd {
        concurrency,
        cold_p99_us,
        admission_wait_us,
        peak_workers,
        knee,
        gated: opts.gate,
    };

    Ok(Report {
        experiment: "exp5-herd",
        label: opts.label.clone(),
        transport: opts.transport.name(),
        url_scheme: url_scheme(&opts.url),
        writers: 1,
        duration_s: elapsed.as_secs_f64(),
        commits: herd.concurrency.len() as u64,
        conflicts: 0,
        failures: 0,
        throughput: 0.0,
        hist: Histogram::new(),
        git_sha: git_sha(),
        json_only: opts.json,
        correctness: None,
        lifecycle: None,
        soak: None,
        burst: None,
        mix_realized: None,
        archival: Archival::from_opts(opts),
        stall: None,
        sweep: None,
        shard: None,
        herd: Some(herd),
    })
}

/// One herd level: `urls.len()` clients fire a cold start *simultaneously* (a
/// shared barrier releases them together), each cold-starting its own database
/// and running the first cold read. Returns the cold-start p99 (µs), the
/// controller's cumulative admission-wait over the level, and the peak resident
/// workers. A fresh controller per level guarantees every database is cold at
/// the barrier.
fn drive_herd(
    urls: &[String],
    opts: &Opts,
    reap: Duration,
    admission_cap: usize,
) -> Result<(u64, u64, u64), BenchError> {
    let cfg = ControllerConfig {
        idle_timeout: opts.idle,
        reap_interval: reap,
        max_concurrent_warms: admission_cap,
        keep_warm: false,
    };
    let ctrl = Arc::new(
        Controller::new(cfg).map_err(|e| BenchError::Connection(format!("controller: {e}")))?,
    );
    let start = ctrl.stats();

    let n = urls.len();
    let barrier = Arc::new(Barrier::new(n));
    let rows = opts.rows;

    let samples = std::thread::scope(|scope| {
        let handles: Vec<_> = urls
            .iter()
            .map(|url| {
                let ctrl = Arc::clone(&ctrl);
                let barrier = Arc::clone(&barrier);
                let url = url.clone();
                scope.spawn(move || -> Result<u64, BenchError> {
                    // Arrive together: the thundering herd is all clients hitting
                    // the cold path at the same instant.
                    barrier.wait();
                    let t0 = Instant::now();
                    let lease = ctrl
                        .start(&url)
                        .map_err(|e| BenchError::Run(format!("herd cold start: {e}")))?;
                    let (got, _reads) = cold_read(&url)?;
                    let cold_us = t0.elapsed().as_micros() as u64;
                    if got != rows {
                        return Err(BenchError::Run(format!(
                            "herd cold read saw {got} rows, expected {rows} (durable loss)"
                        )));
                    }
                    drop(lease); // release so the reaper can idle it out
                    Ok(cold_us)
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().unwrap())
            .collect::<Vec<_>>()
    });

    let mut hist = Histogram::new();
    for s in samples {
        hist.record(s?);
    }

    // Let every instance scale to zero so the level's compute settles, then pull
    // the closing snapshot for the admission-wait and peak-worker deltas.
    for url in urls {
        wait_until_cold(&ctrl, url, opts.idle, reap)?;
    }
    let end = ctrl.stats();
    let wait = end
        .admission_wait_us
        .saturating_sub(start.admission_wait_us);
    Ok((hist.value_at_quantile(0.99), wait, end.peak_workers))
}
