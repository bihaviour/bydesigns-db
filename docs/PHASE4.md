# Phase 4 — Controller: scale-to-zero & branching

Phase 4 adds the lifecycle layer the roadmap calls for — **instant copy-on-write
branches**, a **durable single-writer lease**, and a **scale-to-zero lifecycle
controller** — without moving the storage seam. Everything is additive:
`STORAGE_TRAIT_VERSION` goes to **2** (additive trait surface), `ENGINE_ABI_VERSION`
goes to **2** (`engine_branch` stub → working, same signature), and the C ABI gains
no new symbols.

Epic: #4. Tasks: #21 (lifecycle), #22 (branching), #23 (fencing), #24 (keep-warm/herd).

## #22 Branching — copy-on-write (the headline)

A branch is a cheap LSN pointer over shared immutable layers. Reads at-or-below
the fork point fall through to the parent's already-durable history; writes land
only in a branch-private overlay. Creating a branch is O(1) and copies no pages,
so the base and every sibling are untouched until the branch diverges.

- **`BranchStorage`** (`crates/storage/src/branch.rs`) — a backend-agnostic
  adaptor composing a parent `Arc<dyn Storage>` + a private overlay
  `Box<dyn Storage>`. LSNs are continuous across the fork: the overlay assigns
  local LSNs from 1 and the adaptor presents them shifted by `base_lsn`, so the
  engine above sees one gap-free stream (`1..=base` parent, then `base+1..`
  overlay). One adaptor serves both backends.
- **`open_branch(url, id)`** (`crates/storage/src/lib.rs`) — opens a branch's
  overlay: a sibling `*.branch-<id>` file for `file://`, a child
  `branches/<id>/` key-prefix `ObjectStorage` for `s3://`/`r2://`/`gs://`.
- **Trait surface (v2):** `BranchRef` gains `parent`; `list_branches` and
  `delete_branch` are added. `delete_branch` reclaims only the branch's diverged
  data (its overlay), refuses a branch with live children, and never touches
  shared base layers. Both backends persist `parent` in the branch pointer
  (`branches/<id>.ptr` objects; a `T_BRANCH` frame + a `T_BRANCH_DEL` tombstone
  in `LocalFileStorage`).
- **Engine + C ABI:** `Database::open_branch` + `Connection::branch` open a branch
  over the overlay (replaying parent-up-to-base, then the branch's own log).
  `engine_branch(h, name)` forks at the connection's committed LSN and returns a
  new branch-bound handle (owned by the caller, freed with `engine_close`).
  Branch-of-branch and branching inside a transaction are rejected. The Bun
  wrapper's `branch()` works unchanged against ABI v2.
- **Tests:** conformance C7 strengthened (parent + list + delete);
  `crates/storage/tests/branching.rs` proves write-isolation, sibling isolation,
  O(1) create, reopen persistence, and delete-reclaim on **both** backends;
  `ffi.rs` and the Bun embedded test drive a real branch end-to-end.

## #23 Single-writer fencing — durable lease

Split-brain was already prevented by the take-over CAS epoch (a fresh acquire
strictly bumps the epoch, fencing every prior token). Phase 4 makes the lease
**durable and observable** so the controller can heartbeat it and a peer can tell
a live writer from a dead one.

- The `ObjectStorage` lease object now carries `epoch | owner | expires_at_ms`
  (epoch stays first, so the legacy reader is unaffected). `acquire_fence` stamps
  a live expiry; **`renew_fence` durably re-stamps** it under the same epoch
  (the heartbeat), failing `Fenced` if superseded; **`release_fence` durably
  frees** it (expiry 0) while keeping the epoch, for a fast clean handoff.
- Fencing correctness still rests on the monotonic epoch, not the wall clock; the
  lease timestamp is advisory liveness only.
- The active heartbeat *loop* lives in the controller (below), not the embedded
  engine core, so embedders pay for no background thread.
- **Tests:** conformance C4 strengthened (renew keeps the holder committing;
  release then a fresh acquire fences the released token) on both backends; a new
  object test asserts the durable acquire/renew/release lease lifecycle.

## #21 / #24 Lifecycle controller — `crates/controller`

The controller composes the engine's existing registry-shared `Database`
primitive: opening one acquires the fence and replays the WAL (that *is* the cache
warm); dropping it releases the fence (that *is* the stop). On top of that it adds
the state machine, idle reaper, lease heartbeat, and thundering-herd handling.

- **State machine (#21):** `Cold → Warming → Active → Idle → Stopping → Cold`.
  `start(url)` cold-starts on the first connection and returns a `Lease` that
  keeps the instance Active until dropped. A background reaper idles instances
  with no leases and tears them down past a configurable `idle_timeout`; `status()`
  makes the phase observable. The controller holds **no durable state**, so
  stop/start loses nothing (all state is in storage) — gated by a
  write/stop/restart/read test.
- **Heartbeat (#23):** the reaper renews each warm instance's lease via
  `Database::renew_lease`; on a fence loss it steps the instance down.
- **Thundering herd (#24):** N concurrent `start()` for one cold database warm
  **exactly once** (the rest wait on the in-flight transition); a bounded
  warm-admission semaphore caps how many distinct databases warm at once;
  `keep_warm` holds idle instances resident to cut post-idle latency. Config is
  validated at construction.

## Deliberate scope boundaries

- **Branch-of-branch** is rejected at the engine boundary (the storage adaptor
  supports nesting, but `Connection::branch`/`engine_branch` fork only off a base
  database for now). Rebase / fast-forward / merge and divergence accounting are
  deferred (spec "MAY").
- The controller is an **in-process library** API; container/Cloud-Run supervision
  and HTTP control endpoints are out of scope here.
- Experiment 5 (cold-read percentiles) and the N-concurrent-cold-start saturation
  curve belong to the benchmark harness (cross-cutting epic #6), not this phase.
