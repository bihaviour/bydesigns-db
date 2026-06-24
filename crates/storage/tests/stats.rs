//! `Storage::stats()` — the read-only observability surface (#53 / spec 15).
//! Asserts the backend-neutral counters move on the operations that produce
//! them, and that a branch reports its own (overlay-private) activity.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use twill_storage::{block_on, open_branch, open_storage, Lsn, PageId, WalRecord, WriterId};

fn unique_db_path(tag: &str) -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let mut p = std::env::temp_dir();
    p.push(format!("twill-{tag}-{pid}-{n}.db"));
    let _ = fs::remove_file(&p);
    p
}

#[test]
fn local_stats_track_appends_reads_and_fsyncs() {
    let path = unique_db_path("stats");
    let url = format!("file://{}", path.display());
    let s = open_storage(&url).expect("open");

    // A fresh store starts at zero on every counter.
    let z = s.stats();
    assert_eq!(z, Default::default(), "fresh store has zeroed stats");

    let token = block_on(s.acquire_fence(WriterId(1))).expect("fence");

    // Two WAL appends → two durable appends, two fsyncs, bytes accounted.
    let lsn1 = block_on(s.append_wal(&token, &[WalRecord::new(vec![1, 2, 3])])).expect("wal1");
    block_on(s.append_wal(&token, &[WalRecord::new(vec![4, 5])])).expect("wal2");
    let after_wal = s.stats();
    assert_eq!(after_wal.wal_appends, 2);
    assert!(after_wal.wal_bytes > 0, "wal bytes accounted");
    // acquire_fence + two appends each fsync once.
    assert_eq!(after_wal.fsyncs, 3, "one fsync per durable frame");
    assert_eq!(after_wal.page_reads, 0, "no page reads yet");

    // A page write then a read at a visible LSN bumps the read counters.
    let plsn = block_on(s.put_page(&token, PageId(7), &[9u8; 128])).expect("put_page");
    let _ = block_on(s.get_page(PageId(7), plsn)).expect("get_page");
    let _ = block_on(s.get_page(PageId(7), plsn)).expect("get_page again");
    let after_reads = s.stats();
    assert_eq!(after_reads.page_reads, 2, "two page reads counted");
    assert!(
        after_reads.page_read_bytes >= 2 * 4096,
        "page bytes accounted"
    );

    // A miss (unknown page) does not count as a read.
    let _ = block_on(s.get_page(PageId(999), plsn));
    assert_eq!(s.stats().page_reads, 2, "a not-found read is not counted");

    // flush() performs an fsync.
    let before_flush = s.stats().fsyncs;
    block_on(s.flush()).expect("flush");
    assert_eq!(s.stats().fsyncs, before_flush + 1, "flush fsyncs once");

    // Counters are cumulative, so deltas between two snapshots are exact —
    // the property a consumer relies on for per-scenario reporting.
    let _ = lsn1;
    let _ = fs::remove_file(&path);
}

#[test]
fn branch_stats_are_overlay_private() {
    let path = unique_db_path("stats-branch");
    let url = format!("file://{}", path.display());
    let base = open_storage(&url).expect("open base");
    let token = block_on(base.acquire_fence(WriterId(1))).expect("fence");
    // Drive some base activity, then fork at the committed LSN.
    let base_lsn = block_on(base.append_wal(&token, &[WalRecord::new(vec![1])])).expect("wal");
    let bid = block_on(base.create_branch("b", base_lsn)).expect("branch");
    let base_appends = base.stats().wal_appends;
    drop(base); // release the fence so the branch overlay/base can be opened

    let branch = open_branch(&url, bid).expect("open branch");
    // A fresh branch overlay starts at zero — base activity is not folded in.
    assert_eq!(
        branch.stats().wal_appends,
        0,
        "branch reports its own overlay, not the base's {base_appends} appends"
    );
    let btok = block_on(branch.acquire_fence(WriterId(2))).expect("branch fence");
    block_on(branch.append_wal(&btok, &[WalRecord::new(vec![7, 7])])).expect("branch wal");
    assert_eq!(branch.stats().wal_appends, 1, "branch counts its own write");
    let _ = Lsn(0);
    let _ = fs::remove_file(&path);
}
