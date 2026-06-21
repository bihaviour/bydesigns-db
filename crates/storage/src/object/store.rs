//! The thin object-client seam (spec 04 — "MAY abstract the store behind a thin
//! object-client trait so AWS S3, R2, and MinIO are swapped by config, not
//! rebuild").
//!
//! [`ObjectStorage`](super::ObjectStorage) is written entirely against this
//! trait, so the durability floor is a configuration choice, not a code change:
//! the in-memory [`MemObjectStore`](super::MemObjectStore) for fast tests, the
//! durable [`FsObjectStore`](super::FsObjectStore) for the crash-safety gate and
//! the MinIO/self-hosted target, and (later, behind the same trait) an AWS-SDK
//! or R2-binding client for the cloud tiers.
//!
//! The backend depends on exactly the primitives spec 04 names: GET, PUT, the
//! two **conditional** writes (`put-if-absent` / `put-if-match` — the 2026 CAS
//! unlock that gives ordered append + single-writer fencing with no consensus
//! cluster), DELETE, and LIST. Nothing backend-specific (S3 keys, file offsets)
//! leaks above this line.

use async_trait::async_trait;

/// An opaque store-assigned version tag for an object, used as the `If-Match`
/// precondition on a conditional overwrite. Stable for a given object content.
pub type ETag = String;

/// Failures the object client surfaces. [`ObjectStorage`](super::ObjectStorage)
/// maps these onto [`StorageError`](crate::StorageError); the one that carries
/// real protocol meaning is [`ObjectError::Precondition`] — a failed conditional
/// write (HTTP 412), which is how a CAS race / fencing event is detected.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ObjectError {
    /// The object does not exist (a GET/DELETE/`put-if-match` on a missing key).
    #[error("object not found: {0}")]
    NotFound(String),

    /// A conditional write's precondition failed — the slot is already taken
    /// (`put-if-absent`) or the ETag no longer matches (`put-if-match`). This is
    /// the CAS-lost signal the commit log and fence build on.
    #[error("precondition failed (CAS lost) on {0}")]
    Precondition(String),

    /// A transient backend fault (5xx, throttling, timeout). Retryable; carries
    /// no durability claim either way.
    #[error("transient object-store error: {0}")]
    Transient(String),
}

/// A fetched object: its bytes plus the ETag to use as a later `If-Match`.
pub struct GetResult {
    pub bytes: Vec<u8>,
    pub etag: ETag,
}

/// The narrow S3-compatible object client. All methods are `async`: the cloud
/// impls are network-bound; [`MemObjectStore`](super::MemObjectStore) and
/// [`FsObjectStore`](super::FsObjectStore) resolve synchronously underneath.
///
/// **Durability contract:** when a `put*` returns `Ok`, the object is durable —
/// a crash immediately after MUST leave the object fully readable (no torn
/// object ever becomes visible). This is what lets the commit log treat a
/// successful CAS as the commit point.
#[async_trait]
pub trait ObjectStore: Send + Sync + 'static {
    /// Read an object, or `None` if the key is absent.
    async fn get(&self, key: &str) -> Result<Option<GetResult>, ObjectError>;

    /// Conditional create (`If-None-Match: *`). Fails [`ObjectError::Precondition`]
    /// if the key already exists. The atomic-ordered-append + fencing primitive.
    async fn put_if_absent(&self, key: &str, bytes: &[u8]) -> Result<ETag, ObjectError>;

    /// Conditional overwrite (`If-Match: <etag>`). Fails [`ObjectError::Precondition`]
    /// if the key is absent or its ETag differs. Used to advance mutable heads
    /// (the writer lease) so two writers cannot both win an advance.
    async fn put_if_match(&self, key: &str, bytes: &[u8], etag: &ETag)
        -> Result<ETag, ObjectError>;

    /// Unconditional overwrite. Only valid for objects the single writer fully
    /// owns (e.g. its own retention marker); never used to establish order.
    async fn put(&self, key: &str, bytes: &[u8]) -> Result<ETag, ObjectError>;

    /// Delete an object. Succeeds (idempotently) even if the key is absent.
    async fn delete(&self, key: &str) -> Result<(), ObjectError>;

    /// List every key under `prefix` (no pagination at this layer; the prefixes
    /// the backend lists — log/, delta/, image/, branches/ — are bounded).
    async fn list(&self, prefix: &str) -> Result<Vec<String>, ObjectError>;
}
