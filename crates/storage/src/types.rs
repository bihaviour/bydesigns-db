//! Associated types for [`crate::Storage`].
//!
//! These types are part of the stable seam between the engine (spec 02) and any
//! durability backend (spec 03 / 04). Their byte/wire representation is governed
//! by [`crate::STORAGE_TRAIT_VERSION`]; changing it is a breaking change.

use std::time::Instant;

/// Fixed page size. Compile-time constant; all backends MUST agree (spec 03).
pub const PAGE_SIZE: usize = 4096;

/// Page identity. Stable for the life of the database; never reused after free.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct PageId(pub u64);

/// Log Sequence Number. Strictly monotonic, gap-free per database, never reused.
/// Totally orders every durable mutation and every read snapshot (spec 03).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
pub struct Lsn(pub u64);

impl Lsn {
    /// The zero LSN: the state of a fresh database before any commit.
    pub const ZERO: Lsn = Lsn(0);
}

/// A fixed-size page buffer plus the LSN that produced this version.
#[derive(Clone)]
pub struct Page {
    pub id: PageId,
    /// Version stamp: the LSN this image reflects (`Page.lsn <= requested lsn`).
    pub lsn: Lsn,
    pub bytes: Box<[u8; PAGE_SIZE]>,
}

impl Page {
    /// Build a page from `data`, zero-padding or truncating to `PAGE_SIZE`.
    pub fn from_slice(id: PageId, lsn: Lsn, data: &[u8]) -> Page {
        let mut bytes = Box::new([0u8; PAGE_SIZE]);
        let n = data.len().min(PAGE_SIZE);
        bytes[..n].copy_from_slice(&data[..n]);
        Page { id, lsn, bytes }
    }
}

impl std::fmt::Debug for Page {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Page")
            .field("id", &self.id)
            .field("lsn", &self.lsn)
            .field("bytes", &format_args!("[{} bytes]", PAGE_SIZE))
            .finish()
    }
}

/// One opaque WAL record. The backend does NOT interpret it; it only stores it
/// durably and orders it. Encoding/semantics are owned by Engine Core (spec 02).
#[derive(Clone, PartialEq, Eq)]
pub struct WalRecord {
    /// Pre-serialized by the engine.
    pub bytes: Vec<u8>,
}

impl WalRecord {
    pub fn new(bytes: impl Into<Vec<u8>>) -> WalRecord {
        WalRecord {
            bytes: bytes.into(),
        }
    }
}

impl std::fmt::Debug for WalRecord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WalRecord")
            .field("len", &self.bytes.len())
            .finish()
    }
}

/// A durable WAL record paired with the LSN the backend assigned it on append.
///
/// Returned by [`crate::Storage::scan_wal`] for crash recovery (a v1 addition to
/// the source trait: the engine rebuilds in-memory state by replaying the log).
#[derive(Clone, Debug)]
pub struct LogEntry {
    pub lsn: Lsn,
    pub record: WalRecord,
}

/// Read-only counters a backend exposes through [`crate::Storage::stats`] — the
/// seam-safe observability surface (spec 15 / #53). Every field is a
/// backend-neutral cumulative total: counts and byte/latency aggregates, never
/// a backend-specific concept (no S3 key, file offset, or LSM layer id crosses
/// the seam). A field meaningless for a backend stays `0` (e.g. `cache_*` and
/// `fetch_latency_us_total` are unused by the in-memory-paged
/// [`crate::LocalFileStorage`] but live for object stores). Cumulative, so a
/// consumer takes the delta between two snapshots; `Copy`, so sampling never
/// allocates or blocks the hot path. The future observability/OTLP exporter
/// reads the same struct.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct StorageStats {
    /// Durable WAL appends performed (`append_wal` calls that became durable).
    pub wal_appends: u64,
    /// Total encoded WAL bytes appended.
    pub wal_bytes: u64,
    /// Page versions read (`get_page`/`get_pages`, per page).
    pub page_reads: u64,
    /// Total page bytes returned by reads.
    pub page_read_bytes: u64,
    /// Page reads served from a warm cache without a backend fetch.
    pub cache_hits: u64,
    /// Page reads that missed the cache and fetched from the backend.
    pub cache_misses: u64,
    /// Cumulative backend-fetch latency, microseconds (object stores; ~0 local).
    pub fetch_latency_us_total: u64,
    /// `fsync`/durable-flush operations performed.
    pub fsyncs: u64,
}

/// Unique per engine instance / process; identifies a fence holder.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct WriterId(pub u128);

/// Single-writer fence token. Monotonic `epoch` is the CAS key; any acquire
/// strictly increases it, fencing every prior holder. `lease_until` bounds
/// in-memory validity so a crashed holder's token expires (spec 03).
#[derive(Clone, Debug)]
pub struct FenceToken {
    pub epoch: u64,
    pub owner: WriterId,
    pub lease_until: Instant,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub struct BranchId(pub u64);

impl BranchId {
    /// The root line every database starts on. Branches fork off it (or off
    /// another branch); `parent == ROOT` means "forked from the main line".
    pub const ROOT: BranchId = BranchId(0);
}

/// Resolution of a branch to its fork point, parent, and current head.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct BranchRef {
    pub id: BranchId,
    /// The branch this one forked from (`ROOT` = the main line).
    pub parent: BranchId,
    /// Fork point on the parent.
    pub base_lsn: Lsn,
    /// Current head of this branch (`head == base` for a fresh branch).
    pub head_lsn: Lsn,
}

/// Error taxonomy. Distinguishes RETRYABLE transients from FATAL faults and,
/// critically, the FENCED case so the engine can step down a stale writer.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum StorageError {
    /// Page/branch/LSN does not exist (e.g. requested LSN below PITR floor).
    #[error("not found: {0}")]
    NotFound(String),

    /// This writer has been fenced; a newer epoch holds the token. The engine
    /// MUST stop writing immediately and not retry under the same token.
    #[error("fenced: current epoch {current} > held {held}")]
    Fenced { held: u64, current: u64 },

    /// CAS lost a race for the fence (another writer won). Caller MAY back off.
    #[error("contended: fence acquire lost the CAS race")]
    Contended,

    /// Transient backend failure (S3 5xx, throttling, timeout). RETRYABLE with
    /// backoff; carries no durability claim either way.
    #[error("transient: {0}")]
    Transient(String),

    /// Durability could NOT be confirmed. The commit MUST be treated as failed;
    /// the engine MUST NOT ack. Never silently coerced to success.
    #[error("durability unconfirmed: {0}")]
    DurabilityUnconfirmed(String),

    /// Detected corruption / checksum mismatch on a materialized page.
    #[error("corruption: {0}")]
    Corruption(String),

    /// Configuration/usage error (bad URL scheme, wrong page size). FATAL.
    #[error("invalid: {0}")]
    Invalid(String),
}

impl StorageError {
    /// Whether a caller may retry the operation (outcome possibly unknown).
    pub fn is_retryable(&self) -> bool {
        matches!(self, StorageError::Transient(_) | StorageError::Contended)
    }
}
