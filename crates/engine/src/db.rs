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
use bydesigns_storage::{
    block_on, open_branch as storage_open_branch, open_storage, BranchId, FenceToken, Lsn, Storage,
    WriterId,
};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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
    /// The URL this database was opened from (the base URL for a branch).
    url: String,
    /// `Some(id)` if this handle is a copy-on-write branch view, else the root.
    branch: Option<BranchId>,
    /// Set once a lease renewal observes that a newer writer fenced us; commits
    /// then fail fast (the storage append would reject them anyway).
    fenced: AtomicBool,
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
            url: url.to_string(),
            branch: None,
            fenced: AtomicBool::new(false),
        });

        let mut reg = registry().lock().unwrap();
        // Re-check in case of a race between the unlock above and here.
        if let Some(existing) = reg.get(&key).and_then(Weak::upgrade) {
            return Ok(existing);
        }
        reg.insert(key, Arc::downgrade(&db));
        Ok(db)
    }

    /// Open (or share) a copy-on-write branch of the database at `url`. The
    /// branch must already exist (created via `storage.create_branch`); its
    /// diverged state lives in a private overlay (see `bydesigns_storage::
    /// open_branch`), so a branch writer never touches the base or siblings.
    pub fn open_branch(url: &str, branch: BranchId) -> Result<Arc<Database>> {
        let key = format!("{}#branch={}", registry_key(url), branch.0);
        {
            let reg = registry().lock().unwrap();
            if let Some(existing) = reg.get(&key).and_then(Weak::upgrade) {
                return Ok(existing);
            }
        }

        let storage = storage_open_branch(url, branch)?;
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
            url: url.to_string(),
            branch: Some(branch),
            fenced: AtomicBool::new(false),
        });

        let mut reg = registry().lock().unwrap();
        if let Some(existing) = reg.get(&key).and_then(Weak::upgrade) {
            return Ok(existing);
        }
        reg.insert(key, Arc::downgrade(&db));
        Ok(db)
    }

    pub fn committed_lsn(&self) -> u64 {
        self.store.read().unwrap().committed_lsn
    }

    /// The URL this database (or its base, for a branch) was opened from.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Whether this handle is a branch view (branch-of-branch is rejected).
    pub fn is_branch(&self) -> bool {
        self.branch.is_some()
    }

    /// Durably renew this database's single-writer lease (the lifecycle
    /// controller's heartbeat). On `Fenced` — a newer writer took over — mark
    /// the database fenced so subsequent commits fail fast, and surface the
    /// error so the controller can step the instance down.
    pub fn renew_lease(&self) -> Result<()> {
        match block_on(self.storage.renew_fence(&self.token)) {
            Ok(_) => Ok(()),
            Err(e) => {
                if matches!(e, bydesigns_storage::StorageError::Fenced { .. }) {
                    self.fenced.store(true, Ordering::SeqCst);
                }
                Err(commit_error(e))
            }
        }
    }

    /// True once a lease renewal observed this writer was fenced by a newer one.
    pub fn is_fenced(&self) -> bool {
        self.fenced.load(Ordering::SeqCst)
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
    // Build every vector index from the recovered rows — this is the index's
    // cold-start "warm" (spec 12): no graph is stored, it is replayed.
    store.rebuild_indexes();
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
        // Index definitions are registered empty here; the graph is populated by
        // `rebuild_indexes` once all rows are present (order-independent).
        WalOp::CreateIndex { def } => store.register_index(def),
        WalOp::DropIndex { name } => {
            store.drop_index(&name);
        }
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
