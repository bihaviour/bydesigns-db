# bydesigns-db

> DB Management System with separated process and storage layer. Built on Rust, and compatible with PostgreSQL.

**Status: Phases 1–4 implemented.** The repository holds both the development
specifications and a working Rust implementation: the embeddable `libengine`
with a frozen C ABI, the pluggable `Storage` trait with `LocalFileStorage`
(`file://`) and `ObjectStorage` (`s3://`/`r2://`/`gs://`) backends, MVCC snapshot
isolation, crash-safe WAL durability + replay, an `engine-server` that speaks a
Postgres-wire subset, **copy-on-write branching**, a durable single-writer lease,
and a **scale-to-zero lifecycle controller**. See the [roadmap](#roadmap) and the
[documentation](#documentation) entry points below.

## What this is

`bydesigns-db` is an OLTP database engine that is, at the same time:

- **Embeddable** — links in-process as a library, at function-call latency (SQLite-style), and
- **Storage-disaggregated** — durable state lives on object storage (S3 / Cloudflare R2 / MinIO), so compute is stateless.

These usually pull in opposite directions. The resolution is to keep the engine a
**library** and make its **storage backend pluggable**, pointing the seam at the
network instead of a local file — rather than putting a server at the boundary.
The *same* engine then runs embedded (via FFI) **or** as a **PostgreSQL
wire-compatible server**; the storage choice is configuration (a connection-string
scheme), not a rebuild.

Headline properties: **scale-to-zero**, **true embeddability**, and **instant
branching** (copy-on-write over LSN-versioned immutable layers).

## Architecture at a glance

- **Engine (Rust library)** — SQL parser → planner → executor, MVCC (snapshot isolation via LSN-stamped versions), WAL generation. Ships as `cdylib` + `staticlib` with a stable C ABI (`engine.h`), plus an `engine-server` binary.
- **Pluggable `Storage` trait** — the central seam. Backends: `LocalFileStorage` (pure embedded, zero network), `ObjectStorage` (disaggregated), and `BranchStorage` (copy-on-write overlay composing the two).
- **Object-storage backend** — an LSM page store (versioned by LSN) plus an ordered commit log whose durability bottoms out on **S3 conditional writes (compare-and-swap)** — atomic ordered appends and single-writer fencing without a separate consensus cluster.
- **Branching & lifecycle** — a branch is a cheap LSN pointer over shared immutable layers (writes diverge into a private overlay); the `bydesigns-controller` crate cold-starts instances on first connection and tears them down when idle (scale-to-zero), heartbeating each instance's durable writer lease.
- **Interfaces** — embedded via `bun:ffi` (`@yourdb/bun`) / NAPI; server via the **Postgres wire protocol**, so existing tooling (PostgREST, `Bun.sql`, standard `psql`/pg drivers) connects unchanged.

## Getting started

The backend is selected purely by the connection-string scheme — `file://` for
pure-embedded, `s3://`/`r2://`/`gs://` for disaggregated — with no recompile.

### Embedded (Bun + FFI)

Build the native library, then use the `@yourdb/bun` wrapper (it auto-discovers
the built library, or set `YOURDB_ENGINE_PATH`):

```bash
cargo build -p bydesigns-engine --release    # target/release/libengine.{a,so,dylib}
cd clients/bun && bun test                    # end-to-end embedded tests
bun run examples/notes.ts                      # runnable sample app
```

```ts
import { open } from "@yourdb/bun";

using db = open("file://./local.db");          // or "s3://bucket/db"
db.exec(`CREATE TABLE notes (id INTEGER PRIMARY KEY, body TEXT)`);
db.query("INSERT INTO notes VALUES (?, ?)", [1, "hello"]);
const rows = db.query("SELECT id, body FROM notes");   // [{ id: "1", body: "hello" }]

// Instant copy-on-write branch: sees the base's data, writes in isolation.
using preview = db.branch("preview");
preview.exec("INSERT INTO notes VALUES (2, 'branch-only')");
// the base never sees the branch's write
```

### Server mode (Postgres wire)

The same engine behind a Postgres-wire listener; any Postgres client connects
(cleartext, `sslmode=disable`):

```bash
cargo run -p bydesigns-server -- --listen 127.0.0.1:5433 --db file://./srv.db   # or s3://bucket/db
psql "host=127.0.0.1 port=5433 user=postgres sslmode=disable"
```

### Scale-to-zero (controller, library API)

`bydesigns-controller` manages engine instances: cold-start on first connection,
keep-warm, idle teardown, and thundering-herd admission — all over the same
storage seam.

## Documentation

| Resource | Where | Status |
|---|---|---|
| **User & API guides** (how to embed, deploy, operate) | _published docs site_ | **pending** |
| **Implementation maps** (per-phase: what's built, where, and why) | [`docs/PHASE1.md`](docs/PHASE1.md) · [`PHASE2`](docs/PHASE2.md) · [`PHASE3`](docs/PHASE3.md) · [`PHASE4`](docs/PHASE4.md) | available |
| **Development specifications** (the design source of truth) | [`specs/`](specs/) — open [`specs/index.html`](specs/index.html) | available |
| **C ABI** (the stable embedding contract) | [`crates/engine/include/engine.h`](crates/engine/include/engine.h) | frozen (ABI v2) |
| **Contributor guidance** | [`CLAUDE.md`](CLAUDE.md) and [`.claude/rules/`](.claude/rules/) | available |

The full development spec is a self-contained HTML site under [`specs/`](specs/).
Open [`specs/index.html`](specs/index.html), or serve the folder locally:

```bash
cd specs && python3 -m http.server   # then visit http://localhost:8000
```

Selected specs:

| Spec | |
|---|---|
| [Architecture Overview](specs/01-architecture-overview.html) | The three slots and inter-layer protocols |
| [Engine Core](specs/02-engine-core.html) | C ABI, MVCC, WAL, execution pipeline |
| [Storage Interface](specs/03-storage-interface.html) | The pluggable `Storage` trait (the seam) |
| [Object-Storage Backend](specs/04-object-storage-backend.html) | LSM page store + S3-CAS commit log |
| [Lifecycle & Controller](specs/06-lifecycle-controller.html) | Scale-to-zero, branching, fencing |
| [Server Mode & Wire Protocol](specs/07-server-mode.html) | The pgwire subset |
| [Benchmark & Validation Plan](specs/09-benchmark-plan.html) | Latency/throughput/crash-safety experiments |
| [Roadmap & Build Sequence](specs/13-roadmap.html) | Phased delivery plan |

## Repository layout

```
crates/storage    # the pluggable `Storage` trait (the seam) + LocalFileStorage + ObjectStorage (LSM+CAS) + BranchStorage (copy-on-write) + C1–C8 conformance suite
crates/engine     # libengine: SQL → MVCC → WAL, and the stable C ABI (include/engine.h)
crates/server     # engine-server: the engine behind a Postgres-wire listener (pgwire subset)
crates/controller # lifecycle controller: scale-to-zero, lease heartbeat, keep-warm + thundering-herd admission
clients/bun       # @yourdb/bun: bun:ffi bindings + ergonomic typed wrapper + example
docs/             # per-phase implementation maps (PHASE1–PHASE4)
specs/            # the development specification (HTML); the source of truth for design intent
```

## Build & test

```bash
cargo test                                   # all Rust tests (engine + FFI + storage conformance + controller)
cargo build -p bydesigns-engine --release    # produces target/release/libengine.{a,so,dylib}
cargo fmt --all && cargo clippy --all-targets -- -D warnings   # CI gates

# Bun embedded client (rebuild libengine first after any C-ABI/engine change):
cargo build -p bydesigns-engine --release && (cd clients/bun && bun test)
```

## Roadmap

1. **Embedded library** — `bun:ffi` + `LocalFileStorage`. Fastest path to a working demo, zero infra. ✅ *implemented*
2. **`ObjectStorage`** — LSM-on-S3 page store + S3-CAS commit log → disaggregated + scale-to-zero, still embedded. ✅ *implemented*
3. **`engine-server` + Postgres wire protocol** — remote/server mode for multi-client and tools that expect Postgres. ✅ *implemented*
4. **Controller** — idle stop (scale-to-zero) + branch-on-LSN (instant clones) + single-writer fencing. ✅ *implemented*
5. **Capabilities** — built-in vector search; compose auth / REST / OLAP over the shared storage floor. *planned*

See the [full roadmap](specs/13-roadmap.html) for milestones, dependencies, and exit criteria.

## License

Licensed under the **GNU Affero General Public License v3.0** (AGPL-3.0). See [`LICENSE`](LICENSE).

If you run a modified version of this software as a network service, the AGPL requires you to make your modified source available to its users.
