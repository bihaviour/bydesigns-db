//! A deterministic fault-injection wrapper around the object client, for the
//! Experiment 4 crash-safety gate (spec 09 §Experiment 4, spec 14 R-02).
//!
//! Spec 09 calls for a "fault-injection wrapper around the object-store client
//! that can fail or pause any GET/PUT/CAS at a chosen sequence number", driven by
//! "deterministic, seeded crash injection so a failing schedule is reproducible
//! from its seed." [`FaultObjectStore`] is exactly that: it wraps any
//! [`ObjectStore`] and, when armed with a [`FaultPlan`], fires once at the chosen
//! occurrence of one operation kind — either *before* the inner op runs (nothing
//! is made durable) or *after* it runs (the write is durable but the caller still
//! sees an error, modelling a crash between durability and ack).
//!
//! This is a *validation* artifact, not a production path: it is the seam that
//! lets the seeded crash-storm harness (`crates/storage/tests/crash_safety.rs`)
//! attack the two adversarial commit windows the durability rule must survive:
//!
//! * **(a) after CAS-append issued, before client ack** — [`FaultMode::AfterOp`]
//!   on [`FaultKind::PutIfAbsent`]: the log segment is durable, but `append_wal`
//!   returns an error, so the commit was never acked. Recovery MAY surface it;
//!   it MUST never be torn.
//! * **(b) acked, then crash before page materialization** — modelled by simply
//!   dropping the handle after a clean ack (no flush), so the page lives only in
//!   the durable log; recovery MUST replay it.
//!
//! The wrapper never weakens the underlying durability contract: an `AfterOp`
//! fault performs the real inner write first, so the object store's "an `Ok`
//! return is durable" guarantee still holds for everything the harness treats as
//! durable.

use super::store::{ETag, GetResult, ObjectError, ObjectStore};
use async_trait::async_trait;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// Which object-client operation a [`FaultPlan`] targets. Counting is per-kind,
/// so `fire_at` selects, e.g., the N-th `put_if_absent` (the commit-log CAS
/// append — spec 09's "after the S3 CAS-append is issued").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FaultKind {
    Get,
    PutIfAbsent,
    PutIfMatch,
    Put,
    Delete,
    List,
}

/// When the injected fault fires relative to the real inner operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FaultMode {
    /// Fail *before* the inner op runs: nothing reaches the durable store. Models
    /// a crash on the way to object storage — the commit is never acked and is
    /// absent on recovery (spec 09 invariant a: a non-acked commit may be gone).
    BeforeOp,
    /// Run the inner op (it becomes durable) *then* fail: models a crash after the
    /// CAS-append is durable but before the client sees the ack (Exp 4 case a).
    AfterOp,
}

/// A one-shot, deterministic fault: fire once at the `fire_at`-th occurrence of
/// `kind`, in `mode`. Reproducible from its parameters — no randomness inside.
#[derive(Clone, Copy, Debug)]
pub struct FaultPlan {
    pub kind: FaultKind,
    /// 1-based occurrence of `kind` at which to fire. `0` or a value larger than
    /// the number of ops issued means the fault never fires.
    pub fire_at: u64,
    pub mode: FaultMode,
}

/// An [`ObjectStore`] that wraps another and can inject one deterministic fault.
/// Construct with [`FaultObjectStore::new`], hand the returned `Arc` to
/// [`ObjectStorage::with_store`](super::ObjectStorage::with_store), then
/// [`arm`](FaultObjectStore::arm) it once any setup writes (e.g. fence acquire)
/// are done so the per-kind counter aligns with the operations under test.
pub struct FaultObjectStore {
    inner: Arc<dyn ObjectStore>,
    plan: Mutex<Option<FaultPlan>>,
    fired: AtomicBool,
    get_n: AtomicU64,
    put_if_absent_n: AtomicU64,
    put_if_match_n: AtomicU64,
    put_n: AtomicU64,
    delete_n: AtomicU64,
    list_n: AtomicU64,
}

const INJECTED: &str = "injected crash fault";

impl FaultObjectStore {
    /// Wrap `inner`. Starts disarmed (a transparent pass-through until armed).
    pub fn new(inner: Arc<dyn ObjectStore>) -> Arc<FaultObjectStore> {
        Arc::new(FaultObjectStore {
            inner,
            plan: Mutex::new(None),
            fired: AtomicBool::new(false),
            get_n: AtomicU64::new(0),
            put_if_absent_n: AtomicU64::new(0),
            put_if_match_n: AtomicU64::new(0),
            put_n: AtomicU64::new(0),
            delete_n: AtomicU64::new(0),
            list_n: AtomicU64::new(0),
        })
    }

    /// Arm (or re-arm) the wrapper with `plan`, resetting the one-shot latch.
    pub fn arm(&self, plan: FaultPlan) {
        *self.plan.lock().unwrap() = Some(plan);
        self.fired.store(false, Ordering::SeqCst);
    }

    /// Disarm: become a transparent pass-through again.
    pub fn disarm(&self) {
        *self.plan.lock().unwrap() = None;
    }

    /// Whether the armed fault has fired (the crash point was reached).
    pub fn fired(&self) -> bool {
        self.fired.load(Ordering::SeqCst)
    }

    /// Decide whether this `kind` occurrence (`n`, 1-based) trips the fault.
    /// Latches `fired` so a plan fires at most once.
    fn decide(&self, kind: FaultKind, n: u64) -> Option<FaultMode> {
        if self.fired.load(Ordering::SeqCst) {
            return None;
        }
        let plan = *self.plan.lock().unwrap();
        match plan {
            Some(p) if p.kind == kind && p.fire_at == n => {
                self.fired.store(true, Ordering::SeqCst);
                Some(p.mode)
            }
            _ => None,
        }
    }
}

#[async_trait]
impl ObjectStore for FaultObjectStore {
    async fn get(&self, key: &str) -> Result<Option<GetResult>, ObjectError> {
        let n = self.get_n.fetch_add(1, Ordering::SeqCst) + 1;
        match self.decide(FaultKind::Get, n) {
            Some(FaultMode::BeforeOp) => Err(ObjectError::Transient(INJECTED.into())),
            Some(FaultMode::AfterOp) => {
                let _ = self.inner.get(key).await?;
                Err(ObjectError::Transient(INJECTED.into()))
            }
            None => self.inner.get(key).await,
        }
    }

    async fn put_if_absent(&self, key: &str, bytes: &[u8]) -> Result<ETag, ObjectError> {
        let n = self.put_if_absent_n.fetch_add(1, Ordering::SeqCst) + 1;
        match self.decide(FaultKind::PutIfAbsent, n) {
            Some(FaultMode::BeforeOp) => Err(ObjectError::Transient(INJECTED.into())),
            Some(FaultMode::AfterOp) => {
                // Durable first (object store's Ok == durable), then fail the
                // caller: the CAS-append landed but the commit is never acked.
                let _ = self.inner.put_if_absent(key, bytes).await?;
                Err(ObjectError::Transient(INJECTED.into()))
            }
            None => self.inner.put_if_absent(key, bytes).await,
        }
    }

    async fn put_if_match(
        &self,
        key: &str,
        bytes: &[u8],
        etag: &ETag,
    ) -> Result<ETag, ObjectError> {
        let n = self.put_if_match_n.fetch_add(1, Ordering::SeqCst) + 1;
        match self.decide(FaultKind::PutIfMatch, n) {
            Some(FaultMode::BeforeOp) => Err(ObjectError::Transient(INJECTED.into())),
            Some(FaultMode::AfterOp) => {
                let _ = self.inner.put_if_match(key, bytes, etag).await?;
                Err(ObjectError::Transient(INJECTED.into()))
            }
            None => self.inner.put_if_match(key, bytes, etag).await,
        }
    }

    async fn put(&self, key: &str, bytes: &[u8]) -> Result<ETag, ObjectError> {
        let n = self.put_n.fetch_add(1, Ordering::SeqCst) + 1;
        match self.decide(FaultKind::Put, n) {
            Some(FaultMode::BeforeOp) => Err(ObjectError::Transient(INJECTED.into())),
            Some(FaultMode::AfterOp) => {
                let _ = self.inner.put(key, bytes).await?;
                Err(ObjectError::Transient(INJECTED.into()))
            }
            None => self.inner.put(key, bytes).await,
        }
    }

    async fn delete(&self, key: &str) -> Result<(), ObjectError> {
        let n = self.delete_n.fetch_add(1, Ordering::SeqCst) + 1;
        match self.decide(FaultKind::Delete, n) {
            Some(FaultMode::BeforeOp) => Err(ObjectError::Transient(INJECTED.into())),
            Some(FaultMode::AfterOp) => {
                self.inner.delete(key).await?;
                Err(ObjectError::Transient(INJECTED.into()))
            }
            None => self.inner.delete(key).await,
        }
    }

    async fn list(&self, prefix: &str) -> Result<Vec<String>, ObjectError> {
        let n = self.list_n.fetch_add(1, Ordering::SeqCst) + 1;
        match self.decide(FaultKind::List, n) {
            Some(FaultMode::BeforeOp) => Err(ObjectError::Transient(INJECTED.into())),
            Some(FaultMode::AfterOp) => {
                let _ = self.inner.list(prefix).await?;
                Err(ObjectError::Transient(INJECTED.into()))
            }
            None => self.inner.list(prefix).await,
        }
    }
}
