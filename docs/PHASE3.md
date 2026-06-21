# Phase 3 — engine-server + pgwire (implementation notes)

This document maps the implemented code to the Phase 3 deliverables, exit
criteria, and Definition of Done in [`specs/13-roadmap.html`](../specs/13-roadmap.html)
and [`specs/07-server-mode.html`](../specs/07-server-mode.html), and records the
deliberate scope decisions.

The Phase-3 claim — **the same engine, a second front door** — holds literally:
`engine-server` links the engine's Rust API unchanged and adds exactly one thing,
a Postgres-wire listener. SQL, MVCC, WAL, the C ABI, and the storage seam are all
untouched. A `file://` URL serves the embedded backend; an `s3://`/`r2://`/`gs://`
URL serves the Phase-2 disaggregated backend — the listener is oblivious, because
it only ever calls `Connection`. (Verified: psql runs DDL/DML/SELECT against an
`s3://`-backed server, and the durable CAS log segments land on the object floor.)

## What shipped

| Deliverable (spec 13 §Phase 3 / spec 07) | Where |
|---|---|
| `engine-server` binary — the engine behind a network listener | `crates/server/src/main.rs`, `lib.rs` (thread-per-connection TCP listener) |
| Protocol 3.0 startup, SSL/GSS probe handling, trust auth | `session.rs::serve` |
| Simple query protocol (`Query`) | `session.rs::simple_query` |
| Extended query protocol (`Parse`/`Bind`/`Describe`/`Execute`/`Sync`/`Close`) | `session.rs` |
| Text **and** binary parameter/result formats | `types.rs` (`encode_value`/`decode_param`) |
| Field-tagged `ErrorResponse` with SQLSTATEs (incl. a defined code for fenced/serialization) | `session.rs::describe_error` |
| Connect-time introspection answers (`version()`, `SHOW`, `current_*`, `SET`) | `introspect.rs` |
| Hand-rolled wire codec (no new dependencies) | `protocol.rs` |

The server adds **zero** dependencies (it links only `bydesigns-engine`) and the
only engine-side change is additive and Rust-API-only: `ResultSet` gained a
`types` field and `Statement` a `column_type` accessor, so the server can report
accurate type OIDs. The C ABI (`engine.h`) and `ENGINE_ABI_VERSION` are unchanged.

## Exit criteria → evidence

- **Serve a connection from Bun.sql and from pgbench with no client-side adapter.**
  Verified live in this environment:
  - **psql** (libpq): `CREATE`/`INSERT`/`SELECT` round-trip with correct command
    tags and output, over both `file://` and `s3://` backends.
  - **Bun.sql**: connects, runs parameterized queries (`… WHERE id = $1`) via the
    extended protocol, returns correct rows.
  - **pgbench**: low-contention run, **0 failed transactions** in both `simple`
    and `-M extended` modes.
- **Re-run Experiment 2 (group-commit throughput) in server mode.** `pgbench`
  sustains hundreds of tps end-to-end through the wire protocol with zero failures
  when contention is spread across many rows.
- **Experiment 3 (write-contention wall) over the wire.** With many clients
  hammering a few rows, the engine's first-committer-wins check surfaces as
  `40001` serialization failures over the protocol — the same red-quadrant
  behavior the embedded engine exhibits, now observable through the listener.
- **Same engine, two front doors.** The listener calls `Connection::{open,
  query, prepare, …}` — the identical entry points `bun:ffi` drives in-process.
  No engine fork; an `s3://` URL transparently serves the disaggregated backend.

In-process protocol tests (`crates/server/tests/wire.rs`) cover the simple path,
NULL handling, introspection, error-then-recover, and the extended path with a
bound parameter — all driven by a minimal in-test pg client over a real socket.

## Architecture decisions (and why)

- **Hand-rolled framing, zero new deps.** Spec 07 SHOULD-suggests starting from
  the `pgwire` crate; the project rule is minimal, deliberate dependencies (it
  hand-rolls the WAL codec, base64, and the object codecs). The supported subset
  is small and the message shapes follow the protocol exactly, so hand-rolling
  keeps the dependency surface at zero with full control. Documented deviation.
- **Thread per connection.** The engine is synchronous and single-writer-per-DB;
  a thread per connection matches that model with no async runtime. A
  transaction-mode pooler (PgBouncer/pgcat) in front absorbs serverless
  connection bursts (spec 07) — deliberately *not* bundled into the server.
- **`Describe` materializes once.** Our engine learns a query's columns by
  executing it, so a portal `Describe` runs the statement once and caches the
  result; the following `Execute` reuses it — a DML statement is never run twice.
  A statement `Describe` (which Bun.sql uses) dummy-runs a *SELECT* with NULL
  params to learn its columns (side-effect-free); a DML statement returns
  `NoData` (it is never executed at describe time).
- **Type OIDs from declared column types.** `ResultSet.types` carries each
  column's catalog type, so `RowDescription` reports accurate OIDs even for an
  empty result. `INTEGER` maps to `int8` (the engine stores 64-bit integers);
  like node-postgres, Bun returns `bigint` as a string to preserve precision —
  faithful Postgres `bigint` behavior, not a defect.

## Deliberate Phase-3 boundaries (documented, not accidental)

- **Trust auth only; TLS terminates out-of-process.** SSL/GSS probes are declined
  (`N`) and the client proceeds cleartext; SCRAM-SHA-256 and in-process TLS are
  spec 07 SHOULDs deferred to a hardening pass. Put a TLS-terminating proxy /
  pooler in front for untrusted networks. No credentials live in the repo.
- **One server, one database URL (`--db`).** The startup `database` field is
  accepted but the listener serves the configured backend; per-database routing
  (mapping the startup database to a backend URL / branch) is a controller-era
  refinement (Phase 4).
- **`COPY` and `CancelRequest` are not implemented.** `pgbench -i` uses `COPY`
  for bulk load, so seed tables with `INSERT` (or `psql -f`); the benchmark
  scripts themselves run fine. `COPY` is a spec 07 MAY ("not required for the
  initial client set").
- **Introspection is a pragmatic subset.** The handshake queries real clients
  issue (`version()`, `current_schema/database/user`, `SHOW`, `SET`) are answered;
  deep `pg_catalog` / `information_schema` introspection (psql's `\d`, full
  PostgREST schema reflection) is not. PostgREST itself was not exercised here.
- **Statement-`Describe` row types.** A statement `Describe` reports accurate OIDs
  from the engine's declared column types; a portal `Describe` (after `Bind`)
  additionally reflects materialized-row inference. Both paths are implemented.

## Running

```bash
cargo build -p bydesigns-server --release      # target/release/engine-server
engine-server --listen 127.0.0.1:5433 --db file://./srv.db        # embedded
engine-server --listen 127.0.0.1:5433 --db s3://bucket/mydb       # disaggregated

# any standard Postgres client connects (sslmode=disable — cleartext):
psql "host=127.0.0.1 port=5433 user=app dbname=app sslmode=disable"
#  Bun:  new SQL("postgres://app@127.0.0.1:5433/app?sslmode=disable")
#  pgbench -n -f script.sql -T 5 -c 8 "host=127.0.0.1 port=5433 user=app dbname=app sslmode=disable"

cargo test -p bydesigns-server                 # in-process wire protocol tests
```

Set `BYDESIGNS_WIRE_DEBUG=1` to trace startup parameters and each frontend
message (off by default).
