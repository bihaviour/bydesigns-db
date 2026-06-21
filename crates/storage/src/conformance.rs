//! Backend-agnostic conformance battery (spec 03 §"Conformance test suite").
//!
//! Any [`Storage`](crate::Storage) implementation MUST pass the entire suite to
//! be considered conformant. The factory returns a backend bound to the *same*
//! durable medium each call, so dropping a handle and calling the factory again
//! reopens the same database — this is how durability-after-reopen is checked.
//!
//! C1 (durability after ack) and C4 (fencing) are unconditional gates: a backend
//! that fails either is rejected outright, no matter how fast it is.
//!
//! Note for Phase 1: C7 exercises the *creation* semantics of branches (no base
//! mutation, correct resolve, PITR bound). Full copy-on-write branch-write
//! isolation is a Phase 4 concern (the trait has no per-branch write path yet).

use crate::{
    block_on, BranchId, FenceToken, Lsn, PageId, Storage, StorageError, WalRecord, WriterId,
    PAGE_SIZE,
};

type Factory<'a> = dyn Fn() -> Box<dyn Storage> + 'a;

fn rec(s: &str) -> WalRecord {
    WalRecord::new(s.as_bytes().to_vec())
}

fn page_image(fill: u8) -> Vec<u8> {
    vec![fill; 64]
}

fn padded(data: &[u8]) -> Vec<u8> {
    let mut v = vec![0u8; PAGE_SIZE];
    v[..data.len()].copy_from_slice(data);
    v
}

fn acquire(s: &dyn Storage) -> FenceToken {
    block_on(s.acquire_fence(WriterId(1))).expect("acquire_fence")
}

/// Run the full conformance battery against any backend factory.
pub fn run_conformance(make: &Factory) {
    durability_after_ack(make); // C1
    monotonic_lsn(make); // C2
    snapshot_read_correctness(make); // C3
    fencing_rejects_stale_writer(make); // C4
    crash_consistency_hooks(make); // C5
    batch_read_equivalence(make); // C6
    branch_isolation(make); // C7
    retention_safety(make); // C8
}

/// C1 · durability after ack — no ack-before-durable.
pub fn durability_after_ack(make: &Factory) {
    let acked = {
        let s = make();
        let t = acquire(&*s);
        let lsn = block_on(s.append_wal(&t, &[rec("c1-a"), rec("c1-b")])).expect("append_wal");
        assert!(block_on(s.get_commit_lsn()).unwrap() >= lsn);
        lsn
    }; // drop -> simulate process exit after ack

    let s2 = make();
    assert!(
        block_on(s2.get_commit_lsn()).unwrap() >= acked,
        "C1: acked commit LSN must survive reopen"
    );
    let entries = block_on(s2.scan_wal(Lsn::ZERO)).unwrap();
    let payloads: Vec<_> = entries.iter().map(|e| e.record.bytes.clone()).collect();
    assert!(
        payloads.iter().any(|p| p == b"c1-a") && payloads.iter().any(|p| p == b"c1-b"),
        "C1: acked records must be present and readable after reopen"
    );
}

/// C2 · monotonic LSN — strictly increasing, gap-free; counters never decrease.
pub fn monotonic_lsn(make: &Factory) {
    let s = make();
    let t = acquire(&*s);
    let l1 = block_on(s.append_wal(&t, &[rec("a")])).unwrap();
    let l2 = block_on(s.append_wal(&t, &[rec("b")])).unwrap();
    let l3 = block_on(s.append_wal(&t, &[rec("c"), rec("d")])).unwrap();
    assert!(l1 < l2 && l2 < l3, "C2: LSNs strictly increasing");
    assert_eq!(l2.0, l1.0 + 1, "C2: gap-free single appends");
    assert_eq!(l3.0, l2.0 + 2, "C2: batch of 2 consumes 2 LSNs");
    assert_eq!(block_on(s.durable_lsn()).unwrap(), l3);
    assert_eq!(block_on(s.get_commit_lsn()).unwrap(), l3);
    // Re-read: never decreases.
    assert_eq!(block_on(s.durable_lsn()).unwrap(), l3);
}

/// C3 · snapshot read correctness — `get_page` returns the at-or-before version.
pub fn snapshot_read_correctness(make: &Factory) {
    let s = make();
    let t = acquire(&*s);
    let id = PageId(7);
    let la = block_on(s.put_page(&t, id, &page_image(0xAA))).unwrap();
    // A non-page append in between creates an LSN gap for the page chain.
    let gap = block_on(s.append_wal(&t, &[rec("gap")])).unwrap();
    let lc = block_on(s.put_page(&t, id, &page_image(0xCC))).unwrap();
    assert!(la < gap && gap < lc);

    let pa = block_on(s.get_page(id, la)).unwrap();
    assert_eq!(pa.bytes.to_vec(), padded(&page_image(0xAA)));
    assert_eq!(pa.lsn, la);

    // Read at the gap LSN (between the two page versions) returns the lower one.
    let pgap = block_on(s.get_page(id, gap)).unwrap();
    assert_eq!(pgap.bytes.to_vec(), padded(&page_image(0xAA)));
    assert!(pgap.lsn <= gap, "C3: Page.lsn <= requested");

    let pc = block_on(s.get_page(id, lc)).unwrap();
    assert_eq!(pc.bytes.to_vec(), padded(&page_image(0xCC)));
    assert_eq!(pc.lsn, lc);
}

/// C4 · fencing rejects a stale writer — single-writer safety.
pub fn fencing_rejects_stale_writer(make: &Factory) {
    let s = make();
    let token_a = block_on(s.acquire_fence(WriterId(0xA))).unwrap();
    let token_b = block_on(s.acquire_fence(WriterId(0xB))).unwrap();
    assert!(token_b.epoch > token_a.epoch, "C4: new acquire bumps epoch");

    let stale = block_on(s.append_wal(&token_a, &[rec("from-A")]));
    assert!(
        matches!(stale, Err(StorageError::Fenced { .. })),
        "C4: stale writer must be Fenced, got {stale:?}"
    );
    block_on(s.append_wal(&token_b, &[rec("from-B")])).expect("C4: current writer succeeds");
}

/// C5 · crash-consistency hooks — deterministic recovery, monotone durable LSN.
pub fn crash_consistency_hooks(make: &Factory) {
    let acked;
    {
        let s = make();
        let t = acquire(&*s);
        block_on(s.append_wal(&t, &[rec("c5-1")])).unwrap();
        block_on(s.append_wal(&t, &[rec("c5-2")])).unwrap();
        acked = block_on(s.append_wal(&t, &[rec("c5-3")])).unwrap();
    }
    // Two independent reopens must reconstruct identical, complete state.
    let s1 = make();
    let d1 = block_on(s1.durable_lsn()).unwrap();
    let n1 = block_on(s1.scan_wal(Lsn::ZERO)).unwrap().len();
    drop(s1);
    let s2 = make();
    let d2 = block_on(s2.durable_lsn()).unwrap();
    let n2 = block_on(s2.scan_wal(Lsn::ZERO)).unwrap().len();
    assert!(
        d1 >= acked && d2 >= acked,
        "C5: durable LSN >= every acked commit"
    );
    assert_eq!(d1, d2, "C5: recovery is deterministic");
    assert_eq!(n1, n2, "C5: replay set is deterministic");
}

/// C6 · batch read equivalence — `get_pages` == N × `get_page`, in order.
pub fn batch_read_equivalence(make: &Factory) {
    let s = make();
    let t = acquire(&*s);
    block_on(s.put_page(&t, PageId(1), &page_image(1))).unwrap();
    block_on(s.put_page(&t, PageId(2), &page_image(2))).unwrap();
    let read_lsn = block_on(s.put_page(&t, PageId(3), &page_image(3))).unwrap();

    let ids = [PageId(1), PageId(2), PageId(3), PageId(99)]; // 99 missing
    let batch = block_on(s.get_pages(&ids, read_lsn)).unwrap();
    assert_eq!(batch.len(), ids.len());
    for (i, id) in ids.iter().enumerate() {
        let single = block_on(s.get_page(*id, read_lsn));
        match (&batch[i], &single) {
            (Ok(b), Ok(o)) => {
                assert_eq!(
                    b.bytes.to_vec(),
                    o.bytes.to_vec(),
                    "C6: same bytes for {id:?}"
                );
                assert_eq!(b.lsn, o.lsn);
            }
            (Err(StorageError::NotFound(_)), Err(StorageError::NotFound(_))) => {}
            other => panic!("C6: batch/single mismatch for {id:?}: {other:?}"),
        }
    }
}

/// C7 · branch isolation — creation does not mutate the base; resolve is correct.
pub fn branch_isolation(make: &Factory) {
    let s = make();
    let t = acquire(&*s);
    block_on(s.append_wal(&t, &[rec("base-1")])).unwrap();
    let base = block_on(s.append_wal(&t, &[rec("base-2")])).unwrap();
    let commit_before = block_on(s.get_commit_lsn()).unwrap();
    let durable_before = block_on(s.durable_lsn()).unwrap();

    let b = block_on(s.create_branch("feature", base)).unwrap();
    let r = block_on(s.resolve_branch(b)).unwrap();
    assert_eq!(r.base_lsn, base, "C7: branch base == fork point");
    assert_eq!(r.head_lsn, base, "C7: fresh branch head == base");
    assert_eq!(r.parent, BranchId::ROOT, "C7: forked off the main line");

    assert_eq!(
        block_on(s.get_commit_lsn()).unwrap(),
        commit_before,
        "C7: creating a branch does not advance the base commit LSN"
    );
    assert_eq!(block_on(s.durable_lsn()).unwrap(), durable_before);

    // The branch is listed in the namespace it was created in.
    let listed = block_on(s.list_branches()).unwrap();
    assert!(
        listed.iter().any(|x| x.id == b),
        "C7: created branch appears in list_branches"
    );

    let unknown = block_on(s.resolve_branch(BranchId(424242)));
    assert!(matches!(unknown, Err(StorageError::NotFound(_))));

    // Delete reclaims only the branch; resolving it afterwards is NotFound, and
    // the base commit/durable marks are still untouched.
    block_on(s.delete_branch(b)).expect("C7: delete the branch");
    assert!(matches!(
        block_on(s.resolve_branch(b)),
        Err(StorageError::NotFound(_))
    ));
    assert!(
        matches!(
            block_on(s.delete_branch(BranchId(424242))),
            Err(StorageError::NotFound(_))
        ),
        "C7: deleting an unknown branch is NotFound"
    );
    assert_eq!(block_on(s.get_commit_lsn()).unwrap(), commit_before);
    assert_eq!(block_on(s.durable_lsn()).unwrap(), durable_before);
}

/// C8 · retention safety — floor moves forward; reads below it are snapshot-too-old.
pub fn retention_safety(make: &Factory) {
    let s = make();
    let t = acquire(&*s);
    block_on(s.append_wal(&t, &[rec("r1")])).unwrap();
    let mid = block_on(s.append_wal(&t, &[rec("r2")])).unwrap();
    let _hi = block_on(s.put_page(&t, PageId(5), &page_image(5))).unwrap();

    block_on(s.set_retention_floor(mid)).expect("C8: forward floor move ok");
    assert_eq!(block_on(s.pitr_floor()).unwrap(), mid);

    let backward = block_on(s.set_retention_floor(Lsn(mid.0 - 1)));
    assert!(
        matches!(backward, Err(StorageError::Invalid(_))),
        "C8: backward floor move rejected"
    );

    // Read below floor is snapshot-too-old; at/above floor succeeds.
    let below = block_on(s.get_page(PageId(5), Lsn(mid.0 - 1)));
    assert!(
        matches!(below, Err(StorageError::NotFound(_))),
        "C8: read below floor NotFound"
    );
    block_on(s.get_page(PageId(5), block_on(s.durable_lsn()).unwrap()))
        .expect("C8: read at/above floor succeeds");
}
