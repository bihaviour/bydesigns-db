//! Runs the C1–C8 conformance battery against `LocalFileStorage`, plus a
//! backend-specific torn-trailing-frame recovery test (the in-process analog of
//! a `kill -9` mid-append).

use twill_storage::conformance::run_conformance;
use twill_storage::{block_on, open_storage, Lsn, WriterId};
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

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
fn local_file_storage_passes_conformance() {
    let path = unique_db_path("conf");
    let url = format!("file://{}", path.display());
    let factory = {
        let url = url.clone();
        move || open_storage(&url).expect("open_storage")
    };
    run_conformance(&factory);
    let _ = fs::remove_file(&path);
}

#[test]
fn recovers_from_torn_trailing_frame() {
    let path = unique_db_path("torn");
    let url = format!("file://{}", path.display());

    // Ack one record, then "crash".
    let acked = {
        let s = open_storage(&url).unwrap();
        let t = block_on(s.acquire_fence(WriterId(1))).unwrap();
        block_on(s.append_wal(&t, &[twill_storage::WalRecord::new(b"good".to_vec())])).unwrap()
    };

    // Simulate a partially-written trailing frame: a length prefix promising
    // more bytes than actually follow.
    {
        let mut f = fs::OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(&999u32.to_le_bytes()).unwrap(); // claims a big frame
        f.write_all(&[7u8, 7, 7]).unwrap(); // ...but only 3 bytes of it
        f.sync_all().unwrap();
    }

    // Reopen: the torn tail is discarded; the acked record survives.
    let s2 = open_storage(&url).unwrap();
    assert!(block_on(s2.get_commit_lsn()).unwrap() >= acked);
    let entries = block_on(s2.scan_wal(Lsn::ZERO)).unwrap();
    assert_eq!(entries.len(), 1, "only the durable record should remain");
    assert_eq!(entries[0].record.bytes, b"good");

    // And new appends after recovery are clean and durable.
    let t = block_on(s2.acquire_fence(WriterId(2))).unwrap();
    let next = block_on(s2.append_wal(&t, &[twill_storage::WalRecord::new(b"after".to_vec())]))
        .unwrap();
    assert!(next > acked);
    drop(s2);

    let s3 = open_storage(&url).unwrap();
    let entries = block_on(s3.scan_wal(Lsn::ZERO)).unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[1].record.bytes, b"after");

    let _ = fs::remove_file(&path);
}
