---
name: engine-server-churn-registry-race
description: engine-server can lose acked commits under concurrent short-lived connections to the same file:// URL (Weak-registry re-creation race)
metadata:
  type: project
---

While building the #20 pooler test (2026-06-21), found that `engine-server` can
**lose committed data** under a burst of *concurrent, short-lived* connections
to the same `file://` URL. A burst of 320 connections (8 threads × 40), each a
fresh connect → `BEGIN`/`INSERT` distinct row/`COMMIT` → disconnect, ended with
~307/320 rows persisted — COMMIT returned OK but the rows were gone.

Characterization:
- **Sequential** short-lived connections: no loss (200/200).
- **Concurrent with a stable backend pool** (4 persistent connections, many txns each): no loss (320/320, repeatable) — this is what a transaction-mode pooler presents to the engine.
- Only **concurrent open/close churn** loses data.

Root cause (suspected): the process-global `Database` registry in
`crates/engine/src/db.rs` keys by URL and holds **`Weak<Database>`**. Under churn
the strong count frequently hits zero, so concurrent re-opens race and can create
divergent `Database` instances over the same file → interleaved/lost WAL writes.

This is *mitigated in production* by the transaction-mode pooler (issue #20),
which keeps a small stable backend set — and the architecture assumes a pooler in
front of `engine-server`. But "COMMIT acks then data lost" is a real durability
hole independent of the pooler. The #20 test
(`crates/bench/tests/pgwire.rs::pgwire_transaction_mode_pooling_preserves_correctness`)
deliberately models the pooled (stable-backend) shape, which is correct; it does
**not** assert the raw-churn shape, which fails.

**Why:** durability/single-writer fencing are security properties here (security
rule); an acked-but-lost commit violates the WAL durability contract.
**How to apply:** if asked to fix, look at the `Weak`-based registry re-creation
path in `db.rs::open` (lease/fence interaction on re-open) — not the WAL codec,
which is fine sequentially. Consider serializing registry open/replace or holding
a brief strong ref during handoff. See [[phase5-composition-branch-status]].
