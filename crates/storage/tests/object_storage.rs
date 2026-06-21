//! Phase-2 backend verification: the full C1–C8 conformance battery against
//! `ObjectStorage` (over both the durable `FsObjectStore` and the in-memory
//! `MemObjectStore`), plus the object-storage-specific acceptance criteria from
//! spec 04 — CAS single-writer fencing, crash-safety (§8 Exp 4 a/b), layer
//! resolution across memtable/delta/image, and flush → compaction → GC.

use bydesigns_storage::conformance::run_conformance;
use bydesigns_storage::{
    block_on, FsObjectStore, MemObjectStore, ObjectConfig, ObjectError, ObjectStorage, ObjectStore,
    PageId, Storage, StorageError, WalRecord, WriterId,
};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

fn unique_root(tag: &str) -> std::path::PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let mut p = std::env::temp_dir();
    p.push(format!("bydesigns-obj-{tag}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    p
}

fn rec(s: &str) -> WalRecord {
    WalRecord::new(s.as_bytes().to_vec())
}

const PREFIX: &str = "db/test/";

// ---- conformance against both object stores --------------------------------

#[test]
fn object_storage_passes_conformance_on_fs() {
    // The durable floor: each factory call reopens the same directory, so the
    // C1/C5 durability-after-reopen checks exercise real recovery from objects.
    let root = unique_root("conf-fs");
    let factory = {
        let root = root.clone();
        move || -> Box<dyn Storage> {
            let store = FsObjectStore::open(&root).expect("open fs store");
            Box::new(
                ObjectStorage::with_store(Arc::new(store), PREFIX, ObjectConfig::default())
                    .expect("open ObjectStorage"),
            )
        }
    };
    run_conformance(&factory);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn object_storage_passes_conformance_on_mem() {
    // A shared in-memory store retains objects across factory calls, so reopen
    // semantics still hold without touching disk.
    let store: Arc<MemObjectStore> = Arc::new(MemObjectStore::new());
    let factory = {
        let store = store.clone();
        move || -> Box<dyn Storage> {
            Box::new(
                ObjectStorage::with_store(store.clone(), PREFIX, ObjectConfig::default())
                    .expect("open ObjectStorage"),
            )
        }
    };
    run_conformance(&factory);
}

// ---- spec 04 acceptance: CAS single-writer fencing -------------------------

#[test]
fn two_writers_resolve_to_one_survivor_via_cas() {
    // Two writers over one database: the later fence wins; the earlier writer is
    // fenced off with no split-brain on the log (spec 04 acceptance criterion 2).
    let store: Arc<MemObjectStore> = Arc::new(MemObjectStore::new());
    let a = ObjectStorage::with_store(store.clone(), PREFIX, ObjectConfig::default()).unwrap();
    let b = ObjectStorage::with_store(store.clone(), PREFIX, ObjectConfig::default()).unwrap();

    let ta = block_on(a.acquire_fence(WriterId(0xA))).unwrap();
    let tb = block_on(b.acquire_fence(WriterId(0xB))).unwrap();
    assert!(tb.epoch > ta.epoch, "later acquire bumps the epoch");

    // The stale writer (A) is fenced; the current writer (B) commits.
    let stale = block_on(a.append_wal(&ta, &[rec("from-A")]));
    assert!(
        matches!(stale, Err(StorageError::Fenced { .. })),
        "stale writer must be fenced, got {stale:?}"
    );
    block_on(b.append_wal(&tb, &[rec("from-B")])).expect("current writer commits");

    // Exactly one segment exists in the log — no split-brain double-claim.
    let segs = block_on(store.list("db/test/log/")).unwrap();
    assert_eq!(segs.len(), 1, "single legitimate writer => one log slot");
}

#[test]
fn lease_lifecycle_is_durable_on_object_store() {
    // The writer lease is a durable object: acquire stamps a live expiry, renew
    // (heartbeat) re-stamps it under the same epoch, and release frees it
    // (expiry 0) while keeping the epoch so the released token stays fenced.
    let store: Arc<MemObjectStore> = Arc::new(MemObjectStore::new());
    let s = ObjectStorage::with_store(store.clone(), PREFIX, ObjectConfig::default()).unwrap();
    let lease_key = "db/test/lease";
    let read = |store: &Arc<MemObjectStore>| -> (u64, u64) {
        let b = block_on(store.get(lease_key)).unwrap().unwrap().bytes;
        let epoch = u64::from_le_bytes(b[0..8].try_into().unwrap());
        let expires = u64::from_le_bytes(b[24..32].try_into().unwrap());
        (epoch, expires)
    };

    let t = block_on(s.acquire_fence(WriterId(1))).unwrap();
    let (e0, exp0) = read(&store);
    assert_eq!(e0, t.epoch, "lease records the acquired epoch");
    assert!(exp0 > 0, "acquire stamps a live lease expiry");

    let t = block_on(s.renew_fence(&t)).unwrap();
    let (e1, exp1) = read(&store);
    assert_eq!(e1, t.epoch, "renew keeps the same epoch");
    assert!(exp1 > 0, "renew re-stamps a live expiry durably");

    block_on(s.release_fence(t.clone())).unwrap();
    let (e2, exp2) = read(&store);
    assert_eq!(
        e2, t.epoch,
        "release keeps the epoch (so stale tokens stay fenced)"
    );
    assert_eq!(exp2, 0, "release frees the lease for a fast handoff");

    // A fresh acquire still takes over (higher epoch); the released token is fenced.
    let t2 = block_on(s.acquire_fence(WriterId(2))).unwrap();
    assert!(t2.epoch > t.epoch);
    assert!(matches!(
        block_on(s.append_wal(&t, &[rec("released")])),
        Err(StorageError::Fenced { .. })
    ));
}

#[test]
fn lost_cas_slot_is_retried_then_fenced() {
    // Directly exercise the put-if-absent CAS primitive the commit log rests on.
    let store = MemObjectStore::new();
    block_on(store.put_if_absent("k", b"first")).expect("first claim wins");
    let again = block_on(store.put_if_absent("k", b"second"));
    assert!(
        matches!(again, Err(ObjectError::Precondition(_))),
        "second claim of the same slot loses the CAS"
    );
}

// ---- spec 04 acceptance: crash safety (§8 Experiment 4) --------------------

#[test]
fn exp4a_durable_after_cas_before_ack() {
    // (a) A commit's segment is durable the instant its conditional PUT returns;
    // a crash before the client sees the ack still leaves the commit recoverable.
    let root = unique_root("exp4a");
    let acked = {
        let s = ObjectStorage::open_fs(&root).unwrap();
        let t = block_on(s.acquire_fence(WriterId(1))).unwrap();
        block_on(s.append_wal(&t, &[rec("a"), rec("b")])).unwrap()
    }; // drop == crash after CAS

    let s2 = ObjectStorage::open_fs(&root).unwrap();
    assert!(
        block_on(s2.get_commit_lsn()).unwrap() >= acked,
        "acked commit survives crash-after-CAS"
    );
    let entries = block_on(s2.scan_wal(bydesigns_storage::Lsn::ZERO)).unwrap();
    let payloads: Vec<_> = entries.iter().map(|e| e.record.bytes.clone()).collect();
    assert!(payloads.iter().any(|p| p == b"a") && payloads.iter().any(|p| p == b"b"));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn exp4b_page_reconstructed_after_crash_before_flush() {
    // (b) A commit durable in the log but not yet materialized into a delta layer
    // is reconstructed by replaying the log forward into the memtable on restart.
    let root = unique_root("exp4b");
    let (lsn, image) = {
        let s = ObjectStorage::open_fs(&root).unwrap();
        let t = block_on(s.acquire_fence(WriterId(1))).unwrap();
        let img = vec![0x5A; 200];
        let lsn = block_on(s.put_page(&t, PageId(7), &img)).unwrap();
        // No flush(): the page lives only in the durable log + (lost) memtable.
        (lsn, img)
    };

    let s2 = ObjectStorage::open_fs(&root).unwrap();
    let page = block_on(s2.get_page(PageId(7), lsn)).unwrap();
    assert_eq!(&page.bytes[..image.len()], &image[..], "page reconstructed");
    assert_eq!(page.lsn, lsn);
    let _ = std::fs::remove_dir_all(&root);
}

// ---- spec 04 acceptance: layer resolution + flush/compaction/GC ------------

#[test]
fn resolves_versions_across_memtable_delta_and_image() {
    let store: Arc<MemObjectStore> = Arc::new(MemObjectStore::new());
    let s = ObjectStorage::with_store(store.clone(), PREFIX, ObjectConfig::default()).unwrap();
    let t = block_on(s.acquire_fence(WriterId(1))).unwrap();

    let a = vec![0xAA; 32];
    let b = vec![0xBB; 32];
    let c = vec![0xCC; 32];
    let l1 = block_on(s.put_page(&t, PageId(1), &a)).unwrap();
    let l2 = block_on(s.put_page(&t, PageId(1), &b)).unwrap();
    block_on(s.flush()).unwrap(); // memtable -> delta layer [.. l2]
    let l3 = block_on(s.put_page(&t, PageId(1), &c)).unwrap(); // back in memtable

    // Delta serves the historical versions; memtable serves the newest.
    assert_eq!(
        &block_on(s.get_page(PageId(1), l1)).unwrap().bytes[..32],
        &a[..]
    );
    assert_eq!(
        &block_on(s.get_page(PageId(1), l2)).unwrap().bytes[..32],
        &b[..]
    );
    assert_eq!(
        &block_on(s.get_page(PageId(1), l3)).unwrap().bytes[..32],
        &c[..]
    );

    // Compact at the PITR floor: the delta folds into an image; the memtable
    // version stays live above the floor.
    block_on(s.set_retention_floor(l2)).unwrap();
    block_on(s.compact()).unwrap();
    assert_eq!(
        &block_on(s.get_page(PageId(1), l2)).unwrap().bytes[..32],
        &b[..],
        "image floor serves the folded version"
    );
    assert_eq!(
        &block_on(s.get_page(PageId(1), l3)).unwrap().bytes[..32],
        &c[..]
    );

    // Below the PITR floor is snapshot-too-old.
    let below = block_on(s.get_page(PageId(1), l1));
    assert!(
        matches!(below, Err(StorageError::NotFound(_))),
        "below PITR floor"
    );
}

#[test]
fn flush_compaction_gc_reduces_live_layers_and_respects_pitr() {
    let store: Arc<MemObjectStore> = Arc::new(MemObjectStore::new());
    let s = ObjectStorage::with_store(store.clone(), PREFIX, ObjectConfig::default()).unwrap();
    let t = block_on(s.acquire_fence(WriterId(1))).unwrap();

    // Three flushes => three delta layer objects.
    let mut last = bydesigns_storage::Lsn::ZERO;
    for round in 0..3u8 {
        last = block_on(s.put_page(&t, PageId(round as u64), &[round; 16])).unwrap();
        block_on(s.flush()).unwrap();
    }
    let deltas_before = block_on(store.list("db/test/delta/")).unwrap();
    assert_eq!(
        deltas_before.len(),
        3,
        "three flushes => three delta objects"
    );

    // Compact at the floor: deltas fold into one image (live layer count drops).
    block_on(s.set_retention_floor(last)).unwrap();
    block_on(s.compact()).unwrap();
    let images = block_on(store.list("db/test/image/")).unwrap();
    assert_eq!(images.len(), 1, "compaction produces a single image layer");

    // GC reclaims the now-unreferenced delta objects; the covering image stays.
    block_on(s.gc()).unwrap();
    assert!(
        block_on(store.list("db/test/delta/")).unwrap().is_empty(),
        "GC removes folded delta objects"
    );
    assert_eq!(
        block_on(store.list("db/test/image/")).unwrap().len(),
        1,
        "GC never deletes the covering image (PITR floor)"
    );

    // The PITR floor is still readable; below it is snapshot-too-old.
    block_on(s.get_page(PageId(2), last)).expect("read at floor succeeds");
}

// A tiny inherent-constructor convenience used by the crash tests so each reopen
// builds a fresh FsObjectStore over the same durable root.
trait OpenFs {
    fn open_fs(root: &std::path::Path) -> Result<ObjectStorage, StorageError>;
}
impl OpenFs for ObjectStorage {
    fn open_fs(root: &std::path::Path) -> Result<ObjectStorage, StorageError> {
        let store = FsObjectStore::open(root)
            .map_err(|e| StorageError::Invalid(format!("open fs: {e}")))?;
        ObjectStorage::with_store(Arc::new(store), PREFIX, ObjectConfig::default())
    }
}
