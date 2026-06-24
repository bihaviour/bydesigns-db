//! # twill-db · Lifecycle Controller (spec 06)
//!
//! Scale-to-zero for engine instances: cold-start on the first connection, tear
//! down when idle, and survive a burst of simultaneous cold starts. The
//! controller owns no durable state — every byte lives in object storage behind
//! the [`Storage`](twill_storage) seam — so stopping an instance loses
//! nothing and the next connection cold-starts it again.
//!
//! It composes the engine's existing primitives rather than reimplementing them:
//! a warm instance is an [`twill_engine::Database`] (opening one acquires the
//! writer fence and replays the WAL — that *is* the cache warm); stopping drops
//! it (the engine's `Drop` releases the fence). On top of that the controller
//! adds the state machine, an idle-timeout reaper, a lease heartbeat, and
//! thundering-herd handling (dedup + warm admission).
//!
//! ## State machine (#21)
//!
//! ```text
//! Cold ──start──▶ Warming ──opened──▶ Active ──idle──▶ Idle ──timeout──▶ Stopping ──▶ Cold
//!   ▲                │ (open fails)        ▲   │                                         │
//!   └────────────────┘                     └───┘ (new connection re-activates)          │
//!   └──────────────────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! ## Thundering herd (#24)
//!
//! N concurrent [`Controller::start`] calls for one cold database trigger exactly
//! one warm; the rest wait on the in-flight transition. A bounded warm-admission
//! semaphore caps how many *distinct* databases warm at once, and `keep_warm`
//! holds idle instances resident to cut post-idle latency.

use engine::{Database, EngineError};
use std::collections::HashMap;
use std::ops::Deref;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Lifecycle phase of a single engine instance (spec 06 §Lifecycle states).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LifecycleState {
    /// Stopped; only object-storage bytes bill at rest.
    Cold,
    /// Cold-starting: process/handle open + cache warm in progress.
    Warming,
    /// Serving connections.
    Active,
    /// Warm but with no active connections (eligible for teardown).
    Idle,
    /// Tearing down (drains, releases the fence).
    Stopping,
}

/// Controller tunables (spec 06 §Configuration).
#[derive(Clone, Debug)]
pub struct ControllerConfig {
    /// How long an instance may sit idle (no active leases) before teardown.
    pub idle_timeout: Duration,
    /// How often the reaper runs (idle checks + lease heartbeat).
    pub reap_interval: Duration,
    /// Max number of *distinct* databases that may warm concurrently (admission
    /// control for a thundering herd across many cold databases).
    pub max_concurrent_warms: usize,
    /// Keep idle instances resident instead of tearing them down (warm pool).
    pub keep_warm: bool,
}

impl Default for ControllerConfig {
    fn default() -> ControllerConfig {
        ControllerConfig {
            idle_timeout: Duration::from_secs(30),
            reap_interval: Duration::from_secs(1),
            max_concurrent_warms: 16,
            keep_warm: false,
        }
    }
}

/// A counting semaphore (no external deps; matches the project's zero-dep stance).
struct Semaphore {
    permits: Mutex<usize>,
    cv: Condvar,
}

impl Semaphore {
    fn new(n: usize) -> Semaphore {
        Semaphore {
            permits: Mutex::new(n),
            cv: Condvar::new(),
        }
    }
    fn acquire(&self) {
        let mut p = self.permits.lock().unwrap();
        while *p == 0 {
            p = self.cv.wait(p).unwrap();
        }
        *p -= 1;
    }
    fn release(&self) {
        let mut p = self.permits.lock().unwrap();
        *p += 1;
        self.cv.notify_one();
    }
}

struct InstState {
    phase: LifecycleState,
    handle: Option<Arc<Database>>,
    /// Outstanding leases (active connections). Teardown only when this is 0.
    active: u64,
    last_activity: Instant,
    /// When the instance entered its current `phase` — drives the compute
    /// active/idle accounting (the serverless-efficiency signal): the time
    /// spent in a phase is accrued when the instance leaves it.
    phase_since: Instant,
}

struct Instance {
    url: String,
    st: Mutex<InstState>,
    /// Wakes `start` waiters when a Warming/Stopping transition settles.
    cv: Condvar,
}

struct ControllerInner {
    cfg: ControllerConfig,
    instances: Mutex<HashMap<String, Arc<Instance>>>,
    warm_sem: Semaphore,
    /// Total Cold→warm transitions (a herd of N produces exactly 1 per key).
    warm_count: AtomicU64,
    cur_warms: AtomicU64,
    peak_warms: AtomicU64,
    /// Reuses of an already-warm instance (a `start` that hit Active/Idle).
    warm_starts: AtomicU64,
    /// Warm→Cold teardowns (idle reaper or explicit `stop`) — scale-to-zero.
    scale_to_zero: AtomicU64,
    /// Cumulative microseconds instances spent Active (serving leases).
    compute_active_us: AtomicU64,
    /// Cumulative microseconds instances spent Idle (warm, no leases).
    compute_idle_us: AtomicU64,
    /// Cumulative microseconds `warm` spent blocked on the warm-admission
    /// semaphore — the thundering-herd scheduling/admission wait.
    admission_wait_us: AtomicU64,
    /// Successful lease heartbeats by the reaper (one per warm instance per pass).
    lease_renews: AtomicU64,
    stop: Mutex<bool>,
    stop_cv: Condvar,
}

impl ControllerInner {
    fn get_or_create(&self, url: &str) -> Arc<Instance> {
        let mut m = self.instances.lock().unwrap();
        m.entry(url.to_string())
            .or_insert_with(|| {
                Arc::new(Instance {
                    url: url.to_string(),
                    st: Mutex::new(InstState {
                        phase: LifecycleState::Cold,
                        handle: None,
                        active: 0,
                        last_activity: Instant::now(),
                        phase_since: Instant::now(),
                    }),
                    cv: Condvar::new(),
                })
            })
            .clone()
    }

    /// Move `s` into `new`, accruing the time spent in the phase being left
    /// into the compute active/idle totals (the serverless-efficiency signal).
    /// Cumulative, so a consumer takes the delta between two `stats()` pulls.
    fn enter_phase(&self, s: &mut InstState, new: LifecycleState) {
        let elapsed = s.phase_since.elapsed().as_micros() as u64;
        match s.phase {
            LifecycleState::Active => {
                self.compute_active_us.fetch_add(elapsed, Ordering::SeqCst);
            }
            LifecycleState::Idle => {
                self.compute_idle_us.fetch_add(elapsed, Ordering::SeqCst);
            }
            _ => {}
        }
        s.phase = new;
        s.phase_since = Instant::now();
    }

    /// Open the database under warm-admission control, tracking the gauges.
    fn warm(&self, url: &str) -> Result<Arc<Database>, EngineError> {
        let wait0 = Instant::now();
        self.warm_sem.acquire();
        self.admission_wait_us
            .fetch_add(wait0.elapsed().as_micros() as u64, Ordering::SeqCst);
        let cur = self.cur_warms.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak_warms.fetch_max(cur, Ordering::SeqCst);
        let res = Database::open(url);
        self.cur_warms.fetch_sub(1, Ordering::SeqCst);
        self.warm_sem.release();
        if res.is_ok() {
            self.warm_count.fetch_add(1, Ordering::SeqCst);
        }
        res
    }

    /// One reaper pass: heartbeat live instances, idle out and stop the rest.
    fn reap_once(&self) {
        let insts: Vec<Arc<Instance>> = self.instances.lock().unwrap().values().cloned().collect();
        for inst in insts {
            let (phase, handle) = {
                let s = inst.st.lock().unwrap();
                (s.phase, s.handle.clone())
            };
            let Some(db) = handle else { continue };

            // Heartbeat the writer lease for any warm instance (Active or Idle).
            let warm = matches!(phase, LifecycleState::Active | LifecycleState::Idle);
            let fenced = if warm {
                match db.renew_lease() {
                    Ok(()) => {
                        self.lease_renews.fetch_add(1, Ordering::SeqCst);
                        false
                    }
                    Err(_) => true,
                }
            } else {
                false
            };

            let mut s = inst.st.lock().unwrap();
            if fenced {
                s.handle = None;
                self.enter_phase(&mut s, LifecycleState::Cold);
                s.last_activity = Instant::now();
                inst.cv.notify_all();
                continue;
            }
            if s.active == 0 {
                if s.phase == LifecycleState::Active {
                    self.enter_phase(&mut s, LifecycleState::Idle);
                }
                let idle_for = s.last_activity.elapsed();
                if s.phase == LifecycleState::Idle
                    && idle_for >= self.cfg.idle_timeout
                    && !self.cfg.keep_warm
                {
                    self.enter_phase(&mut s, LifecycleState::Stopping); // accrue the Idle time
                    s.handle = None; // dropped at end of scope → engine releases the fence
                    self.enter_phase(&mut s, LifecycleState::Cold);
                    s.last_activity = Instant::now();
                    self.scale_to_zero.fetch_add(1, Ordering::SeqCst);
                    inst.cv.notify_all();
                }
            } else if s.phase == LifecycleState::Idle {
                self.enter_phase(&mut s, LifecycleState::Active);
            }
        }
    }
}

/// A handle to a warm engine instance. Holds the instance active for as long as
/// it lives; dropping it releases the connection so the instance can idle out.
pub struct Lease {
    inst: Arc<Instance>,
    db: Arc<Database>,
}

impl Lease {
    /// The warm database. Also reachable via `Deref`.
    pub fn database(&self) -> &Database {
        &self.db
    }
    /// A shared handle to the warm database.
    pub fn handle(&self) -> Arc<Database> {
        self.db.clone()
    }
}

impl Deref for Lease {
    type Target = Database;
    fn deref(&self) -> &Database {
        &self.db
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        let mut s = self.inst.st.lock().unwrap();
        s.active = s.active.saturating_sub(1);
        s.last_activity = Instant::now();
        self.inst.cv.notify_all();
    }
}

/// The lifecycle controller. Scale-to-zero engine instances keyed by URL.
pub struct Controller {
    inner: Arc<ControllerInner>,
    reaper: Option<JoinHandle<()>>,
}

impl Controller {
    /// Build a controller and start its background reaper.
    pub fn new(cfg: ControllerConfig) -> Result<Controller, EngineError> {
        if cfg.max_concurrent_warms == 0 {
            return Err(EngineError::misuse(
                "max_concurrent_warms must be at least 1",
            ));
        }
        if cfg.reap_interval.is_zero() || cfg.idle_timeout.is_zero() {
            return Err(EngineError::misuse(
                "reap_interval and idle_timeout must be non-zero",
            ));
        }
        let inner = Arc::new(ControllerInner {
            warm_sem: Semaphore::new(cfg.max_concurrent_warms),
            instances: Mutex::new(HashMap::new()),
            warm_count: AtomicU64::new(0),
            cur_warms: AtomicU64::new(0),
            peak_warms: AtomicU64::new(0),
            warm_starts: AtomicU64::new(0),
            scale_to_zero: AtomicU64::new(0),
            compute_active_us: AtomicU64::new(0),
            compute_idle_us: AtomicU64::new(0),
            admission_wait_us: AtomicU64::new(0),
            lease_renews: AtomicU64::new(0),
            stop: Mutex::new(false),
            stop_cv: Condvar::new(),
            cfg,
        });
        let reaper_inner = Arc::clone(&inner);
        let reaper = std::thread::spawn(move || run_reaper(reaper_inner));
        Ok(Controller {
            inner,
            reaper: Some(reaper),
        })
    }

    /// Get a lease on the engine for `url`, cold-starting it if necessary. A
    /// burst of concurrent calls for one cold URL triggers exactly one warm; the
    /// rest wait for it. The lease keeps the instance Active until dropped.
    pub fn start(&self, url: &str) -> Result<Lease, EngineError> {
        let inst = self.inner.get_or_create(url);
        loop {
            let mut s = inst.st.lock().unwrap();
            match s.phase {
                LifecycleState::Active | LifecycleState::Idle => {
                    self.inner.enter_phase(&mut s, LifecycleState::Active);
                    s.active += 1;
                    s.last_activity = Instant::now();
                    let db = s.handle.clone().expect("active instance has a handle");
                    self.inner.warm_starts.fetch_add(1, Ordering::SeqCst);
                    return Ok(Lease {
                        inst: inst.clone(),
                        db,
                    });
                }
                LifecycleState::Cold => {
                    // Win the right to warm; everyone else will see Warming.
                    self.inner.enter_phase(&mut s, LifecycleState::Warming);
                    drop(s);
                    let res = self.inner.warm(&inst.url);
                    let mut s = inst.st.lock().unwrap();
                    match res {
                        Ok(db) => {
                            self.inner.enter_phase(&mut s, LifecycleState::Active);
                            s.handle = Some(db.clone());
                            s.active += 1;
                            s.last_activity = Instant::now();
                            inst.cv.notify_all();
                            return Ok(Lease {
                                inst: inst.clone(),
                                db,
                            });
                        }
                        Err(e) => {
                            // A failed warm returns cleanly to Cold (no durable effect).
                            self.inner.enter_phase(&mut s, LifecycleState::Cold);
                            inst.cv.notify_all();
                            return Err(e);
                        }
                    }
                }
                LifecycleState::Warming | LifecycleState::Stopping => {
                    // Another transition is in flight; wait on it, then re-loop.
                    // The guard drops at the end of this arm, before we re-lock.
                    let _guard = inst.cv.wait(s).unwrap();
                }
            }
        }
    }

    /// Current lifecycle state of `url`, or `None` if never started.
    pub fn status(&self, url: &str) -> Option<LifecycleState> {
        self.inner
            .instances
            .lock()
            .unwrap()
            .get(url)
            .map(|i| i.st.lock().unwrap().phase)
    }

    /// Force an idle instance to stop now (no-op if it has active leases).
    pub fn stop(&self, url: &str) {
        let inst = self.inner.instances.lock().unwrap().get(url).cloned();
        if let Some(inst) = inst {
            let mut s = inst.st.lock().unwrap();
            if s.active == 0 {
                // Only a warm→Cold transition is a scale-to-zero event; stopping
                // an already-cold instance is a no-op for the counter.
                if s.handle.is_some() {
                    self.inner.scale_to_zero.fetch_add(1, Ordering::SeqCst);
                }
                s.handle = None;
                self.inner.enter_phase(&mut s, LifecycleState::Cold);
                s.last_activity = Instant::now();
                inst.cv.notify_all();
            }
        }
    }

    /// Total Cold→warm transitions observed (one per key per cold start).
    pub fn warm_count(&self) -> u64 {
        self.inner.warm_count.load(Ordering::SeqCst)
    }

    /// Peak number of databases warming simultaneously (≤ max_concurrent_warms).
    pub fn peak_concurrent_warms(&self) -> u64 {
        self.inner.peak_warms.load(Ordering::SeqCst)
    }

    /// A read-only [`ControllerStats`] snapshot — the compute/scheduler tier of
    /// the #53 observability surface (spec 15). Pulled by Twill Bench's
    /// lifecycle scenarios and a future OTLP exporter; the lifecycle scenarios'
    /// active/idle-seconds and admission-wait histograms layer on top in a later
    /// step. Cumulative counters plus the live `warm_instances` gauge.
    pub fn stats(&self) -> ControllerStats {
        // One pass over the instance table: count resident instances and fold in
        // the time the currently-warm ones have spent in their live phase so far
        // (the settled totals only accrue when an instance *leaves* a phase), so
        // the active/idle read is accurate as-of the call even mid-cycle.
        let mut warm_instances = 0u64;
        let mut active_us = self.inner.compute_active_us.load(Ordering::SeqCst);
        let mut idle_us = self.inner.compute_idle_us.load(Ordering::SeqCst);
        for inst in self.inner.instances.lock().unwrap().values() {
            let s = inst.st.lock().unwrap();
            let live = s.phase_since.elapsed().as_micros() as u64;
            match s.phase {
                LifecycleState::Active => {
                    warm_instances += 1;
                    active_us += live;
                }
                LifecycleState::Idle => {
                    warm_instances += 1;
                    idle_us += live;
                }
                _ => {}
            }
        }
        ControllerStats {
            cold_starts: self.inner.warm_count.load(Ordering::SeqCst),
            warm_starts: self.inner.warm_starts.load(Ordering::SeqCst),
            scale_to_zero_events: self.inner.scale_to_zero.load(Ordering::SeqCst),
            peak_workers: self.inner.peak_warms.load(Ordering::SeqCst),
            warm_instances,
            compute_active_us: active_us,
            compute_idle_us: idle_us,
            admission_wait_us: self.inner.admission_wait_us.load(Ordering::SeqCst),
            lease_renew_total: self.inner.lease_renews.load(Ordering::SeqCst),
        }
    }
}

/// Read-only controller observability snapshot (#53 / spec 15). Cumulative
/// counters plus a live gauge; a consumer takes the delta between two pulls.
/// The compute active/idle durations, admission-wait, and lease-renew totals
/// named in the settled metric vocabulary land here with the scale-to-zero
/// scenario — the serverless-efficiency report is pure arithmetic over them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ControllerStats {
    /// Cold→warm transitions (cold starts); one per key per cold start.
    pub cold_starts: u64,
    /// Reuses of an already-warm instance (a `start` that hit Active/Idle) —
    /// `warm_starts / (cold_starts + warm_starts)` is the worker-reuse ratio.
    pub warm_starts: u64,
    /// Warm→Cold teardowns (idle reaper or explicit `stop`).
    pub scale_to_zero_events: u64,
    /// Peak number of databases warming simultaneously.
    pub peak_workers: u64,
    /// Databases currently resident (Active or Idle) — a gauge.
    pub warm_instances: u64,
    /// Cumulative microseconds instances spent serving (Active) — the numerator
    /// of `utilization` and `compute_seconds_per_query`. Includes the in-flight
    /// active time of any currently-resident instance as-of the pull.
    pub compute_active_us: u64,
    /// Cumulative microseconds instances spent warm-but-idle (Idle). With
    /// `compute_active_us`, `active / (active + idle)` is utilization.
    pub compute_idle_us: u64,
    /// Cumulative microseconds `start` blocked on warm admission (the
    /// thundering-herd queue) — the scheduler/admission latency segment.
    pub admission_wait_us: u64,
    /// Successful single-writer lease heartbeats by the reaper.
    pub lease_renew_total: u64,
}

impl Drop for Controller {
    fn drop(&mut self) {
        {
            let mut stop = self.inner.stop.lock().unwrap();
            *stop = true;
            self.inner.stop_cv.notify_all();
        }
        if let Some(h) = self.reaper.take() {
            let _ = h.join();
        }
        // Drop all warm handles so the engine releases their fences.
        self.inner.instances.lock().unwrap().clear();
    }
}

fn run_reaper(inner: Arc<ControllerInner>) {
    loop {
        {
            let stop = inner.stop.lock().unwrap();
            if *stop {
                return;
            }
            let (stop, _) = inner
                .stop_cv
                .wait_timeout(stop, inner.cfg.reap_interval)
                .unwrap();
            if *stop {
                return;
            }
        }
        inner.reap_once();
    }
}

#[cfg(test)]
mod tests;
