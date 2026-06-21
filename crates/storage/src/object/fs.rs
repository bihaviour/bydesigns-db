//! `FsObjectStore` — a durable [`ObjectStore`] backed by a local directory tree.
//!
//! This is the self-hosted / MinIO-style durability floor (spec 04 lists MinIO
//! as a first-class S3-compatible tier) and the medium the crash-safety gate
//! runs against: every `put*` writes to a temp file, `fsync`s it, atomically
//! renames it into place, and `fsync`s the parent directory — so a returned
//! `Ok` means the object is durable and a crash can never expose a torn object
//! (the precondition the commit log's "CAS success == commit" rule rests on).
//!
//! Each key maps to a file under the root; one ETag is the content's CRC32 + a
//! length tag, recomputed on read so `put_if_match` can compare without a
//! sidecar. A single mutex serializes conditional ops, making each CAS atomic
//! (this backend is single-writer-per-DB by design — the lock is belt-and-
//! suspenders, not the concurrency model).

use super::codec::crc32;
use super::store::{ETag, GetResult, ObjectError, ObjectStore};
use async_trait::async_trait;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

/// Durable object store rooted at a local directory (one root per database).
pub struct FsObjectStore {
    root: PathBuf,
    /// Serializes conditional ops so the check-then-write is atomic.
    cas_lock: Mutex<()>,
    /// Disambiguates concurrent temp-file names within a process.
    tmp_seq: AtomicU64,
}

impl FsObjectStore {
    /// Open (creating if absent) a store rooted at `root`.
    pub fn open(root: impl AsRef<Path>) -> Result<FsObjectStore, ObjectError> {
        let root = root.as_ref().to_path_buf();
        fs::create_dir_all(&root)
            .map_err(|e| ObjectError::Transient(format!("create root {root:?}: {e}")))?;
        Ok(FsObjectStore {
            root,
            cas_lock: Mutex::new(()),
            tmp_seq: AtomicU64::new(0),
        })
    }

    /// Resolve a key to a path under the root, rejecting traversal attempts.
    fn path_for(&self, key: &str) -> Result<PathBuf, ObjectError> {
        if key.is_empty() || key.split('/').any(|seg| seg == ".." || seg == ".") {
            return Err(ObjectError::Transient(format!("invalid object key: {key}")));
        }
        Ok(self.root.join(key))
    }

    /// Atomically place `bytes` at `path`: temp write → fsync → rename → fsync dir.
    fn write_atomic(&self, path: &Path, bytes: &[u8]) -> Result<(), ObjectError> {
        let parent = path
            .parent()
            .ok_or_else(|| ObjectError::Transient("object key has no parent".into()))?;
        fs::create_dir_all(parent)
            .map_err(|e| ObjectError::Transient(format!("create dir {parent:?}: {e}")))?;

        let n = self.tmp_seq.fetch_add(1, Ordering::Relaxed);
        let tmp = parent.join(format!(".tmp-{}-{}", std::process::id(), n));

        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
            .map_err(|e| ObjectError::Transient(format!("create tmp {tmp:?}: {e}")))?;
        f.write_all(bytes)
            .map_err(|e| ObjectError::Transient(format!("write tmp: {e}")))?;
        f.sync_all()
            .map_err(|e| ObjectError::Transient(format!("fsync tmp: {e}")))?;
        drop(f);

        fs::rename(&tmp, path).map_err(|e| {
            let _ = fs::remove_file(&tmp);
            ObjectError::Transient(format!("rename into {path:?}: {e}"))
        })?;
        sync_dir(parent);
        Ok(())
    }

    fn read_raw(&self, path: &Path) -> Result<Option<Vec<u8>>, ObjectError> {
        let mut f = match File::open(path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(ObjectError::Transient(format!("open {path:?}: {e}"))),
        };
        let mut bytes = Vec::new();
        f.read_to_end(&mut bytes)
            .map_err(|e| ObjectError::Transient(format!("read {path:?}: {e}")))?;
        Ok(Some(bytes))
    }
}

/// Content-derived ETag: stable for given bytes, cheap to recompute on read.
fn etag_of(bytes: &[u8]) -> ETag {
    format!("{:08x}-{:x}", crc32(bytes), bytes.len())
}

/// Best-effort directory fsync so a rename is durable. Ignored where the
/// platform refuses to fsync a directory; the target is Linux.
fn sync_dir(dir: &Path) {
    if let Ok(d) = File::open(dir) {
        let _ = d.sync_all();
    }
}

#[async_trait]
impl ObjectStore for FsObjectStore {
    async fn get(&self, key: &str) -> Result<Option<GetResult>, ObjectError> {
        let path = self.path_for(key)?;
        Ok(self.read_raw(&path)?.map(|bytes| GetResult {
            etag: etag_of(&bytes),
            bytes,
        }))
    }

    async fn put_if_absent(&self, key: &str, bytes: &[u8]) -> Result<ETag, ObjectError> {
        let path = self.path_for(key)?;
        let _guard = self.cas_lock.lock().unwrap();
        if path.exists() {
            return Err(ObjectError::Precondition(key.to_string()));
        }
        self.write_atomic(&path, bytes)?;
        Ok(etag_of(bytes))
    }

    async fn put_if_match(
        &self,
        key: &str,
        bytes: &[u8],
        etag: &ETag,
    ) -> Result<ETag, ObjectError> {
        let path = self.path_for(key)?;
        let _guard = self.cas_lock.lock().unwrap();
        match self.read_raw(&path)? {
            Some(cur) if &etag_of(&cur) == etag => {
                self.write_atomic(&path, bytes)?;
                Ok(etag_of(bytes))
            }
            _ => Err(ObjectError::Precondition(key.to_string())),
        }
    }

    async fn put(&self, key: &str, bytes: &[u8]) -> Result<ETag, ObjectError> {
        let path = self.path_for(key)?;
        let _guard = self.cas_lock.lock().unwrap();
        self.write_atomic(&path, bytes)?;
        Ok(etag_of(bytes))
    }

    async fn delete(&self, key: &str) -> Result<(), ObjectError> {
        let path = self.path_for(key)?;
        let _guard = self.cas_lock.lock().unwrap();
        match fs::remove_file(&path) {
            Ok(()) => {
                if let Some(parent) = path.parent() {
                    sync_dir(parent);
                }
                Ok(())
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(ObjectError::Transient(format!("delete {path:?}: {e}"))),
        }
    }

    async fn list(&self, prefix: &str) -> Result<Vec<String>, ObjectError> {
        let mut out = Vec::new();
        walk(&self.root, &self.root, &mut out)?;
        out.retain(|k| k.starts_with(prefix));
        out.sort();
        Ok(out)
    }
}

/// Recursively collect every object key (root-relative path) under `dir`,
/// skipping in-flight temp files so a half-written object is never listed.
fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) -> Result<(), ObjectError> {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(ObjectError::Transient(format!("read_dir {dir:?}: {e}"))),
    };
    for entry in entries {
        let entry = entry.map_err(|e| ObjectError::Transient(e.to_string()))?;
        let path = entry.path();
        let name = entry.file_name();
        if name.to_string_lossy().starts_with(".tmp-") {
            continue;
        }
        if path.is_dir() {
            walk(root, &path, out)?;
        } else if let Ok(rel) = path.strip_prefix(root) {
            out.push(
                rel.to_string_lossy()
                    .replace(std::path::MAIN_SEPARATOR, "/"),
            );
        }
    }
    Ok(())
}
