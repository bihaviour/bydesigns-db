# twill-bench

The embedded (in-process) benchmark driver for the validation plan
(`pages/specs/09-benchmark-plan.html`; issue #6 / #29). It drives the engine
directly through the Rust/FFI API and reports latency as **percentiles**
(p50/p99/p999) via a compact HDR-style histogram — never mean-only, as spec 09
requires.

## Run

```bash
cargo build -p twill-bench --release
BIN=target/release/twill-bench

# Exp 1 — single-commit latency floor (one sequential writer)
$BIN exp1 --url file:///tmp/bench.db --duration-ms 5000

# Exp 2 — group-commit throughput curve (N independent-row writers)
$BIN exp2 --url file:///tmp/bench.db --writers 8 --duration-ms 5000

# Exp 3 — write-contention wall (N writers on the same row)
$BIN exp3 --url file:///tmp/bench.db --writers 8 --duration-ms 5000
```

Each run prints a human summary plus a one-line JSON record (experiment, backend,
git SHA, writers, throughput, p50/p99/p999) for archiving and plotting.

Flags: `--url` (required), `--writers`, `--warmup-ms` (default 200),
`--duration-ms` (default 1000), `--label`.

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

- **Exp 2** only lifts above the Exp 1 ceiling once **group commit** is
  implemented in the engine commit path; until then exp2 reports a flat plateau
  with a large tail (writers serialize one round-trip at a time), which is the
  expected pre-group-commit signal.
- **Exp 3** counts the first-committer-wins conflicts it retries; the retry loop
  is what keeps a contended counter correct (see `pages/docs/hot-row.html`).
- Server-mode drivers (`pgbench`, TPC-C via `go-tpc`/BenchBase) cover the same
  experiments over the pgwire path and live outside this crate.
