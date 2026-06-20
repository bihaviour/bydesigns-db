//! # bydesigns-db · Pluggable Storage Interface (spec 03)
//!
//! A single narrow trait the engine calls instead of touching disk — the seam
//! that makes *embeddable* and *storage-disaggregated* stop contradicting each
//! other. `file://` selects [`LocalFileStorage`] (pure embedded, zero network);
//! `s3://`/`r2://`/`gs://` will select the disaggregated `ObjectStorage` backend
//! in Phase 2. The engine above the seam does not know or care which is wired in.
//!
//! ## Relationship to the source trait
//!
//! The design note's minimal seam is three methods (`get_page`, `append_wal`,
//! `flush`). Spec 03 extends that to a buildable v1 (batch reads, durable-commit
//! point, CAS fencing, branch pointers, GC/PITR hooks). This crate implements
//! that v1, plus two **buildable concretizations** the engine needs in Phase 1,
//! each forward-compatible with the object-storage backend:
//!
//! * [`Storage::scan_wal`] — recovery read path. The engine replays the durable
//!   log to rebuild in-memory state on open / after a crash (spec 02 says "on
//!   restart, WAL replay determines outcome"). `ObjectStorage` realizes it by
//!   scanning the S3-CAS commit log from the PITR floor.
//! * [`Storage::put_page`] — the page store's write path. The source trait only
//!   names the read path (`get_page`); a page store must also accept versioned
//!   page images. `ObjectStorage` realizes it as an LSM layer write. It shares a
//!   single monotonic LSN counter with `append_wal`, so LSN order is total.

mod types;

pub mod conformance;
pub mod local;

pub use types::{
    BranchId, BranchRef, FenceToken, LogEntry, Lsn, Page, PageId, StorageError, WalRecord,
    WriterId, PAGE_SIZE,
};

use async_trait::async_trait;

/// Bumped (major) for any signature change, associated-type byte-layout change,
/// or contract weakening. The engine refuses to open a backend reporting an
/// incompatible major version.
pub const STORAGE_TRAIT_VERSION: u32 = 1;

/// The one seam. The engine calls this; it never touches disk directly.
///
/// All methods are `async` because a backend may be network-bound (S3/R2);
/// [`LocalFileStorage`] resolves them synchronously under the hood. The two
/// invariants that override everything (spec 03):
///
/// 1. `append_wal` returns a commit LSN ONLY after the records are durable —
///    never ack a commit from an in-memory buffer.
/// 2. `get_page(id, lsn)` returns the greatest page version with version-LSN
///    `<= lsn` — the MVCC read floor.
#[async_trait]
pub trait Storage: Send + Sync + 'static {
    // ---- Read path -------------------------------------------------------
    /// Page version visible at-or-before `lsn`. MVCC snapshot read floor:
    /// returns the greatest version with `Page.lsn <= lsn`.
    async fn get_page(&self, page_id: PageId, lsn: Lsn) -> Result<Page, StorageError>;

    /// Batch read: same semantics as [`Storage::get_page`] per id, but the
    /// backend MAY coalesce / parallelize. Result order corresponds to `ids`.
    async fn get_pages(
        &self,
        ids: &[PageId],
        lsn: Lsn,
    ) -> Result<Vec<Result<Page, StorageError>>, StorageError> {
        // Default: serial fan-out. Backends SHOULD override to coalesce I/O.
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            out.push(self.get_page(*id, lsn).await);
        }
        Ok(out)
    }

    // ---- Write path ------------------------------------------------------
    /// Durably append WAL records under a valid fence token. Returns the commit
    /// LSN ONLY after the records are durable. No ack-before-durable.
    async fn append_wal(
        &self,
        token: &FenceToken,
        records: &[WalRecord],
    ) -> Result<Lsn, StorageError>;

    /// Durably write a versioned page image under a valid fence token; returns
    /// the version LSN (shares the monotonic LSN counter with `append_wal`).
    /// The page store's write path — a v1 buildable concretization (see module
    /// docs). The engine's Phase-1 path is WAL-centric; this backs the page
    /// read API that becomes load-bearing for `ObjectStorage` cold reads.
    async fn put_page(
        &self,
        token: &FenceToken,
        page_id: PageId,
        image: &[u8],
    ) -> Result<Lsn, StorageError>;

    /// Recovery read: durable WAL records in LSN order with `lsn > after`.
    /// A v1 buildable concretization for engine restart replay (see module docs).
    async fn scan_wal(&self, after: Lsn) -> Result<Vec<LogEntry>, StorageError>;

    /// Force any buffered durable state fully settled. Idempotent.
    async fn flush(&self) -> Result<(), StorageError>;

    // ---- Durable commit point -------------------------------------------
    /// Latest LSN known to be durable on this backend (recovery + visibility).
    async fn durable_lsn(&self) -> Result<Lsn, StorageError>;

    /// Latest LSN that is durable AND a committed transaction boundary — the
    /// high-water mark a fresh reader may safely read at.
    async fn get_commit_lsn(&self) -> Result<Lsn, StorageError>;

    // ---- Single-writer fencing (CAS token) ------------------------------
    /// Acquire the single-writer token. CAS over the previous epoch; a new
    /// holder strictly increases `epoch`, invalidating all prior tokens.
    async fn acquire_fence(&self, owner: WriterId) -> Result<FenceToken, StorageError>;

    /// Renew (lease extend) an existing token. Fails `Fenced` if superseded.
    async fn renew_fence(&self, token: &FenceToken) -> Result<FenceToken, StorageError>;

    /// Voluntarily relinquish the token so the next writer can acquire fast.
    async fn release_fence(&self, token: FenceToken) -> Result<(), StorageError>;

    // ---- Snapshot / branch (copy-on-write) ------------------------------
    /// Create a branch as a new LSN pointer over shared immutable layers.
    async fn create_branch(&self, name: &str, base_lsn: Lsn) -> Result<BranchId, StorageError>;

    /// Resolve a branch to `{base_lsn, head_lsn}`.
    async fn resolve_branch(&self, branch: BranchId) -> Result<BranchRef, StorageError>;

    // ---- GC / PITR hooks -------------------------------------------------
    /// Declare the retention floor: the oldest LSN any live reader or branch
    /// still needs. The backend MAY reclaim versions strictly older than this.
    /// MUST only move forward; a value below the current floor is `Invalid`.
    async fn set_retention_floor(&self, lsn: Lsn) -> Result<(), StorageError>;

    /// Oldest LSN still recoverable (start of the PITR window).
    async fn pitr_floor(&self) -> Result<Lsn, StorageError>;
}

pub use local::LocalFileStorage;

/// Dispatch a storage URL to a concrete backend. Called once per open.
///
/// `file://` selects [`LocalFileStorage`]; `s3://`/`r2://`/`gs://` are reserved
/// for the Phase-2 `ObjectStorage` backend and currently return `Invalid`. An
/// unknown scheme is rejected rather than silently defaulting (spec 02 warning).
pub fn open_storage(url: &str) -> Result<Box<dyn Storage>, StorageError> {
    let scheme = url
        .split_once("://")
        .map(|(s, _)| s)
        .ok_or_else(|| StorageError::Invalid(format!("missing scheme in url: {url}")))?;
    match scheme {
        "file" => Ok(Box::new(LocalFileStorage::open(url)?)),
        "s3" | "r2" | "gs" => Err(StorageError::Invalid(format!(
            "scheme '{scheme}://' (ObjectStorage) is a Phase-2 backend, not yet implemented"
        ))),
        other => Err(StorageError::Invalid(format!("unknown scheme: {other}"))),
    }
}

/// Drive a future to completion on the current thread, parking between polls.
///
/// The async `Storage` trait is the stable seam; the engine's C ABI is
/// synchronous (`engine_commit` blocks until durable). This minimal,
/// dependency-free executor bridges the two: it works for any future and never
/// busy-spins (it parks the thread until the waker fires). `LocalFileStorage`
/// futures resolve in a single poll.
pub fn block_on<F: std::future::Future>(fut: F) -> F::Output {
    use std::sync::Arc;
    use std::task::{Context, Poll, Wake, Waker};

    struct ThreadWaker(std::thread::Thread);
    impl Wake for ThreadWaker {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
        fn wake_by_ref(self: &Arc<Self>) {
            self.0.unpark();
        }
    }

    let waker = Waker::from(Arc::new(ThreadWaker(std::thread::current())));
    let mut cx = Context::from_waker(&waker);
    let mut fut = std::pin::pin!(fut);
    loop {
        match fut.as_mut().poll(&mut cx) {
            Poll::Ready(v) => return v,
            Poll::Pending => std::thread::park(),
        }
    }
}
