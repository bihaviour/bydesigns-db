---
name: phase5-composition-branch-status
description: Status of branch phase5-composition-pooler — composition/pooler work for issues #20/#26/#27/#28
metadata:
  type: project
---

Branch `phase5-composition-pooler` (started 2026-06-21) works the remaining open
GitHub issues after Phase 5. Status as of last session:

- **#7** (Phase 1 scaffold) — verified done, **closed**.
- **#26** better-auth — DONE. Real adapter `clients/bun/src/better-auth.ts`
  (maps better-auth CRUD onto the SQL subset: OR-chain for IN, client-side
  OFFSET, literal LIMIT). Automated test `test/better-auth.test.ts` (runs in
  CI's `bun test`), example `examples/auth-app.ts`, docs `pages/docs/auth.html`.
- **#20** pooler — DONE. `deploy/pooler/{pgbouncer.ini,pgcat.toml,README.md}`,
  test `pgwire_transaction_mode_pooling_preserves_correctness`, Bun.sql docs.
  (Surfaced [[engine-server-churn-registry-race]] while doing this.)
- **#28** DuckDB/HTAP — DONE. Materializer `clients/bun/src/olap.ts` (Parquet via
  DuckDB COPY, CSV fallback, configurable cadence), test `test/olap.test.ts`
  (DuckDB round-trip skipIf no CLI), example `examples/duckdb-olap.ts`, docs
  `pages/docs/olap.html`.
- **#27** PostgREST — **BLOCKED / awaiting user decision.** engine-server only
  answers handshake introspection (`crates/server/src/introspect.rs`), NOT the
  deep `pg_catalog`/`information_schema` schema-cache query PostgREST runs at
  startup. The Phase-3 spec admits this ("PostgREST itself was not exercised").
  Making it work needs catalog-introspection emulation in engine-server
  (version-brittle). Options offered: implement it / config+honest-docs+defer /
  skip. User leaning undecided; recommended config+docs+defer.

Depth decision for composition tasks (user-approved): real automated CI test
for the JS-library case (better-auth); runnable-example + docs for external
binaries (DuckDB, pooler) — not gated in CI to avoid provisioning Haskell/C/CLI
toolchains. Not yet a PR (user hasn't asked); commits are per-issue on the branch.
