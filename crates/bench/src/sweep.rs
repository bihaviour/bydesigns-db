//! The spec-09 validation-campaign sweeps (issue #91): the Exp-2 group-commit
//! window sweep (V-2) and the Exp-3 N-database sharding orchestrator (V-3). Both
//! compose the steady-state experiment writers ([`crate::writer_loop`]) across a
//! swept dimension and hand the resulting curve to [`crate::analysis`] for the
//! falsifiable verdict — the plateau knee and W1-engagement gate for the window
//! sweep, the near-linear-scaling gate and cross-DB CAS finding for the shard
//! sweep.
//!
//! Both stay CLI-only and pull no engine internals: the sweeps drive the public
//! commit path and read percentiles back out, exactly as a real campaign on a
//! deployed host would. The "window" the Exp-2 sweep parameterises is the
//! offered write concurrency — in this engine's leader/follower group commit
//! (`crates/engine/src/group_commit.rs`) the number of commits concurrently
//! enqueued *is* the coalescing window; sweeping it traces the plateau height
//! against the Exp-1 tail without adding an engine knob (V-2 guardrail: "the
//! window is an engine config knob driven from the bench, not new engine code").

use crate::hist::Histogram;
use crate::{
    analysis, git_sha, resolve_target, run_tag, setup_schema, url_scheme, writer_loop, Archival,
    BenchError, Opts, Report, Target, Transport,
};
use std::time::{Duration, Instant};

/// A geometric ladder `1, 2, 4, … max` (inclusive of `max`), the swept points
/// for both campaigns. `max` is always the final point even when not a power of
/// two, so the sweep's right edge is exactly what the operator asked for.
fn ladder(max: usize) -> Vec<usize> {
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

// ───────────────────────────── V-2: Exp-2 window sweep ─────────────────────

/// The Exp-2 group-commit-window sweep result (V-2): throughput and the p99 tail
/// at each swept concurrency point, the Exp-1 single-lane ceiling, the detected
/// plateau knee, and the W1-engagement verdict (plateau ÷ ceiling).
pub struct Sweep {
    /// The swept points (offered write concurrency = the coalescing window).
    pub windows: Vec<u64>,
    /// Sustained throughput (commits/sec) at each point.
    pub throughput: Vec<f64>,
    /// The p99 commit-latency tail (µs) at each point — the cost side of the
    /// Pareto frontier the plateau trades against.
    pub p99_us: Vec<u64>,
    /// The Exp-1 single-lane ceiling (commits/sec at concurrency 1).
    pub exp1_ceiling: f64,
    /// Index into the ladders of the detected plateau knee, if any.
    pub knee: Option<usize>,
    /// The plateau throughput (the curve's max).
    pub plateau: f64,
    /// Whether `--gate` was set, so the engagement guard maps to the exit code.
    pub gated: bool,
}

impl Sweep {
    /// The W1-lever payoff: plateau ÷ Exp-1 ceiling.
    pub fn gain(&self) -> f64 {
        analysis::plateau_gain(self.plateau, self.exp1_ceiling)
    }

    /// V-2 gate: group commit is not engaging (plateau ≤ 1.5× the ceiling).
    /// Only fails the run when `--gate` was set.
    pub fn gate_failed(&self) -> bool {
        self.gated && analysis::group_commit_not_engaging(self.plateau, self.exp1_ceiling)
    }

    /// The window at the knee, or the window of the plateau if no knee was found.
    fn knee_window(&self) -> u64 {
        match self.knee {
            Some(i) => self.windows.get(i).copied().unwrap_or(0),
            None => {
                // No knee → report the window where the plateau was reached.
                let mut best = (0usize, f64::MIN);
                for (i, &t) in self.throughput.iter().enumerate() {
                    if t > best.1 {
                        best = (i, t);
                    }
                }
                self.windows.get(best.0).copied().unwrap_or(0)
            }
        }
    }

    pub fn print_human(&self) {
        println!("── exp2 group-commit-window sweep (V-2) ───────────");
        println!(
            "exp1 ceiling {:.0} commits/s (concurrency 1)",
            self.exp1_ceiling
        );
        for i in 0..self.windows.len() {
            println!(
                "window {:>4}  throughput={:>10.0}/s  p99={}µs",
                self.windows[i], self.throughput[i], self.p99_us[i],
            );
        }
        println!(
            "plateau      {:.0}/s at window {}  ({:.1}× exp1 ceiling)",
            self.plateau,
            self.knee_window(),
            self.gain(),
        );
        let verdict = if analysis::group_commit_not_engaging(self.plateau, self.exp1_ceiling) {
            "GROUP COMMIT NOT ENGAGING (plateau ≤ 1.5× ceiling — check batching)"
        } else {
            "engaged (batching lifts throughput well above the single-lane ceiling)"
        };
        println!("verdict      {verdict}");
    }

    pub fn to_json(&self) -> String {
        let arr_u = |v: &[u64]| v.iter().map(u64::to_string).collect::<Vec<_>>().join(",");
        let arr_f = |v: &[f64]| {
            v.iter()
                .map(|x| format!("{x:.1}"))
                .collect::<Vec<_>>()
                .join(",")
        };
        format!(
            "{{\"windows\":[{}],\"throughput\":[{}],\"p99_us\":[{}],\
             \"exp1_ceiling\":{:.1},\"plateau\":{:.1},\"gain\":{:.3},\
             \"knee_window\":{},\"engaged\":{}}}",
            arr_u(&self.windows),
            arr_f(&self.throughput),
            arr_u(&self.p99_us),
            self.exp1_ceiling,
            self.plateau,
            self.gain(),
            self.knee_window(),
            !analysis::group_commit_not_engaging(self.plateau, self.exp1_ceiling),
        )
    }
}

/// Run the Exp-2 group-commit-window sweep (V-2). Drives the independent-row
/// write workload at each concurrency point `1, 2, 4, … --sweep-max`, builds the
/// throughput-vs-window curve, finds the plateau knee, and reports the
/// W1-engagement verdict — failing (under `--gate`) when the plateau is ≤ 1.5×
/// the single-lane ceiling.
pub(crate) fn run_group_commit_sweep(opts: &Opts) -> Result<Report, BenchError> {
    let target = resolve_target(opts)?;
    let mut setup = target.open()?;
    setup_schema(&mut setup, false)?;

    let points = ladder(opts.sweep_max);
    let mut windows = Vec::new();
    let mut throughput = Vec::new();
    let mut p99_us = Vec::new();

    let run_start = Instant::now();
    for &writers in &points {
        let (hist, secs) = drive_independent(&target, writers, opts.warmup, opts.duration)?;
        let commits = hist.count();
        windows.push(writers as u64);
        throughput.push(commits as f64 / secs.max(f64::MIN_POSITIVE));
        p99_us.push(hist.value_at_quantile(0.99));
    }
    let elapsed = run_start.elapsed();

    let exp1_ceiling = throughput.first().copied().unwrap_or(0.0);
    let plateau = throughput.iter().copied().fold(0.0f64, f64::max);
    let xs: Vec<f64> = windows.iter().map(|&w| w as f64).collect();
    let knee = analysis::knee(&xs, &throughput);

    let sweep = Sweep {
        windows,
        throughput,
        p99_us,
        exp1_ceiling,
        knee,
        plateau,
        gated: opts.gate,
    };

    Ok(sweep_report("exp2-window-sweep", opts, elapsed, sweep))
}

/// Drive `writers` independent-row writers for one timed window and return the
/// merged commit-latency histogram plus the timed-window length in seconds. Each
/// writer commits unique-key INSERTs, so they never conflict — the
/// group-commit-coalescing path, not the contention path.
fn drive_independent(
    target: &Target,
    writers: usize,
    warmup: Duration,
    duration: Duration,
) -> Result<(Histogram, f64), BenchError> {
    let tag = run_tag();
    let tallies = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..writers)
            .map(|w| {
                let target = target.clone();
                scope.spawn(move || writer_loop(&target, w, tag, false, warmup, duration))
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().unwrap())
            .collect::<Vec<_>>()
    });
    let mut merged = Histogram::new();
    for t in tallies {
        merged.merge(&t?.hist);
    }
    Ok((merged, duration.as_secs_f64()))
}

// ───────────────────────────── V-3: Exp-3 sharding ─────────────────────────

/// The Exp-3 N-database sharding sweep result (V-3): aggregate throughput as the
/// lane count grows, the single-DB contended throughput against the Exp-1
/// ceiling (confirming all writers serialize in one DB), and the near-linear-
/// scaling verdict that answers whether the S3-CAS commit log is a cross-DB
/// serialization point.
pub struct Shard {
    /// The swept lane counts (`1, 2, 4, … --databases`).
    pub shards: Vec<u64>,
    /// Aggregate throughput (commits/sec across all DBs) at each lane count.
    pub aggregate: Vec<f64>,
    /// Writers contending on the one hot row *within* each database.
    pub per_db_writers: usize,
    /// The Exp-1 single-lane independent-row ceiling (commits/sec) — the bar the
    /// single-DB contended throughput must sit at or below (acceptance (a)).
    pub exp1_ceiling: f64,
    /// Whether `--gate` was set, so the scaling guard maps to the exit code.
    pub gated: bool,
}

impl Shard {
    /// The single-DB (lane count 1) aggregate throughput.
    fn single_db(&self) -> f64 {
        self.aggregate.first().copied().unwrap_or(0.0)
    }

    /// The W2-lever payoff: how near-linear the aggregate scales with lanes.
    pub fn efficiency(&self) -> f64 {
        let xs: Vec<f64> = self.shards.iter().map(|&n| n as f64).collect();
        analysis::scaling_efficiency(&xs, &self.aggregate)
    }

    /// V-3 gate: the sharding lever did *not* recover near-linear scaling
    /// (efficiency < 0.8) — the documented cross-DB serialization finding. Only
    /// fails the run when `--gate` was set.
    pub fn gate_failed(&self) -> bool {
        let xs: Vec<f64> = self.shards.iter().map(|&n| n as f64).collect();
        self.gated && !analysis::sharding_scales_linearly(&xs, &self.aggregate)
    }

    pub fn print_human(&self) {
        println!("── exp3 N-database sharding sweep (V-3) ────────────");
        println!(
            "exp1 ceiling {:.0} commits/s  ·  {} writer(s)/DB on the hot row",
            self.exp1_ceiling, self.per_db_writers,
        );
        for i in 0..self.shards.len() {
            println!(
                "lanes {:>4}   aggregate={:>10.0}/s",
                self.shards[i], self.aggregate[i],
            );
        }
        // Acceptance (a): one contended DB serializes — at or below the ceiling.
        let serializes = self.single_db() <= self.exp1_ceiling * 1.10;
        println!(
            "single-DB    {:.0}/s {} exp1 ceiling — {}",
            self.single_db(),
            if serializes { "≤" } else { ">" },
            if serializes {
                "writers serialize (W2 confirmed)"
            } else {
                "unexpectedly above the single-lane ceiling"
            },
        );
        let eff = self.efficiency();
        let xs: Vec<f64> = self.shards.iter().map(|&n| n as f64).collect();
        let verdict = if analysis::sharding_scales_linearly(&xs, &self.aggregate) {
            format!("SHARDING SCALES (efficiency {eff:.2} ≥ 0.80 — lanes are independent)")
        } else {
            format!(
                "SUB-LINEAR (efficiency {eff:.2} < 0.80 — a shared resource serializes \
                 across DBs; investigate the S3-CAS commit log)"
            )
        };
        println!("verdict      {verdict}");
    }

    pub fn to_json(&self) -> String {
        let arr_u = |v: &[u64]| v.iter().map(u64::to_string).collect::<Vec<_>>().join(",");
        let arr_f = |v: &[f64]| {
            v.iter()
                .map(|x| format!("{x:.1}"))
                .collect::<Vec<_>>()
                .join(",")
        };
        let xs: Vec<f64> = self.shards.iter().map(|&n| n as f64).collect();
        format!(
            "{{\"shards\":[{}],\"aggregate\":[{}],\"per_db_writers\":{},\
             \"exp1_ceiling\":{:.1},\"single_db\":{:.1},\"efficiency\":{:.3},\
             \"scales_linearly\":{}}}",
            arr_u(&self.shards),
            arr_f(&self.aggregate),
            self.per_db_writers,
            self.exp1_ceiling,
            self.single_db(),
            self.efficiency(),
            analysis::sharding_scales_linearly(&xs, &self.aggregate),
        )
    }
}

/// Run the Exp-3 N-database sharding orchestrator (V-3). For each lane count
/// `1, 2, 4, … --databases`, drives `--writers` contending writers on one hot
/// row in *each* of N independent databases under a single synchronized timed
/// window, and reports aggregate throughput vs lane count — proving (or
/// refuting) that the many-small-DBs design recovers linear scaling. Embedded /
/// in-process pgwire only: each lane is a distinct database, which an external
/// single-`--db` server cannot host.
pub(crate) fn run_sharding(opts: &Opts) -> Result<Report, BenchError> {
    if opts.server.is_some() {
        return Err(BenchError::Config(
            "exp3-shard drives N independent databases; an external --server hosts one \
             --db, so it cannot serve the sharding variant. Use embedded (default) or \
             in-process pgwire (--transport pgwire without --server)."
                .into(),
        ));
    }

    // The Exp-1 single-lane ceiling: one independent-row writer, no contention.
    let ceiling_target = open_for_url(opts, &shard_url(&opts.url, "ceiling"))?;
    {
        let mut s = ceiling_target.open()?;
        setup_schema(&mut s, false)?;
    }
    let (ceiling_hist, ceiling_secs) =
        drive_independent(&ceiling_target, 1, opts.warmup, opts.duration)?;
    let exp1_ceiling = ceiling_hist.count() as f64 / ceiling_secs.max(f64::MIN_POSITIVE);

    let lanes = ladder(opts.databases);
    let mut shards = Vec::new();
    let mut aggregate = Vec::new();

    let run_start = Instant::now();
    for &n in &lanes {
        let agg = drive_shards(opts, n)?;
        shards.push(n as u64);
        aggregate.push(agg);
    }
    let elapsed = run_start.elapsed();

    let shard = Shard {
        shards,
        aggregate,
        per_db_writers: opts.writers,
        exp1_ceiling,
        gated: opts.gate,
    };

    Ok(shard_report("exp3-sharding", opts, elapsed, shard))
}

/// Drive `n` independent databases, each with `opts.writers` writers contending
/// on its one hot row, under a single timed window; return aggregate throughput
/// (total commits across all DBs / the timed-window length). The DBs share no
/// state — distinct URLs → distinct engine `Database`s → distinct commit lanes —
/// so any failure of aggregate throughput to scale points at a resource shared
/// *below* the engine (the storage CAS log).
fn drive_shards(opts: &Opts, n: usize) -> Result<f64, BenchError> {
    // Build + seed one target per database up front (the contended row must
    // exist before the timed window opens).
    let mut targets = Vec::with_capacity(n);
    for k in 0..n {
        let url = shard_url(&opts.url, &format!("lane{k}"));
        let target = open_for_url(opts, &url)?;
        let mut s = target.open()?;
        setup_schema(&mut s, true)?;
        targets.push(target);
    }

    let tag = run_tag();
    let warmup = opts.warmup;
    let duration = opts.duration;
    let per_db = opts.writers;

    // One pacer: every (db, writer) thread warms up and runs its timed window
    // together (spawned within microseconds, identical warm-up), so the windows
    // align — the synchronized start/stop the variant calls for.
    let tallies = std::thread::scope(|scope| {
        let mut handles = Vec::new();
        for target in &targets {
            for w in 0..per_db {
                let target = target.clone();
                handles.push(
                    scope.spawn(move || writer_loop(&target, w, tag, true, warmup, duration)),
                );
            }
        }
        handles
            .into_iter()
            .map(|h| h.join().unwrap())
            .collect::<Vec<_>>()
    });

    let mut commits = 0u64;
    for t in tallies {
        commits += t?.hist.count();
    }
    Ok(commits as f64 / duration.as_secs_f64().max(f64::MIN_POSITIVE))
}

/// Derive a distinct per-shard URL by suffixing the base. Backend-agnostic: a
/// sibling file for `file://`, a sibling key for an object store — the same
/// suffix works for both because the engine treats the whole post-scheme string
/// as the database locator.
fn shard_url(base: &str, suffix: &str) -> String {
    format!("{base}-{suffix}")
}

/// Resolve a [`Target`] for an arbitrary URL honouring the transport. Embedded
/// returns a direct handle; in-process pgwire spins a dedicated listener for
/// *this* URL (each shard is its own backend, so it needs its own server). An
/// external `--server` is rejected upstream for the sharding path.
fn open_for_url(opts: &Opts, url: &str) -> Result<Target, BenchError> {
    match opts.transport {
        Transport::Embedded => Ok(Target::Embedded(url.to_string())),
        Transport::Pgwire => {
            let listener = std::net::TcpListener::bind("127.0.0.1:0")
                .map_err(|e| BenchError::Connection(format!("bind shard server: {e}")))?;
            let addr = listener
                .local_addr()
                .map_err(|e| BenchError::Connection(format!("shard server addr: {e}")))?
                .to_string();
            let url = url.to_string();
            std::thread::spawn(move || {
                let _ = twill_server::serve_listener(listener, &url);
            });
            Ok(Target::Pgwire(addr))
        }
    }
}

// ───────────────────────────── shared report glue ─────────────────────────

fn sweep_report(name: &'static str, opts: &Opts, elapsed: Duration, sweep: Sweep) -> Report {
    let mut report = base_report(name, opts, elapsed);
    report.commits = sweep.windows.len() as u64;
    report.throughput = sweep.plateau;
    report.sweep = Some(sweep);
    report
}

fn shard_report(name: &'static str, opts: &Opts, elapsed: Duration, shard: Shard) -> Report {
    let mut report = base_report(name, opts, elapsed);
    report.commits = shard.shards.len() as u64;
    report.throughput = shard.aggregate.iter().copied().fold(0.0, f64::max);
    report.shard = Some(shard);
    report
}

/// A bare [`Report`] shell the sweep/shard fills in. The per-point distributions
/// live in the sweep/shard sections; the top-level histogram stays empty (the
/// sweep is a curve, not one distribution).
fn base_report(name: &'static str, opts: &Opts, elapsed: Duration) -> Report {
    Report {
        experiment: name,
        label: opts.label.clone(),
        transport: opts.transport.name(),
        url_scheme: url_scheme(&opts.url),
        writers: opts.writers,
        duration_s: elapsed.as_secs_f64(),
        commits: 0,
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
        herd: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ladder_is_geometric_and_includes_max() {
        assert_eq!(ladder(8), vec![1, 2, 4, 8]);
        assert_eq!(ladder(1), vec![1]);
        assert_eq!(ladder(6), vec![1, 2, 4, 6]);
        assert_eq!(ladder(0), vec![1]);
    }
}
