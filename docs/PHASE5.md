# Phase 5 — Capabilities: vector search in-core (+ composition)

Phase 5 grows the platform by the Capabilities deciding rule (spec
[12](../specs/12-capabilities.html), roadmap §Phase 5): **storage/execution
capabilities go INTO the engine; interface/service capabilities are COMPOSED
AROUND it.** The one capability that is *built in* is **vector search**; the three
that are *composed around* (better-auth, PostgREST, DuckDB) are adopted unmodified
and demonstrated as glue, never welded into the core.

Everything is additive and the storage seam never moves: `ENGINE_ABI_VERSION`
goes to **3** (the vector type, HNSW, distance operators, and the `v…` bind
encoding — no C symbols added or removed), while **`STORAGE_TRAIT_VERSION` stays
`2`**. The vector index needs no new storage capability: it rides the existing
`append_wal`/replay path exactly like the rows do.

## Built IN — vector search (the headline)

| Deliverable (spec 12) | Where |
|---|---|
| `vector(N)` type — fixed-length `f32`, dimension declared at column time, validated on insert | `crates/engine/src/value.rs` (`Value::Vector`, `ColumnType::Vector(u32)`, `parse_vector`/`format_vector`), enforced in `exec.rs::check_vector_dims` |
| HNSW access method behind the **same `Storage` trait** the rows use | `crates/engine/src/vector.rs` (`VectorIndex` — multi-layer navigable small world, `hnswlib`/`usearch` lineage, dependency-free) |
| Distance operators the planner pushes into the index scan | `<->` L2, `<=>` cosine, `<#>` inner product (`sql.rs` lexer/parser + `exec.rs::vec_distance`) |
| Top-k nearest-neighbour answered by the access method, not a full scan + sort | `exec.rs::knn_select` (+ `knn_plan`): detects `ORDER BY <col> <dist-op> <q> ASC LIMIT k`, searches the index, MVCC-filters the candidates |

### Why it branches / scales-to-zero / S3-backs for free

The engine is WAL-centric: rows are not stored as pages, they are replayed from
the durable WAL into the in-memory MVCC store on open (`db.rs::replay`). The
vector index is built the same way — it is a **derived structure over the
column's vectors**, not a side file. The only durable artifacts are the
`CreateIndex`/`DropIndex` WAL ops (`wal.rs`) and the vector values themselves; the
graph is rebuilt by `Store::rebuild_indexes` after replay (the index's cold-start
"warm"). Three properties fall straight out, with no special-casing:

- **Branching branches the index** (spec 12's differentiator, gated by
  `tests/vector.rs::branch_branches_the_vector_index` and the Bun
  `branching forks the vector index` test). A branch is a `BranchStorage` whose
  replay includes the parent's `CreateIndex` + the in-window inserts, so the
  branch rebuilds its *own* graph; its diverged writes never touch the base.
- **Scale-to-zero**: idle compute drops, the next open replays the WAL and
  rebuilds the graph — the warm is the replay.
- **S3-backing / PITR**: the vectors and the index definition are LSN-versioned
  WAL records like everything else, so the object backend and point-in-time
  window cover them unchanged (`s3://` works with no recompile, same as rows).

### MVCC + index maintenance

The index maps `vid -> vector` for every row version that has one; **visibility is
resolved at query time** against the row version the vid identifies, so the index
stays MVCC-agnostic. Inserts/updates add the new vid (`Store::index_row_inserted`);
deletes need no index change (the row version's `delete_lsn` filters it out); a
rolled-back pending insert is tombstoned out (`Store::rollback_pending`). The KNN
scan over-fetches (`KNN_OVERFETCH`) to absorb invisible/filtered hits, then takes
the first `k` that pass `WHERE` and snapshot visibility.

### SQL surface added

- `embedding VECTOR(768)` column type (dimension required, validated).
- Vector literals `[1, 2, 3]` and `'[1,2,3]'` text coercion (pgvector-style).
- `CREATE INDEX name ON table USING hnsw (col [vector_cosine_ops])
  [WITH (m=…, ef_construction=…, ef_search=…, metric='cosine'|'l2'|'inner_product')]`
  and `DROP INDEX [IF EXISTS] name` (autocommit DDL, like `CREATE TABLE`).
- Distance operators usable in projection, `WHERE`, and `ORDER BY`. Without a
  matching index the query still works as a brute-force scan + sort (the operator
  evaluates per row); with one it is answered by the access method — same results,
  proven by `tests/vector.rs::hnsw_index_answers_top_k_and_matches_brute_force`.

## Composed AROUND — placement, not core code

Per the rule, none of these enter the engine binary; the core stays "rows +
vectors + the storage trait". They are demonstrated as the thin glue we own:

- **better-auth (service, in-process).** Auth state is ordinary rows in the
  embedded engine — no external auth service, and because it is rows it **branches
  and recovers with the database**. Shown in `clients/bun/examples/compose.ts`
  (`users`/`sessions`, a staging-branch with its own users).
- **PostgREST (interface, in front).** Attaches over **server mode** (pgwire,
  Phase 3) with zero engine changes — wire-compatibility is the contract. Absent
  in embedded deployments; nothing to build here.
- **DuckDB (OLAP, over shared storage).** A second engine over the same floor. The
  only code we own is the **materialization job**: publish an open columnar
  snapshot DuckDB reads directly, atomically (temp + rename). Shown in
  `compose.ts` (CSV via `read_csv_auto`; Parquet/Iceberg is the production format,
  the writer being an off-the-shelf piece, not built in-house).

## Tests

- `crates/engine/tests/vector.rs` — type round-trip + dimension validation,
  the three distance operators (brute force), HNSW top-k vs brute-force parity,
  `WHERE`-filtered + MVCC-correct KNN, branch isolation of the index, rebuild from
  WAL on restart, rollback tombstoning, exact nearest over a larger set, and the
  `CREATE INDEX` guards.
- `crates/engine/tests/ffi.rs::vector_search_via_c_abi` — the vector type, an
  HNSW index, and a `v…`-bound query through the same C ABI `bun:ffi` binds.
- `clients/bun/test/vector.test.ts` — the embedded path end-to-end, including the
  branch-the-memory payoff. Examples: `vector-memory.ts`, `compose.ts`.

## Deliberate scope boundaries (documented, not accidental)

- **The index is derived, not paged.** Spec 12 says "store the graph as pages, not
  a side file"; the engine's rows are themselves WAL-derived in-memory state, not
  pages, so the *consistent* realization is a WAL-derived index — same durability
  path, same branch/scale-to-zero semantics, never a side file. The literal page
  layout is a Phase-2-style cold-read optimization, not a Phase-5 requirement.
- **Deletes are tombstones.** A removed vid is filtered from results but its node
  stays in the graph (navigation is unaffected; it is a valid point in space).
  Incremental-vs-full rebuild under heavy delete churn is a spec "MAY" / open
  question, deferred.
- **HNSW is approximate by design.** Recall is governed by `ef_search`; for the
  test-scale data the search is effectively exhaustive, so results are exact.
- **Composed engines are adopted, not vendored.** better-auth/PostgREST/DuckDB are
  external; Phase 5 builds only the glue (the vector capability and the
  materialization job), per spec 12's "build only thin glue".
- **SQL subset unchanged otherwise.** Joins, GROUP BY, subqueries, and `DISTINCT`
  remain out of scope; `ORDER BY` ranks by expressions, not output aliases.
