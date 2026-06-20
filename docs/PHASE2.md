# Phase 2 — Object-Storage Backend (implementation notes)

This document maps the implemented code to the Phase 2 deliverables, exit
criteria, and Definition of Done in [`specs/13-roadmap.html`](../specs/13-roadmap.html)
and [`specs/04-object-storage-backend.html`](../specs/04-object-storage-backend.html),
and records the deliberate scope decisions.

The headline Phase-2 claim — **flip the connection string, not the binary** — is
real: the engine and the C ABI are byte-for-byte unchanged. `open_storage`
dispatches `s3://`/`r2://`/`gs://` to the new `ObjectStorage` backend, and the
same compiled `libengine` runs disaggregated. (`crates/engine/tests/ffi.rs::s3_scheme_opens_the_object_backend`
proves it end-to-end through the frozen ABI.)

## What shipped

| Deliverable (spec 13 §Phase 2 / spec 04) | Where |
|---|---|
| `ObjectStorage` — second concrete `Storage` impl, disaggregated & scale-to-zero | `crates/storage/src/object/mod.rs` |
| **Commit log** — ordered append on object-store CAS (put-if-absent), durable = commit, doubles as the fence | `object/mod.rs` (`append_segment`, `acquire_fence`) |
| **Page store** — LSM: in-memory memtable → immutable `delta/` layers → `image/` layers, with compaction + GC past the PITR window | `object/mod.rs` (`flush_locked`, `compact`, `gc`, `get_page`) |
| Thin **object-client seam** so AWS S3 / R2 / MinIO swap by config, not rebuild | `object/store.rs` (`trait ObjectStore`) |
| `MemObjectStore` (fast CAS/fencing tests) + `FsObjectStore` (durable, crash-safe, the MinIO/self-hosted floor) | `object/mem.rs`, `object/fs.rs` |
| Local cache that keeps object-store latency off the read hot path | `object/mod.rs` (`cache`, `load_layer` — immutable layer objects are cache-safe) |
| Hand-rolled durable-object codecs (segment / delta / image), CRC-checked | `object/codec.rs` |

The `Storage` trait signature is **unchanged**, so `STORAGE_TRAIT_VERSION` stays
`1` — Phase 2 is purely additive (a new backend behind the existing seam), which
is exactly the property the roadmap requires.

## How the two sub-systems work

```
            engine core (unchanged)
                  │ Storage trait
                  ▼
            local cache (parsed immutable layers)
            miss │            │ commit
        ┌────────┴───┐   ┌────┴──────────┐
        │ PAGE STORE │   │  COMMIT LOG   │
        │  LSM/memtbl│   │ CAS append    │
        │  delta/img │   │ put-if-absent │
        └─────┬──────┘   └──────┬────────┘
              ▼                  ▼
        ObjectStore (Mem | Fs | …future S3/R2 client…)
```

- **Write path.** `append_wal` / `put_page` serialize their items into one log
  segment and claim the next slot `log/<seq>` with **put-if-absent**. The CAS
  success is the commit point — the ack happens only after the object store
  confirms the object durable. Each log item consumes one LSN, keeping the LSN
  stream gap-free across both write paths (same model as `LocalFileStorage`'s
  frames, now on object slots). Page items are also applied into the memtable.
- **Read path.** `get_page(id, lsn)` resolves the at-or-before version by
  scanning memtable → delta layers (newest→oldest, pruned by the LSN span in the
  object name) → the image floor. Parsed layers are cached (immutable ⇒ safe).
- **Flush / compaction / GC.** A full memtable (or `flush()`) serializes to an
  immutable `delta/L<lo>-L<hi>.delta`. `compact()` folds every layer at-or-below
  the PITR floor into a fresh `image/img-L<floor>.image`, retaining layers above
  the floor so any snapshot inside the window stays reconstructable. `gc()`
  physically deletes the now-unreferenced objects.
- **Fencing.** `acquire_fence` advances the durable writer epoch in `lease` via
  conditional write (put-if-match / put-if-absent); a write under a stale epoch
  is rejected `Fenced`, and a lost log-slot CAS re-validates the lease before
  retrying. Single-writer-per-DB with no consensus cluster — the bucket is the
  consensus surface.

## Exit criteria → evidence

- **Same binary opens an `s3://` DB with no recompile; correctness suite green on
  object storage.** C1–C8 run against `ObjectStorage` over both stores
  (`crates/storage/tests/object_storage.rs::object_storage_passes_conformance_on_{fs,mem}`),
  and the engine opens `s3://` end-to-end through the unchanged C ABI
  (`crates/engine/tests/ffi.rs::s3_scheme_opens_the_object_backend`).
- **§8 Experiment 4 (crash safety), unconditional.**
  `exp4a_durable_after_cas_before_ack` (a segment is durable the instant its
  conditional PUT returns; a crash before the ack still recovers it) and
  `exp4b_page_reconstructed_after_crash_before_flush` (a commit durable in the
  log but not yet flushed to a delta is rebuilt by replaying the log forward).
  The `FsObjectStore` makes every object all-or-nothing (temp write → fsync →
  atomic rename → fsync dir), so no torn object ever becomes visible.
- **Two concurrent writers resolve to exactly one survivor via CAS fencing, no
  split-brain.** `two_writers_resolve_to_one_survivor_via_cas` (the loser is
  `Fenced`; exactly one log slot exists).
- **`get_page` returns the correct at-or-before version across memtable, delta,
  and image layers.** `resolves_versions_across_memtable_delta_and_image`.
- **Flush → compaction → GC reduces live layer count; GC never deletes a layer
  inside the PITR window.** `flush_compaction_gc_reduces_live_layers_and_respects_pitr`
  (3 deltas → 1 image; folded deltas reclaimed; covering image retained; reads
  below the floor are snapshot-too-old).
- **Durability invariant (non-negotiable).** A commit is acked only after the
  conditional PUT returns `Ok`; the `ObjectStore` contract guarantees a returned
  `Ok` is durable. Caching hides read latency only, never commit latency.

## Architecture decisions (and why)

- **A thin `ObjectStore` trait is the swappable durability floor** (spec 04 "MAY
  abstract the store behind a thin object-client trait"). `ObjectStorage` is
  written entirely against the five primitives the design needs — GET, PUT, the
  two conditional writes (the CAS unlock), DELETE, LIST — so an AWS-SDK or R2
  client drops in later with no change below the seam. `MemObjectStore` and
  `FsObjectStore` are two impls of it; the cloud tiers are a third.
- **The commit log is the only durability record; the LSM is a derived cache.**
  Memtable/delta/image can be lost and fully rebuilt by replaying the durable
  log forward. This is what makes Experiment 4(b) pass and keeps recovery simple.
- **CAS does double duty** (spec 04 §"What CAS buys us"): the put-if-absent that
  orders a log slot is the same primitive that fences a stale writer. No
  Raft/Paxos/ZooKeeper.
- **PITR-aware compaction.** An image layer is a read *floor* at its `image_lsn`,
  so compacting everything to the high-water LSN would make in-window snapshots
  unreadable. Compaction targets `image_lsn = retention_floor` and keeps layers
  above it — the standard object-storage-LSM (Neon/SlateDB-style) discipline.

## Deliberate Phase-2 boundaries (documented, not accidental)

- **The object root for `s3://` URLs is a durable `FsObjectStore`** under
  `$BYDESIGNS_OBJECT_ROOT` (default: a temp dir), the MinIO/self-hosted tier — so
  the whole stack is testable and demonstrable offline with no cloud credentials
  or network. A real AWS-S3 / Cloudflare-R2 client is a future `ObjectStore`
  impl behind the same trait; nothing above it changes. No secrets ever live in
  the repo (`file://`-only in tests, per the security rule).
- **`ObjectStore` futures are driven synchronously under the backend lock** (via
  the crate `block_on`), mirroring the synchronous C-ABI commit path. The
  *signatures* are async, so a fully pipelined async path and group commit
  (spec 04 §Configuration `group_commit_window`) drop in without moving the seam;
  they are a throughput optimization (Experiment 2), not a Phase-2 gate for
  correctness.
- **Live layer membership is tracked in memory and rediscovered by `list` on
  open.** Valid because the design is single-writer-per-DB and every object is
  immutably named; a single mutable `manifest` pointer is an optimization to cut
  LIST cost at scale, not a correctness requirement.
- **Branch creation only (copy-on-write branch *writes* are Phase 4).** Same
  boundary as Phase 1: branch pointers persist as `branches/<id>` objects and
  resolve correctly; per-branch write isolation arrives with the controller.
- **Log segments are retained (not GC'd) in Phase 2**, matching `LocalFileStorage`
  (which never truncates its file): the engine replays the WAL from the origin on
  open. Trimming the log below the PITR floor is a later refinement; delta/image
  GC is implemented and tested.

## Running

```bash
cargo test                                   # adds tests/object_storage.rs (C1–C8 on both stores + Exp 4 + LSM)
cargo clippy --all-targets -- -D warnings    # clean
cargo build -p bydesigns-engine --release    # same libengine; s3:// is a runtime flip
cd clients/bun && bun test                    # unchanged C ABI still green
```
