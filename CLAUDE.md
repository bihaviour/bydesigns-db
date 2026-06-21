# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

`bydesigns-db` is a serverless OLTP database engine: an embeddable Rust library
(SQLite-style, function-call latency) whose **storage backend is pluggable**, so
the *same* engine runs either purely embedded (`file://`) or storage-disaggregated
on object storage (`s3://`/`r2://`/`gs://`). The full design lives as an HTML spec
site under `pages/specs/` (start at `pages/specs/13-roadmap.html` for the phased plan).

**Phases 1, 2, 3, 4, and 5 are implemented.** Phase 1 is the embedded library
(`file://`); Phase 2 adds the disaggregated `ObjectStorage` backend
(`s3://`/`r2://`/`gs://`) — an LSM page store + CAS commit log over a pluggable
object-client seam — selected purely by connection string, with the engine and C
ABI unchanged. Phase 3 adds `engine-server`: the same engine behind a Postgres-wire
listener (a defined pgwire subset), serving either backend by connection string.
Phase 4 adds copy-on-write branching (the `engine_branch` stub is now a working
branch — `STORAGE_TRAIT_VERSION` 2, `ENGINE_ABI_VERSION` 2 at Phase 4), a durable
single-writer lease (acquire/renew/release), and the `bydesigns-controller`
lifecycle controller (scale-to-zero + keep-warm). Phase 5 adds the in-core
**vector capability** — a `vector(N)` type, an HNSW access method
(`CREATE INDEX … USING hnsw`), the distance operators `<->`/`<=>`/`<#>`, and a
top-k nearest-neighbour query — riding the *same* WAL/replay path the rows do, so
it branches and scales-to-zero with the database (`ENGINE_ABI_VERSION` 3;
`STORAGE_TRAIT_VERSION` stays 2, the storage seam is untouched). Everything stays
*additive* because the storage seam never moves. See the per-phase implementation
maps under `pages/specs/phase-1-embedded.html`–`pages/specs/phase-5-capabilities.html`
for the implementation maps and the deliberate scope decisions.

## Layout

```
crates/storage    # the pluggable `Storage` trait (the seam) + LocalFileStorage + ObjectStorage (LSM+CAS over the object/ client seam) + BranchStorage (copy-on-write) + C1–C8 conformance suite
crates/engine     # libengine: SQL → MVCC → WAL, plus the stable C ABI (include/engine.h)
crates/server     # engine-server: the engine behind a Postgres-wire listener (pgwire subset); links the engine unchanged
crates/controller # lifecycle controller: scale-to-zero instances, lease heartbeat, keep-warm + thundering-herd admission (Phase 4)
clients/bun       # @yourdb/bun: bun:ffi bindings + ergonomic typed wrapper + example
pages/            # the website + documentation (static HTML, deployed to GitHub Pages):
                  #   index.html  — home (project overview)
                  #   docs/       — user documentation (connect, branch, pool, operate)
                  #   specs/      — development guidelines: the 14 design specs (the source of
                  #                 truth for design intent) + per-phase implementation maps
                  #   release/    — releases & upcoming roadmap
                  #   assets/     — one shared design system (app.css + app.js + section manifests)
```

## Commands

```bash
# Rust workspace
cargo test                                    # all tests (engine + FFI + storage conformance + controller)
cargo test -p bydesigns-engine --test engine  # one test binary
cargo test -p bydesigns-engine mvcc_snapshot_isolation   # one test by name
cargo test -p bydesigns-storage --test branching         # Phase 4 copy-on-write branching (both backends)
cargo test -p bydesigns-controller            # Phase 4 lifecycle: scale-to-zero + thundering herd
cargo fmt --all                               # format (CI runs `cargo fmt --check`)
cargo clippy --all-targets                    # lint (CI runs with `-D warnings`)
cargo build -p bydesigns-engine --release     # build target/release/libengine.{a,so,dylib}

# Server mode (Phase 3): the engine behind a Postgres-wire listener
cargo run -p bydesigns-server -- --listen 127.0.0.1:5433 --db file://./srv.db   # or s3://bucket/db
# any Postgres client connects (cleartext): psql/Bun.sql/pgbench with sslmode=disable

# Bun client (needs the built libengine; auto-discovered from target/{release,debug})
cd clients/bun
bun test                                      # end-to-end embedded tests
YOURDB_ENGINE_PATH=/abs/path/libengine.so bun test   # explicit library override
bun run examples/notes.ts                      # runnable sample app

# Website + docs (static HTML in pages/, no build step; deployed to GitHub Pages)
bunx serve pages                               # preview the site locally (uses bunx, not python3)
```

The Bun layer loads the native library via `bun:ffi`; if a change touches the C
ABI or engine behaviour, **rebuild the release `libengine` before running
`bun test`**, otherwise Bun runs against a stale binary.

## Architecture: the seam is everything

The whole design rests on one idea — the engine never touches disk; it talks to a
single narrow `Storage` trait (`crates/storage/src/lib.rs`). Keep these invariants
intact, because every later phase depends on them:

- **The `Storage` trait is `async` and signature-stable.** It is async (even
  though `LocalFileStorage` is synchronous) so Phase 2's network-bound
  `ObjectStorage` drops in with no signature change — and it did: `ObjectStorage`
  is additive, `STORAGE_TRAIT_VERSION` stays `1`. The synchronous C ABI bridges
  to it with a tiny dependency-free `block_on` in `crates/storage/src/lib.rs`. Do
  not make the trait sync or add backend-specific concepts to it.
- **Backend is chosen by URL scheme** in `open_storage` (`file://` →
  `LocalFileStorage`; `s3://`/`r2://`/`gs://` → `ObjectStorage`). Unknown schemes
  are rejected, never silently defaulted.
- **Durability is WAL-centric.** `append_wal` must be durable (fsync) before it
  returns the commit LSN — never ack from a buffer. `LocalFileStorage`
  (`crates/storage/src/local.rs`) writes CRC-checked, length-prefixed frames and
  recovers by discarding a torn trailing frame, so every acked commit survives a
  crash. This is gated by the C1–C8 conformance suite (`crates/storage/src/conformance.rs`).
- **The C ABI (`crates/engine/include/engine.h`) is frozen** and hand-written to
  match `crates/engine/src/ffi.rs`. Every export is wrapped in `catch_unwind`
  (no Rust panic crosses FFI → `ENGINE_ERR_INTERNAL`); null handles →
  `ENGINE_ERR_MISUSE`. If you change `ffi.rs`, update `engine.h` and bump
  `ENGINE_ABI_VERSION` in both places.

## Engine internals (crates/engine/src)

A statement flows `sql.rs` (hand-written lexer + recursive-descent parser →
`Stmt` AST) → `exec.rs` (evaluate against the MVCC store) → `conn.rs` (transaction
state machine + commit durability). Supporting modules: `store.rs` (MVCC row
versions + visibility, plus the vector-index registry), `wal.rs` (engine-owned WAL
op encoding), `db.rs` (shared `Database` + cross-handle registry + WAL replay),
`catalog.rs`, `value.rs`, `vector.rs` (Phase 5: the HNSW access method).

- **MVCC / snapshot isolation.** Every row version is stamped `create_lsn` /
  `delete_lsn`; readers capture a snapshot LSN and filter by visibility
  (`store.rs::RowVersion`). The pending (uncommitted) stamp is `PENDING` (`u64::MAX`).
- **Single writer per database** via a write lane (`db.rs::WriteLane`); writers
  serialize, readers never block. A first-committer-wins check
  (`exec.rs::check_no_conflict`) keeps explicit-transaction SI correct.
- **Cross-handle sharing.** Multiple connections to the same `file://` URL in one
  process share one `Database` (a process-global registry in `db.rs`), so the
  snapshot-isolation guarantee holds across handles.
- **Commit = one `append_wal` batch** of the transaction's WAL ops + a `Commit`
  marker; the returned LSN is the commit LSN at which pending versions are
  published. Recovery replays the log, grouping ops up to each marker.

## Phase 4: branching & lifecycle

- **Branching is a storage-seam concern, not an engine special-case.** A branch is
  a `BranchStorage` (`crates/storage/src/branch.rs`): a parent `dyn Storage` read
  at-or-below the fork LSN + a private overlay for diverged writes, composed
  backend-agnostically (sibling file for `file://`, child key-prefix for `s3://`).
  `open_branch(url, id)` builds it. The engine just opens a `Database` over it
  (`db.rs::open_branch` → `conn.rs::branch` → `engine_branch`).
- **The single-writer lease is durable.** `acquire_fence`/`renew_fence`/
  `release_fence` carry epoch + owner + expiry; fencing correctness still rests on
  the monotonic CAS epoch (take-over), the lease timestamp is advisory liveness.
- **The lifecycle controller (`crates/controller`) owns no durable state.** It
  composes the engine's registry-shared `Database` (open = fence + replay = warm;
  `Drop` = release fence = stop) into a `Cold→Warming→Active→Idle→Stopping→Cold`
  machine with an idle reaper, lease heartbeat (`Database::renew_lease`), and
  thundering-herd admission. Don't move lifecycle/heartbeat threads into the
  embedded engine core — embedders must stay thread-free.

## Phase 5: vector search (in-core)

- **The vector index rides the WAL, not a side file.** A `VectorIndex`
  (`crates/engine/src/vector.rs`, HNSW) is a *derived* structure over a table's
  `vector(N)` column — registered by a `CreateIndex` WAL op and rebuilt from the
  rows by `Store::rebuild_indexes` after replay, exactly like the in-memory row
  store. That is why branching branches the index and scale-to-zero re-warms it
  for free; do not add a separate durable graph or move the index off the WAL path.
- **Built-in vs composed (spec 12).** Vector search is the one capability built
  *into* the engine; better-auth/PostgREST/DuckDB are composed *around* it and must
  not enter the core (no interface/service/OLAP code in `crates/engine`). The
  composition glue lives in `clients/bun/examples/` (`vector-memory.ts`,
  `compose.ts`).
- **KNN planner.** `exec.rs::knn_select` recognizes `ORDER BY <col> <dist-op> <q>
  ASC LIMIT k`, uses the matching HNSW index, over-fetches, then MVCC-filters; with
  no index the distance operator still works as a brute-force scan + sort.

## Phase-1 scope boundaries (intentional)

These are deliberate, not omissions — don't "fix" them without checking the roadmap:

- `engine_branch` is implemented as of Phase 4: it forks a copy-on-write branch
  at the connection's committed LSN and returns a new branch-bound handle.
  Branch-of-branch and branching inside a transaction are rejected (NULL + error).
- DDL (`CREATE`/`DROP TABLE`, `CREATE`/`DROP INDEX`) runs in autocommit only;
  inside an explicit transaction it returns `ENGINE_ERR_TXN`. Row DML is fully
  transactional.
- The SQL surface is a focused subset; unsupported syntax returns `ENGINE_ERR_SQL`
  (joins, GROUP BY, subqueries, DISTINCT are out of scope). Phase 5 adds the
  `vector(N)` type, the `<->`/`<=>`/`<#>` distance operators, and HNSW indexes.

## When changing things

- Treat `pages/specs/` as the design source of truth; align changes with the
  relevant spec page and keep the matching `pages/specs/phase-*.html` implementation
  map accurate. User-facing behaviour changes should also be reflected in the docs
  under `pages/docs/`.
- Storage-trait changes must keep all C1–C8 conformance tests green (and the
  Phase-4 branching battery, `crates/storage/tests/branching.rs`) and bump
  `STORAGE_TRAIT_VERSION` per the trait's versioning policy (currently `2`).
- Engine-behaviour or ABI changes: update `engine.h`, the Rust FFI tests
  (`crates/engine/tests/ffi.rs`), and re-run `bun test` against a fresh release build.

## Project rules

Detailed, enforceable rules live in `.claude/rules/` and are imported here:

@.claude/rules/storage-seam.md
@.claude/rules/rust.md
@.claude/rules/testing.md
@.claude/rules/security.md
@.claude/rules/git-workflow.md
