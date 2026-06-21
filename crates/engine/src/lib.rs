//! # bydesigns-db · Engine Core (spec 02) — `libengine`
//!
//! The single Rust library that parses, plans, and executes SQL, manages MVCC
//! transactions under snapshot isolation, and generates the WAL — routing every
//! durable byte through the pluggable [`bydesigns_storage::Storage`] seam rather
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
mod db;
mod error;
mod exec;
mod sql;
mod store;
mod value;
mod wal;

pub mod conn;
pub mod ffi;

pub use conn::{Connection, Statement};
pub use error::{EngineError, EngineStatus, Result};
pub use exec::ResultSet;
pub use value::{ColumnType, Value};

/// ABI version embedded in `engine.h`; bindings verify it at load time.
pub const ENGINE_ABI_VERSION: u32 = 1;
