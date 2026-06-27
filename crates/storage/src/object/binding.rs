//! `BindingObjectStore` — the R2-**binding** object client (EX-1 / #100, spec 11).
//!
//! The WASM/Cloudflare-Workers deployment target is **a port, not a recompile**
//! (spec 11): the [`Storage`](crate::Storage) trait and the engine core stay
//! unchanged; only the executor threading and the *backend binding* fork. This
//! is that backend-binding fork — and it composes over the **same**
//! [`ObjectStorage`](super::ObjectStorage) (LSM page store + CAS commit log) by
//! being just another [`ObjectStore`] behind the thin object-client seam.
//!
//! Why a distinct client at all? In a Worker isolate there are no S3 credentials
//! and (often) no raw outbound TCP — the durable store is reached through the
//! Worker's **R2 binding**, a host object the runtime injects. So instead of the
//! HTTP/S3 client the `s3://` tier uses, the get/put/delete/list primitives are
//! forwarded across the JS↔WASM boundary to that binding. R2 exposes exactly the
//! two conditional writes the commit log + fence need (`If-None-Match: *` /
//! `If-Match: <etag>`), so the CAS-append-and-fence design carries over verbatim.
//!
//! [`BindingObjectStore`] is generic over a [`BindingHost`] — the host-call seam
//! — so it is fully testable off-target with the in-memory [`MemBindingHost`].
//! The concrete Worker host (forwarding to the real R2 binding over host imports)
//! lives in the Worker entrypoint crate behind `#[cfg(target_arch = "wasm32")]`;
//! it is described in `pages/specs/11-deployment-targets.html` and is the one
//! piece that cannot be exercised off a Worker.
//!
//! The durability contract is unchanged: a `put*` returns `Ok` only once the
//! object is durable in R2, so the commit log still treats a successful CAS as
//! the commit point. Nothing R2-specific leaks above the [`ObjectStore`] line.

use super::store::{ETag, GetResult, ObjectError, ObjectStore};
use async_trait::async_trait;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

/// Failures the host binding surfaces, mapped onto [`ObjectError`] at the seam.
/// The load-bearing one is [`BindingError::Precondition`] — a failed conditional
/// write (R2 returns a 412), the CAS-lost / fencing signal.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum BindingError {
    /// The key is absent (GET/`put-if-match`/DELETE on a missing object).
    #[error("binding: object not found: {0}")]
    NotFound(String),
    /// A conditional write's precondition failed (the slot is taken / the ETag
    /// no longer matches). The CAS-lost signal the commit log + fence build on.
    #[error("binding: precondition failed (CAS lost) on {0}")]
    Precondition(String),
    /// A transient host/binding fault (throttling, timeout). Retryable; carries
    /// no durability claim either way.
    #[error("binding: transient host error: {0}")]
    Transient(String),
}

impl From<BindingError> for ObjectError {
    fn from(e: BindingError) -> ObjectError {
        match e {
            BindingError::NotFound(k) => ObjectError::NotFound(k),
            BindingError::Precondition(k) => ObjectError::Precondition(k),
            BindingError::Transient(m) => ObjectError::Transient(m),
        }
    }
}

/// The host-call seam: the small set of R2-binding operations the Worker glue
/// forwards across the JS↔WASM boundary. Synchronous because the Worker
/// entrypoint bridges the R2 promises to the single-threaded WASM executor
/// before re-entering Rust (see the `wasm` variant of
/// [`block_on`](crate::block_on)); keeping the trait host-agnostic is what lets
/// the whole backend be unit-tested off-target via [`MemBindingHost`].
///
/// Contract mirrors [`ObjectStore`]: a `put*` that returns `Ok` is durable.
pub trait BindingHost: Send + Sync + 'static {
    /// Read an object's `(bytes, etag)`, or `None` if absent.
    fn get(&self, key: &str) -> Result<Option<(Vec<u8>, ETag)>, BindingError>;
    /// Conditional create (`If-None-Match: *`) — fails `Precondition` if present.
    fn put_if_absent(&self, key: &str, bytes: &[u8]) -> Result<ETag, BindingError>;
    /// Conditional overwrite (`If-Match: <etag>`) — fails `Precondition` on a
    /// missing key or stale ETag.
    fn put_if_match(&self, key: &str, bytes: &[u8], etag: &ETag) -> Result<ETag, BindingError>;
    /// Unconditional overwrite of a single-writer-owned object.
    fn put(&self, key: &str, bytes: &[u8]) -> Result<ETag, BindingError>;
    /// Delete (idempotent — absent key still succeeds).
    fn delete(&self, key: &str) -> Result<(), BindingError>;
    /// List every key under `prefix`.
    fn list(&self, prefix: &str) -> Result<Vec<String>, BindingError>;
}

/// An [`ObjectStore`] that bottoms out on a [`BindingHost`] (the Worker R2
/// binding). Drop it into [`ObjectStorage::with_store`](super::ObjectStorage::with_store)
/// and the *same* engine + LSM/CAS backend runs in a Worker isolate against R2 —
/// the storage-disaggregated path, no seam moved.
pub struct BindingObjectStore<H: BindingHost> {
    host: H,
}

impl<H: BindingHost> BindingObjectStore<H> {
    pub fn new(host: H) -> BindingObjectStore<H> {
        BindingObjectStore { host }
    }
}

#[async_trait]
impl<H: BindingHost> ObjectStore for BindingObjectStore<H> {
    async fn get(&self, key: &str) -> Result<Option<GetResult>, ObjectError> {
        Ok(self
            .host
            .get(key)?
            .map(|(bytes, etag)| GetResult { bytes, etag }))
    }

    async fn put_if_absent(&self, key: &str, bytes: &[u8]) -> Result<ETag, ObjectError> {
        Ok(self.host.put_if_absent(key, bytes)?)
    }

    async fn put_if_match(
        &self,
        key: &str,
        bytes: &[u8],
        etag: &ETag,
    ) -> Result<ETag, ObjectError> {
        Ok(self.host.put_if_match(key, bytes, etag)?)
    }

    async fn put(&self, key: &str, bytes: &[u8]) -> Result<ETag, ObjectError> {
        Ok(self.host.put(key, bytes)?)
    }

    async fn delete(&self, key: &str) -> Result<(), ObjectError> {
        Ok(self.host.delete(key)?)
    }

    async fn list(&self, prefix: &str) -> Result<Vec<String>, ObjectError> {
        Ok(self.host.list(prefix)?)
    }
}

/// An in-process [`BindingHost`] with strong CAS semantics — the off-target
/// stand-in for the Worker R2 binding. Mirrors
/// [`MemObjectStore`](super::MemObjectStore), but reached through the binding
/// seam, so a test proves the *port* composes (engine → `ObjectStorage` →
/// binding) without a Worker. Not durable across process exit.
#[derive(Default)]
pub struct MemBindingHost {
    map: Mutex<BTreeMap<String, (Vec<u8>, ETag)>>,
    etag_seq: AtomicU64,
}

impl MemBindingHost {
    pub fn new() -> MemBindingHost {
        MemBindingHost::default()
    }

    fn next_etag(&self) -> ETag {
        format!("b{}", self.etag_seq.fetch_add(1, Ordering::Relaxed))
    }
}

impl BindingHost for MemBindingHost {
    fn get(&self, key: &str) -> Result<Option<(Vec<u8>, ETag)>, BindingError> {
        let map = self.map.lock().unwrap();
        Ok(map.get(key).map(|(b, e)| (b.clone(), e.clone())))
    }

    fn put_if_absent(&self, key: &str, bytes: &[u8]) -> Result<ETag, BindingError> {
        let mut map = self.map.lock().unwrap();
        if map.contains_key(key) {
            return Err(BindingError::Precondition(key.to_string()));
        }
        let etag = self.next_etag();
        map.insert(key.to_string(), (bytes.to_vec(), etag.clone()));
        Ok(etag)
    }

    fn put_if_match(&self, key: &str, bytes: &[u8], etag: &ETag) -> Result<ETag, BindingError> {
        let mut map = self.map.lock().unwrap();
        match map.get(key) {
            Some((_, cur)) if cur == etag => {
                let new = self.next_etag();
                map.insert(key.to_string(), (bytes.to_vec(), new.clone()));
                Ok(new)
            }
            _ => Err(BindingError::Precondition(key.to_string())),
        }
    }

    fn put(&self, key: &str, bytes: &[u8]) -> Result<ETag, BindingError> {
        let mut map = self.map.lock().unwrap();
        let etag = self.next_etag();
        map.insert(key.to_string(), (bytes.to_vec(), etag.clone()));
        Ok(etag)
    }

    fn delete(&self, key: &str) -> Result<(), BindingError> {
        self.map.lock().unwrap().remove(key);
        Ok(())
    }

    fn list(&self, prefix: &str) -> Result<Vec<String>, BindingError> {
        let map = self.map.lock().unwrap();
        Ok(map
            .keys()
            .filter(|k| k.starts_with(prefix))
            .cloned()
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::{Config, ObjectStorage};
    use crate::{block_on, Storage, WalRecord, WriterId};
    use std::sync::Arc;

    /// The port's exit-criterion shape, at the storage seam: an `ObjectStorage`
    /// backed by the R2 *binding* (not S3) durably appends a WAL record and reads
    /// it back — the "SELECT 1 against R2 from a Worker" reduced to the seam the
    /// engine actually calls, with no Worker and no network.
    #[test]
    fn binding_backed_object_storage_round_trips_durably() {
        let host = MemBindingHost::new();
        let store = BindingObjectStore::new(host);
        let s =
            ObjectStorage::with_store(Arc::new(store), "db/wasm-port/", Config::default()).unwrap();

        let token = block_on(s.acquire_fence(WriterId(1))).unwrap();
        let lsn = block_on(s.append_wal(&token, &[WalRecord::new(b"hello-r2".to_vec())])).unwrap();
        assert!(lsn.0 >= 1, "append returns a durable commit LSN");

        // The append is durable and replayable through the same recovery path the
        // engine uses on cold start (scale-to-zero re-warm).
        let entries = block_on(s.scan_wal(crate::Lsn::ZERO)).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].record.bytes, b"hello-r2");
        assert_eq!(block_on(s.get_commit_lsn()).unwrap(), lsn);
    }

    /// The conditional-write primitive R2 provides is real CAS through the
    /// binding: only one `put_if_absent` wins a slot (the fence / ordered-append
    /// guarantee the commit log relies on).
    #[test]
    fn binding_put_if_absent_is_cas() {
        let host = MemBindingHost::new();
        assert!(host.put_if_absent("log/1", b"a").is_ok());
        assert!(matches!(
            host.put_if_absent("log/1", b"b"),
            Err(BindingError::Precondition(_))
        ));
        // The winner's bytes stand.
        assert_eq!(host.get("log/1").unwrap().unwrap().0, b"a");
    }
}
