//! # twill-db · Engine Core (spec 02) — `libengine`
//!
//! The single Rust library that parses, plans, and executes SQL, manages MVCC
//! transactions under snapshot isolation, and generates the WAL — routing every
//! durable byte through the pluggable [`twill_storage::Storage`] seam rather
//! than touching disk directly. It ships as `cdylib` + `staticlib` (for FFI
//! consumers such as `bun:ffi`) and `rlib` (for native Rust embedding and the
//! future `engine-server`).
//!
//! ## Phase 1 scope
//!
//! This is the Phase-1 deliverable: the engine bound to the `LocalFileStorage`
//! backend (`file://`), with a frozen C ABI ([`mod@ffi`] / `include/engine.h`),
//! MVCC snapshot isolation, and crash-safe WAL durability + replay. The same
//! library will embed against `s3://` in Phase 2 with no recompile (the seam is
//! a connection-string scheme, not a rebuild).
//!
//! Engine internals are WAL-centric for Phase 1: the working set lives in the
//! in-process store (the buffer the cache spec formalizes), durability is the
//! WAL, and recovery replays it. The page read API on the storage trait is
//! implemented and conformance-tested there, and becomes the cold-read path when
//! `ObjectStorage` arrives in Phase 2.

mod catalog;
mod datetime;
mod db;
mod error;
mod exec;
mod group_commit;
mod json;
mod lex;
mod session;
mod sql;
mod store;
mod value;
mod vector;
mod wal;

pub mod conn;
pub mod ffi;

pub use conn::{
    CatalogColumn, CatalogForeignKey, CatalogPolicy, CatalogTable, Connection, Statement,
};
pub use db::{Database, EngineStats};
pub use error::{EngineError, EngineStatus, Result};
pub use exec::ResultSet;
pub use value::{ColumnType, Value};
pub use vector::{IndexParams, Metric};

/// Re-export the storage observability snapshot so embedders reading
/// [`EngineStats::storage`] need not depend on `twill-storage` directly (#53).
pub use twill_storage::StorageStats;

/// Re-export the copy-on-write branch identifiers so a consumer (the management
/// CLI's `branch` commands) can name and reflect branches without depending on
/// `twill-storage` directly. `BranchId` addresses a branch; `BranchRef` is a
/// reflected `{id, parent, base_lsn, head_lsn}` from [`Connection::list_branches`].
pub use twill_storage::{BranchId, BranchRef};

/// ABI version embedded in `engine.h`; bindings verify it at load time.
///
/// v3 (Phase 5): adds the in-core vector capability — the `vector(N)` type, the
/// HNSW access method (`CREATE INDEX … USING hnsw`), the distance operators
/// (`<->` / `<=>` / `<#>`), and the `'v…'` bind-parameter encoding for vectors.
/// No C symbols were added or removed; the bump signals the new behaviour so a
/// binding can refuse a stale engine.
///
/// v2 (Phase 4): `engine_branch` went from a reserved stub to a working
/// copy-on-write branch — same signature, returning a live branch handle.
pub const ENGINE_ABI_VERSION: u32 = 3;
