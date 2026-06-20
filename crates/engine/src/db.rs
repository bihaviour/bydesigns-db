//! Shared per-database state: the storage backend, the single-writer fence
//! token, the MVCC store, and the write lane that serializes writers.
//!
//! Multiple connections (engine handles) to the *same* URL share one
//! `Database` via a process-global registry, so a writer on one handle and a
//! reader on another see consistent MVCC snapshots within the process. On open
//! the engine acquires the single-writer fence and replays the durable WAL to
//! rebuild in-memory state (spec 02 — "on restart, WAL replay determines
//! outcome").

use crate::error::{EngineError, EngineStatus, Result};
use crate::store::Store;
use crate::wal::WalOp;
use bydesigns_storage::{block_on, open_storage, FenceToken, Lsn, Storage, WriterId};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, RwLock, Weak};

/// A simple counting write lane: at most one writer holds it at a time. Held
/// across the statements of a write transaction; released on commit/rollback.
/// Not tied to a guard lifetime, so a connection can hold it across FFI calls.
pub struct WriteLane {
    locked: Mutex<bool>,
    cv: Condvar,
}

impl WriteLane {
    fn new() -> WriteLane {
        WriteLane {
            locked: Mutex::new(false),
            cv: Condvar::new(),
        }
    }
    pub fn acquire(&self) {
        let mut g = self.locked.lock().unwrap();
        while *g {
            g = self.cv.wait(g).unwrap();
        }
        *g = true;
    }
    pub fn release(&self) {
        let mut g = self.locked.lock().unwrap();
        *g = false;
        self.cv.notify_one();
    }
}

pub struct Database {
    pub(crate) storage: Box<dyn Storage>,
    pub(crate) token: FenceToken,
    pub(crate) store: RwLock<Store>,
    pub(crate) lane: WriteLane,
    key: String,
}

fn registry() -> &'static Mutex<HashMap<String, Weak<Database>>> {
    static REG: OnceLock<Mutex<HashMap<String, Weak<Database>>>> = OnceLock::new();
    REG.get_or_init(|| Mutex::new(HashMap::new()))
}

fn next_writer_id() -> WriterId {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let pid = std::process::id() as u128;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let c = COUNTER.fetch_add(1, Ordering::Relaxed) as u128;
    WriterId((pid << 96) ^ (nanos << 16) ^ c)
}

/// Registry key: canonical absolute path for `file://` URLs; the URL otherwise.
fn registry_key(url: &str) -> String {
    if let Some(rest) = url.strip_prefix("file://") {
        let path = rest.split('?').next().unwrap_or(rest);
        match std::path::absolute(path) {
            Ok(p) => p.to_string_lossy().into_owned(),
            Err(_) => path.to_string(),
        }
    } else {
        url.to_string()
    }
}

impl Database {
    /// Open (or share) the database named by `url`, acquiring the writer fence
    /// and replaying the durable WAL.
    pub fn open(url: &str) -> Result<Arc<Database>> {
        let key = registry_key(url);
        {
            let reg = registry().lock().unwrap();
            if let Some(existing) = reg.get(&key).and_then(Weak::upgrade) {
                return Ok(existing);
            }
        }

        let storage = open_storage(url)?;
        let token = block_on(storage.acquire_fence(next_writer_id()))?;

        let mut store = Store::default();
        replay(storage.as_ref(), &mut store)?;
        let committed = block_on(storage.get_commit_lsn())?;
        store.committed_lsn = store.committed_lsn.max(committed.0);

        let db = Arc::new(Database {
            storage,
            token,
            store: RwLock::new(store),
            lane: WriteLane::new(),
            key: key.clone(),
        });

        let mut reg = registry().lock().unwrap();
        // Re-check in case of a race between the unlock above and here.
        if let Some(existing) = reg.get(&key).and_then(Weak::upgrade) {
            return Ok(existing);
        }
        reg.insert(key, Arc::downgrade(&db));
        Ok(db)
    }

    pub fn committed_lsn(&self) -> u64 {
        self.store.read().unwrap().committed_lsn
    }
}

impl Drop for Database {
    fn drop(&mut self) {
        // Best-effort: drop the registry entry and release the fence.
        if let Ok(mut reg) = registry().lock() {
            if let Some(w) = reg.get(&self.key) {
                if w.strong_count() == 0 {
                    reg.remove(&self.key);
                }
            }
        }
        let _ = block_on(self.storage.release_fence(self.token.clone()));
    }
}

/// Rebuild the store by replaying durable WAL records, grouping each
/// transaction's ops up to its `Commit` marker and stamping the produced
/// versions with the marker's commit LSN. A trailing markerless group (an
/// incomplete transaction) is discarded.
fn replay(storage: &dyn Storage, store: &mut Store) -> Result<()> {
    let entries = block_on(storage.scan_wal(Lsn::ZERO))?;
    let mut group: Vec<WalOp> = Vec::new();
    for entry in entries {
        let op = WalOp::decode(&entry.record.bytes)?;
        match op {
            WalOp::Commit => {
                let commit_lsn = entry.lsn.0;
                for op in group.drain(..) {
                    apply_replay(store, op, commit_lsn);
                }
                store.committed_lsn = store.committed_lsn.max(commit_lsn);
            }
            other => group.push(other),
        }
    }
    // `group` non-empty here would be an incomplete trailing txn: discard.
    Ok(())
}

fn apply_replay(store: &mut Store, op: WalOp, commit_lsn: u64) {
    match op {
        WalOp::CreateTable { schema } => store.replay_create(schema),
        WalOp::DropTable { name } => store.replay_drop(&name),
        WalOp::Insert { table, vid, values } => {
            store.replay_insert(&table, vid, values, commit_lsn)
        }
        WalOp::Delete { table, vid } => store.replay_delete(&table, vid, commit_lsn),
        WalOp::Commit => {}
    }
}

/// Map a storage error during commit to the engine's conflict/storage codes.
pub fn commit_error(e: bydesigns_storage::StorageError) -> EngineError {
    use bydesigns_storage::StorageError::*;
    let status = match &e {
        Fenced { .. } | Contended => EngineStatus::ErrConflict,
        _ => EngineStatus::ErrStorage,
    };
    EngineError::new(status, e.to_string())
}
