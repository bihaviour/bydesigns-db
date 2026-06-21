//! Experiment 4 — crash safety of the commit path, the hard durability gate
//! (spec 09 §Experiment 4; spec 14 R-02). This is the validation that runs *here*
//! (offline, deterministic) before any tool stores real data: it does not produce
//! a latency number, it produces a pass/fail.
//!
//! Method (spec 09): a deterministic, seeded [`FaultObjectStore`] fires at a
//! chosen CAS-append (`put_if_absent`) sequence number, the harness drives a
//! commit storm until the injected fault stops it (== the `kill -9`), then the
//! *same durable store* is reopened (== restart) and a recovery oracle asserts:
//!
//!   * **no acked-write loss** — every commit the client saw acked is recoverable
//!     (`durable_lsn` and `get_commit_lsn` are both >= the max acked LSN);
//!   * **no torn / half state** — each recovered record is byte-for-byte intact
//!     and the recovered LSNs are strictly increasing and gap-free.
//!
//! Both adversarial windows from spec 09 are attacked across both object backends
//! (in-memory and the durable filesystem floor), over a sweep of seeds so a
//! failing schedule is reproducible from its seed.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use twill_storage::{
    block_on, FaultKind, FaultMode, FaultObjectStore, FaultPlan, FsObjectStore, Lsn,
    MemObjectStore, ObjectConfig, ObjectStorage, ObjectStore, Storage, WalRecord, WriterId,
};

const PREFIX: &str = "db/crash/";

fn unique_root(tag: &str) -> std::path::PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let mut p = std::env::temp_dir();
    p.push(format!("twill-crash-{tag}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    p
}

/// One seeded crash + recovery cycle, asserting the durability oracle.
///
/// `open_crash` builds a fresh `Storage` over a durable underlying store together
/// with the `FaultObjectStore` wrapping it; `open_recover` reopens the *same*
/// durable store with no fault (the restart). The fault is armed only after the
/// fence is acquired, so `fire_at` counts commit-segment CAS appends exactly.
fn assert_crash_safe(
    label: &str,
    commits: usize,
    fire_at: u64,
    mode: FaultMode,
    open_crash: impl Fn() -> (ObjectStorage, Arc<FaultObjectStore>),
    open_recover: impl Fn() -> ObjectStorage,
) {
    // ---- Phase 1: the crash run -------------------------------------------
    let acked: Vec<(usize, u64)> = {
        let (s, fault) = open_crash();
        let tok = block_on(s.acquire_fence(WriterId(1))).expect("acquire fence");
        // Arm after setup writes so put-if-absent #1 is the first commit segment.
        fault.arm(FaultPlan {
            kind: FaultKind::PutIfAbsent,
            fire_at,
            mode,
        });

        let mut acked = Vec::new();
        for i in 0..commits {
            let payload = format!("commit-{i}");
            match block_on(s.append_wal(&tok, &[WalRecord::new(payload.into_bytes())])) {
                Ok(lsn) => acked.push((i, lsn.0)),
                // The injected fault IS the crash: stop issuing immediately.
                Err(_) => break,
            }
        }
        acked
        // `s` (and the fault-wrapped client) drop here == process death: all
        // in-memory commit/durable/memtable state is gone. Only the underlying
        // durable store survives into Phase 2.
    };

    // ---- Phase 2: recovery oracle over the same durable store --------------
    let s = open_recover();
    let durable = block_on(s.durable_lsn()).expect("durable_lsn").0;
    let commit_lsn = block_on(s.get_commit_lsn()).expect("get_commit_lsn").0;
    let entries = block_on(s.scan_wal(Lsn(0))).expect("scan_wal");

    let max_acked = acked.iter().map(|(_, l)| *l).max().unwrap_or(0);

    // No acked-write loss: the durable + committed marks cover every ack.
    assert!(
        durable >= max_acked,
        "{label} (fire_at={fire_at}, {mode:?}): acked-write loss — durable_lsn {durable} < max acked {max_acked}",
    );
    assert!(
        commit_lsn >= max_acked,
        "{label} (fire_at={fire_at}, {mode:?}): commit LSN {commit_lsn} < max acked {max_acked}",
    );

    // No torn / half state: each acked record is recoverable, byte-intact.
    let by_lsn: HashMap<u64, &[u8]> = entries
        .iter()
        .map(|e| (e.lsn.0, e.record.bytes.as_slice()))
        .collect();
    for (i, lsn) in &acked {
        let got = by_lsn.get(lsn).unwrap_or_else(|| {
            panic!("{label} (fire_at={fire_at}, {mode:?}): acked commit {i} (lsn {lsn}) missing after recovery")
        });
        let want = format!("commit-{i}");
        assert_eq!(
            *got,
            want.as_bytes(),
            "{label} (fire_at={fire_at}, {mode:?}): torn/half record recovered at lsn {lsn}",
        );
    }

    // Recovery is deterministic and ordered: strictly increasing, gap-free LSNs.
    let mut prev = 0u64;
    for e in &entries {
        assert!(
            e.lsn.0 == prev + 1,
            "{label} (fire_at={fire_at}, {mode:?}): non-monotone/gappy recovery LSN {} after {prev}",
            e.lsn.0,
        );
        prev = e.lsn.0;
    }
}

// ---- in-memory backend: a wide seed sweep over both adversarial modes -------

#[test]
fn exp4_crash_storm_mem() {
    const COMMITS: usize = 24;
    for seed in 1..=240u64 {
        // A shared in-memory store persists objects across the crash/restart pair.
        let store: Arc<MemObjectStore> = Arc::new(MemObjectStore::new());
        let fire_at = (seed % COMMITS as u64) + 1; // 1..=COMMITS
        let mode = if seed % 2 == 0 {
            FaultMode::AfterOp // (a) durable CAS-append, no ack
        } else {
            FaultMode::BeforeOp // never reached durability
        };

        let open_crash = {
            let store = store.clone();
            move || {
                let fault = FaultObjectStore::new(store.clone());
                let s = ObjectStorage::with_store(
                    fault.clone() as Arc<dyn ObjectStore>,
                    PREFIX,
                    ObjectConfig::default(),
                )
                .expect("open crash ObjectStorage");
                (s, fault)
            }
        };
        let open_recover = {
            let store = store.clone();
            move || {
                ObjectStorage::with_store(store.clone(), PREFIX, ObjectConfig::default())
                    .expect("reopen ObjectStorage")
            }
        };
        assert_crash_safe("mem", COMMITS, fire_at, mode, open_crash, open_recover);
    }
}

// ---- durable filesystem floor: the same sweep against real recovery ---------

#[test]
fn exp4_crash_storm_fs() {
    const COMMITS: usize = 16;
    for seed in 1..=80u64 {
        // A fresh durable root per seed: reopening it is a genuine restart that
        // recovers purely from the objects on disk.
        let root = unique_root("storm");
        let fire_at = (seed % COMMITS as u64) + 1;
        let mode = if seed % 2 == 0 {
            FaultMode::AfterOp
        } else {
            FaultMode::BeforeOp
        };

        let open_crash = {
            let root = root.clone();
            move || {
                let inner = Arc::new(FsObjectStore::open(&root).expect("open fs store"));
                let fault = FaultObjectStore::new(inner);
                let s = ObjectStorage::with_store(
                    fault.clone() as Arc<dyn ObjectStore>,
                    PREFIX,
                    ObjectConfig::default(),
                )
                .expect("open crash ObjectStorage");
                (s, fault)
            }
        };
        let open_recover = {
            let root = root.clone();
            move || {
                let inner = Arc::new(FsObjectStore::open(&root).expect("reopen fs store"));
                ObjectStorage::with_store(inner, PREFIX, ObjectConfig::default())
                    .expect("reopen ObjectStorage")
            }
        };
        assert_crash_safe("fs", COMMITS, fire_at, mode, open_crash, open_recover);
        let _ = std::fs::remove_dir_all(&root);
    }
}

// ---- the two named adversarial windows from spec 09, called out explicitly ---

#[test]
fn exp4a_durable_cas_append_without_ack_is_recoverable_not_torn() {
    // (a) Crash after the CAS-append is durable but before the client ack: the
    // unacked commit MAY appear on recovery, but the prior acked commits MUST all
    // survive intact and no torn state may exist.
    let store: Arc<MemObjectStore> = Arc::new(MemObjectStore::new());
    let open_crash = {
        let store = store.clone();
        move || {
            let fault = FaultObjectStore::new(store.clone());
            let s = ObjectStorage::with_store(
                fault.clone() as Arc<dyn ObjectStore>,
                PREFIX,
                ObjectConfig::default(),
            )
            .unwrap();
            (s, fault)
        }
    };
    let open_recover = {
        let store = store.clone();
        move || ObjectStorage::with_store(store.clone(), PREFIX, ObjectConfig::default()).unwrap()
    };
    assert_crash_safe("exp4a", 8, 5, FaultMode::AfterOp, open_crash, open_recover);
}

#[test]
fn exp4b_acked_commit_survives_crash_before_flush() {
    // (b) Clean acks (no fault), then "crash" by dropping the handle before any
    // flush — the pages live only in the durable log. Reopening must replay every
    // acked commit from the log alone (page materialization is derivable, not a
    // durability boundary).
    let store: Arc<MemObjectStore> = Arc::new(MemObjectStore::new());
    let acked: Vec<(usize, u64)> = {
        let s = ObjectStorage::with_store(store.clone(), PREFIX, ObjectConfig::default()).unwrap();
        let tok = block_on(s.acquire_fence(WriterId(7))).unwrap();
        let mut acked = Vec::new();
        for i in 0..12usize {
            let lsn =
                block_on(s.append_wal(&tok, &[WalRecord::new(format!("commit-{i}"))])).unwrap();
            acked.push((i, lsn.0));
        }
        acked
        // drop without flush() == crash before materialization
    };
    let s = ObjectStorage::with_store(store.clone(), PREFIX, ObjectConfig::default()).unwrap();
    let entries = block_on(s.scan_wal(Lsn(0))).unwrap();
    let by_lsn: HashMap<u64, &[u8]> = entries
        .iter()
        .map(|e| (e.lsn.0, e.record.bytes.as_slice()))
        .collect();
    for (i, lsn) in &acked {
        assert_eq!(
            by_lsn.get(lsn).copied(),
            Some(format!("commit-{i}").as_bytes()),
            "exp4b: acked commit {i} lost or torn after crash-before-flush",
        );
    }
    assert!(block_on(s.durable_lsn()).unwrap().0 >= acked.last().unwrap().1);
}
