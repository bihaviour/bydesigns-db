# twill-bench

The benchmarking, correctness, and serverless-efficiency driver for Twill DB
(the validation plan `pages/specs/09-benchmark-plan.html` and the CLI it
operationalizes, `pages/specs/15-twill-bench.html`; issue #6 / #29). It reports
latency as **percentiles** (p50/p90/p95/p99/p999) via a compact HDR-style
histogram — never mean-only, as spec 09 requires — and, for the correctness
profiles, **asserts an ACID invariant over the data it just drove**, failing the
run (exit code 2) when an invariant is violated however fast it ran. It drives
the *same* work over two transports, so the embedded and server paths are
measured identically:

- **embedded** (default) — the engine in-process through the Rust/FFI API.
- **pgwire** (`--transport pgwire`) — the engine through the Postgres wire
  protocol. With no `--server` it spins up an in-process `engine-server`
  listener (so the wire path is offline-testable); with `--server host:port` it
  drives a deployed `engine-server`, the form a real-host run (or `pgbench`)
  takes.

## Run

The subcommands group into families (spec 15 "Command structure").

### Experiments (spec 09 — one lever each)

```bash
cargo build -p twill-bench --release
BIN=target/release/twill-bench

# Exp 1 — single-commit latency floor (one sequential writer)
$BIN exp1 --url file:///tmp/bench.db --duration-ms 5000

# Exp 2 — group-commit throughput curve (N independent-row writers)
$BIN exp2 --url file:///tmp/bench.db --writers 8 --duration-ms 5000

# Exp 3 — write-contention wall (N writers on the same row)
$BIN exp3 --url file:///tmp/bench.db --writers 8 --duration-ms 5000

# Server path (pgwire): same experiments through the Postgres wire protocol.
# No --server → an in-process listener is spun up on the given --url backend.
$BIN exp2 --transport pgwire --url file:///tmp/bench.db --writers 8 --duration-ms 5000

# …or drive an already-running engine-server (implies pgwire):
$BIN exp2 --server 127.0.0.1:5433 --url file:///srv.db --writers 8 --duration-ms 5000
```

### Request-mix scenarios (named workload shapes)

A ratio-controlled mix of `SELECT`/`INSERT`/`UPDATE`/`DELETE` over a pre-seeded
working set (`--rows`), approximating an application's request distribution. The
op kind for each request is drawn from the scenario's fixed ratios by a
deterministic per-writer PRNG, so a run is reproducible.

```bash
$BIN read-heavy  --url file:///tmp/bench.db --writers 8 --duration-ms 5000 --rows 10000  # 90% read / 10% insert
$BIN write-heavy --url file:///tmp/bench.db --writers 8 --duration-ms 5000               # 20% read / 80% insert
$BIN mixed-oltp  --url file:///tmp/bench.db --writers 8 --duration-ms 5000               # 70/20/8/2
```

### Correctness profiles (assert an ACID invariant)

Fixed-work (`--ops` per writer, so the expected result is known) contended
workloads that drive the engine hard, then **assert an invariant over the
result** and exit non-zero (code 2) on violation. Conflicts are retried, so a
violation can only come from a real isolation/durability bug.

```bash
# N writers increment one row; asserts the final value == writers × ops.
$BIN counter --url file:///tmp/bench.db --writers 8 --ops 1000

# Concurrent atomic transfers between two accounts; asserts the summed balance
# is conserved (no torn transfer leaks or destroys value).
$BIN bank-transfer --url file:///tmp/bench.db --writers 8 --ops 1000

# Concurrent stock decrements (read → refuse-oversell → decrement, in a txn);
# seeded with exactly writers × ops units, asserts the shelf lands at exactly 0
# (no decrement lost, no stock driven negative).
$BIN inventory --url file:///tmp/bench.db --writers 8 --ops 1000

# Concurrent client-side read-modify-write edits to one document (read rev,
# write rev + 1); asserts the final rev == writers × ops (no lost edit — proof
# snapshot isolation conflicts the colliding commits).
$BIN document-editing --url file:///tmp/bench.db --writers 8 --ops 1000
```

A test-only `--inject-fault lost-update` hook makes `counter` deliberately drop
one acked increment, so the suite can prove the checker itself bites: a seeded
violation must exit 2, not pass.

### Lifecycle scenario (serverless-efficiency report)

`scale-to-zero` is spec 09 **Experiment 5** (cold read): it drives `query → idle
past the controller's reaper → query` for `--cycles` cold-boot samples, so each
cycle pays a real cold start (fence acquire + WAL replay) and a first cold read.
It is **controller-driven and in-process** — the scenario owns a
`twill-controller` and *pulls* its `ControllerStats` snapshot at the run
boundaries (the #53 metric source: pull, never scrape or push), reporting the
cold-boot percentile distribution plus the controller-sourced lifecycle figures
(cold/warm starts, scale-to-zero events, compute active/idle, admission wait,
lease renews) and the derived serverless-efficiency numbers (utilization,
compute-seconds/query) under the settled `twill_*` vocabulary.

```bash
# 20 cold-boot cycles; --idle-ms sets the reaper window (short for a smoke run;
# spec 09 Exp 5 uses a long idle window on a real object-store deployment).
$BIN scale-to-zero --url file:///tmp/bench.db --rows 1000 --cycles 20 --idle-ms 100
```

A deployed server runs its own controller out of the bench's reach, so the
`--server`/`--transport pgwire` form is rejected; run it embedded against any
backend URL (`file://` for a smoke run, an object store for the spec-09 tail).

### Soak scenario (interval-sampled time series; leak/drift verdict)

`long-run` is a multi-hour/day **stability** test (issue #80): it catches what
only shows up over time — memory leaks, fd/connection leaks, scheduler drift,
slow latency degradation. Unlike every other scenario (a single start→end
delta), it captures a **time series**: an interval sampler pulls a `stats()`
snapshot plus a process resource probe (RSS / open fds / threads from Linux
`/proc/self`, degrading to zeros where `/proc` is absent) every
`--sample-interval-ms`, then fits a least-squares slope over the post-warm-up
window for **memory, fds, and p99**. A metric is flagged as a leak/drift only
when its projected growth crosses *both* a relative threshold
(`--drift-threshold`, default 10%) and an absolute noise floor — so short-run
jitter never trips it, but an unbounded climb does. A detected leak/drift fails
the run with the correctness exit code (2), exactly like a violated invariant.

```bash
# Steady read load over a 1000-row working set; sample every 2s for an hour.
$BIN long-run --url file:///tmp/bench.db --rows 1000 \
  --duration-ms 3600000 --sample-interval-ms 2000 --warmup-ms 30000
```

The load is a deliberately **steady-state** point-read workload over a
pre-seeded set: a soak is a stability baseline, and an unbounded ingest load
would grow the WAL and the in-memory MVCC store on its own (no vacuum this
phase), drowning the very signal the soak exists to find. Like `scale-to-zero`,
it samples this process's own resources, so the `--server`/`--transport pgwire`
form is rejected — run it embedded. The JSON record carries a `soak` section
with the per-metric `first`/`last`/`slope`/`peak`/`growth_frac` and the
PASS/FAIL `drift_pass` verdict.

### Release comparison (CI regression gate)

Diff two archived JSON records into a PASS/regression verdict — pure
post-processing, no engine or transport needed. A regression (throughput down,
or p99/p999 up, beyond `--threshold`, default 10%) exits 1.

```bash
$BIN exp2 --url file:///tmp/bench.db --writers 8 --json > baseline.json
# … build the candidate …
$BIN exp2 --url file:///tmp/bench.db --writers 8 --json > candidate.json
$BIN compare --baseline baseline.json --candidate candidate.json --threshold 0.10
```

Each run prints a human summary plus a one-line JSON record (experiment, backend,
git SHA, writers, throughput, p50/p90/p95/p99/p999, and — for the profiles — the
correctness verdict) for archiving and plotting; `--json` emits only that record.

Flags: `--url` (required for runs), `--transport embedded|pgwire`,
`--server HOST:PORT` (implies pgwire), `--writers`, `--warmup-ms` (default 200),
`--duration-ms` (default 1000, experiments/scenarios), `--ops` (default 200,
correctness profiles), `--rows` (default 1000, mix working set / cold-read set),
`--cycles` (default 20, scale-to-zero cold-boot samples), `--idle-ms` (default
100, scale-to-zero reaper window), `--sample-interval-ms` (default 1000,
long-run time-series sample period), `--drift-threshold` (default 0.10, long-run
leak/drift trip fraction), `--label`, `--json`. The JSON record carries the
`transport` so embedded and server runs are distinguishable when archived
together.

### Exit codes

`0` success · `1` benchmark failed (or `compare` regression) · `2` correctness
invariant violated · `3` configuration/usage error · `4` connection error.

## What runs here vs on a real host

`file://` exercises the engine and the full commit/recovery path with a **local
fsync** — useful for smoke runs and for comparing engine versions, but it does
**not** carry the S3/CAS network round-trip that defines the W1 latency tail. The
gating numbers in spec 09 (Exp 1 same-region/cross-region, the Exp 2 plateau, the
Exp 3 wall) must be taken against a real object store on a real host:

```bash
$BIN exp1 --url s3://my-bucket/benchdb --duration-ms 60000
```

Set `TWILL_BENCH_GIT_SHA` to pin the recorded commit in CI/automation.

## Notes

- **Exp 2** measures the **group-commit** lever (implemented in the engine commit
  path: concurrent commits coalesce into one durable append — see
  `crates/engine/src/group_commit.rs`). The Exp-2 plateau therefore rises above
  the Exp-1 single-writer ceiling. On `file://` the lift is modest because a
  local `fsync` is microseconds, so per-commit overhead dominates rather than the
  durable handoff; the dramatic 10–100× plateau the spec targets appears on a
  real object store, where each commit otherwise pays a ~10ms network round-trip
  that batching amortizes across the group.
- **Exp 3** counts the first-committer-wins conflicts it retries; the retry loop
  is what keeps a contended counter correct (see `pages/docs/hot-row.html`).
  Over `--transport pgwire` the conflict arrives as SQLSTATE `40001`
  (serialization_failure) and the driver retries it, exactly as `pgbench
  --max-tries` would.
- **Server-mode (`--transport pgwire`)** drives the experiments through the
  Postgres wire path via a small in-crate client (`src/pgclient.rs`) — no
  external Postgres tooling, so the wire path is exercised in `cargo test`
  (`tests/pgwire.rs`) against an in-process listener. `tests/pgwire.rs` also
  pins the **pooler** property (issue #20): a bounded backend pool carries the
  whole transaction load with no lost or duplicated commits, modelling what a
  transaction-mode pooler (PgBouncer/pgcat) presents to the engine. The pooler
  configs and a `pgbench` soak command live in `deploy/pooler/`. `pgbench` and TPC-C (via
  `go-tpc`/BenchBase) remain the off-the-shelf drivers for real-host runs and a
  realistic OLTP mix (spec 09); point `--server` at the same `engine-server`
  they target to compare.
