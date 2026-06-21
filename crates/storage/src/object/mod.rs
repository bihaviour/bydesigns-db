//! `ObjectStorage` — the disaggregated, scale-to-zero backend (spec 04).
//!
//! This is the Phase-2 deliverable: a second [`Storage`] implementation that
//! makes the engine storage-disaggregated **without changing the engine or the
//! C ABI** — `open_storage` flips from `file://` to `s3://`/`r2://`/`gs://` and
//! the same compiled library now bottoms out on object storage. Everything above
//! the [`Storage`] seam is untouched.
//!
//! It is two cooperating sub-systems over one immutable, LSN-versioned object
//! namespace (one prefix per database), both terminating on the same
//! [`ObjectStore`] durability floor:
//!
//! * **Commit log** — an ordered, append-only WAL. Each `append_wal` /
//!   `put_page` writes one segment at `log/<seq>` with **put-if-absent** (S3
//!   conditional write / CAS). A successful CAS *is* the commit point and *is*
//!   the fence: only one writer can win a slot, so a stale writer fences itself
//!   off. No Raft/Paxos/ZooKeeper anywhere (the 2026 "log on S3" unlock).
//! * **Page store** — an LSM tree. Committed page images land in an in-memory
//!   memtable; a flush serializes the memtable to an immutable `delta/` layer;
//!   compaction folds layers at-or-below the PITR floor into an `image/` layer;
//!   GC reclaims the superseded objects. `get_page` resolves the at-or-before
//!   version by scanning memtable → deltas (newest→oldest, pruned by LSN span) →
//!   the image floor.
//!
//! **Durability rule (non-negotiable, spec 04 / §8 Exp 4):** a commit is acked
//! only after its segment's conditional PUT returns `Ok` — the object store's
//! contract is that a returned `Ok` is durable. The memtable/delta/image
//! machinery is a *derived read cache* over the log; losing it loses nothing,
//! because recovery replays the durable log forward to rebuild it.
//!
//! ## Phase-2 boundaries (deliberate, see `pages/specs/phase-2-object-storage.html`)
//!
//! * The async [`ObjectStore`] futures are driven to completion synchronously
//!   under the backend's lock (via the crate `block_on`), mirroring the
//!   synchronous C-ABI commit path. A fully pipelined async path and group
//!   commit are later optimizations; the *signatures* are already async, so they
//!   drop in without moving the seam.
//! * Live layer membership is tracked in memory and rediscovered by `list` on
//!   open (valid because the design is single-writer-per-DB and every object is
//!   immutably named); a single mutable `manifest` pointer is an optimization
//!   for cutting LIST cost at scale, not a correctness requirement here.

mod codec;
mod fs;
mod mem;
mod store;

pub use fs::FsObjectStore;
pub use mem::MemObjectStore;
pub use store::{ETag, GetResult, ObjectError, ObjectStore};

use crate::types::*;
use crate::Storage;
use async_trait::async_trait;
use codec::{LogItem, PageRecord};
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const FENCE_LEASE: Duration = Duration::from_secs(10);

/// Wall-clock milliseconds since the Unix epoch (lease expiry stamp). Lease
/// liveness is advisory: fencing correctness rests on the monotonic CAS epoch,
/// not the clock. The stamp lets a peer observe whether a writer is still alive.
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Lease object payload: `epoch:u64 | owner:u128 | expires_at_ms:u64`. The epoch
/// stays first so the legacy 8-byte reader (`durable_epoch`) keeps working.
fn encode_lease(epoch: u64, owner: u128, expires_ms: u64) -> Vec<u8> {
    let mut b = Vec::with_capacity(32);
    b.extend_from_slice(&epoch.to_le_bytes());
    b.extend_from_slice(&owner.to_le_bytes());
    b.extend_from_slice(&expires_ms.to_le_bytes());
    b
}

/// Tunables for the LSM page store and the CAS commit log (spec 04 §Configuration).
#[derive(Clone, Debug)]
pub struct Config {
    /// Memtable size that triggers an automatic flush to a delta layer.
    pub flush_threshold_bytes: usize,
    /// Bounded retries on benign CAS contention before declaring a fence event.
    pub cas_max_retries: u32,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            flush_threshold_bytes: 64 * 1024 * 1024,
            cas_max_retries: 8,
        }
    }
}

/// Live metadata for an immutable delta layer covering the inclusive LSN span.
#[derive(Clone)]
struct DeltaMeta {
    lo: u64,
    hi: u64,
    key: String,
}

/// Live metadata for the image layer (the read-path floor) at `image_lsn`.
#[derive(Clone)]
struct ImageMeta {
    image_lsn: u64,
    key: String,
}

/// All mutable backend state, behind one lock (the design is single-writer).
struct Inner {
    /// Next commit-log slot to claim (put-if-absent at `log/<next_seq>`).
    next_seq: u64,
    /// Next LSN to assign; one per log item, gap-free.
    next_lsn: u64,
    durable_lsn: u64,
    commit_lsn: u64,
    /// Writer-lease epoch we last observed/own; the durable lease object is truth.
    epoch: u64,
    retention_floor: u64,
    /// (page_id, lsn) -> image, for committed-but-unflushed page versions.
    memtable: BTreeMap<(u64, u64), Vec<u8>>,
    memtable_bytes: usize,
    /// Highest LSN captured by a durable layer; memtable holds only `> flushed_hw`.
    flushed_hw: u64,
    deltas: Vec<DeltaMeta>,
    image: Option<ImageMeta>,
    branches: HashMap<u64, BranchRef>,
    next_branch_id: u64,
}

/// The disaggregated backend. Construct via [`ObjectStorage::open`] (URL → an
/// [`FsObjectStore`] floor) or [`ObjectStorage::with_store`] (any object client).
pub struct ObjectStorage {
    store: Arc<dyn ObjectStore>,
    /// Per-database key prefix, e.g. `db/<db_id>/`. All keys are built under it.
    prefix: String,
    config: Config,
    inner: Mutex<Inner>,
    /// Parsed immutable layers, keyed by object key. Safe to cache forever
    /// because layer objects are write-once (spec 04 "immutability is the safety
    /// property"). This is the local cache that keeps S3 latency off reads.
    cache: Mutex<HashMap<String, Arc<Vec<PageRecord>>>>,
}

impl ObjectStorage {
    /// Open against an explicit object client (any [`ObjectStore`] impl). The
    /// cloud tiers (AWS S3 / R2) drop in here behind the same trait, no change
    /// to anything below. `prefix` is the per-database key prefix.
    pub fn with_store(
        store: Arc<dyn ObjectStore>,
        prefix: &str,
        config: Config,
    ) -> Result<ObjectStorage, StorageError> {
        let prefix = if prefix.is_empty() || prefix.ends_with('/') {
            prefix.to_string()
        } else {
            format!("{prefix}/")
        };
        let s = ObjectStorage {
            store,
            prefix,
            config,
            inner: Mutex::new(Inner {
                next_seq: 1,
                next_lsn: 1,
                durable_lsn: 0,
                commit_lsn: 0,
                epoch: 0,
                retention_floor: 0,
                memtable: BTreeMap::new(),
                memtable_bytes: 0,
                flushed_hw: 0,
                deltas: Vec::new(),
                image: None,
                branches: HashMap::new(),
                next_branch_id: 1,
            }),
            cache: Mutex::new(HashMap::new()),
        };
        s.recover()?;
        Ok(s)
    }

    /// Open the database named by an object-storage URL, backed by a durable
    /// [`FsObjectStore`] floor (the MinIO/self-hosted tier). The bucket maps to a
    /// directory under `$BYDESIGNS_OBJECT_ROOT` (default: a temp dir), so the
    /// same URL reopens the same durable data across process restarts.
    pub fn open(url: &str) -> Result<ObjectStorage, StorageError> {
        let (bucket, db_id) = parse_object_url(url)?;
        let base = std::env::var_os("BYDESIGNS_OBJECT_ROOT")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::env::temp_dir().join("bydesigns-object"));
        let root = base.join(&bucket);
        let store = fs::FsObjectStore::open(&root)
            .map_err(|e| StorageError::Invalid(format!("object root {root:?}: {e}")))?;
        ObjectStorage::with_store(Arc::new(store), &format!("db/{db_id}/"), Config::default())
    }

    /// Open a branch's private write overlay: a second `ObjectStorage` rooted at
    /// the parent's `branches/<id>/` sub-prefix over the *same* durable floor, so
    /// the branch's diverged log/layers persist beside (but isolated from) the
    /// base. Used by [`crate::open_branch`] for `s3://`/`r2://`/`gs://`.
    pub fn open_branch_overlay(url: &str, branch: BranchId) -> Result<ObjectStorage, StorageError> {
        let (bucket, db_id) = parse_object_url(url)?;
        let base = std::env::var_os("BYDESIGNS_OBJECT_ROOT")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::env::temp_dir().join("bydesigns-object"));
        let root = base.join(&bucket);
        let store = fs::FsObjectStore::open(&root)
            .map_err(|e| StorageError::Invalid(format!("object root {root:?}: {e}")))?;
        let prefix = format!("db/{db_id}/branches/{:020}/", branch.0);
        ObjectStorage::with_store(Arc::new(store), &prefix, Config::default())
    }

    // ---- key helpers -----------------------------------------------------
    fn lease_key(&self) -> String {
        format!("{}lease", self.prefix)
    }
    fn retention_key(&self) -> String {
        format!("{}retention", self.prefix)
    }
    fn log_prefix(&self) -> String {
        format!("{}log/", self.prefix)
    }
    fn log_key(&self, seq: u64) -> String {
        format!("{}log/{seq:020}", self.prefix)
    }
    fn delta_key(&self, lo: u64, hi: u64) -> String {
        format!("{}delta/L{lo:020}-L{hi:020}.delta", self.prefix)
    }
    fn image_key(&self, lsn: u64) -> String {
        format!("{}image/img-L{lsn:020}.image", self.prefix)
    }
    /// Branch pointer object. Suffixed `.ptr` so it never collides with the
    /// branch's private overlay objects under `branches/<id>/`.
    fn branch_key(&self, id: u64) -> String {
        format!("{}branches/{id:020}.ptr", self.prefix)
    }

    // ---- synchronous object-store bridges (no .await held across the lock) -
    fn obj_get(&self, key: &str) -> Result<Option<GetResult>, StorageError> {
        crate::block_on(self.store.get(key)).map_err(map_obj_err)
    }
    fn obj_put_if_absent(&self, key: &str, bytes: &[u8]) -> Result<(), ObjectError> {
        crate::block_on(self.store.put_if_absent(key, bytes)).map(|_| ())
    }
    fn obj_put(&self, key: &str, bytes: &[u8]) -> Result<(), StorageError> {
        crate::block_on(self.store.put(key, bytes))
            .map(|_| ())
            .map_err(map_obj_err)
    }
    fn obj_delete(&self, key: &str) -> Result<(), StorageError> {
        crate::block_on(self.store.delete(key)).map_err(map_obj_err)
    }
    fn obj_list(&self, prefix: &str) -> Result<Vec<String>, StorageError> {
        crate::block_on(self.store.list(prefix)).map_err(map_obj_err)
    }

    /// Current durable writer epoch (0 if no lease object yet) + its ETag.
    fn durable_epoch(&self) -> Result<(u64, Option<ETag>), StorageError> {
        match self.obj_get(&self.lease_key())? {
            Some(g) if g.bytes.len() >= 8 => {
                let e = u64::from_le_bytes(g.bytes[..8].try_into().unwrap());
                Ok((e, Some(g.etag)))
            }
            Some(g) => Ok((0, Some(g.etag))),
            None => Ok((0, None)),
        }
    }

    /// Reject a writer whose epoch is not the current durable epoch (fenced).
    fn check_fence(&self, token: &FenceToken) -> Result<(), StorageError> {
        let (current, _) = self.durable_epoch()?;
        if token.epoch != current {
            return Err(StorageError::Fenced {
                held: token.epoch,
                current,
            });
        }
        Ok(())
    }

    /// Append one segment of items, CAS-claiming the next log slot. Returns the
    /// LSN of the last item. Retries benign CAS contention; fences on a lost lease.
    fn append_segment(&self, token: &FenceToken, items: Vec<LogItem>) -> Result<Lsn, StorageError> {
        self.check_fence(token)?;
        let mut g = self.inner.lock().unwrap();
        let bytes = codec::encode_segment(&items);

        let mut attempt = 0u32;
        loop {
            let seq = g.next_seq;
            match self.obj_put_if_absent(&self.log_key(seq), &bytes) {
                Ok(()) => break,
                Err(ObjectError::Precondition(_)) => {
                    // Slot taken: another writer raced. Refresh our seq from the
                    // bucket, then re-validate we still hold the lease.
                    g.next_seq = self.scan_next_seq()?;
                    let (current, _) = self.durable_epoch()?;
                    if token.epoch != current {
                        return Err(StorageError::Fenced {
                            held: token.epoch,
                            current,
                        });
                    }
                    attempt += 1;
                    if attempt > self.config.cas_max_retries {
                        return Err(StorageError::Contended);
                    }
                    continue;
                }
                Err(ObjectError::Transient(m)) => return Err(StorageError::Transient(m)),
                Err(ObjectError::NotFound(m)) => return Err(StorageError::Transient(m)),
            }
        }

        // Segment is durable. Assign LSNs and apply page items to the memtable.
        let seq = g.next_seq;
        let first = g.next_lsn;
        let mut has_wal = false;
        for item in &items {
            let lsn = g.next_lsn;
            g.next_lsn += 1;
            match item {
                LogItem::Wal(_) => has_wal = true,
                LogItem::Page { page_id, image } => {
                    g.memtable.insert((*page_id, lsn), image.clone());
                    g.memtable_bytes += image.len();
                }
            }
        }
        let last = g.next_lsn - 1;
        debug_assert_eq!(last, first + items.len() as u64 - 1);
        g.next_seq = seq + 1;
        g.durable_lsn = last;
        if has_wal {
            g.commit_lsn = last;
        }

        if g.memtable_bytes >= self.config.flush_threshold_bytes {
            self.flush_locked(&mut g)?;
        }
        Ok(Lsn(last))
    }

    /// Highest existing log slot + 1, by listing the log prefix.
    fn scan_next_seq(&self) -> Result<u64, StorageError> {
        let keys = self.obj_list(&self.log_prefix())?;
        let max = keys
            .iter()
            .filter_map(|k| k.rsplit('/').next())
            .filter_map(|n| n.parse::<u64>().ok())
            .max()
            .unwrap_or(0);
        Ok(max + 1)
    }

    /// Serialize the memtable to a new immutable delta layer (a no-op if empty).
    fn flush_locked(&self, g: &mut Inner) -> Result<(), StorageError> {
        if g.memtable.is_empty() {
            return Ok(());
        }
        let lo = g.flushed_hw + 1;
        let hi = g.durable_lsn;
        let records: Vec<PageRecord> = g
            .memtable
            .iter()
            .map(|((page_id, lsn), image)| PageRecord {
                page_id: *page_id,
                lsn: *lsn,
                image: image.clone(),
            })
            .collect();
        let key = self.delta_key(lo, hi);
        // Immutable, uniquely named → put-if-absent; a name clash means a logic
        // bug, surfaced rather than silently overwriting.
        self.obj_put_if_absent(&key, &codec::encode_delta(&records))
            .map_err(map_obj_err)?;
        g.deltas.push(DeltaMeta { lo, hi, key });
        g.flushed_hw = hi;
        g.memtable.clear();
        g.memtable_bytes = 0;
        Ok(())
    }

    /// Fold every layer at-or-below the PITR floor into a fresh image layer at
    /// `image_lsn = retention_floor`, retaining layers above the floor so any
    /// snapshot inside the PITR window stays reconstructable. The superseded
    /// objects are dropped from the live set and reclaimed by [`Self::gc`].
    ///
    /// A no-op until a retention floor is set (nothing is reclaimable yet).
    pub async fn compact(&self) -> Result<(), StorageError> {
        let mut g = self.inner.lock().unwrap();
        self.flush_locked(&mut g)?;
        let image_lsn = g.retention_floor;
        if image_lsn == 0 {
            return Ok(());
        }

        // Greatest version per page with lsn <= image_lsn, from old image + deltas.
        let mut merged: BTreeMap<u64, (u64, Vec<u8>)> = BTreeMap::new();
        if let Some(img) = &g.image {
            for r in self.load_layer(&img.key)?.iter() {
                if r.lsn <= image_lsn {
                    merge_keep_greatest(&mut merged, r);
                }
            }
        }
        for d in &g.deltas {
            for r in self.load_layer(&d.key)?.iter() {
                if r.lsn <= image_lsn {
                    merge_keep_greatest(&mut merged, r);
                }
            }
        }

        let records: Vec<PageRecord> = merged
            .into_iter()
            .map(|(page_id, (lsn, image))| PageRecord {
                page_id,
                lsn,
                image,
            })
            .collect();
        let new_key = self.image_key(image_lsn);
        // Re-compacting at an unchanged floor would clash; overwrite is safe
        // because the content is a pure function of the durable layers.
        self.obj_put(&new_key, &codec::encode_image(&records))?;

        // Advance the live set: keep layers straddling/above the floor.
        g.deltas.retain(|d| d.hi > image_lsn);
        g.image = Some(ImageMeta {
            image_lsn,
            key: new_key,
        });
        Ok(())
    }

    /// Physically delete bucket objects no longer in the live set (folded deltas,
    /// superseded images). Safe by construction: the live set references every
    /// layer needed to serve any read at-or-above the retention floor.
    pub async fn gc(&self) -> Result<(), StorageError> {
        let g = self.inner.lock().unwrap();
        let mut live: std::collections::HashSet<&str> =
            g.deltas.iter().map(|d| d.key.as_str()).collect();
        if let Some(img) = &g.image {
            live.insert(img.key.as_str());
        }
        let mut victims = Vec::new();
        for prefix in [
            format!("{}delta/", self.prefix),
            format!("{}image/", self.prefix),
        ] {
            for key in self.obj_list(&prefix)? {
                if !live.contains(key.as_str()) {
                    victims.push(key);
                }
            }
        }
        drop(g);
        for key in &victims {
            self.obj_delete(key)?;
            self.cache.lock().unwrap().remove(key);
        }
        Ok(())
    }

    /// Load and parse an immutable layer object, populating the local cache.
    fn load_layer(&self, key: &str) -> Result<Arc<Vec<PageRecord>>, StorageError> {
        if let Some(hit) = self.cache.lock().unwrap().get(key) {
            return Ok(hit.clone());
        }
        let got = self
            .obj_get(key)?
            .ok_or_else(|| StorageError::Corruption(format!("missing live layer {key}")))?;
        let records = if key.contains("/image/") {
            codec::decode_image(&got.bytes)?
        } else {
            codec::decode_delta(&got.bytes)?
        };
        let arc = Arc::new(records);
        self.cache
            .lock()
            .unwrap()
            .insert(key.to_string(), arc.clone());
        Ok(arc)
    }

    /// Rebuild in-memory state from the durable objects (open / after a crash).
    /// Composes three independent scans: layer discovery (sets the flushed
    /// high-water), commit-log replay (the LSN stream + unflushed memtable), and
    /// branch loading; then reads the lease epoch and retention floor.
    fn recover(&self) -> Result<(), StorageError> {
        let (deltas, image, flushed_hw) = self.discover_layers()?;
        let replay = self.replay_log(flushed_hw)?;
        let (branches, next_branch_id) = self.load_branches()?;
        let (epoch, _) = self.durable_epoch()?;
        let retention_floor = match self.obj_get(&self.retention_key())? {
            Some(g) if g.bytes.len() >= 8 => u64::from_le_bytes(g.bytes[..8].try_into().unwrap()),
            _ => 0,
        };

        let mut g = self.inner.lock().unwrap();
        g.next_seq = replay.max_seq + 1;
        g.next_lsn = replay.next_lsn;
        g.durable_lsn = replay.durable_lsn;
        g.commit_lsn = replay.commit_lsn;
        g.epoch = epoch;
        g.retention_floor = retention_floor;
        g.memtable = replay.memtable;
        g.memtable_bytes = replay.memtable_bytes;
        g.flushed_hw = flushed_hw;
        g.deltas = deltas;
        g.image = image;
        g.branches = branches;
        g.next_branch_id = next_branch_id;
        Ok(())
    }

    /// Discover live delta/image layers from the bucket. Returns the deltas
    /// (ascending by `hi`), the covering image (greatest `image_lsn`), and the
    /// flushed high-water LSN that bounds log replay into the memtable.
    fn discover_layers(&self) -> Result<(Vec<DeltaMeta>, Option<ImageMeta>, u64), StorageError> {
        let mut deltas = Vec::new();
        for key in self.obj_list(&format!("{}delta/", self.prefix))? {
            if let Some((lo, hi)) = parse_delta_name(&key) {
                deltas.push(DeltaMeta { lo, hi, key });
            }
        }
        deltas.sort_by_key(|d| d.hi);

        let mut image: Option<ImageMeta> = None;
        for key in self.obj_list(&format!("{}image/", self.prefix))? {
            if let Some(image_lsn) = parse_image_name(&key) {
                if image
                    .as_ref()
                    .map(|i| image_lsn > i.image_lsn)
                    .unwrap_or(true)
                {
                    image = Some(ImageMeta { image_lsn, key });
                }
            }
        }

        let flushed_hw = deltas
            .iter()
            .map(|d| d.hi)
            .chain(image.iter().map(|i| i.image_lsn))
            .max()
            .unwrap_or(0);
        Ok((deltas, image, flushed_hw))
    }

    /// Replay the durable commit log in slot order, rebuilding the gap-free LSN
    /// stream, the durable/commit marks, and the unflushed memtable tail (page
    /// items with `lsn > flushed_hw`).
    fn replay_log(&self, flushed_hw: u64) -> Result<LogReplay, StorageError> {
        let mut log_keys = self.obj_list(&self.log_prefix())?;
        log_keys.sort();
        let mut max_seq = 0u64;
        let mut next_lsn = 1u64;
        let mut durable_lsn = 0u64;
        let mut commit_lsn = 0u64;
        let mut memtable: BTreeMap<(u64, u64), Vec<u8>> = BTreeMap::new();
        let mut memtable_bytes = 0usize;
        for key in &log_keys {
            if let Some(seq) = key.rsplit('/').next().and_then(|n| n.parse::<u64>().ok()) {
                max_seq = max_seq.max(seq);
            }
            let got = self
                .obj_get(key)?
                .ok_or_else(|| StorageError::Corruption(format!("missing log segment {key}")))?;
            let mut has_wal = false;
            for item in codec::decode_segment(&got.bytes)? {
                let lsn = next_lsn;
                next_lsn += 1;
                durable_lsn = lsn;
                match item {
                    LogItem::Wal(_) => has_wal = true,
                    LogItem::Page { page_id, image } if lsn > flushed_hw => {
                        memtable_bytes += image.len();
                        memtable.insert((page_id, lsn), image);
                    }
                    LogItem::Page { .. } => {}
                }
            }
            if has_wal {
                commit_lsn = durable_lsn;
            }
        }
        Ok(LogReplay {
            max_seq,
            next_lsn,
            durable_lsn,
            commit_lsn,
            memtable,
            memtable_bytes,
        })
    }

    /// Load branch pointers from the bucket; returns the map and the next id.
    fn load_branches(&self) -> Result<(HashMap<u64, BranchRef>, u64), StorageError> {
        let mut branches = HashMap::new();
        let mut next_branch_id = 1u64;
        for key in self.obj_list(&format!("{}branches/", self.prefix))? {
            // Only pointer objects; skip each branch's private overlay objects.
            if !key.ends_with(".ptr") {
                continue;
            }
            let Some(got) = self.obj_get(&key)? else {
                continue;
            };
            if got.bytes.len() >= 24 {
                let id = u64::from_le_bytes(got.bytes[0..8].try_into().unwrap());
                let base = u64::from_le_bytes(got.bytes[8..16].try_into().unwrap());
                let parent = u64::from_le_bytes(got.bytes[16..24].try_into().unwrap());
                branches.insert(
                    id,
                    BranchRef {
                        id: BranchId(id),
                        parent: BranchId(parent),
                        base_lsn: Lsn(base),
                        head_lsn: Lsn(base),
                    },
                );
                next_branch_id = next_branch_id.max(id + 1);
            }
        }
        Ok((branches, next_branch_id))
    }
}

/// Accumulator for [`ObjectStorage::replay_log`].
struct LogReplay {
    max_seq: u64,
    next_lsn: u64,
    durable_lsn: u64,
    commit_lsn: u64,
    memtable: BTreeMap<(u64, u64), Vec<u8>>,
    memtable_bytes: usize,
}

/// Keep the greater-LSN version of a page when merging layers for compaction.
fn merge_keep_greatest(map: &mut BTreeMap<u64, (u64, Vec<u8>)>, r: &PageRecord) {
    map.entry(r.page_id)
        .and_modify(|cur| {
            if r.lsn >= cur.0 {
                *cur = (r.lsn, r.image.clone());
            }
        })
        .or_insert((r.lsn, r.image.clone()));
}

#[async_trait]
impl Storage for ObjectStorage {
    async fn get_page(&self, page_id: PageId, lsn: Lsn) -> Result<Page, StorageError> {
        let query = lsn.0;
        let g = self.inner.lock().unwrap();
        if query < g.retention_floor {
            return Err(StorageError::NotFound(format!(
                "lsn {query} below PITR floor {}",
                g.retention_floor
            )));
        }

        // 1. memtable — the newest (unflushed) versions.
        if let Some(((_, vlsn), image)) = g
            .memtable
            .range((page_id.0, 0)..=(page_id.0, query))
            .next_back()
        {
            return Ok(Page::from_slice(page_id, Lsn(*vlsn), image));
        }

        // 2. delta layers, newest→oldest, pruned by LSN span.
        let mut deltas: Vec<DeltaMeta> = g.deltas.clone();
        let image = g.image.clone();
        drop(g);
        deltas.sort_by_key(|d| std::cmp::Reverse(d.hi));
        for d in &deltas {
            if d.lo > query {
                continue; // span starts after the snapshot; cannot match
            }
            let recs = self.load_layer(&d.key)?;
            if let Some(best) = best_in(&recs, page_id.0, query) {
                return Ok(Page::from_slice(page_id, Lsn(best.lsn), &best.image));
            }
        }

        // 3. image floor.
        if let Some(img) = &image {
            if img.image_lsn <= query {
                let recs = self.load_layer(&img.key)?;
                if let Some(best) = best_in(&recs, page_id.0, query) {
                    return Ok(Page::from_slice(page_id, Lsn(best.lsn), &best.image));
                }
            }
        }

        Err(StorageError::NotFound(format!(
            "page {} has no version at-or-before lsn {query}",
            page_id.0
        )))
    }

    async fn append_wal(
        &self,
        token: &FenceToken,
        records: &[WalRecord],
    ) -> Result<Lsn, StorageError> {
        if records.is_empty() {
            return Err(StorageError::Invalid("append_wal: empty batch".into()));
        }
        let items = records
            .iter()
            .map(|r| LogItem::Wal(r.bytes.clone()))
            .collect();
        self.append_segment(token, items)
    }

    async fn put_page(
        &self,
        token: &FenceToken,
        page_id: PageId,
        image: &[u8],
    ) -> Result<Lsn, StorageError> {
        if image.len() > PAGE_SIZE {
            return Err(StorageError::Invalid(format!(
                "page image {} exceeds PAGE_SIZE {PAGE_SIZE}",
                image.len()
            )));
        }
        self.append_segment(
            token,
            vec![LogItem::Page {
                page_id: page_id.0,
                image: image.to_vec(),
            }],
        )
    }

    async fn scan_wal(&self, after: Lsn) -> Result<Vec<LogEntry>, StorageError> {
        // Recovery read: replay the durable log and return WAL records past
        // `after`, in LSN order. Not on the hot path (engine open).
        let mut log_keys = self.obj_list(&self.log_prefix())?;
        log_keys.sort();
        let mut out = Vec::new();
        let mut lsn = 0u64;
        for key in &log_keys {
            let got = self
                .obj_get(key)?
                .ok_or_else(|| StorageError::Corruption(format!("missing log segment {key}")))?;
            for item in codec::decode_segment(&got.bytes)? {
                lsn += 1;
                if let LogItem::Wal(bytes) = item {
                    if lsn > after.0 {
                        out.push(LogEntry {
                            lsn: Lsn(lsn),
                            record: WalRecord::new(bytes),
                        });
                    }
                }
            }
        }
        Ok(out)
    }

    async fn flush(&self) -> Result<(), StorageError> {
        // The commit log is already durable; settle the derived memtable into a
        // delta layer. Idempotent.
        let mut g = self.inner.lock().unwrap();
        self.flush_locked(&mut g)
    }

    async fn durable_lsn(&self) -> Result<Lsn, StorageError> {
        Ok(Lsn(self.inner.lock().unwrap().durable_lsn))
    }

    async fn get_commit_lsn(&self) -> Result<Lsn, StorageError> {
        Ok(Lsn(self.inner.lock().unwrap().commit_lsn))
    }

    async fn acquire_fence(&self, owner: WriterId) -> Result<FenceToken, StorageError> {
        let ttl = FENCE_LEASE.as_millis() as u64;
        let mut attempt = 0u32;
        loop {
            let (cur_epoch, etag) = self.durable_epoch()?;
            let new_epoch = cur_epoch + 1;
            // A new holder strictly increases the epoch, fencing every prior
            // token (take-over model). The lease stamp is advisory liveness.
            let bytes = encode_lease(new_epoch, owner.0, now_ms() + ttl);
            let res = match &etag {
                Some(e) => crate::block_on(self.store.put_if_match(&self.lease_key(), &bytes, e)),
                None => crate::block_on(self.store.put_if_absent(&self.lease_key(), &bytes)),
            };
            match res {
                Ok(_) => {
                    self.inner.lock().unwrap().epoch = new_epoch;
                    return Ok(FenceToken {
                        epoch: new_epoch,
                        owner,
                        lease_until: Instant::now() + FENCE_LEASE,
                    });
                }
                Err(ObjectError::Precondition(_)) => {
                    attempt += 1;
                    if attempt > self.config.cas_max_retries {
                        return Err(StorageError::Contended);
                    }
                    continue; // another writer advanced the lease; re-read and retry
                }
                Err(e) => return Err(map_obj_err(e)),
            }
        }
    }

    async fn renew_fence(&self, token: &FenceToken) -> Result<FenceToken, StorageError> {
        // Durably re-stamp the lease under the *same* epoch: heartbeat. Fails
        // `Fenced` if a newer writer has taken over (epoch advanced). This is the
        // liveness signal a peer reads before deciding the writer is dead.
        let ttl = FENCE_LEASE.as_millis() as u64;
        let mut attempt = 0u32;
        loop {
            let (current, etag) = self.durable_epoch()?;
            if token.epoch != current {
                return Err(StorageError::Fenced {
                    held: token.epoch,
                    current,
                });
            }
            let bytes = encode_lease(token.epoch, token.owner.0, now_ms() + ttl);
            let res = match &etag {
                Some(e) => crate::block_on(self.store.put_if_match(&self.lease_key(), &bytes, e)),
                None => crate::block_on(self.store.put_if_absent(&self.lease_key(), &bytes)),
            };
            match res {
                Ok(_) => {
                    return Ok(FenceToken {
                        epoch: token.epoch,
                        owner: token.owner,
                        lease_until: Instant::now() + FENCE_LEASE,
                    })
                }
                Err(ObjectError::Precondition(_)) => {
                    attempt += 1;
                    if attempt > self.config.cas_max_retries {
                        return Err(StorageError::Contended);
                    }
                    continue; // raced a concurrent lease write; re-read and retry
                }
                Err(e) => return Err(map_obj_err(e)),
            }
        }
    }

    async fn release_fence(&self, token: FenceToken) -> Result<(), StorageError> {
        // Clean handoff: durably mark the lease expired (expires_at = 0) while
        // KEEPING the epoch, so a peer sees the slot is free immediately yet the
        // released token can never re-pass the fence (a fresh acquire still bumps
        // the epoch). Best-effort: if we've already been fenced, nothing to do.
        let (current, etag) = self.durable_epoch()?;
        if token.epoch != current {
            return Ok(());
        }
        let bytes = encode_lease(token.epoch, token.owner.0, 0);
        if let Some(e) = &etag {
            let _ = crate::block_on(self.store.put_if_match(&self.lease_key(), &bytes, e));
        }
        Ok(())
    }

    async fn create_branch(&self, _name: &str, base_lsn: Lsn) -> Result<BranchId, StorageError> {
        let mut g = self.inner.lock().unwrap();
        if base_lsn.0 < g.retention_floor {
            return Err(StorageError::NotFound(format!(
                "base lsn {} below PITR floor {}",
                base_lsn.0, g.retention_floor
            )));
        }
        let id = g.next_branch_id;
        // Pointer payload: id | base_lsn | parent. Branches created off this
        // line have parent ROOT; branch-of-branch is tracked in the overlay's
        // own namespace (see BranchStorage).
        let mut payload = Vec::with_capacity(24);
        payload.extend_from_slice(&id.to_le_bytes());
        payload.extend_from_slice(&base_lsn.0.to_le_bytes());
        payload.extend_from_slice(&BranchId::ROOT.0.to_le_bytes());
        self.obj_put_if_absent(&self.branch_key(id), &payload)
            .map_err(map_obj_err)?;
        g.next_branch_id += 1;
        let bref = BranchRef {
            id: BranchId(id),
            parent: BranchId::ROOT,
            base_lsn,
            head_lsn: base_lsn,
        };
        g.branches.insert(id, bref);
        Ok(BranchId(id))
    }

    async fn resolve_branch(&self, branch: BranchId) -> Result<BranchRef, StorageError> {
        self.inner
            .lock()
            .unwrap()
            .branches
            .get(&branch.0)
            .copied()
            .ok_or_else(|| StorageError::NotFound(format!("branch {}", branch.0)))
    }

    async fn list_branches(&self) -> Result<Vec<BranchRef>, StorageError> {
        let mut v: Vec<BranchRef> = self
            .inner
            .lock()
            .unwrap()
            .branches
            .values()
            .copied()
            .collect();
        v.sort_by_key(|b| b.id.0);
        Ok(v)
    }

    async fn delete_branch(&self, branch: BranchId) -> Result<(), StorageError> {
        {
            let g = self.inner.lock().unwrap();
            if !g.branches.contains_key(&branch.0) {
                return Err(StorageError::NotFound(format!("branch {}", branch.0)));
            }
            if g.branches.values().any(|b| b.parent == branch) {
                return Err(StorageError::Invalid(format!(
                    "branch {} has live children; delete them first",
                    branch.0
                )));
            }
        }
        // Drop the pointer, then reclaim only the branch's diverged objects
        // under its private `branches/<id>/` sub-prefix. Shared base layers,
        // which live directly under the database prefix, are never touched.
        self.obj_delete(&self.branch_key(branch.0))?;
        let overlay_prefix = format!("{}branches/{:020}/", self.prefix, branch.0);
        for key in self.obj_list(&overlay_prefix)? {
            self.obj_delete(&key)?;
            self.cache.lock().unwrap().remove(&key);
        }
        self.inner.lock().unwrap().branches.remove(&branch.0);
        Ok(())
    }

    async fn set_retention_floor(&self, lsn: Lsn) -> Result<(), StorageError> {
        let mut g = self.inner.lock().unwrap();
        if lsn.0 < g.retention_floor {
            return Err(StorageError::Invalid(format!(
                "retention floor must move forward: {} < {}",
                lsn.0, g.retention_floor
            )));
        }
        self.obj_put(&self.retention_key(), &lsn.0.to_le_bytes())?;
        g.retention_floor = lsn.0;
        Ok(())
    }

    async fn pitr_floor(&self) -> Result<Lsn, StorageError> {
        Ok(Lsn(self.inner.lock().unwrap().retention_floor))
    }
}

/// Greatest version of `page_id` with `lsn <= query` within a parsed layer.
fn best_in(recs: &[PageRecord], page_id: u64, query: u64) -> Option<&PageRecord> {
    recs.iter()
        .filter(|r| r.page_id == page_id && r.lsn <= query)
        .max_by_key(|r| r.lsn)
}

fn map_obj_err(e: ObjectError) -> StorageError {
    match e {
        ObjectError::NotFound(m) => StorageError::NotFound(m),
        // A lost CAS on a non-log object is benign contention the caller may retry.
        ObjectError::Precondition(_) => StorageError::Contended,
        ObjectError::Transient(m) => StorageError::Transient(m),
    }
}

/// Parse `s3://bucket/db_id` (or r2://, gs://) into `(bucket, db_id)`.
fn parse_object_url(url: &str) -> Result<(String, String), StorageError> {
    let (scheme, rest) = url
        .split_once("://")
        .ok_or_else(|| StorageError::Invalid(format!("missing scheme in url: {url}")))?;
    if !matches!(scheme, "s3" | "r2" | "gs") {
        return Err(StorageError::Invalid(format!(
            "not an object-storage url: {url}"
        )));
    }
    let rest = rest.split('?').next().unwrap_or(rest);
    let mut parts = rest.splitn(2, '/');
    let bucket = parts
        .next()
        .filter(|b| !b.is_empty())
        .ok_or_else(|| StorageError::Invalid(format!("object url has empty bucket: {url}")))?;
    let db_id = match parts.next() {
        Some(p) if !p.is_empty() => p.trim_end_matches('/'),
        _ => "default",
    };
    // Keep keys filesystem-safe for the FsObjectStore floor.
    let safe = |s: &str| s.replace(['/', '\\'], "_");
    Ok((safe(bucket), safe(db_id)))
}

fn parse_delta_name(key: &str) -> Option<(u64, u64)> {
    let name = key.rsplit('/').next()?;
    let body = name.strip_prefix('L')?.strip_suffix(".delta")?;
    let (lo, hi) = body.split_once("-L")?;
    Some((lo.parse().ok()?, hi.parse().ok()?))
}

fn parse_image_name(key: &str) -> Option<u64> {
    let name = key.rsplit('/').next()?;
    name.strip_prefix("img-L")?
        .strip_suffix(".image")?
        .parse()
        .ok()
}
