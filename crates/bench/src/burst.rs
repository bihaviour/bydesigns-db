//! The `burst` autoscaling-stress scenario (spec 15 — lifecycle scenarios;
//! issue #79, follow-up to #49/#53). Unlike the steady-state experiments and the
//! request-mix scenarios — which run a *fixed* writer count as fast as they can
//! (open-loop, work-bounded) — `burst` holds a **target request rate** and lets
//! the system's worker count respond, driving the [`controller`] through load
//! swings (`idle → 500 → 5k → 20k rps → idle`, repeated) to measure cold starts,
//! warm starts, scaling latency, and worker allocation.
//!
//! Three pieces compose the scenario:
//!
//!   * a **closed-loop rate driver** ([`Pacer`], B1) — a transport-agnostic
//!     token-bucket that holds a target rps with bounded, *seeded* jitter
//!     regardless of how fast the system responds, exposing a [`Pacer::set_rate`]
//!     knob the schedule drives;
//!   * a **load-shape schedule** ([`Schedule`], B2) — a deterministic ramp
//!     descriptor (plateau rps + dwell + ramp duration) with the default `burst`
//!     shape baked in;
//!   * a **multi-connection fan-out** driver ([`run_burst`], B3) — M connections
//!     under one shared pacer so the bench process is not the bottleneck at the
//!     peak rate, sampling the [`ControllerStats`](controller::ControllerStats)
//!     snapshot at plateau boundaries (B4) for the cold/warm-start counts, peak
//!     workers, admission wait, and per-ramp scaling latency.
//!
//! Like `scale-to-zero`, it is **controller-driven and in-process**: the scenario
//! owns a [`Controller`] so it can both drive the lifecycle and *pull* the stats
//! snapshot (the #53 rule — pull, never scrape or push). A deployed pgwire server
//! runs its own controller out of the bench's reach, so the `--server` / pgwire
//! form is rejected here (that is the spec-09 scale form against a real
//! deployment); the in-process embedded path is the CI gate.

use crate::hist::Histogram;
use crate::workload::Rng;
use crate::{git_sha, run_tag, url_scheme, BenchError, Lifecycle, Opts, Report, Transport};
use controller::{Controller, ControllerConfig, LifecycleState};
use engine::Connection;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The scenario's own table; dropped + recreated each run so the post-run row
/// count (the acked-write-loss check) is exact regardless of any residue.
const TABLE: &str = "bench_burst";

// ── B1 — closed-loop rate driver ────────────────────────────────────────────

/// The result of asking the pacer for a token at wall offset `now`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Acquire {
    /// A token was due — consume it and issue one request.
    Ready,
    /// Not yet due; wait this many nanoseconds (then re-ask).
    NotYet(u64),
    /// The pacer is paused (rate 0 — an idle plateau): issue nothing.
    Paused,
}

/// A closed-loop token-bucket pacer: it holds a target rate (tokens/second)
/// **regardless of how fast the consumer drains tokens**, which is what makes
/// `burst` a closed-loop (rate-bounded) load rather than the open-loop
/// (work-bounded) experiments. Tokens become available on a fixed cadence
/// (`1 / rate`) with bounded, *seeded* jitter; a consumer takes one via
/// [`Acquire::Ready`] only when it is due, so the realized offered rate tracks
/// the target however many connections share the pacer or however fast the
/// system responds.
///
/// `cursor_ns` is the wall offset (from the driver origin) at which the next
/// token is available. [`try_acquire`](Pacer::try_acquire) advances it by one
/// jittered interval per token (jitter is zero-mean, so the long-run rate is the
/// target) and clamps it to never lag `now` by more than one interval — so a slow
/// patch never lets the rate race ahead in an unbounded catch-up flood. It is
/// driven by a clock value the caller passes, so the rate property is
/// unit-testable on a virtual clock without sleeping.
pub struct Pacer {
    /// Nanoseconds between tokens at the current rate; `0` == paused (rate 0).
    interval_ns: u64,
    /// Wall offset (ns from origin) at which the next token is available.
    cursor_ns: u128,
    /// Jitter half-width as a fraction of the interval, in basis points
    /// (e.g. `1000` == ±10%). Bounded so the offered cadence stays close to
    /// uniform.
    jitter_bp: u64,
    /// Seeded jitter source — a run's offered schedule is reproducible.
    rng: Rng,
}

impl Pacer {
    /// A pacer seeded for reproducible jitter, starting paused (`set_rate` arms
    /// it). `jitter_bp` is the jitter half-width as basis points of the interval.
    pub fn new(jitter_bp: u64, seed: u64) -> Pacer {
        Pacer {
            interval_ns: 0,
            cursor_ns: 0,
            jitter_bp,
            rng: Rng::new(seed),
        }
    }

    /// Set the target rate (tokens/second). A rate `<= 0` pauses the pacer
    /// (`try_acquire` returns [`Acquire::Paused`]) — the idle plateau.
    pub fn set_rate(&mut self, rate_hz: f64) {
        self.interval_ns = if rate_hz > 0.0 {
            (1e9 / rate_hz).round().max(1.0) as u64
        } else {
            0
        };
    }

    /// The realized interval target (ns) — `0` when paused. Exposed for tests.
    pub fn interval_ns(&self) -> u64 {
        self.interval_ns
    }

    /// Ask for a token at wall offset `now_ns` (from the driver origin). Consumes
    /// and returns [`Acquire::Ready`] when one is due, advancing availability by
    /// one jittered interval; [`Acquire::NotYet`] with the nanoseconds to wait
    /// otherwise; [`Acquire::Paused`] when the rate is 0. Availability is clamped
    /// into `[now - interval, now + interval]` before each decision, which bounds
    /// catch-up after a slow patch to a token or two *and* keeps the pacer
    /// responsive when the rate rises mid-schedule: a stale far-future cursor left
    /// over from a low-rate plateau is pulled back to one new (short) interval out,
    /// so the higher rate takes effect immediately rather than waiting out the old
    /// interval.
    pub fn try_acquire(&mut self, now_ns: u128) -> Acquire {
        if self.interval_ns == 0 {
            return Acquire::Paused;
        }
        // Clamp availability into `[now - interval, now + interval + jitter]`. The
        // lower bound caps catch-up after a slow patch; the upper bound is widened
        // by the max jitter so a normal jittered step (≤ interval + jitter) is
        // never clipped — only a *stale* far-future cursor left from a low-rate
        // plateau is pulled back, keeping a rate increase responsive without
        // biasing the long-run cadence.
        let half = self.interval_ns.saturating_mul(self.jitter_bp) / 10_000;
        let lo = now_ns.saturating_sub(self.interval_ns as u128);
        let hi = now_ns + self.interval_ns as u128 + half as u128;
        self.cursor_ns = self.cursor_ns.clamp(lo, hi);
        if now_ns < self.cursor_ns {
            return Acquire::NotYet((self.cursor_ns - now_ns) as u64);
        }
        // Due: consume one token and advance by a jittered interval (zero-mean,
        // so the long-run cadence is exactly the target rate).
        let jitter = if half > 0 {
            self.rng.below(2 * half + 1) as i128 - half as i128
        } else {
            0
        };
        let step = (self.interval_ns as i128 + jitter).max(1) as u128;
        self.cursor_ns += step;
        Acquire::Ready
    }

    /// Reset availability to `now_ns` so the first token after a resume fires
    /// immediately (the driver calls this when load resumes from an idle plateau).
    pub fn reanchor(&mut self, now_ns: u128) {
        self.cursor_ns = now_ns;
    }
}

// ── B2 — load-shape schedule ────────────────────────────────────────────────

/// One leg of the load shape: linearly ramp the offered rate from `from` to `to`
/// over `ramp`, then hold `to` for `dwell`. A plateau is `from == to`; the idle
/// plateau is `to == 0` (and a long enough `dwell` for the reaper to scale the
/// instance to zero).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Leg {
    pub from: f64,
    pub to: f64,
    pub ramp: Duration,
    pub dwell: Duration,
}

/// A deterministic load-shape: a sequence of [`Leg`]s the driver walks, driving
/// the pacer's `set_rate` from `rate_at(t)`. The default [`Schedule::burst`]
/// shape is the issue's `idle → 500 → 5k → 20k → idle`, repeated.
#[derive(Clone, Debug)]
pub struct Schedule {
    pub legs: Vec<Leg>,
}

impl Schedule {
    /// The default `burst` shape (issue #79): each cycle ramps idle → tier-1 →
    /// tier-2 → peak then back to idle, holding `dwell` at each active plateau and
    /// `idle_dwell` (long enough to scale to zero) at the idle plateau. The 500 /
    /// 5k tiers scale with `peak_rps` (the 20k tier), so `--peak-rps` keeps the
    /// *shape* and only moves the ceiling.
    pub fn burst(
        peak_rps: f64,
        cycles: u64,
        ramp: Duration,
        dwell: Duration,
        idle_dwell: Duration,
    ) -> Schedule {
        // Tiers as fractions of the peak (500 / 5 000 / 20 000).
        let t1 = peak_rps * (500.0 / 20_000.0);
        let t2 = peak_rps * (5_000.0 / 20_000.0);
        let mut legs = Vec::with_capacity(cycles as usize * 4);
        for _ in 0..cycles.max(1) {
            legs.push(Leg {
                from: 0.0,
                to: t1,
                ramp,
                dwell,
            });
            legs.push(Leg {
                from: t1,
                to: t2,
                ramp,
                dwell,
            });
            legs.push(Leg {
                from: t2,
                to: peak_rps,
                ramp,
                dwell,
            });
            legs.push(Leg {
                from: peak_rps,
                to: 0.0,
                ramp,
                dwell: idle_dwell,
            });
        }
        Schedule { legs }
    }

    /// Total wall-clock duration of the schedule.
    pub fn duration(&self) -> Duration {
        self.legs.iter().map(|l| l.ramp + l.dwell).sum()
    }

    /// The target rate at offset `t` from the start — piecewise linear over each
    /// leg's ramp, then flat across its dwell. `0` past the end.
    pub fn rate_at(&self, t: Duration) -> f64 {
        let mut acc = Duration::ZERO;
        for leg in &self.legs {
            let ramp_end = acc + leg.ramp;
            if t < ramp_end {
                // Inside the ramp: linearly interpolate from→to.
                let span = leg.ramp.as_secs_f64();
                let frac = if span > 0.0 {
                    (t - acc).as_secs_f64() / span
                } else {
                    1.0
                };
                return leg.from + (leg.to - leg.from) * frac.clamp(0.0, 1.0);
            }
            let dwell_end = ramp_end + leg.dwell;
            if t < dwell_end {
                return leg.to;
            }
            acc = dwell_end;
        }
        0.0
    }

    /// The `(offset, plateau_rps)` of each leg's dwell start — where the offered
    /// rate has settled at the leg's target. The driver samples the controller
    /// snapshot at these boundaries (B4).
    pub fn boundaries(&self) -> Vec<(Duration, f64)> {
        let mut acc = Duration::ZERO;
        let mut out = Vec::with_capacity(self.legs.len());
        for leg in &self.legs {
            acc += leg.ramp;
            out.push((acc, leg.to));
            acc += leg.dwell;
        }
        out
    }
}

// ── B3 + B4 — fan-out driver + report wiring ────────────────────────────────

/// Shared, lock-light state every fan-out worker touches under one pacer.
struct Shared {
    pacer: Mutex<Pacer>,
    /// The driver's wall-clock origin; due offsets are relative to it.
    origin: Instant,
    running: AtomicBool,
    /// Tokens the pacer issued (the offered demand) and requests that acked.
    offered: AtomicU64,
    acked: AtomicU64,
    /// First fatal error from any worker (stops the run), if any.
    fatal: Mutex<Option<String>>,
}

/// Per-worker tally folded into the report after the run.
struct WorkerTally {
    /// End-to-end request latency over the timed load.
    hist: Histogram,
    /// Cold-start (scaling) latencies — the per-ramp scaling-latency
    /// distribution: a request whose instance was not already `Active` paid the
    /// cold start / admission wait, so its `start` latency lands here.
    scaling: Histogram,
    conflicts: u64,
}

pub(crate) fn run_burst(opts: &Opts) -> Result<Report, BenchError> {
    // Controller-driven and in-process, exactly like scale-to-zero: a deployed
    // server owns its own controller the bench cannot pull from in-process.
    if opts.transport == Transport::Pgwire || opts.server.is_some() {
        return Err(BenchError::Config(
            "burst is controller-driven (in-process embedded); \
             drop --transport pgwire / --server (the deployed form is the spec-09 scale form)"
                .into(),
        ));
    }
    let url = opts.url.clone();
    let connections = opts.writers.max(1);

    // A reaper fast enough to scale an idle instance to zero between cycles; the
    // idle plateau dwell is sized off the same window so each cycle's burst pays a
    // real cold start (mirrors scale-to-zero's reaper sizing).
    let reap = (opts.idle / 4).max(Duration::from_millis(5));
    let cfg = ControllerConfig {
        idle_timeout: opts.idle,
        reap_interval: reap,
        max_concurrent_warms: 16,
        keep_warm: false,
    };
    let ctrl = Arc::new(
        Controller::new(cfg).map_err(|e| BenchError::Connection(format!("controller: {e}")))?,
    );

    // The idle plateau must outlast idle_timeout + a couple reaper passes so the
    // instance actually reaches Cold before the next cycle re-warms it.
    let idle_dwell = opts.idle * 3 + reap * 3 + Duration::from_millis(50);
    let schedule = Schedule::burst(
        opts.peak_rps,
        opts.cycles,
        opts.ramp,
        opts.dwell,
        idle_dwell,
    );

    // Reset the table so the post-run row count == the acked-write count exactly.
    let tag = run_tag();
    reset_table(&url)?;

    let shared = Arc::new(Shared {
        pacer: Mutex::new(Pacer::new(1_000, tag as u64)), // ±10% seeded jitter
        origin: Instant::now(),
        running: AtomicBool::new(true),
        offered: AtomicU64::new(0),
        acked: AtomicU64::new(0),
        fatal: Mutex::new(None),
    });

    let start_stats = ctrl.stats();

    // Fan out M workers under the one shared pacer (B3).
    let tallies = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..connections)
            .map(|w| {
                let shared = Arc::clone(&shared);
                let ctrl = Arc::clone(&ctrl);
                let url = url.clone();
                scope.spawn(move || worker_loop(&shared, &ctrl, &url, tag, w))
            })
            .collect();

        // The coordinator (this thread) walks the schedule in real time, driving
        // the pacer's rate and sampling the live warm-instance gauge so the report
        // can prove the worker count rose under load (scale-up). Scale-down is
        // read after the run from the controller's durable scale-to-zero counter.
        let max_warm = drive_schedule(&shared, &ctrl, &schedule);

        shared.running.store(false, Ordering::SeqCst);
        let tallies: Vec<WorkerTally> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        (tallies, max_warm)
    });
    let (tallies, max_warm) = tallies;

    // A worker hit a fatal engine/transport error → fail the run.
    if let Some(m) = shared.fatal.lock().unwrap().take() {
        return Err(BenchError::Run(m));
    }

    let elapsed = shared.origin.elapsed();
    let end_stats = ctrl.stats();
    // Scale-to-zero teardowns observed over the run — the durable, exact
    // scale-down signal (the controller bumps this on every warm→Cold reap),
    // read instead of sampling the transient Cold state during an idle plateau.
    let scale_downs = end_stats
        .scale_to_zero_events
        .saturating_sub(start_stats.scale_to_zero_events);

    // Merge the per-worker latency + scaling distributions.
    let mut hist = Histogram::new();
    let mut scaling = Histogram::new();
    let mut conflicts = 0u64;
    for t in tallies {
        hist.merge(&t.hist);
        scaling.merge(&t.scaling);
        conflicts += t.conflicts;
    }

    let offered = shared.offered.load(Ordering::SeqCst);
    let acked = shared.acked.load(Ordering::SeqCst);

    // The acked-write-loss gate: every acked INSERT must survive the
    // scale-to-zero teardowns. Final cold read counts the durable rows; a short
    // fall means a teardown lost an acked commit.
    let durable = final_row_count(&url)?;
    if durable != acked {
        return Err(BenchError::Run(format!(
            "durable rows {durable} != acked writes {acked} \
             (acked-write loss across scale-to-zero teardown)"
        )));
    }
    // The autoscaling gate: the worker count must have both risen under load and
    // fallen back to zero during the idle plateaus.
    if max_warm == 0 {
        return Err(BenchError::Run(
            "instance never warmed under burst load (no scale-up observed)".into(),
        ));
    }
    // Gated on the durable counter, not a sampled Cold transition, so a starved
    // reaper or a sampler miss on a loaded CI host can't flake a real scale-down.
    if scale_downs == 0 {
        return Err(BenchError::Run(
            "instance never scaled to zero on an idle plateau (no scale-down observed)".into(),
        ));
    }

    let lifecycle = Lifecycle {
        cold_starts: end_stats
            .cold_starts
            .saturating_sub(start_stats.cold_starts),
        warm_starts: end_stats
            .warm_starts
            .saturating_sub(start_stats.warm_starts),
        scale_to_zero: scale_downs,
        peak_workers: end_stats.peak_workers,
        compute_active_us: end_stats
            .compute_active_us
            .saturating_sub(start_stats.compute_active_us),
        compute_idle_us: end_stats
            .compute_idle_us
            .saturating_sub(start_stats.compute_idle_us),
        admission_wait_us: end_stats
            .admission_wait_us
            .saturating_sub(start_stats.admission_wait_us),
        lease_renews: end_stats
            .lease_renew_total
            .saturating_sub(start_stats.lease_renew_total),
        page_reads: 0,
        queries: acked,
    };

    let burst = Burst {
        peak_rps: opts.peak_rps,
        cycles: opts.cycles,
        connections,
        offered,
        realized: acked,
        max_warm_instances: max_warm,
        scaling,
    };

    Ok(Report {
        experiment: "burst",
        label: opts.label.clone(),
        transport: opts.transport.name(),
        url_scheme: url_scheme(&opts.url),
        writers: connections,
        duration_s: elapsed.as_secs_f64(),
        commits: acked,
        conflicts,
        failures: 0,
        throughput: acked as f64 / elapsed.as_secs_f64().max(f64::MIN_POSITIVE),
        hist,
        git_sha: git_sha(),
        json_only: opts.json,
        correctness: None,
        lifecycle: Some(lifecycle),
        soak: None,
        burst: Some(burst),
        mix_realized: None,
        archival: crate::Archival::from_opts(opts),
        stall: None,
        sweep: None,
        shard: None,
        herd: None,
    })
}

/// The burst-specific report section (B4): the offered/realized rate tracking,
/// the peak warm-instance count (the scale-up the run observed), and the
/// per-ramp scaling-latency distribution. The controller deltas (cold/warm
/// starts, peak workers, admission wait) ride the shared [`Lifecycle`] section
/// under the settled `twill_*` names.
pub(crate) struct Burst {
    pub peak_rps: f64,
    pub cycles: u64,
    /// Fan-out connection count (M) under the one pacer.
    pub connections: usize,
    /// Tokens the pacer issued (offered demand) over the run.
    pub offered: u64,
    /// Requests that acked (realized).
    pub realized: u64,
    /// Peak resident worker count sampled across the run (≥1 proves scale-up;
    /// `1` on the single-URL embedded path — the controller keys instances by
    /// URL, so a deployment with many databases is where this climbs).
    pub max_warm_instances: u64,
    /// Per-ramp scaling (cold-start) latencies; its percentiles are the scaling
    /// latency the report quotes.
    pub scaling: Histogram,
}

impl Burst {
    /// Realized / offered — how closely the system kept up with the offered
    /// schedule. `1.0` when every issued token completed; below `1.0` when the
    /// system fell behind the offered rate (the closed-loop signal).
    pub(crate) fn rate_tracking(&self) -> f64 {
        if self.offered > 0 {
            self.realized as f64 / self.offered as f64
        } else {
            0.0
        }
    }
}

/// One fan-out worker: pull tokens from the shared pacer and, for each, drive one
/// request through the controller (lease → INSERT → release) so the offered rate
/// — not a fixed work count — paces the load. Times the lease acquire as the
/// scaling latency when the instance was not already warm.
fn worker_loop(
    shared: &Shared,
    ctrl: &Controller,
    url: &str,
    tag: u128,
    worker: usize,
) -> WorkerTally {
    let mut hist = Histogram::new();
    let mut scaling = Histogram::new();
    let mut conflicts = 0u64;
    let mut seq: u64 = 0;

    // Cap on how long a worker parks before re-checking `running`, so a low-rate
    // plateau or a pause never makes shutdown sluggish.
    const MAX_PARK: Duration = Duration::from_millis(4);

    while shared.running.load(Ordering::SeqCst) {
        // Ask the shared pacer (the single rate gate) for a token at wall time.
        let now = shared.origin.elapsed().as_nanos();
        let acquired = {
            let mut p = shared.pacer.lock().unwrap();
            p.try_acquire(now)
        };
        match acquired {
            Acquire::Ready => {}
            Acquire::NotYet(wait_ns) => {
                std::thread::sleep(Duration::from_nanos(wait_ns).min(MAX_PARK));
                continue;
            }
            Acquire::Paused => {
                std::thread::sleep(MAX_PARK);
                continue;
            }
        }
        shared.offered.fetch_add(1, Ordering::SeqCst);

        // A request that finds the instance not already Active pays the cold
        // start / admission wait — its lease-acquire latency is the scaling cost.
        let cold = !matches!(ctrl.status(url), Some(LifecycleState::Active));
        let t0 = Instant::now();
        let lease = match ctrl.start(url) {
            Ok(l) => l,
            Err(e) => {
                *shared.fatal.lock().unwrap() = Some(format!("worker {worker}: cold start: {e}"));
                shared.running.store(false, Ordering::SeqCst);
                break;
            }
        };
        let start_us = t0.elapsed().as_micros() as u64;
        if cold {
            scaling.record(start_us);
        }

        // The acked write, over a connection sharing the warm instance (the
        // engine's registry dedups — no second cold start). Retry a
        // first-committer conflict exactly as a real client would.
        let key = format!("{tag}-{worker}-{seq}");
        seq += 1;
        match request(url, &key) {
            Ok(retries) => {
                conflicts += retries;
                shared.acked.fetch_add(1, Ordering::SeqCst);
                hist.record(t0.elapsed().as_micros() as u64);
            }
            Err(m) => {
                *shared.fatal.lock().unwrap() = Some(format!("worker {worker}: {m}"));
                shared.running.store(false, Ordering::SeqCst);
                drop(lease);
                break;
            }
        }
        drop(lease); // release so the instance can idle out on the idle plateau
    }

    WorkerTally {
        hist,
        scaling,
        conflicts,
    }
}

/// One request: open a connection sharing the warm instance and INSERT one keyed
/// row, retrying a first-committer conflict. Returns the retry count.
fn request(url: &str, key: &str) -> Result<u64, String> {
    let mut conn = Connection::open(url).map_err(|e| format!("open: {e}"))?;
    let mut retries = 0u64;
    loop {
        match conn.exec(&format!("INSERT INTO {TABLE} (k, v) VALUES ('{key}', 1)")) {
            Ok(()) => return Ok(retries),
            Err(e) if e.status == engine::EngineStatus::ErrConflict => {
                retries += 1;
            }
            Err(e) => return Err(format!("insert: {e}")),
        }
    }
}

/// The coordinator: walk the schedule in real time, driving the pacer's rate from
/// `rate_at(now)` and sampling the live warm-instance gauge for the peak resident
/// worker count (the scale-up evidence). Scale-*down* is deliberately not sampled
/// here — it is read after the run from the controller's durable `scale_to_zero`
/// counter, which is exact and immune to reaper-vs-sampler timing races on a
/// loaded host (sampling the transient Cold state flaked under CI contention).
fn drive_schedule(shared: &Shared, ctrl: &Controller, schedule: &Schedule) -> u64 {
    // How often the coordinator re-points the pacer at the schedule + samples the
    // gauge. Fine enough to track a ramp without busy-spinning.
    let tick = Duration::from_millis(5);
    let total = schedule.duration();
    let mut max_warm = 0u64;
    let boundaries = schedule.boundaries();
    let mut next_boundary = 0usize;
    let mut was_paused = false;

    let mut elapsed = Duration::ZERO;
    while elapsed < total {
        let rate = schedule.rate_at(elapsed);
        {
            let mut p = shared.pacer.lock().unwrap();
            let resuming = was_paused && rate > 0.0;
            p.set_rate(rate);
            if resuming {
                // Fire the first post-idle token immediately as load resumes.
                p.reanchor(shared.origin.elapsed().as_nanos());
            }
        }
        was_paused = rate <= 0.0;

        // Sample the live worker-count gauge (the scale-up signal).
        let warm = ctrl.stats().warm_instances;
        max_warm = max_warm.max(warm);

        // Cross plateau boundaries: a sample point for a deployed run's snapshot
        // deltas (the in-process run folds the whole-run delta into the report).
        while next_boundary < boundaries.len() && elapsed >= boundaries[next_boundary].0 {
            next_boundary += 1;
        }

        std::thread::sleep(tick);
        elapsed = shared.origin.elapsed();
    }

    max_warm
}

/// Drop + recreate the burst table so the post-run row count is exactly the acked
/// writes (no residue from a prior run against the same durable database).
fn reset_table(url: &str) -> Result<(), BenchError> {
    let mut conn =
        Connection::open(url).map_err(|e| BenchError::Connection(format!("open {url}: {e}")))?;
    let _ = conn.exec(&format!("DROP TABLE {TABLE}"));
    conn.exec(&format!(
        "CREATE TABLE {TABLE} (k TEXT PRIMARY KEY, v INTEGER)"
    ))
    .map_err(|e| BenchError::Run(format!("create table: {e}")))?;
    Ok(())
}

/// Count the durable rows after the run — the acked-write-loss check's read side.
/// A fresh connection shares whatever warm instance is resident, or cold-starts
/// one (replaying the WAL), so the count reflects only durable state.
fn final_row_count(url: &str) -> Result<u64, BenchError> {
    let mut conn =
        Connection::open(url).map_err(|e| BenchError::Connection(format!("open {url}: {e}")))?;
    let rs = conn
        .query(&format!("SELECT k FROM {TABLE}"))
        .map_err(|e| BenchError::Run(format!("final read: {e}")))?;
    Ok(rs.rows.len() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// B1: the closed-loop pacer holds its target rate within tolerance under a
    /// fixed seed. Drive it on a virtual clock that jumps to each token's due time
    /// (the consumer always keeps up), count the tokens that fire in a one-second
    /// window, and assert the realized rate tracks the target — the property the
    /// closed-loop driver rests on, deterministic because the jitter is seeded.
    #[test]
    fn pacer_realized_rate_tracks_target_under_fixed_seed() {
        for &rate in &[500.0_f64, 5_000.0, 20_000.0] {
            let mut p = Pacer::new(1_000, 0xB0_07); // ±10% jitter, fixed seed
            p.set_rate(rate);
            let window_ns = 1_000_000_000u128; // 1 second
            let mut now = 0u128;
            let mut count = 0u64;
            while now < window_ns {
                match p.try_acquire(now) {
                    Acquire::Ready => count += 1,
                    Acquire::NotYet(d) => now += d as u128, // jump to the next due
                    Acquire::Paused => unreachable!("armed pacer is never paused"),
                }
            }
            let realized = count as f64; // tokens per the 1s window
            let err = (realized - rate).abs() / rate;
            assert!(
                err < 0.02,
                "rate {rate}: realized {realized:.1} off by {err:.4}"
            );
        }
    }

    /// B1: a paused pacer (rate 0 — the idle plateau) issues nothing, and the
    /// cursor is clamped so a resumed plateau never fires an unbounded flood of
    /// past-due tokens (catch-up is bounded to a single token).
    #[test]
    fn pacer_pauses_at_zero_and_bounds_catch_up() {
        let mut p = Pacer::new(0, 1);
        p.set_rate(0.0);
        assert_eq!(p.try_acquire(0), Acquire::Paused, "rate 0 issues nothing");

        // 1ms interval; the cursor sits at 0 while wall time jumps 1s ahead. The
        // clamp drops that backlog so only a token or two are immediately due, not
        // the ~1000 the elapsed gap would otherwise imply.
        p.set_rate(1_000.0);
        let far = 1_000_000_000u128; // 1s of wall time with no acquisitions
        let mut fired = 0u64;
        // Drain every immediately-due token (a flood would blow past the valve).
        while let Acquire::Ready = p.try_acquire(far) {
            fired += 1;
            if fired > 100 {
                break;
            }
        }
        assert!(
            (1..=2).contains(&fired),
            "catch-up is bounded to a token or two, not a flood (fired {fired})"
        );
    }

    /// B2: the default burst schedule expands to the expected `(t, rps)` timeline
    /// — idle at the start of a cycle, the tier rates across the dwells, and back
    /// to idle. Deterministic, so the offered shape is reproducible.
    #[test]
    fn burst_schedule_expands_to_expected_timeline() {
        let ramp = Duration::from_millis(100);
        let dwell = Duration::from_millis(200);
        let idle = Duration::from_millis(400);
        let s = Schedule::burst(20_000.0, 1, ramp, dwell, idle);

        // Four legs in one cycle: idle→500, 500→5k, 5k→20k, 20k→idle.
        assert_eq!(s.legs.len(), 4);

        // Start of the run: the first ramp begins at idle (0 rps).
        assert!(s.rate_at(Duration::ZERO).abs() < 1e-6);
        // Halfway up the first ramp (50ms): half of tier-1 (500) == 250.
        assert!((s.rate_at(Duration::from_millis(50)) - 250.0).abs() < 1.0);
        // Into the first dwell (150ms): settled at tier-1 (500).
        assert!((s.rate_at(Duration::from_millis(150)) - 500.0).abs() < 1e-6);

        // The plateau boundaries report each leg's settled tier in order.
        let b = s.boundaries();
        let rates: Vec<f64> = b.iter().map(|(_, r)| *r).collect();
        assert_eq!(rates, vec![500.0, 5_000.0, 20_000.0, 0.0]);

        // The peak dwell sits at the peak rate; the final idle dwell is 0.
        let (peak_t, _) = b[2];
        assert!((s.rate_at(peak_t + Duration::from_millis(10)) - 20_000.0).abs() < 1e-6);
        let (idle_t, _) = b[3];
        assert!(s.rate_at(idle_t + Duration::from_millis(10)).abs() < 1e-6);

        // Past the end: 0.
        assert!(s.rate_at(s.duration() + Duration::from_millis(1)).abs() < 1e-6);
    }

    /// B2: `--peak-rps` keeps the shape and only moves the ceiling — the 500 / 5k
    /// tiers scale proportionally with the peak.
    #[test]
    fn burst_schedule_scales_tiers_with_peak() {
        let s = Schedule::burst(
            2_000.0,
            1,
            Duration::from_millis(10),
            Duration::from_millis(10),
            Duration::from_millis(10),
        );
        let rates: Vec<f64> = s.boundaries().iter().map(|(_, r)| *r).collect();
        // Tiers are peak/40, peak/4, peak → 50, 500, 2000 at peak 2000.
        assert_eq!(rates, vec![50.0, 500.0, 2_000.0, 0.0]);
    }
}
