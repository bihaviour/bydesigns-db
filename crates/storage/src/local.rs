//! `LocalFileStorage` — the pure-embedded backend: a single `.db` file, zero
//! network (spec 03). Durability bottoms out on `fsync`; there is no S3/CAS
//! round-trip, so commits are microseconds, not network latency.
//!
//! ## On-disk format
//!
//! A fixed 64-byte header followed by an append-only sequence of CRC-checked
//! frames:
//!
//! ```text
//! header: "BYDESIGN" | format:u32 | page_size:u32 | reserved...
//! frame:  len:u32 | type:u8 | payload[len-1] | crc32:u32   (crc over type+payload)
//! ```
//!
//! Each frame is written then `fsync`'d before the call returns, so an acked
//! mutation always survives a crash. A crash mid-write leaves a torn trailing
//! frame (short read or CRC mismatch); recovery stops at the last good frame and
//! truncates the tail — never replaying a half-written batch (the C1 guarantee).
//!
//! LSNs are assigned deterministically by replaying frames in order: a WAL batch
//! of N records consumes N consecutive LSNs (the last is the commit boundary); a
//! page image consumes one. Metadata frames (fence/retention/branch) consume
//! none, keeping the data LSN sequence gap-free.

use crate::types::*;
use crate::Storage;
use async_trait::async_trait;
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const MAGIC: &[u8; 8] = b"BYDESIGN";
const FORMAT_VERSION: u32 = 1;
const HEADER_SIZE: u64 = 64;
const FENCE_LEASE: Duration = Duration::from_secs(10);

/// A page's version chain: `(version_lsn, image)` entries in ascending LSN.
type PageChain = Vec<(u64, Box<[u8; PAGE_SIZE]>)>;

// Frame type tags.
const T_WAL: u8 = 1;
const T_PAGE: u8 = 2;
const T_FENCE: u8 = 3;
const T_RETENTION: u8 = 4;
const T_BRANCH: u8 = 5;
const T_BRANCH_DEL: u8 = 6;

/// A parsed frame with its replay-assigned LSN(s).
enum Frame {
    Wal {
        records: Vec<(Lsn, WalRecord)>,
        commit_lsn: Lsn,
    },
    Page {
        lsn: Lsn,
        page_id: u64,
        image: Vec<u8>,
    },
    Fence {
        epoch: u64,
    },
    Retention {
        floor: u64,
    },
    Branch {
        id: u64,
        base_lsn: u64,
        parent: u64,
    },
    BranchDel {
        id: u64,
    },
}

struct Inner {
    file: File,
    /// Append offset (end of the last good frame).
    end: u64,
    next_lsn: u64,
    durable_lsn: u64,
    commit_lsn: u64,
    epoch: u64,
    retention_floor: u64,
    /// page_id -> version chain ordered by LSN (ascending).
    pages: HashMap<u64, PageChain>,
    branches: HashMap<u64, BranchRef>,
    next_branch_id: u64,
    /// Read-only observability counters (#53). Backend-neutral cumulative
    /// totals; snapshotted by [`Storage::stats`]. Held under the same lock as
    /// the rest of `Inner` — `stats()` is sampled at scenario boundaries, never
    /// on a hot inner loop, so the brief lock is fine.
    stats: StorageStats,
}

/// Pure-embedded backend persisting all state in one `.db` file.
pub struct LocalFileStorage {
    path: PathBuf,
    inner: Mutex<Inner>,
}

impl LocalFileStorage {
    /// Open (creating if absent) the `.db` file named by a `file://` URL.
    pub fn open(url: &str) -> Result<LocalFileStorage, StorageError> {
        let path = parse_file_url(url)?;
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false) // never truncate: the file is our durable log
            .open(&path)
            .map_err(|e| StorageError::Invalid(format!("cannot open {path:?}: {e}")))?;

        let file_len = file
            .metadata()
            .map_err(|e| StorageError::Transient(e.to_string()))?
            .len();

        let inner = if file_len == 0 {
            write_header(&mut file)?;
            Inner {
                file,
                end: HEADER_SIZE,
                next_lsn: 1,
                durable_lsn: 0,
                commit_lsn: 0,
                epoch: 0,
                retention_floor: 0,
                pages: HashMap::new(),
                branches: HashMap::new(),
                next_branch_id: 1,
                stats: StorageStats::default(),
            }
        } else {
            recover(file)?
        };

        // Heal a torn trailing frame discovered during recovery.
        let actual_len = inner
            .file
            .metadata()
            .map_err(|e| StorageError::Transient(e.to_string()))?
            .len();
        if actual_len > inner.end {
            inner
                .file
                .set_len(inner.end)
                .map_err(|e| StorageError::Transient(e.to_string()))?;
            inner
                .file
                .sync_all()
                .map_err(|e| StorageError::DurabilityUnconfirmed(e.to_string()))?;
        }

        Ok(LocalFileStorage {
            path,
            inner: Mutex::new(inner),
        })
    }

    /// The backing file path (for diagnostics / branch sibling files).
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }
}

impl Inner {
    /// Append one frame (type + payload), fsync, advance counters. Durable on Ok.
    fn append_frame(&mut self, tag: u8, payload: &[u8]) -> Result<(), StorageError> {
        let len = (payload.len() + 1) as u32; // type byte + payload
        let crc = crc32_pair(tag, payload);

        self.file
            .seek(SeekFrom::Start(self.end))
            .map_err(|e| StorageError::Transient(e.to_string()))?;
        let mut buf = Vec::with_capacity(4 + len as usize + 4);
        buf.extend_from_slice(&len.to_le_bytes());
        buf.push(tag);
        buf.extend_from_slice(payload);
        buf.extend_from_slice(&crc.to_le_bytes());
        self.file
            .write_all(&buf)
            .map_err(|e| StorageError::DurabilityUnconfirmed(e.to_string()))?;
        self.file
            .sync_all()
            .map_err(|e| StorageError::DurabilityUnconfirmed(e.to_string()))?;

        self.end += buf.len() as u64;
        self.stats.fsyncs += 1;
        Ok(())
    }
}

#[async_trait]
impl Storage for LocalFileStorage {
    async fn get_page(&self, page_id: PageId, lsn: Lsn) -> Result<Page, StorageError> {
        let mut g = self.inner.lock().unwrap();
        if lsn.0 < g.retention_floor {
            return Err(StorageError::NotFound(format!(
                "lsn {} below PITR floor {}",
                lsn.0, g.retention_floor
            )));
        }
        let chain = g
            .pages
            .get(&page_id.0)
            .ok_or_else(|| StorageError::NotFound(format!("page {}", page_id.0)))?;
        // Greatest version with version-LSN <= requested lsn.
        let best = chain.iter().rev().find(|(vlsn, _)| *vlsn <= lsn.0);
        match best {
            Some((vlsn, image)) => {
                let page = Page {
                    id: page_id,
                    lsn: Lsn(*vlsn),
                    bytes: image.clone(),
                };
                g.stats.page_reads += 1;
                g.stats.page_read_bytes += PAGE_SIZE as u64;
                Ok(page)
            }
            None => Err(StorageError::NotFound(format!(
                "page {} has no version at-or-before lsn {}",
                page_id.0, lsn.0
            ))),
        }
    }

    async fn append_wal(
        &self,
        token: &FenceToken,
        records: &[WalRecord],
    ) -> Result<Lsn, StorageError> {
        if records.is_empty() {
            return Err(StorageError::Invalid("append_wal: empty batch".into()));
        }
        let mut g = self.inner.lock().unwrap();
        check_fence(&g, token)?;

        // payload: count:u32 | (len:u32 | bytes)*
        let mut payload = Vec::new();
        payload.extend_from_slice(&(records.len() as u32).to_le_bytes());
        for r in records {
            payload.extend_from_slice(&(r.bytes.len() as u32).to_le_bytes());
            payload.extend_from_slice(&r.bytes);
        }
        let wal_bytes = payload.len() as u64;
        g.append_frame(T_WAL, &payload)?;

        let first = g.next_lsn;
        let last = first + records.len() as u64 - 1;
        g.next_lsn = last + 1;
        g.durable_lsn = last;
        g.commit_lsn = last;
        g.stats.wal_appends += 1;
        g.stats.wal_bytes += wal_bytes;
        Ok(Lsn(last))
    }

    async fn put_page(
        &self,
        token: &FenceToken,
        page_id: PageId,
        image: &[u8],
    ) -> Result<Lsn, StorageError> {
        if image.len() > PAGE_SIZE {
            return Err(StorageError::Invalid(format!(
                "page image {} exceeds PAGE_SIZE {}",
                image.len(),
                PAGE_SIZE
            )));
        }
        let mut g = self.inner.lock().unwrap();
        check_fence(&g, token)?;

        let mut payload = Vec::with_capacity(8 + PAGE_SIZE);
        payload.extend_from_slice(&page_id.0.to_le_bytes());
        payload.extend_from_slice(image);
        g.append_frame(T_PAGE, &payload)?;

        let lsn = g.next_lsn;
        g.next_lsn = lsn + 1;
        g.durable_lsn = lsn;
        let mut boxed = Box::new([0u8; PAGE_SIZE]);
        boxed[..image.len()].copy_from_slice(image);
        g.pages.entry(page_id.0).or_default().push((lsn, boxed));
        Ok(Lsn(lsn))
    }

    async fn scan_wal(&self, after: Lsn) -> Result<Vec<LogEntry>, StorageError> {
        // Recovery read: re-parse the durable log and return WAL records past
        // `after`. Called once at engine open; not on the hot path.
        let path = self.path.clone();
        let mut file = OpenOptions::new()
            .read(true)
            .open(&path)
            .map_err(|e| StorageError::Transient(e.to_string()))?;
        let (frames, _end, _next) = read_log(&mut file)?;
        let mut out = Vec::new();
        for f in frames {
            if let Frame::Wal { records, .. } = f {
                for (lsn, rec) in records {
                    if lsn.0 > after.0 {
                        out.push(LogEntry { lsn, record: rec });
                    }
                }
            }
        }
        Ok(out)
    }

    async fn flush(&self) -> Result<(), StorageError> {
        let mut g = self.inner.lock().unwrap();
        g.file
            .sync_all()
            .map_err(|e| StorageError::DurabilityUnconfirmed(e.to_string()))?;
        g.stats.fsyncs += 1;
        Ok(())
    }

    async fn durable_lsn(&self) -> Result<Lsn, StorageError> {
        Ok(Lsn(self.inner.lock().unwrap().durable_lsn))
    }

    async fn get_commit_lsn(&self) -> Result<Lsn, StorageError> {
        Ok(Lsn(self.inner.lock().unwrap().commit_lsn))
    }

    async fn acquire_fence(&self, owner: WriterId) -> Result<FenceToken, StorageError> {
        let mut g = self.inner.lock().unwrap();
        let new_epoch = g.epoch + 1;
        g.append_frame(T_FENCE, &new_epoch.to_le_bytes())?;
        g.epoch = new_epoch;
        Ok(FenceToken {
            epoch: new_epoch,
            owner,
            lease_until: Instant::now() + FENCE_LEASE,
        })
    }

    async fn renew_fence(&self, token: &FenceToken) -> Result<FenceToken, StorageError> {
        let g = self.inner.lock().unwrap();
        if token.epoch != g.epoch {
            return Err(StorageError::Fenced {
                held: token.epoch,
                current: g.epoch,
            });
        }
        Ok(FenceToken {
            epoch: token.epoch,
            owner: token.owner,
            lease_until: Instant::now() + FENCE_LEASE,
        })
    }

    async fn release_fence(&self, _token: FenceToken) -> Result<(), StorageError> {
        // The next acquire bumps the epoch regardless, so release is a no-op here
        // (idempotent). A future revision MAY persist a release marker.
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
        // Pointer frame: id | base_lsn | parent (ROOT for branches off this line).
        let mut payload = Vec::with_capacity(24);
        payload.extend_from_slice(&id.to_le_bytes());
        payload.extend_from_slice(&base_lsn.0.to_le_bytes());
        payload.extend_from_slice(&BranchId::ROOT.0.to_le_bytes());
        g.append_frame(T_BRANCH, &payload)?;
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
            let mut g = self.inner.lock().unwrap();
            if !g.branches.contains_key(&branch.0) {
                return Err(StorageError::NotFound(format!("branch {}", branch.0)));
            }
            if g.branches.values().any(|b| b.parent == branch) {
                return Err(StorageError::Invalid(format!(
                    "branch {} has live children; delete them first",
                    branch.0
                )));
            }
            // Append a tombstone (the log is append-only) and drop from the map.
            g.append_frame(T_BRANCH_DEL, &branch.0.to_le_bytes())?;
            g.branches.remove(&branch.0);
        }
        // Reclaim the branch's diverged data: its single sibling overlay file.
        let mut p = self.path.clone().into_os_string();
        p.push(format!(".branch-{}", branch.0));
        let _ = std::fs::remove_file(std::path::PathBuf::from(p));
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
        g.append_frame(T_RETENTION, &lsn.0.to_le_bytes())?;
        g.retention_floor = lsn.0;
        Ok(())
    }

    async fn pitr_floor(&self) -> Result<Lsn, StorageError> {
        Ok(Lsn(self.inner.lock().unwrap().retention_floor))
    }

    fn stats(&self) -> StorageStats {
        self.inner.lock().unwrap().stats
    }
}

fn check_fence(g: &Inner, token: &FenceToken) -> Result<(), StorageError> {
    if token.epoch != g.epoch {
        return Err(StorageError::Fenced {
            held: token.epoch,
            current: g.epoch,
        });
    }
    Ok(())
}

fn parse_file_url(url: &str) -> Result<PathBuf, StorageError> {
    let rest = url
        .strip_prefix("file://")
        .ok_or_else(|| StorageError::Invalid(format!("not a file:// url: {url}")))?;
    // Drop any query string; LocalFileStorage takes no query params in Phase 1.
    let rest = rest.split('?').next().unwrap_or(rest);
    if rest.is_empty() {
        return Err(StorageError::Invalid("file:// url has empty path".into()));
    }
    Ok(PathBuf::from(rest))
}

fn write_header(file: &mut File) -> Result<(), StorageError> {
    let mut header = [0u8; HEADER_SIZE as usize];
    header[0..8].copy_from_slice(MAGIC);
    header[8..12].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
    header[12..16].copy_from_slice(&(PAGE_SIZE as u32).to_le_bytes());
    file.seek(SeekFrom::Start(0))
        .map_err(|e| StorageError::Transient(e.to_string()))?;
    file.write_all(&header)
        .map_err(|e| StorageError::DurabilityUnconfirmed(e.to_string()))?;
    file.sync_all()
        .map_err(|e| StorageError::DurabilityUnconfirmed(e.to_string()))?;
    Ok(())
}

/// Read and validate the header + every good frame, returning the parsed frames,
/// the offset of the end of the last good frame, and the next LSN to assign.
fn read_log(file: &mut File) -> Result<(Vec<Frame>, u64, u64), StorageError> {
    file.seek(SeekFrom::Start(0))
        .map_err(|e| StorageError::Transient(e.to_string()))?;
    let mut all = Vec::new();
    file.read_to_end(&mut all)
        .map_err(|e| StorageError::Transient(e.to_string()))?;

    if all.len() < HEADER_SIZE as usize {
        return Err(StorageError::Corruption("file shorter than header".into()));
    }
    if &all[0..8] != MAGIC {
        return Err(StorageError::Corruption("bad magic".into()));
    }
    let fmt = u32::from_le_bytes(all[8..12].try_into().unwrap());
    if fmt != FORMAT_VERSION {
        return Err(StorageError::Invalid(format!(
            "unsupported format version {fmt} (expected {FORMAT_VERSION})"
        )));
    }
    let ps = u32::from_le_bytes(all[12..16].try_into().unwrap());
    if ps as usize != PAGE_SIZE {
        return Err(StorageError::Invalid(format!(
            "page size mismatch: file {ps} vs build {PAGE_SIZE}"
        )));
    }

    let mut frames = Vec::new();
    let mut pos = HEADER_SIZE as usize;
    let mut next_lsn: u64 = 1;
    let total = all.len();

    while pos + 4 <= total {
        let len = u32::from_le_bytes(all[pos..pos + 4].try_into().unwrap()) as usize;
        let frame_end = pos + 4 + len + 4;
        if len == 0 || frame_end > total {
            break; // torn / truncated trailing frame
        }
        let body = &all[pos + 4..pos + 4 + len];
        let crc_stored = u32::from_le_bytes(all[pos + 4 + len..frame_end].try_into().unwrap());
        if crc32(body) != crc_stored {
            break; // torn frame; stop here
        }
        let tag = body[0];
        let payload = &body[1..];
        match tag {
            T_WAL => {
                if payload.len() < 4 {
                    break;
                }
                let count = u32::from_le_bytes(payload[0..4].try_into().unwrap()) as usize;
                let mut p = 4;
                let mut records = Vec::with_capacity(count);
                let mut torn = false;
                for _ in 0..count {
                    if p + 4 > payload.len() {
                        torn = true;
                        break;
                    }
                    let rlen = u32::from_le_bytes(payload[p..p + 4].try_into().unwrap()) as usize;
                    p += 4;
                    if p + rlen > payload.len() {
                        torn = true;
                        break;
                    }
                    let lsn = Lsn(next_lsn);
                    next_lsn += 1;
                    records.push((lsn, WalRecord::new(payload[p..p + rlen].to_vec())));
                    p += rlen;
                }
                if torn || records.len() != count {
                    break;
                }
                let commit_lsn = records.last().map(|(l, _)| *l).unwrap_or(Lsn::ZERO);
                frames.push(Frame::Wal {
                    records,
                    commit_lsn,
                });
            }
            T_PAGE => {
                if payload.len() < 8 {
                    break;
                }
                let page_id = u64::from_le_bytes(payload[0..8].try_into().unwrap());
                let image = payload[8..].to_vec();
                let lsn = Lsn(next_lsn);
                next_lsn += 1;
                frames.push(Frame::Page {
                    lsn,
                    page_id,
                    image,
                });
            }
            T_FENCE => {
                if payload.len() < 8 {
                    break;
                }
                let epoch = u64::from_le_bytes(payload[0..8].try_into().unwrap());
                frames.push(Frame::Fence { epoch });
            }
            T_RETENTION => {
                if payload.len() < 8 {
                    break;
                }
                let floor = u64::from_le_bytes(payload[0..8].try_into().unwrap());
                frames.push(Frame::Retention { floor });
            }
            T_BRANCH => {
                if payload.len() < 24 {
                    break;
                }
                let id = u64::from_le_bytes(payload[0..8].try_into().unwrap());
                let base_lsn = u64::from_le_bytes(payload[8..16].try_into().unwrap());
                let parent = u64::from_le_bytes(payload[16..24].try_into().unwrap());
                frames.push(Frame::Branch {
                    id,
                    base_lsn,
                    parent,
                });
            }
            T_BRANCH_DEL => {
                if payload.len() < 8 {
                    break;
                }
                let id = u64::from_le_bytes(payload[0..8].try_into().unwrap());
                frames.push(Frame::BranchDel { id });
            }
            _ => break, // unknown tag: treat as torn boundary
        }
        pos = frame_end;
    }

    Ok((frames, pos as u64, next_lsn))
}

/// Rebuild [`Inner`] from a non-empty file.
fn recover(mut file: File) -> Result<Inner, StorageError> {
    let (frames, end, next_lsn) = read_log(&mut file)?;
    let mut durable_lsn = 0u64;
    let mut commit_lsn = 0u64;
    let mut epoch = 0u64;
    let mut retention_floor = 0u64;
    let mut pages: HashMap<u64, PageChain> = HashMap::new();
    let mut branches: HashMap<u64, BranchRef> = HashMap::new();
    let mut next_branch_id = 1u64;

    for f in frames {
        match f {
            Frame::Wal { commit_lsn: c, .. } => {
                durable_lsn = durable_lsn.max(c.0);
                commit_lsn = commit_lsn.max(c.0);
            }
            Frame::Page {
                lsn,
                page_id,
                image,
            } => {
                durable_lsn = durable_lsn.max(lsn.0);
                let mut boxed = Box::new([0u8; PAGE_SIZE]);
                let n = image.len().min(PAGE_SIZE);
                boxed[..n].copy_from_slice(&image[..n]);
                pages.entry(page_id).or_default().push((lsn.0, boxed));
            }
            Frame::Fence { epoch: e } => epoch = epoch.max(e),
            Frame::Retention { floor } => retention_floor = retention_floor.max(floor),
            Frame::Branch {
                id,
                base_lsn,
                parent,
            } => {
                branches.insert(
                    id,
                    BranchRef {
                        id: BranchId(id),
                        parent: BranchId(parent),
                        base_lsn: Lsn(base_lsn),
                        head_lsn: Lsn(base_lsn),
                    },
                );
                next_branch_id = next_branch_id.max(id + 1);
            }
            Frame::BranchDel { id } => {
                branches.remove(&id);
            }
        }
    }

    Ok(Inner {
        file,
        end,
        next_lsn,
        durable_lsn,
        commit_lsn,
        epoch,
        retention_floor,
        pages,
        branches,
        next_branch_id,
        stats: StorageStats::default(),
    })
}

// ---- CRC32 (IEEE 802.3), table-built on first use -------------------------

fn crc32_table() -> &'static [u32; 256] {
    use std::sync::OnceLock;
    static TABLE: OnceLock<[u32; 256]> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut t = [0u32; 256];
        let mut i = 0;
        while i < 256 {
            let mut c = i as u32;
            let mut k = 0;
            while k < 8 {
                c = if c & 1 != 0 {
                    0xEDB8_8320 ^ (c >> 1)
                } else {
                    c >> 1
                };
                k += 1;
            }
            t[i] = c;
            i += 1;
        }
        t
    })
}

fn crc32(data: &[u8]) -> u32 {
    let table = crc32_table();
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc = table[((crc ^ b as u32) & 0xFF) as usize] ^ (crc >> 8);
    }
    crc ^ 0xFFFF_FFFF
}

/// CRC over a one-byte tag followed by a payload, without allocating.
fn crc32_pair(tag: u8, payload: &[u8]) -> u32 {
    let table = crc32_table();
    let mut crc = 0xFFFF_FFFFu32;
    crc = table[((crc ^ tag as u32) & 0xFF) as usize] ^ (crc >> 8);
    for &b in payload {
        crc = table[((crc ^ b as u32) & 0xFF) as usize] ^ (crc >> 8);
    }
    crc ^ 0xFFFF_FFFF
}
