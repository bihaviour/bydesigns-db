# Phase 1 — Embedded Library (implementation notes)

This document maps the implemented code to the Phase 1 deliverables, exit
criteria, and Definition of Done in [`specs/13-roadmap.html`](../specs/13-roadmap.html),
and records the deliberate scope decisions.

## What shipped

| Deliverable (spec 13 §Phase 1) | Where |
|---|---|
| `libengine.a` / `libengine.so` (+`.dylib`/`.dll`) static & dynamic libs | `crates/engine` (`crate-type = ["cdylib","staticlib","rlib"]`) |
| `engine.h` — the stable C ABI (`engine_open`/`exec`/`query`/`prepare`/`begin`/`commit`/…) | `crates/engine/include/engine.h`, implemented in `crates/engine/src/ffi.rs` |
| `@yourdb/bun` thin TypeScript wrapper over `bun:ffi` | `clients/bun/` (`src/ffi.ts` raw bindings, `src/index.ts` typed API) |
| `LocalFileStorage` — first concrete `Storage` impl | `crates/storage/src/local.rs` |
| Local cache / in-process working set (the SHOULD) | the engine's in-process MVCC store (`crates/engine/src/store.rs`) is the buffer; durability is the WAL |

## Exit criteria → evidence

- **Open a `file://` DB from Bun via FFI; DDL + DML + queries; correct results across a process restart.**
  `clients/bun/test/embedded.test.ts` (`persists across reopen`) and
  `crates/engine/tests/engine.rs` (`persists_across_restart`). State is rebuilt
  purely by replaying the durable WAL (`crates/engine/src/db.rs::replay`).
- **Basic correctness gate + MVCC snapshot isolation** (a reader sees a stable
  snapshot across a concurrent committed write).
  `engine.rs::mvcc_snapshot_isolation` and `embedded.test.ts` (`MVCC snapshot
  isolation across two handles`). Visibility rules: `store.rs::RowVersion`.
- **`@yourdb/bun` demonstrated end-to-end with no native build step beyond the
  prebuilt library.** `clients/bun/examples/notes.ts`.
- **Storage seam conformance** (durability-after-ack, monotonic LSN, snapshot
  reads, fencing, crash hooks, batch reads, branch creation, retention).
  C1–C8 in `crates/storage/src/conformance.rs`, run against `LocalFileStorage`
  in `crates/storage/tests/conformance.rs`, plus a torn-trailing-frame recovery
  test (the in-process analog of `kill -9` mid-append).
- **No panics across FFI; misuse is defined, not UB.** Every export is wrapped
  in `catch_unwind` → `ENGINE_ERR_INTERNAL`; null handles → `ENGINE_ERR_MISUSE`
  (`crates/engine/tests/ffi.rs`).

## Architecture decisions (and why)

- **The `Storage` trait is `async` from Phase 1.** Spec 13 requires the trait to
  stay signature-stable once Phase 2 adds the network-bound `ObjectStorage`
  backend. Making it async now avoids a later breaking change; `LocalFileStorage`
  resolves synchronously and the engine drives it with a tiny dependency-free
  `block_on` (`crates/storage/src/lib.rs`). The C ABI stays synchronous
  (`engine_commit` blocks until durable).
- **Two buildable additions to the source trait,** both forward-compatible with
  `ObjectStorage`: `scan_wal` (recovery read — the engine replays the durable log
  on open) and `put_page` (the page store's write path; the source trait names
  only the read path `get_page`). They share one monotonic LSN counter.
- **WAL-centric engine.** Durability and recovery go through the WAL; the working
  set lives in the in-process store (the buffer the cache spec formalizes). The
  page read API (`get_page`/`get_pages`) is implemented and conformance-tested in
  the storage layer and becomes the cold-read path when `ObjectStorage` lands in
  Phase 2 (where cold reads pay a network round-trip and the cache is mandatory).
- **Single writer per database, snapshot isolation.** Writers serialize through a
  write lane; readers capture a snapshot LSN and never block. A minimal
  first-committer-wins check (`exec.rs::check_no_conflict`) keeps SI correct for
  explicit transactions under serialized writers.
- **Crash safety.** `LocalFileStorage` writes CRC-checked, length-prefixed frames
  and `fsync`s before returning the commit LSN — never ack-before-durable. On
  reopen, a torn trailing frame is detected and truncated; the WAL replay then
  rebuilds state, so every acked commit survives and no half-state is replayed.

## Deliberate Phase-1 limitations (documented, not accidental)

- **Branching (`engine_branch`) is reserved, not implemented.** The roadmap
  explicitly forbids folding a later phase's concern into Phase 1; the ABI symbol
  is frozen here but returns NULL with an explanatory `engine_last_error`
  (copy-on-write branching is Phase 4).
- **DDL runs in autocommit only.** `CREATE`/`DROP TABLE` inside an explicit
  transaction returns `ENGINE_ERR_TXN`. (Row DML is fully transactional.)
- **SQL subset.** A focused hand-written parser supports `CREATE/DROP TABLE`,
  `INSERT`, `SELECT` (projection, `WHERE`, `ORDER BY`, `LIMIT`, `COUNT/SUM/
  MIN/MAX/AVG`), `UPDATE`, `DELETE`, and `BEGIN/COMMIT/ROLLBACK`. Joins, GROUP
  BY, subqueries, and `DISTINCT` are out of scope for Phase 1 and rejected with
  `ENGINE_ERR_SQL`.
- **`s3://`/`r2://` schemes are rejected** with a clear "Phase-2 backend" error;
  unknown schemes are rejected outright (no silent default).
- **In-process database sharing.** Multiple handles to the same `file://` URL in
  one process share MVCC state via a registry (so the snapshot-isolation
  guarantee holds across handles). Cross-process concurrent writers are not a
  Phase 1 target (single-writer-per-DB; fencing is conformance-tested).

## Running

```bash
cargo test                                   # storage conformance + engine correctness + FFI
cargo build -p bydesigns-engine --release    # target/release/libengine.{a,so}
cd clients/bun && bun test                    # embedded end-to-end
```
