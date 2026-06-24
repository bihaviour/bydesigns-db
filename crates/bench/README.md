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

The subcommands group into four families (spec 15 "Command structure").

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
correctness profiles), `--rows` (default 1000, mix working set), `--label`,
`--json`. The JSON record carries the `transport` so embedded and server runs are
distinguishable when archived together.

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
