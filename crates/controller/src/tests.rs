//! Lifecycle controller tests (#21 state machine / idle teardown, #24 herd).

use super::*;
use engine::Connection;
use std::sync::atomic::AtomicU64;
use std::sync::Barrier;

fn unique_url(tag: &str) -> String {
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let mut p = std::env::temp_dir();
    p.push(format!("twill-ctl-{tag}-{}-{n}.db", std::process::id()));
    let _ = std::fs::remove_file(&p);
    format!("file://{}", p.display())
}

fn fast_cfg() -> ControllerConfig {
    ControllerConfig {
        idle_timeout: Duration::from_millis(80),
        reap_interval: Duration::from_millis(20),
        max_concurrent_warms: 4,
        keep_warm: false,
    }
}

#[test]
fn rejects_invalid_config() {
    assert!(Controller::new(ControllerConfig {
        max_concurrent_warms: 0,
        ..ControllerConfig::default()
    })
    .is_err());
    assert!(Controller::new(ControllerConfig {
        idle_timeout: Duration::ZERO,
        ..ControllerConfig::default()
    })
    .is_err());
}

#[test]
fn cold_to_active_to_idle_to_cold_and_restart() {
    let ctrl = Controller::new(fast_cfg()).unwrap();
    let url = unique_url("cycle");
    assert_eq!(ctrl.status(&url), None, "unknown until first start");

    {
        let _lease = ctrl.start(&url).unwrap();
        assert_eq!(
            ctrl.status(&url),
            Some(LifecycleState::Active),
            "first connection cold-starts to Active"
        );
    } // lease dropped: instance has no active connections

    // The reaper idles it out and tears it down once idle_timeout elapses.
    std::thread::sleep(Duration::from_millis(400));
    assert_eq!(
        ctrl.status(&url),
        Some(LifecycleState::Cold),
        "idle instance scales to zero"
    );

    // A stopped engine restarts on the next connection (#21 acceptance).
    let _lease = ctrl.start(&url).unwrap();
    assert_eq!(ctrl.status(&url), Some(LifecycleState::Active));
}

#[test]
fn keep_warm_holds_an_idle_instance_resident() {
    let cfg = ControllerConfig {
        keep_warm: true,
        ..fast_cfg()
    };
    let ctrl = Controller::new(cfg).unwrap();
    let url = unique_url("keepwarm");
    {
        let _lease = ctrl.start(&url).unwrap();
    }
    std::thread::sleep(Duration::from_millis(300));
    // It idles but is never torn down — post-idle latency stays low.
    assert_ne!(
        ctrl.status(&url),
        Some(LifecycleState::Cold),
        "keep_warm prevents scale-to-zero"
    );
}

#[test]
fn stop_start_preserves_durable_state() {
    // Use a long idle timeout so only our explicit stop tears the instance down.
    let cfg = ControllerConfig {
        idle_timeout: Duration::from_secs(30),
        reap_interval: Duration::from_millis(20),
        ..ControllerConfig::default()
    };
    let ctrl = Controller::new(cfg).unwrap();
    let url = unique_url("durable");

    {
        let _lease = ctrl.start(&url).unwrap();
        // A connection shares the warmed engine via the engine registry.
        let mut c = Connection::open(&url).unwrap();
        c.exec("CREATE TABLE t (id INTEGER PRIMARY KEY)").unwrap();
        c.exec("INSERT INTO t VALUES (1)").unwrap();
        c.exec("INSERT INTO t VALUES (2)").unwrap();
    } // drop the connection and the lease → no strong refs to the engine

    ctrl.stop(&url);
    assert_eq!(ctrl.status(&url), Some(LifecycleState::Cold));

    // Restart cold and read back: nothing was lost (all state is in storage).
    let _lease = ctrl.start(&url).unwrap();
    let mut c = Connection::open(&url).unwrap();
    let rs = c.query("SELECT id FROM t").unwrap();
    assert_eq!(rs.rows.len(), 2, "committed rows survive stop/start");
}

#[test]
fn thundering_herd_of_cold_starts_warms_exactly_once() {
    // Long idle timeout so nothing reaps mid-test.
    let cfg = ControllerConfig {
        idle_timeout: Duration::from_secs(30),
        reap_interval: Duration::from_millis(50),
        max_concurrent_warms: 8,
        keep_warm: false,
    };
    let ctrl = Arc::new(Controller::new(cfg).unwrap());
    let url = unique_url("herd");

    const N: usize = 16;
    let barrier = Arc::new(Barrier::new(N));
    let mut handles = Vec::new();
    for _ in 0..N {
        let c = ctrl.clone();
        let u = url.clone();
        let b = barrier.clone();
        handles.push(std::thread::spawn(move || {
            b.wait(); // release all threads together → maximal contention
            c.start(&u).unwrap()
        }));
    }
    let leases: Vec<Lease> = handles.into_iter().map(|h| h.join().unwrap()).collect();

    assert_eq!(
        ctrl.warm_count(),
        1,
        "a burst of {N} cold starts triggers exactly one warm"
    );
    // Every connection landed on the same warm engine instance.
    let first = leases[0].handle();
    for l in &leases[1..] {
        assert!(
            Arc::ptr_eq(&first, &l.handle()),
            "all share one warm engine"
        );
    }
    assert!(
        ctrl.peak_concurrent_warms() <= 8,
        "warm admission never exceeds the configured bound"
    );
}

#[test]
fn stats_count_cold_starts_warm_starts_and_scale_to_zero() {
    // `Controller::stats()` is the compute/scheduler tier of the #53
    // observability surface: cold starts, warm reuses, and scale-to-zero
    // teardowns, pulled (not scraped, not pushed) at scenario boundaries.
    let ctrl = Controller::new(fast_cfg()).unwrap();
    let url = unique_url("stats");

    let z = ctrl.stats();
    assert_eq!(
        z,
        ControllerStats::default(),
        "fresh controller is all zeros"
    );

    // First start cold-starts; a second concurrent-with-warm start reuses it.
    let lease = ctrl.start(&url).unwrap();
    let lease2 = ctrl.start(&url).unwrap(); // hits Active → a warm start
    let s = ctrl.stats();
    assert_eq!(s.cold_starts, 1, "one cold start");
    assert_eq!(s.warm_starts, 1, "second start reused the warm instance");
    assert_eq!(s.warm_instances, 1, "one resident instance");
    assert_eq!(s.scale_to_zero_events, 0, "still warm");

    // Drop both leases and let the idle reaper tear it down → a scale-to-zero.
    drop(lease);
    drop(lease2);
    std::thread::sleep(Duration::from_millis(400));
    assert_eq!(
        ctrl.status(&url),
        Some(LifecycleState::Cold),
        "scaled to zero"
    );
    let s = ctrl.stats();
    assert_eq!(s.scale_to_zero_events, 1, "one teardown counted");
    assert_eq!(s.warm_instances, 0, "no resident instances after teardown");

    // A restart cold-starts again; the cumulative counter advances.
    let _lease = ctrl.start(&url).unwrap();
    assert_eq!(ctrl.stats().cold_starts, 2, "restart is a fresh cold start");
}

#[test]
fn explicit_stop_counts_as_scale_to_zero() {
    let cfg = ControllerConfig {
        keep_warm: true, // the reaper won't tear it down; stop() must
        ..fast_cfg()
    };
    let ctrl = Controller::new(cfg).unwrap();
    let url = unique_url("stop-stats");

    {
        let _lease = ctrl.start(&url).unwrap();
    } // idle but kept warm
    std::thread::sleep(Duration::from_millis(120));
    assert_eq!(
        ctrl.stats().scale_to_zero_events,
        0,
        "kept warm, no teardown"
    );

    ctrl.stop(&url);
    assert_eq!(ctrl.status(&url), Some(LifecycleState::Cold));
    assert_eq!(
        ctrl.stats().scale_to_zero_events,
        1,
        "an explicit stop of a warm instance is a scale-to-zero"
    );
    // Stopping an already-cold instance is a no-op for the counter.
    ctrl.stop(&url);
    assert_eq!(ctrl.stats().scale_to_zero_events, 1, "no double count");
}
