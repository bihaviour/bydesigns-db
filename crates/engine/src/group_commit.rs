//! Group commit: coalesce concurrent transactions' WAL into one durable append.
//!
//! Spec 02 / spec 09 (the W1 lever): the expensive part of a commit is the
//! durable handoff — an `fsync` for `file://`, a CAS round-trip for object
//! storage. With one append per commit, throughput is capped at `1 / latency`
//! no matter how many writers there are (the Experiment-1 ceiling). Group commit
//! batches the WAL records of every transaction that is ready to commit into a
//! single [`Storage::append_wal`](twill_storage::Storage::append_wal), amortizing
//! that one handoff across the batch — the Experiment-2 plateau.
//!
//! The durability rule is never bent: a commit is acknowledged only after its
//! records are durable. Batching changes *how many* commits share one durable
//! write, never *when* a commit is considered done (spec 10 — "never acknowledge
//! a commit before its WAL record is durably stored, even under group commit").
//!
//! ## How it works (leader / follower, no background thread)
//!
//! A committing connection enqueues its encoded WAL batch and then either:
//!
//! * becomes the **leader** (no flush in progress) — it drains the queue in
//!   submission order into a bounded batch, performs one `append_wal`, then under
//!   the store write lock publishes each member's pending versions at that
//!   member's commit LSN and advances the reader high-water mark once for the
//!   whole batch; or
//! * becomes a **follower** — it parks until the leader fills in its result.
//!
//! The leader keeps flushing successive batches until the queue drains, so every
//! enqueued commit is covered without promoting a background thread (the embedded
//! engine core stays thread-free — see `.claude/rules/rust.md`). Commit LSNs are
//! assigned from the single contiguous LSN range the batch's append returns, so
//! they stay strictly monotonic and gap-free.

use crate::db::{commit_error, Database};
use crate::error::Result;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex};
use twill_storage::{block_on, WalRecord};

/// Upper bound on records coalesced into one durable append (spec 09
/// `group_commit_max_batch`): caps a single CAS-append / fsync so it stays
/// bounded even under a large backlog. A batch always contains at least one
/// transaction, even if that transaction alone exceeds the cap.
const MAX_BATCH_RECORDS: usize = 1024;

/// One transaction waiting to be made durable.
struct Member {
    /// The in-flight writer id whose pending store versions this commit owns.
    owner: u64,
    /// Encoded WAL records (the transaction's ops followed by a `Commit` marker).
    records: Vec<WalRecord>,
    /// Set by the leader once the batch resolves; `Ok(commit_lsn)` or the error.
    result: Option<Result<u64>>,
    done: bool,
}

struct Inner {
    /// Member ids awaiting flush, in submission (and therefore LSN) order.
    queue: VecDeque<u64>,
    members: HashMap<u64, Member>,
    /// True while a leader is draining the queue.
    leader_active: bool,
    next_id: u64,
}

/// Per-[`Database`] commit coordinator (one instance, shared by all handles).
pub struct GroupCommit {
    inner: Mutex<Inner>,
    cv: Condvar,
    /// Durable appends performed (batches). With coalescing this is < `commits`.
    batches: AtomicU64,
    /// Transactions committed durably. Diagnostics for the W1 plateau and the
    /// gate proving batching actually engages (`commits > batches`).
    commits: AtomicU64,
}

impl GroupCommit {
    pub fn new() -> GroupCommit {
        GroupCommit {
            inner: Mutex::new(Inner {
                queue: VecDeque::new(),
                members: HashMap::new(),
                leader_active: false,
                next_id: 1,
            }),
            cv: Condvar::new(),
            batches: AtomicU64::new(0),
            commits: AtomicU64::new(0),
        }
    }

    /// `(batches, commits)`: durable appends performed and transactions committed.
    /// Under concurrency `commits > batches` proves coalescing engaged.
    pub(crate) fn metrics(&self) -> (u64, u64) {
        (
            self.batches.load(Ordering::Relaxed),
            self.commits.load(Ordering::Relaxed),
        )
    }

    /// Commit `records` (the transaction's WAL ops plus a trailing `Commit`
    /// marker) for the transaction tagged `owner`, coalescing with any concurrent
    /// commits. Returns the transaction's durable commit LSN.
    ///
    /// The caller MUST hold the write lane on entry; this releases it only after
    /// the records are enqueued, so a DDL [`GroupCommit::quiesce`] (which
    /// re-acquires the lane) always observes an in-flight commit, while other
    /// writers are free to prepare and batch in behind us.
    pub fn commit(&self, db: &Database, owner: u64, records: Vec<WalRecord>) -> Result<u64> {
        let (id, am_leader) = {
            let mut g = self.inner.lock().unwrap();
            let id = g.next_id;
            g.next_id += 1;
            g.members.insert(
                id,
                Member {
                    owner,
                    records,
                    result: None,
                    done: false,
                },
            );
            g.queue.push_back(id);
            let am_leader = !g.leader_active;
            if am_leader {
                g.leader_active = true;
            }
            (id, am_leader)
        };

        // Records are queued; safe to hand off the write lane now.
        db.lane.release();

        if am_leader {
            self.lead(db);
        }

        // Whether we led or followed, our result is filled by the leader. Reclaim
        // it (followers park here until woken).
        let mut g = self.inner.lock().unwrap();
        loop {
            if g.members.get(&id).map(|m| m.done).unwrap_or(false) {
                return g.members.remove(&id).unwrap().result.unwrap();
            }
            g = self.cv.wait(g).unwrap();
        }
    }

    /// Leader loop: flush batches until the queue is empty, then relinquish.
    fn lead(&self, db: &Database) {
        loop {
            let batch = self.drain_batch();
            if !batch.is_empty() {
                self.flush_batch(db, &batch);
            }
            let mut g = self.inner.lock().unwrap();
            if g.queue.is_empty() {
                g.leader_active = false;
                // Wake any follower whose member we just resolved, and any
                // quiesce() waiter.
                self.cv.notify_all();
                return;
            }
            // More arrived while we flushed; stay leader and take the next batch.
        }
    }

    /// Pop a bounded prefix of the queue (in submission order) to flush together.
    fn drain_batch(&self) -> Vec<u64> {
        let mut g = self.inner.lock().unwrap();
        let mut batch = Vec::new();
        let mut nrec = 0usize;
        while let Some(&id) = g.queue.front() {
            let len = g.members[&id].records.len();
            // Always take at least one; otherwise respect the record cap.
            if !batch.is_empty() && nrec + len > MAX_BATCH_RECORDS {
                break;
            }
            g.queue.pop_front();
            nrec += len;
            batch.push(id);
        }
        batch
    }

    /// Append one batch durably, then publish (or roll back) each member.
    fn flush_batch(&self, db: &Database, batch: &[u64]) {
        // Gather the concatenated records and per-member (id, owner, len) without
        // holding the coordinator lock across the durable append.
        let (all, members): (Vec<WalRecord>, Vec<(u64, u64, usize)>) = {
            let g = self.inner.lock().unwrap();
            let mut all = Vec::new();
            let mut members = Vec::with_capacity(batch.len());
            for &id in batch {
                let m = &g.members[&id];
                members.push((id, m.owner, m.records.len()));
                all.extend(m.records.iter().cloned());
            }
            (all, members)
        };

        self.batches.fetch_add(1, Ordering::Relaxed);
        match block_on(db.storage.append_wal(&db.token, &all)) {
            Ok(last) => {
                self.commits
                    .fetch_add(members.len() as u64, Ordering::Relaxed);
                // The append consumed one contiguous LSN range ending at `last`;
                // each member's commit LSN is its Commit marker — the last record
                // of its slice.
                let total: u64 = members.iter().map(|(_, _, n)| *n as u64).sum();
                let first = last.0 - total + 1;
                let mut store = db.store.write().unwrap();
                let mut acc: u64 = 0;
                let mut max_lsn = 0u64;
                let mut results: Vec<(u64, u64)> = Vec::with_capacity(members.len());
                for (id, owner, n) in &members {
                    acc += *n as u64;
                    let commit_lsn = first + acc - 1;
                    store.finalize_owner(*owner, commit_lsn);
                    max_lsn = commit_lsn;
                    results.push((*id, commit_lsn));
                }
                // Advance the reader high-water mark once, atomically for the
                // whole batch (we hold the store write lock).
                if max_lsn > store.committed_lsn {
                    store.committed_lsn = max_lsn;
                }
                drop(store);
                self.resolve(results.into_iter().map(|(id, l)| (id, Ok(l))).collect());
            }
            Err(e) => {
                // Durability unconfirmed for the whole batch: discard every
                // member's pending versions and fail them all.
                let err = commit_error(e);
                let mut store = db.store.write().unwrap();
                for (_, owner, _) in &members {
                    store.rollback_owner(*owner);
                }
                drop(store);
                let results: Vec<(u64, Result<u64>)> = members
                    .iter()
                    .map(|(id, _, _)| (*id, Err(err.clone())))
                    .collect();
                self.resolve(results);
            }
        }
    }

    /// Record each member's result and wake any parked followers.
    fn resolve(&self, results: Vec<(u64, Result<u64>)>) {
        let mut g = self.inner.lock().unwrap();
        for (id, r) in results {
            if let Some(m) = g.members.get_mut(&id) {
                m.result = Some(r);
                m.done = true;
            }
        }
        self.cv.notify_all();
    }

    /// Block until no commit is in flight. DDL uses this (while holding the write
    /// lane, so no new commit can start) to run exclusively against a quiesced
    /// pipeline before its own direct durable append.
    pub fn quiesce(&self) {
        let mut g = self.inner.lock().unwrap();
        while g.leader_active || !g.queue.is_empty() {
            g = self.cv.wait(g).unwrap();
        }
    }
}

impl Default for GroupCommit {
    fn default() -> GroupCommit {
        GroupCommit::new()
    }
}
