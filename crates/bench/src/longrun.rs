//! The `long-run` soak scenario (spec 15 — "Benchmark scenarios", `long-run`;
//! Observability Decision 2 — interval sampling). Issue #80, part of #78.
//!
//! Every other scenario reports a single start→end delta; a soak run is a
//! *stability* test whose verdict is "did anything trend up that shouldn't?" —
//! memory leaks, fd/connection leaks, scheduler drift, slow latency
//! degradation. Catching those needs a **time series**, not a delta, so this
//! scenario adds three pieces the other scenarios do not have:
//!
//!   * **L1 — interval sampler** ([`run_sampler`]): a timer loop that pulls a
//!     sample every `--sample-interval-ms` for the run duration, decoupled from
//!     the load driver, into an in-memory series.
//!   * **L2 — process resource probe** ([`ResourceSample`]): RSS, open-fd, and
//!     thread counts from Linux `/proc/self`, degrading to zeros (never a crash)
//!     where `/proc` is unavailable — no extra dependency (guardrail 1, #78).
//!   * **L3 — trend / leak analysis** ([`analyze`]): a least-squares slope per
//!     metric (memory, fds, p99) over the post-warm-up window, flagging a
//!     leak/drift when the projected growth crosses both a relative threshold
//!     (`--drift-threshold`) and an absolute noise floor.
//!
//! The metric source is the same pull-based snapshot the rest of the bench uses
//! ([`Connection::stats`] / `StorageStats` — #53 Decision 2), so the engine core
//! stays thread-free and unaware of the sampler. A detected leak/drift fails the
//! run with the correctness exit code (2, #51-class), proven by a seeded-growth
//! integration test; a flat control run passes.
//!
//! Like `scale-to-zero`, the soak samples *this* process, so it is embedded-only
//! (the RSS/fd of the bench process is the engine's only when in-process); the
//! `--transport pgwire` / `--server` form is rejected.

use crate::hist::Histogram;
use crate::workload::Rng;
use crate::{git_sha, run_tag, url_scheme, BenchError, Fault, Opts, Report, Transport};
use engine::Connection;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The soak scenario's own table: a pre-seeded fixed working set the writers read
/// from. The load is deliberately **steady-state** (point reads over a bounded
/// set), not unbounded ingestion — a soak is a *stability* baseline, so memory,
/// fds, and p99 must be genuinely flat under a healthy engine for a real leak or
/// drift to stand out. An insert-only load would grow the WAL and the in-memory
/// MVCC store without bound (no vacuum this phase), drifting on its own and
/// drowning the signal the soak exists to find.
const TABLE: &str = "bench_soak";

/// One sample captured by the interval sampler ([`run_sampler`]) at a point in
/// the run: the time offset (the x-axis for slope fitting) plus the engine and
/// process gauges the trend analysis fits a line through.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Sample {
    /// Seconds since the sampler started — the regression x-axis.
    pub t_s: f64,
    /// Engine commits counter (cumulative) — a liveness gauge in the record.
    pub commits: u64,
    /// Resident set size, bytes (`0` where `/proc` is unavailable).
    pub rss_bytes: u64,
    /// Open file-descriptor count (`0` where `/proc` is unavailable).
    pub fds: u64,
    /// OS thread count (`0` where `/proc` is unavailable).
    pub threads: u64,
    /// p99 commit latency over the most recent sample window, µs.
    pub p99_us: u64,
}

/// A point-in-time resource snapshot of *this* process from Linux `/proc/self`
/// (L2). Best-effort and dependency-free: any field whose source can't be read
/// (a non-Linux build, a sandbox without `/proc`) stays `0`, so sampling
/// degrades cleanly to zeros rather than failing the run.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ResourceSample {
    pub rss_bytes: u64,
    pub fds: u64,
    pub threads: u64,
}

impl ResourceSample {
    /// Probe the live process. Reads `/proc/self/status` for RSS + thread count
    /// and counts `/proc/self/fd`; unreadable sources leave their field `0`.
    pub fn probe() -> ResourceSample {
        let (rss_bytes, threads) = match std::fs::read_to_string("/proc/self/status") {
            Ok(s) => parse_status(&s),
            Err(_) => (0, 0),
        };
        ResourceSample {
            rss_bytes,
            threads,
            fds: count_open_fds(),
        }
    }
}

/// Parse `VmRSS` (returned as **bytes**) and `Threads` from `/proc/self/status`
/// content. Factored out so a fixture can pin the parser without a live `/proc`
/// (L2 unit test). A missing field yields `0` — the same graceful-degradation
/// contract as a missing file.
fn parse_status(content: &str) -> (u64, u64) {
    let mut rss_bytes = 0u64;
    let mut threads = 0u64;
    for line in content.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            // `VmRSS:\t  123456 kB` — the value is in kibibytes.
            if let Some(kb) = rest
                .split_whitespace()
                .next()
                .and_then(|n| n.parse::<u64>().ok())
            {
                rss_bytes = kb.saturating_mul(1024);
            }
        } else if let Some(rest) = line.strip_prefix("Threads:") {
            threads = rest.trim().parse().unwrap_or(0);
        }
    }
    (rss_bytes, threads)
}

/// Count the entries in `/proc/self/fd` (each open descriptor is one entry).
/// `0` where the directory can't be read.
fn count_open_fds() -> u64 {
    match std::fs::read_dir("/proc/self/fd") {
        Ok(entries) => entries.count() as u64,
        Err(_) => 0,
    }
}

/// The interval sampler (L1): pull a [`Sample`] now, sleep `interval`, repeat
/// until `deadline`, returning the captured series. The per-tick work is the
/// `sample` closure (a `stats()` pull + resource probe + latency-window read in
/// the real run), so the loop itself is the only thing this owns — which is what
/// makes it unit-testable with a trivial closure. The sleep is clamped to the
/// remaining window so the loop never overshoots the deadline.
fn run_sampler(
    deadline: Instant,
    interval: Duration,
    mut sample: impl FnMut(f64) -> Sample,
) -> Vec<Sample> {
    let start = Instant::now();
    let mut series = Vec::new();
    while Instant::now() < deadline {
        series.push(sample(start.elapsed().as_secs_f64()));
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        std::thread::sleep(interval.min(remaining));
    }
    series
}

/// The trend of one gated metric over the soak: where it started and ended, its
/// peak, the fitted slope, and whether that slope amounts to a leak/drift (L3).
#[derive(Clone, Debug)]
pub(crate) struct MetricTrend {
    /// Metric name as it appears in the report (`memory`, `fds`, `p99`, …).
    pub name: &'static str,
    /// First / last analyzed (post-warm-up) value.
    pub first: f64,
    pub last: f64,
    /// Largest value seen across the whole series (warm-up included).
    pub peak: f64,
    /// Least-squares slope over the analyzed window, units-of-metric per second.
    pub slope_per_s: f64,
    /// Projected growth over the analyzed span (`slope × span`) as a fraction of
    /// the baseline — the relative drift the threshold is compared against.
    pub growth_frac: f64,
    /// Whether this metric contributes to the PASS/FAIL verdict. Informational
    /// metrics (e.g. threads) are reported but never flip the verdict.
    pub gated: bool,
    /// `true` when a gated metric crossed both the relative threshold and its
    /// absolute floor — a flagged leak/drift.
    pub leaking: bool,
}

/// Per-metric absolute floor for the projected growth: a leak must be *both*
/// relatively (over `--drift-threshold`) and absolutely significant, so a short
/// control run whose RSS or p99 wobbles by a few pages / microseconds is never
/// flagged. A real leak grows without bound and clears these easily.
struct Gate {
    name: &'static str,
    /// Projected absolute growth (in the metric's own units) below which the
    /// metric is treated as flat regardless of the relative fraction.
    floor: f64,
}

/// The gated metrics and their noise floors (L3 flags memory, fds, p99).
const GATES: [Gate; 3] = [
    // 8 MiB of resident growth — well above allocator slack on a short run.
    Gate {
        name: "memory",
        floor: 8.0 * 1024.0 * 1024.0,
    },
    // 4 descriptors — a real fd/connection leak climbs unbounded.
    Gate {
        name: "fds",
        floor: 4.0,
    },
    // 200 µs of p99 drift — above the run-to-run jitter of a warm window.
    Gate {
        name: "p99",
        floor: 200.0,
    },
];

/// The result of the trend analysis: the per-metric trends plus the overall
/// PASS/FAIL drift verdict (L3 → L4). `passed` is false iff any *gated* metric
/// is leaking.
#[derive(Clone, Debug)]
pub(crate) struct SoakAnalysis {
    pub trends: Vec<MetricTrend>,
    /// Post-warm-up samples the slopes were fit over (`< 2` → can't fit → PASS).
    pub analyzed: usize,
    pub passed: bool,
}

/// Pulls one metric's value out of a [`Sample`] for the per-metric slope fit.
type Extract = fn(&Sample) -> f64;

/// Least-squares slope of `ys` over `xs` (units of y per unit of x). Returns
/// `0.0` for fewer than two points or a degenerate (zero-variance) x — the
/// "can't claim a trend" answer, never a NaN.
fn slope(xs: &[f64], ys: &[f64]) -> f64 {
    let n = xs.len();
    if n < 2 || n != ys.len() {
        return 0.0;
    }
    let nf = n as f64;
    let mean_x = xs.iter().sum::<f64>() / nf;
    let mean_y = ys.iter().sum::<f64>() / nf;
    let mut num = 0.0;
    let mut den = 0.0;
    for (x, y) in xs.iter().zip(ys.iter()) {
        num += (x - mean_x) * (y - mean_y);
        den += (x - mean_x) * (x - mean_x);
    }
    if den.abs() < f64::EPSILON {
        0.0
    } else {
        num / den
    }
}

/// Fit a [`MetricTrend`] for one metric over the analyzed (post-warm-up) window.
/// `extract` pulls the metric's value out of a sample; `peak_all` is the max
/// over the *entire* series (warm-up included) so a transient spike still shows.
fn trend(
    name: &'static str,
    gated: bool,
    floor: f64,
    threshold: f64,
    xs: &[f64],
    ys: &[f64],
    peak_all: f64,
) -> MetricTrend {
    let first = ys.first().copied().unwrap_or(0.0);
    let last = ys.last().copied().unwrap_or(0.0);
    let slope_per_s = slope(xs, ys);
    let span = match (xs.first(), xs.last()) {
        (Some(a), Some(b)) => (b - a).max(0.0),
        _ => 0.0,
    };
    let projected = slope_per_s * span;
    // Relative to the baseline; guard a zero/near-zero baseline so a metric that
    // legitimately starts at 0 (e.g. RSS where `/proc` is absent) can't divide
    // to ∞. The absolute floor is what actually gates such metrics.
    let baseline = first.abs().max(1.0);
    let growth_frac = projected / baseline;
    let leaking = gated && growth_frac > threshold && projected > floor;
    MetricTrend {
        name,
        first,
        last,
        peak: peak_all,
        slope_per_s,
        growth_frac,
        gated,
        leaking,
    }
}

/// The trend / leak analysis (L3). Drop samples inside the warm-up window, fit a
/// slope per gated metric (memory, fds, p99) plus the informational `threads`
/// gauge, and decide the verdict: PASS unless a gated metric's projected growth
/// crosses both the relative `threshold` and its absolute floor. Fewer than two
/// post-warm-up samples can't support a trend, so the run passes (no evidence of
/// a leak), with `analyzed` recording how many samples were actually fit.
pub(crate) fn analyze(samples: &[Sample], warmup: Duration, threshold: f64) -> SoakAnalysis {
    let warm_s = warmup.as_secs_f64();
    let kept: Vec<&Sample> = samples.iter().filter(|s| s.t_s >= warm_s).collect();

    let xs: Vec<f64> = kept.iter().map(|s| s.t_s).collect();
    let peak = |f: Extract| samples.iter().map(f).fold(0.0, f64::max);

    // (name, gated, floor, value extractor) for every reported metric.
    let metrics: [(&'static str, bool, f64, Extract); 5] = [
        (GATES[0].name, true, GATES[0].floor, |s| s.rss_bytes as f64),
        (GATES[1].name, true, GATES[1].floor, |s| s.fds as f64),
        (GATES[2].name, true, GATES[2].floor, |s| s.p99_us as f64),
        // Threads and the cumulative commits counter are sampled and reported,
        // but a change in either is not a gated leak verdict on its own — they
        // are informational (a stalled `commits` slope is a liveness signal, not
        // a leak). `commits` is the `stats()` pull the sampler makes each tick.
        ("threads", false, 0.0, |s| s.threads as f64),
        ("commits", false, 0.0, |s| s.commits as f64),
    ];

    let mut trends = Vec::with_capacity(metrics.len());
    for (name, gated, floor, extract) in metrics {
        let ys: Vec<f64> = kept.iter().map(|s| extract(s)).collect();
        trends.push(trend(
            name,
            gated,
            floor,
            threshold,
            &xs,
            &ys,
            peak(extract),
        ));
    }

    let passed = !trends.iter().any(|t| t.leaking);
    SoakAnalysis {
        trends,
        analyzed: kept.len(),
        passed,
    }
}

/// The soak section of a [`Report`] (L4): the run parameters, the fitted
/// per-metric trends, and the PASS/FAIL drift verdict. Lives alongside the
/// shared latency histogram (which holds the full-run percentile distribution);
/// this carries the *time-series* view the histogram cannot.
pub(crate) struct Soak {
    pub samples: u64,
    pub analyzed: u64,
    pub interval_ms: u64,
    pub warmup_s: f64,
    pub threshold: f64,
    pub analysis: SoakAnalysis,
}

impl Soak {
    pub fn passed(&self) -> bool {
        self.analysis.passed
    }

    /// The gated metrics that tripped the verdict, worst (largest relative
    /// growth) first — the "worst offenders" the human summary names.
    pub fn offenders(&self) -> Vec<&MetricTrend> {
        let mut v: Vec<&MetricTrend> = self.analysis.trends.iter().filter(|t| t.leaking).collect();
        v.sort_by(|a, b| b.growth_frac.total_cmp(&a.growth_frac));
        v
    }

    /// Render the soak section as a JSON object for the archived record (L4).
    pub fn to_json(&self) -> String {
        let trends = self
            .analysis
            .trends
            .iter()
            .map(|t| {
                format!(
                    "{{\"name\":\"{}\",\"first\":{:.3},\"last\":{:.3},\"peak\":{:.3},\
                     \"slope_per_s\":{:.6},\"growth_frac\":{:.6},\"gated\":{},\"leaking\":{}}}",
                    t.name,
                    t.first,
                    t.last,
                    t.peak,
                    t.slope_per_s,
                    t.growth_frac,
                    t.gated,
                    t.leaking
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        format!(
            "{{\"samples\":{},\"analyzed\":{},\"sample_interval_ms\":{},\"warmup_s\":{:.3},\
             \"drift_threshold\":{:.6},\"drift_pass\":{},\"trends\":[{}]}}",
            self.samples,
            self.analyzed,
            self.interval_ms,
            self.warmup_s,
            self.threshold,
            self.passed(),
            trends,
        )
    }

    /// The human-readable soak summary printed above the JSON line.
    pub fn print_human(&self) {
        println!(
            "soak         {} samples ({} analyzed)  interval={}ms  warmup={:.2}s  threshold={:.0}%",
            self.samples,
            self.analyzed,
            self.interval_ms,
            self.warmup_s,
            self.threshold * 100.0,
        );
        for t in &self.analysis.trends {
            let flag = if t.leaking {
                "  ⚠ LEAK/DRIFT"
            } else if t.gated {
                ""
            } else {
                "  (info)"
            };
            println!(
                "  {:<8} first={:.0} last={:.0} peak={:.0} slope/s={:.3} growth={:+.1}%{}",
                t.name,
                t.first,
                t.last,
                t.peak,
                t.slope_per_s,
                t.growth_frac * 100.0,
                flag,
            );
        }
        let offenders = self.offenders();
        if self.passed() {
            println!("drift        no-leak-or-drift — PASS");
        } else {
            let worst = offenders
                .iter()
                .map(|t| t.name)
                .collect::<Vec<_>>()
                .join(", ");
            println!("drift        no-leak-or-drift — FAIL (worst: {worst})");
        }
    }
}

/// Run the `long-run` soak: drive a steady insert load from `--writers` writers
/// while the interval sampler pulls a `stats()` + resource sample every
/// `--sample-interval-ms`, then fit the trend and emit the verdict.
pub(crate) fn run_long_run(opts: &Opts) -> Result<Report, BenchError> {
    // The soak samples this process's own RSS/fds, which only reflect the engine
    // when it runs in-process; a deployed server is out of reach. Reject the
    // pgwire/server form (consistent with `scale-to-zero`).
    if opts.transport == Transport::Pgwire || opts.server.is_some() {
        return Err(BenchError::Config(
            "long-run samples this process's resources (in-process embedded); \
             drop --transport pgwire / --server"
                .into(),
        ));
    }

    let url = opts.url.clone();
    let tag = run_tag();

    // Setup: a fresh table seeded with a fixed `--rows` working set the writers
    // read from for the whole run (steady-state load — see [`TABLE`]).
    seed(&url, opts.rows)?;

    // The live latency window: writers record each commit here; the sampler
    // swaps it out per tick to read that window's p99 (L1's p99 series source).
    let window = Arc::new(Mutex::new(Histogram::new()));
    // The full-run histogram for the percentile distribution in the record.
    let full = Arc::new(Mutex::new(Histogram::new()));

    // A sampler-owned connection sharing the same registry `Database`, so its
    // `stats()` pull observes the engine the writers are driving.
    let sampler_conn = Connection::open(&url)
        .map_err(|e| BenchError::Connection(format!("open {url} (sampler): {e}")))?;

    let run_start = Instant::now();
    let deadline = run_start + opts.duration;

    let (series, writer_result) = std::thread::scope(|scope| {
        let rows = opts.rows;
        let handles: Vec<_> = (0..opts.writers)
            .map(|w| {
                let url = url.clone();
                let window = Arc::clone(&window);
                let full = Arc::clone(&full);
                scope.spawn(move || soak_reader(&url, w, tag, rows, deadline, &window, &full))
            })
            .collect();

        // Sample on the scope's own thread so the series comes straight back.
        let series = run_sampler(deadline, opts.sample_interval, |t_s| {
            let stats = sampler_conn.stats();
            let res = ResourceSample::probe();
            // Swap the live window out and read its p99 (then start a fresh one).
            let win = {
                let mut g = window.lock().unwrap();
                std::mem::take(&mut *g)
            };
            let p99_us = if win.count() > 0 {
                win.value_at_quantile(0.99)
            } else {
                0
            };
            Sample {
                t_s,
                commits: stats.commits,
                rss_bytes: res.rss_bytes,
                fds: res.fds,
                threads: res.threads,
                p99_us,
            }
        });

        let writer_result = handles.into_iter().try_for_each(|h| h.join().unwrap());
        (series, writer_result)
    });
    writer_result?;

    let elapsed = run_start.elapsed();
    let mut series = series;

    // Test-only fault: seed monotonic growth into the sampled series so the
    // negative test proves the trend checker bites (a real leak does this on its
    // own). It rides on top of the real samples, so it also fires where `/proc`
    // is unavailable and the real gauges are flat zeros.
    if opts.inject_fault == Some(Fault::Leak) {
        seed_leak(&mut series);
    }

    let analysis = analyze(&series, opts.warmup, opts.drift_threshold);
    let soak = Soak {
        samples: series.len() as u64,
        analyzed: analysis.analyzed as u64,
        interval_ms: opts.sample_interval.as_millis() as u64,
        warmup_s: opts.warmup.as_secs_f64(),
        threshold: opts.drift_threshold,
        analysis,
    };

    let hist = Arc::try_unwrap(full)
        .map(|m| m.into_inner().unwrap())
        .unwrap_or_else(|arc| arc.lock().unwrap().clone());
    let commits = hist.count();

    Ok(Report {
        experiment: "long-run",
        label: opts.label.clone(),
        transport: opts.transport.name(),
        url_scheme: url_scheme(&opts.url),
        writers: opts.writers,
        duration_s: elapsed.as_secs_f64(),
        commits,
        conflicts: 0,
        failures: 0,
        throughput: commits as f64 / elapsed.as_secs_f64().max(f64::MIN_POSITIVE),
        hist,
        git_sha: git_sha(),
        json_only: opts.json,
        correctness: None,
        lifecycle: None,
        soak: Some(soak),
        burst: None,
    })
}

/// Inject a monotonically rising series on top of the real samples so the trend
/// checker has an unambiguous leak/drift to catch (test-only QA hook). Steps are
/// sized to clear every gate's absolute floor within a handful of samples.
fn seed_leak(series: &mut [Sample]) {
    // 16 MiB / sample RSS, +1 fd / sample, +250 µs / sample p99 — each clears
    // its floor (8 MiB / 4 fds / 200 µs) over the analyzed window.
    const RSS_STEP: u64 = 16 * 1024 * 1024;
    const P99_STEP: u64 = 250;
    for (i, s) in series.iter_mut().enumerate() {
        let i = i as u64;
        s.rss_bytes = s.rss_bytes.saturating_add(i.saturating_mul(RSS_STEP));
        s.fds = s.fds.saturating_add(i);
        s.p99_us = s.p99_us.saturating_add(i.saturating_mul(P99_STEP));
    }
}

/// Drop + recreate the soak table and seed a fixed `--rows` working set in one
/// transaction (DDL is autocommit; the inserts batch under one explicit txn so
/// seeding is one durable append). The keys are `0..rows`, the space the readers
/// point at.
fn seed(url: &str, rows: u64) -> Result<(), BenchError> {
    let mut conn =
        Connection::open(url).map_err(|e| BenchError::Connection(format!("open {url}: {e}")))?;
    // Reset any residue from a prior run so the read working set is exact.
    let _ = conn.exec(&format!("DROP TABLE {TABLE}"));
    conn.exec(&format!(
        "CREATE TABLE {TABLE} (k INTEGER PRIMARY KEY, v INTEGER)"
    ))
    .map_err(|e| BenchError::Run(format!("create soak table: {e}")))?;
    conn.exec("BEGIN")
        .map_err(|e| BenchError::Run(format!("begin: {e}")))?;
    for k in 0..rows {
        conn.exec(&format!("INSERT INTO {TABLE} (k, v) VALUES ({k}, {k})"))
            .map_err(|e| BenchError::Run(format!("seed insert {k}: {e}")))?;
    }
    conn.exec("COMMIT")
        .map_err(|e| BenchError::Run(format!("commit: {e}")))?;
    Ok(())
}

/// One soak reader: point-read random keys from the seeded working set until the
/// deadline, recording each read's latency into both the live window (for the
/// sampler's p99) and the full-run histogram (for the record's distribution).
/// A steady read load keeps the resource baseline flat (no unbounded store
/// growth), which is what lets the trend checker isolate a real leak/drift.
fn soak_reader(
    url: &str,
    writer: usize,
    tag: u128,
    rows: u64,
    deadline: Instant,
    window: &Mutex<Histogram>,
    full: &Mutex<Histogram>,
) -> Result<(), BenchError> {
    let mut conn =
        Connection::open(url).map_err(|e| BenchError::Connection(format!("open {url}: {e}")))?;
    let mut rng = Rng::new(tag as u64 ^ ((writer as u64).wrapping_mul(0x100_0000_01b3)));
    while Instant::now() < deadline {
        let k = rng.below(rows);
        let t0 = Instant::now();
        conn.query(&format!("SELECT v FROM {TABLE} WHERE k = {k}"))
            .map_err(|e| BenchError::Run(format!("reader {writer} read failed: {e}")))?;
        let us = t0.elapsed().as_micros() as u64;
        window.lock().unwrap().record(us);
        full.lock().unwrap().record(us);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// L1: over a short window the sampler captures ≈ duration/interval samples.
    /// A trivial probe isolates the loop's cadence from any real sampling work.
    #[test]
    fn sampler_captures_expected_sample_count() {
        let interval = Duration::from_millis(20);
        let deadline = Instant::now() + Duration::from_millis(200);
        let series = run_sampler(deadline, interval, |t_s| Sample {
            t_s,
            ..Sample::default()
        });
        // ~10 expected (200ms / 20ms); allow scheduler slop on a loaded CI host.
        assert!(
            (7..=13).contains(&series.len()),
            "expected ≈10 samples, got {}",
            series.len()
        );
        // Timestamps are monotonically non-decreasing and within the window.
        for w in series.windows(2) {
            assert!(w[1].t_s >= w[0].t_s);
        }
    }

    /// L2: the `/proc/self/status` parser pulls VmRSS (→ bytes) and the thread
    /// count out of a captured fixture, without a live `/proc`.
    #[test]
    fn parses_proc_status_fixture() {
        let fixture = "\
Name:\ttwill-bench
State:\tR (running)
VmPeak:\t  912345 kB
VmRSS:\t  123456 kB
Threads:\t7
nonsense line without colon
";
        let (rss, threads) = parse_status(fixture);
        assert_eq!(rss, 123456 * 1024);
        assert_eq!(threads, 7);
    }

    /// L2: missing fields degrade to zeros, never a panic — the same contract as
    /// a missing `/proc` on a non-Linux build.
    #[test]
    fn parses_proc_status_missing_fields_as_zero() {
        let (rss, threads) = parse_status("Name:\tx\nState:\tS\n");
        assert_eq!((rss, threads), (0, 0));
    }

    /// L2: the live probe never crashes, whatever the platform. On Linux it
    /// returns nonzero RSS/threads; elsewhere it degrades to zeros.
    #[test]
    fn resource_probe_does_not_crash() {
        let r = ResourceSample::probe();
        if cfg!(target_os = "linux") {
            assert!(r.rss_bytes > 0, "Linux RSS should be nonzero");
            assert!(r.threads >= 1, "Linux thread count should be ≥ 1");
        }
        // No assertion off-Linux: zeros are the defined graceful-degradation.
    }

    /// L3: the least-squares slope tracks a known line and is zero on a flat or
    /// degenerate input.
    #[test]
    fn slope_fits_a_line() {
        let xs = [0.0, 1.0, 2.0, 3.0];
        // y = 5x + 1.
        let ys = [1.0, 6.0, 11.0, 16.0];
        assert!((slope(&xs, &ys) - 5.0).abs() < 1e-9);
        assert_eq!(slope(&xs, &[2.0, 2.0, 2.0, 2.0]), 0.0);
        assert_eq!(slope(&[1.0], &[1.0]), 0.0);
    }

    /// Build a synthetic series with a chosen per-metric shape for the analysis
    /// tests: `n` samples one second apart.
    fn series(n: usize, rss: impl Fn(usize) -> u64, fds: impl Fn(usize) -> u64) -> Vec<Sample> {
        (0..n)
            .map(|i| Sample {
                t_s: i as f64,
                rss_bytes: rss(i),
                fds: fds(i),
                threads: 5,
                p99_us: 100,
                commits: i as u64,
            })
            .collect()
    }

    /// L3: a flat series is PASS — no gated metric trends up past its floor.
    #[test]
    fn flat_series_passes() {
        let s = series(20, |_| 64 * 1024 * 1024, |_| 12);
        let a = analyze(&s, Duration::from_secs(2), 0.10);
        assert!(a.passed, "a flat series must not be flagged as a leak");
        assert!(a.trends.iter().all(|t| !t.leaking));
    }

    /// L3: a steadily rising RSS series (a memory leak) is FAIL, and `memory` is
    /// the flagged metric.
    #[test]
    fn rising_memory_series_is_flagged() {
        // +32 MiB per second from a 64 MiB base — clears the 8 MiB floor easily.
        let s = series(20, |i| (64 + 32 * i as u64) * 1024 * 1024, |_| 12);
        let a = analyze(&s, Duration::from_secs(2), 0.10);
        assert!(!a.passed, "a rising RSS series must be flagged");
        let mem = a.trends.iter().find(|t| t.name == "memory").unwrap();
        assert!(mem.leaking);
        // fds were flat, so they must not be flagged.
        assert!(!a.trends.iter().find(|t| t.name == "fds").unwrap().leaking);
    }

    /// L3: a small absolute drift that is relatively large is *not* flagged — the
    /// absolute floor guards a low-baseline metric from a false positive.
    #[test]
    fn small_absolute_growth_under_floor_passes() {
        // fds 8 → 10 over the run: +25% relative, but only +2 fds (< 4 floor).
        let s = series(20, |_| 64 * 1024 * 1024, |i| 8 + (i as u64) / 10);
        let a = analyze(&s, Duration::from_secs(2), 0.10);
        assert!(a.passed, "a sub-floor absolute growth must not be flagged");
    }

    /// L3: fewer than two post-warm-up samples can't support a trend → PASS with
    /// `analyzed` reflecting how little was usable.
    #[test]
    fn insufficient_samples_pass() {
        let s = series(3, |i| (64 + 100 * i as u64) * 1024 * 1024, |_| 12);
        // Warm-up past all but the last sample.
        let a = analyze(&s, Duration::from_secs(10), 0.10);
        assert!(a.passed);
        assert!(a.analyzed < 2);
    }

    /// L4: the seeded-leak hook makes every gated metric rise so the verdict
    /// fails even when the real gauges start flat (e.g. `/proc` absent).
    #[test]
    fn seeded_leak_fails_the_verdict() {
        let mut s = series(20, |_| 0, |_| 0); // flat zeros, as off-Linux
        seed_leak(&mut s);
        let a = analyze(&s, Duration::from_secs(2), 0.10);
        assert!(!a.passed, "the seeded leak must trip the verdict");
        // All three gated metrics climb under the seed.
        for name in ["memory", "fds", "p99"] {
            assert!(
                a.trends.iter().find(|t| t.name == name).unwrap().leaking,
                "{name} should be flagged under the seeded leak"
            );
        }
    }
}
