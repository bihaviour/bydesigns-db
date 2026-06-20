# bydesigns-db

> DB Management System with separated process and storage layer. Built on Rust, and compatible with PostgreSQL.

**Status: Phase 1 implemented (embedded library).** The repository holds both the development specifications and a working Rust implementation of [Phase 1](specs/13-roadmap.html) — the embeddable `libengine` with a frozen C ABI, the pluggable `Storage` trait + `LocalFileStorage` backend, MVCC snapshot isolation, crash-safe WAL durability + replay, and the `@yourdb/bun` FFI wrapper. Later phases (object storage, server mode, controller) follow the roadmap below.

## What this is

`bydesigns-db` is an OLTP database engine that is, at the same time:

- **Embeddable** — links in-process as a library, at function-call latency (SQLite-style), and
- **Storage-disaggregated** — durable state lives on object storage (S3 / Cloudflare R2 / MinIO), so compute is stateless.

These usually pull in opposite directions. The resolution is to keep the engine a **library** and make its **storage backend pluggable**, pointing the seam at the network instead of a local file — rather than putting a server at the boundary. The *same* engine then runs embedded (via FFI) **or** as a **PostgreSQL wire-compatible server**; the storage choice is configuration, not a rebuild.

Headline properties: **scale-to-zero**, **true embeddability**, and **instant branching** (copy-on-write over LSN-versioned immutable layers).

## Architecture at a glance

- **Engine (Rust library)** — SQL parser → planner → executor, MVCC (snapshot isolation via LSN-stamped versions), WAL generation, and a local page cache. Ships as `cdylib` + `staticlib` with a stable C ABI (`engine.h`), plus an `engine-server` binary.
- **Pluggable `Storage` trait** — the central seam. Two backends: `LocalFileStorage` (pure embedded, zero network) and `ObjectStorage` (disaggregated).
- **Object-storage backend** — an LSM page store (versioned by LSN) plus an ordered commit log whose durability bottoms out on **S3 conditional writes (compare-and-swap)** — atomic ordered appends and single-writer fencing without a separate consensus cluster.
- **Interfaces** — embedded via `bun:ffi` / NAPI; server via the **Postgres wire protocol**, so existing tooling (PostgREST, `Bun.sql`, standard `psql`/pg drivers) connects unchanged.

## Specifications

The full development spec is a self-contained HTML site under [`specs/`](specs/). Open [`specs/index.html`](specs/index.html) in a browser, or serve the folder locally:

```bash
cd specs && python3 -m http.server
# then visit http://localhost:8000
```

Selected documents:

| Spec | |
|---|---|
| [Architecture Overview](specs/01-architecture-overview.html) | The three slots and inter-layer protocols |
| [Engine Core](specs/02-engine-core.html) | C ABI, MVCC, WAL, execution pipeline |
| [Storage Interface](specs/03-storage-interface.html) | The pluggable `Storage` trait (the seam) |
| [Object-Storage Backend](specs/04-object-storage-backend.html) | LSM page store + S3-CAS commit log |
| [Benchmark & Validation Plan](specs/09-benchmark-plan.html) | Latency/throughput/crash-safety experiments |
| [Roadmap & Build Sequence](specs/13-roadmap.html) | Phased delivery plan |

## Phase 1 — build & run

Phase 1 (the embedded library) is implemented as a Cargo workspace plus a Bun client:

```
crates/storage   # the pluggable `Storage` trait (the seam) + LocalFileStorage + C1–C8 conformance suite
crates/engine    # libengine: SQL → MVCC → WAL, and the stable C ABI (include/engine.h)
clients/bun      # @yourdb/bun: bun:ffi bindings + ergonomic typed wrapper + example
```

Build the engine and run the Rust test suite (correctness, MVCC snapshot isolation, persistence-across-restart, storage conformance, crash recovery):

```bash
cargo test                                   # all Rust tests
cargo build -p bydesigns-engine --release    # produces target/release/libengine.{a,so,dylib}
```

Then the embedded path from Bun (auto-discovers the built library, or set `YOURDB_ENGINE_PATH`):

```bash
cd clients/bun
bun test                                     # end-to-end embedded tests
bun run examples/notes.ts                     # runnable sample app
```

Embedded quickstart:

```ts
import { open } from "@yourdb/bun";

using db = open("file://./local.db");          // storage backend chosen by URL scheme
db.exec(`CREATE TABLE notes (id INTEGER PRIMARY KEY, body TEXT)`);
db.query("INSERT INTO notes VALUES (?, ?)", [1, "hello"]);
const rows = db.query("SELECT id, body FROM notes");   // [{ id: "1", body: "hello" }]
```

The C ABI frozen in this phase is [`crates/engine/include/engine.h`](crates/engine/include/engine.h); every later phase reuses it unchanged.

## Roadmap

1. **Embedded library first** — `bun:ffi` + `LocalFileStorage`. Fastest path to a working demo, zero infra. ✅ *implemented*
2. **Add `ObjectStorage`** — LSM-on-S3 page store + S3-CAS commit log → disaggregated + scale-to-zero, still embedded.
3. **Add `engine-server` + Postgres wire protocol** — remote/server mode for multi-client and tools that expect Postgres.
4. **Add the controller** — idle stop + branch-on-LSN → scale-to-zero and instant clones.

See the [full roadmap](specs/13-roadmap.html) for milestones, dependencies, and exit criteria.

## License

Licensed under the **GNU Affero General Public License v3.0** (AGPL-3.0). See [`LICENSE`](LICENSE).

If you run a modified version of this software as a network service, the AGPL requires you to make your modified source available to its users.
