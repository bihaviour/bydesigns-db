//! Phase-4 branching: copy-on-write write-isolation (issue #22), proven on both
//! backends. A branch reads the base's shared immutable history at-or-below its
//! fork point and writes only to a private overlay, so the base and every
//! sibling are untouched by a branch's writes — and creating a branch copies no
//! pages (near-zero marginal storage until divergence).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use twill_storage::{
    block_on, open_branch, open_storage, BranchStorage, FenceToken, Lsn, MemObjectStore,
    ObjectConfig, ObjectStorage, ObjectStore, PageId, Storage, StorageError, WalRecord, WriterId,
};

fn unique_dir(tag: &str) -> std::path::PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let mut p = std::env::temp_dir();
    p.push(format!("twill-branch-{tag}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn acquire(s: &dyn Storage) -> FenceToken {
    block_on(s.acquire_fence(WriterId(1))).expect("acquire_fence")
}

fn img(fill: u8) -> Vec<u8> {
    vec![fill; 48]
}

// ---- file:// backend, through open_storage / open_branch -------------------

#[test]
fn file_branch_is_copy_on_write_isolated() {
    let dir = unique_dir("file-cow");
    let url = format!("file://{}/main.db", dir.display());

    // Base line: two versions of page 1 (A then B); B is the fork point.
    let (lsn_a, base) = {
        let s = open_storage(&url).unwrap();
        let t = acquire(&*s);
        let la = block_on(s.put_page(&t, PageId(1), &img(0xAA))).unwrap();
        let lb = block_on(s.put_page(&t, PageId(1), &img(0xBB))).unwrap();
        (la, lb)
    };

    // Fork at B, then diverge the branch with its own version C of page 1.
    let branch_id = {
        let s = open_storage(&url).unwrap();
        block_on(s.create_branch("feature", base)).unwrap()
    };

    let branch = open_branch(&url, branch_id).unwrap();
    let bt = acquire(&*branch);
    let lsn_c = block_on(branch.put_page(&bt, PageId(1), &img(0xCC))).unwrap();
    assert!(lsn_c.0 > base.0, "branch LSNs continue past the fork point");

    // The branch sees: its own C at head, the shared B at the fork, the shared
    // A below the fork.
    assert_eq!(
        &block_on(branch.get_page(PageId(1), lsn_c)).unwrap().bytes[..48],
        &img(0xCC)[..],
        "branch head is its diverged version"
    );
    assert_eq!(
        &block_on(branch.get_page(PageId(1), base)).unwrap().bytes[..48],
        &img(0xBB)[..],
        "at the fork point the branch reads the shared base version"
    );
    assert_eq!(
        &block_on(branch.get_page(PageId(1), lsn_a)).unwrap().bytes[..48],
        &img(0xAA)[..],
        "below the fork the branch reads shared base history"
    );

    // The base is completely untouched: still B at head, no C, marks unchanged.
    let s2 = open_storage(&url).unwrap();
    assert_eq!(block_on(s2.durable_lsn()).unwrap(), base);
    assert_eq!(
        &block_on(s2.get_page(PageId(1), block_on(s2.durable_lsn()).unwrap()))
            .unwrap()
            .bytes[..48],
        &img(0xBB)[..],
        "base never sees the branch's write"
    );
}

#[test]
fn file_sibling_branches_are_isolated() {
    let dir = unique_dir("file-siblings");
    let url = format!("file://{}/main.db", dir.display());

    let base = {
        let s = open_storage(&url).unwrap();
        let t = acquire(&*s);
        block_on(s.put_page(&t, PageId(7), &img(0x10))).unwrap()
    };
    let (b1, b2) = {
        let s = open_storage(&url).unwrap();
        let b1 = block_on(s.create_branch("one", base)).unwrap();
        let b2 = block_on(s.create_branch("two", base)).unwrap();
        (b1, b2)
    };

    let s1 = open_branch(&url, b1).unwrap();
    let s2 = open_branch(&url, b2).unwrap();
    let t1 = acquire(&*s1);
    let t2 = acquire(&*s2);
    let l1 = block_on(s1.put_page(&t1, PageId(7), &img(0x11))).unwrap();
    let l2 = block_on(s2.put_page(&t2, PageId(7), &img(0x22))).unwrap();

    assert_eq!(
        &block_on(s1.get_page(PageId(7), l1)).unwrap().bytes[..48],
        &img(0x11)[..]
    );
    assert_eq!(
        &block_on(s2.get_page(PageId(7), l2)).unwrap().bytes[..48],
        &img(0x22)[..],
        "a sibling branch never sees the other's diverged write"
    );
}

#[test]
fn file_branch_create_copies_no_pages() {
    let dir = unique_dir("file-o1");
    let url = format!("file://{}/main.db", dir.display());
    let base = {
        let s = open_storage(&url).unwrap();
        let t = acquire(&*s);
        for v in 0..16u8 {
            block_on(s.put_page(&t, PageId(v as u64), &img(v))).unwrap();
        }
        block_on(s.durable_lsn()).unwrap()
    };
    let id = {
        let s = open_storage(&url).unwrap();
        block_on(s.create_branch("cheap", base)).unwrap()
    };
    // Opening the branch creates only an empty overlay log (just its header);
    // no base page was copied.
    let _branch = open_branch(&url, id).unwrap();
    let overlay = dir.join(format!("main.db.branch-{}", id.0));
    let len = std::fs::metadata(&overlay).unwrap().len();
    assert!(
        len <= 64,
        "fresh branch overlay holds no diverged data (got {len} bytes)"
    );
}

#[test]
fn file_branch_diverges_and_survives_reopen() {
    let dir = unique_dir("file-reopen");
    let url = format!("file://{}/main.db", dir.display());
    let base = {
        let s = open_storage(&url).unwrap();
        let t = acquire(&*s);
        block_on(s.append_wal(&t, &[WalRecord::new(b"base".to_vec())])).unwrap()
    };
    let id = {
        let s = open_storage(&url).unwrap();
        block_on(s.create_branch("persist", base)).unwrap()
    };
    let head = {
        let branch = open_branch(&url, id).unwrap();
        let bt = acquire(&*branch);
        block_on(branch.append_wal(&bt, &[WalRecord::new(b"branch-write".to_vec())])).unwrap()
    };
    // Reopen the branch: its diverged commit is still there.
    let branch = open_branch(&url, id).unwrap();
    assert_eq!(
        block_on(branch.get_commit_lsn()).unwrap(),
        head,
        "branch's diverged commit survives reopen"
    );
    let entries = block_on(branch.scan_wal(Lsn::ZERO)).unwrap();
    let payloads: Vec<_> = entries.iter().map(|e| e.record.bytes.clone()).collect();
    assert!(
        payloads.iter().any(|p| p == b"base"),
        "replay includes base"
    );
    assert!(
        payloads.iter().any(|p| p == b"branch-write"),
        "replay includes the branch's own diverged commit"
    );
}

#[test]
fn file_delete_branch_reclaims_only_diverged_data() {
    let dir = unique_dir("file-delete");
    let url = format!("file://{}/main.db", dir.display());
    let base = {
        let s = open_storage(&url).unwrap();
        let t = acquire(&*s);
        block_on(s.append_wal(&t, &[WalRecord::new(b"keep".to_vec())])).unwrap()
    };
    let id = {
        let s = open_storage(&url).unwrap();
        block_on(s.create_branch("doomed", base)).unwrap()
    };
    {
        let branch = open_branch(&url, id).unwrap();
        let bt = acquire(&*branch);
        block_on(branch.append_wal(&bt, &[WalRecord::new(b"gone".to_vec())])).unwrap();
    }
    let overlay = dir.join(format!("main.db.branch-{}", id.0));
    assert!(overlay.exists(), "overlay file exists before delete");

    {
        let s = open_storage(&url).unwrap();
        block_on(s.delete_branch(id)).unwrap();
        assert!(matches!(
            block_on(s.resolve_branch(id)),
            Err(StorageError::NotFound(_))
        ));
    }
    assert!(
        !overlay.exists(),
        "delete reclaims the branch's diverged overlay file"
    );
    // The base survives the branch deletion intact.
    let s = open_storage(&url).unwrap();
    assert_eq!(block_on(s.get_commit_lsn()).unwrap(), base);
}

// ---- object backend, through the composed BranchStorage adaptor ------------

#[test]
fn object_branch_is_copy_on_write_isolated() {
    // One shared in-memory object store; base and branch overlay are distinct
    // key prefixes over it, exactly as open_branch wires s3:// in production.
    let store: Arc<MemObjectStore> = Arc::new(MemObjectStore::new());
    let prefix = "db/test/";

    let base_handle =
        ObjectStorage::with_store(store.clone(), prefix, ObjectConfig::default()).unwrap();
    let t = acquire(&base_handle);
    let lsn_a = block_on(base_handle.put_page(&t, PageId(1), &img(0xAA))).unwrap();
    let base = block_on(base_handle.put_page(&t, PageId(1), &img(0xBB))).unwrap();
    let branch_id = block_on(base_handle.create_branch("feature", base)).unwrap();
    let bref = block_on(base_handle.resolve_branch(branch_id)).unwrap();

    // A second handle over the same store is the branch's read-through parent;
    // the overlay is a child-prefix ObjectStorage over the same store.
    let parent = ObjectStorage::with_store(store.clone(), prefix, ObjectConfig::default()).unwrap();
    let overlay = ObjectStorage::with_store(
        store.clone(),
        &format!("{prefix}branches/{:020}/", branch_id.0),
        ObjectConfig::default(),
    )
    .unwrap();
    let branch = BranchStorage::new(Arc::new(parent), Box::new(overlay), bref);

    let bt = acquire(&branch);
    let lsn_c = block_on(branch.put_page(&bt, PageId(1), &img(0xCC))).unwrap();
    assert!(lsn_c.0 > base.0);

    assert_eq!(
        &block_on(branch.get_page(PageId(1), lsn_c)).unwrap().bytes[..48],
        &img(0xCC)[..]
    );
    assert_eq!(
        &block_on(branch.get_page(PageId(1), base)).unwrap().bytes[..48],
        &img(0xBB)[..],
        "fork point reads the shared base version"
    );
    assert_eq!(
        &block_on(branch.get_page(PageId(1), lsn_a)).unwrap().bytes[..48],
        &img(0xAA)[..],
        "below the fork reads shared base history"
    );

    // The base, observed through a fresh handle, never saw C.
    let after = ObjectStorage::with_store(store.clone(), prefix, ObjectConfig::default()).unwrap();
    assert_eq!(block_on(after.durable_lsn()).unwrap(), base);
    assert_eq!(
        &block_on(after.get_page(PageId(1), block_on(after.durable_lsn()).unwrap()))
            .unwrap()
            .bytes[..48],
        &img(0xBB)[..]
    );

    // Creating the branch copied no page objects into its overlay prefix.
    let diverged =
        block_on(store.list(&format!("{prefix}branches/{:020}/delta/", branch_id.0))).unwrap();
    assert!(diverged.is_empty(), "no pages copied on branch create");
}
