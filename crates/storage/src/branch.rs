//! `BranchStorage` — copy-on-write branch overlay (spec 06 §Branching).
//!
//! A branch is a cheap LSN pointer over shared immutable layers: reads at-or-
//! below the fork point fall through to the parent's already-durable history;
//! writes land only in a branch-private overlay backend. Creating a branch is
//! O(1) and adds no storage until the branch diverges (its first write), so the
//! base and every sibling are untouched by a branch's writes.
//!
//! The adaptor is backend-agnostic: parent and overlay are both `dyn Storage`,
//! so the same copy-on-write semantics work for `file://` (a sibling-file
//! overlay) and `s3://` (a child-prefix overlay) with no per-backend branch
//! logic. LSNs stay continuous across the seam: the overlay assigns its own
//! local LSNs from 1, and the adaptor presents them shifted by `base_lsn`, so
//! the engine above sees one gap-free stream (`1..=base` from the parent, then
//! `base+1..` from the overlay).

use crate::types::*;
use crate::Storage;
use async_trait::async_trait;
use std::sync::Arc;

/// A branch view: read-only parent history `<= base_lsn` plus a private,
/// writable overlay holding everything the branch has diverged.
pub struct BranchStorage {
    parent: Arc<dyn Storage>,
    overlay: Box<dyn Storage>,
    base_lsn: u64,
    #[allow(dead_code)]
    id: BranchId,
    #[allow(dead_code)]
    parent_id: BranchId,
}

impl BranchStorage {
    /// Compose a branch from its `parent` line, a fresh private `overlay`, and
    /// the resolved `BranchRef` (fork point + identity).
    pub fn new(
        parent: Arc<dyn Storage>,
        overlay: Box<dyn Storage>,
        bref: BranchRef,
    ) -> BranchStorage {
        BranchStorage {
            parent,
            overlay,
            base_lsn: bref.base_lsn.0,
            id: bref.id,
            parent_id: bref.parent,
        }
    }

    /// Overlay-local LSN → branch-global LSN.
    fn to_global(&self, local: Lsn) -> Lsn {
        Lsn(self.base_lsn + local.0)
    }

    /// Branch-global LSN → overlay-local LSN (clamped at the fork point).
    fn to_local(&self, global: Lsn) -> Lsn {
        Lsn(global.0.saturating_sub(self.base_lsn))
    }
}

#[async_trait]
impl Storage for BranchStorage {
    async fn get_page(&self, page_id: PageId, lsn: Lsn) -> Result<Page, StorageError> {
        // At or below the fork point, the version lives in the shared parent.
        if lsn.0 <= self.base_lsn {
            return self.parent.get_page(page_id, lsn).await;
        }
        // Above it, prefer a branch-private version; otherwise the version
        // visible at the fork point (the parent's at-or-before-base image).
        match self.overlay.get_page(page_id, self.to_local(lsn)).await {
            Ok(p) => Ok(Page {
                id: page_id,
                lsn: self.to_global(p.lsn),
                bytes: p.bytes,
            }),
            Err(StorageError::NotFound(_)) => {
                self.parent.get_page(page_id, Lsn(self.base_lsn)).await
            }
            Err(e) => Err(e),
        }
    }

    async fn append_wal(
        &self,
        token: &FenceToken,
        records: &[WalRecord],
    ) -> Result<Lsn, StorageError> {
        let last = self.overlay.append_wal(token, records).await?;
        Ok(self.to_global(last))
    }

    async fn put_page(
        &self,
        token: &FenceToken,
        page_id: PageId,
        image: &[u8],
    ) -> Result<Lsn, StorageError> {
        let lsn = self.overlay.put_page(token, page_id, image).await?;
        Ok(self.to_global(lsn))
    }

    async fn scan_wal(&self, after: Lsn) -> Result<Vec<LogEntry>, StorageError> {
        let mut out = Vec::new();
        if after.0 < self.base_lsn {
            // Replay the parent's committed history up to the fork point, then
            // the branch's own diverged log on top.
            for e in self.parent.scan_wal(after).await? {
                if e.lsn.0 <= self.base_lsn {
                    out.push(e);
                }
            }
            for e in self.overlay.scan_wal(Lsn::ZERO).await? {
                out.push(LogEntry {
                    lsn: self.to_global(e.lsn),
                    record: e.record,
                });
            }
        } else {
            for e in self.overlay.scan_wal(self.to_local(after)).await? {
                out.push(LogEntry {
                    lsn: self.to_global(e.lsn),
                    record: e.record,
                });
            }
        }
        Ok(out)
    }

    async fn flush(&self) -> Result<(), StorageError> {
        self.overlay.flush().await
    }

    async fn durable_lsn(&self) -> Result<Lsn, StorageError> {
        Ok(self.to_global(self.overlay.durable_lsn().await?))
    }

    async fn get_commit_lsn(&self) -> Result<Lsn, StorageError> {
        Ok(self.to_global(self.overlay.get_commit_lsn().await?))
    }

    // Fencing is per-branch: the overlay owns the branch's commit log + token,
    // so a branch writer fences only other writers of the same branch.
    async fn acquire_fence(&self, owner: WriterId) -> Result<FenceToken, StorageError> {
        self.overlay.acquire_fence(owner).await
    }

    async fn renew_fence(&self, token: &FenceToken) -> Result<FenceToken, StorageError> {
        self.overlay.renew_fence(token).await
    }

    async fn release_fence(&self, token: FenceToken) -> Result<(), StorageError> {
        self.overlay.release_fence(token).await
    }

    // Sub-branches (branch-of-branch) live in the overlay's own namespace.
    async fn create_branch(&self, name: &str, base_lsn: Lsn) -> Result<BranchId, StorageError> {
        self.overlay
            .create_branch(name, self.to_local(base_lsn))
            .await
    }

    async fn resolve_branch(&self, branch: BranchId) -> Result<BranchRef, StorageError> {
        self.overlay.resolve_branch(branch).await
    }

    async fn list_branches(&self) -> Result<Vec<BranchRef>, StorageError> {
        self.overlay.list_branches().await
    }

    async fn delete_branch(&self, branch: BranchId) -> Result<(), StorageError> {
        self.overlay.delete_branch(branch).await
    }

    async fn set_retention_floor(&self, lsn: Lsn) -> Result<(), StorageError> {
        self.overlay.set_retention_floor(self.to_local(lsn)).await
    }

    async fn pitr_floor(&self) -> Result<Lsn, StorageError> {
        Ok(self.to_global(self.overlay.pitr_floor().await?))
    }
}
