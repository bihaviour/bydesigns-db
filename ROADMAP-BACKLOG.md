# Roadmap backlog — scoped, not yet started

This is the scoping index for the four roadmap tracks that are **deliberately
not yet implemented** (the genuinely-open items from the [release roadmap](pages/release/index.html),
as distinct from everything Phases 1–6 already shipped). Each track is a GitHub
**epic** with linked **sub-issues**; this file is the at-a-glance map. The
detail — files touched, key questions, acceptance criteria — lives in each issue.

Shared invariant across every track: **the storage seam never moves.**
`STORAGE_TRAIT_VERSION` and `ENGINE_ABI_VERSION` stay put unless an issue calls
out otherwise; the C1–C8 conformance suite, branching, and scale-to-zero stay
green.

---

## 1. Phase 7 — Row-Level Security · epic [#88](https://github.com/bihaviour/twill-db/issues/88)

Engine-native per-row enforcement (the predicate is built *into* the executor;
identity/JWT stay composed *around* it). Spec 17 is still **Proposal** awaiting
sign-off — the first sub-issue is the design lock.

| Sub-issue | Scope |
|---|---|
| [#92](https://github.com/bihaviour/twill-db/issues/92) P7-1 | Session context + `auth.*` accessors (foundation) |
| [#93](https://github.com/bihaviour/twill-db/issues/93) P7-2 | Policy DDL + catalog persistence (`CreatePolicy` WAL op) |
| [#94](https://github.com/bihaviour/twill-db/issues/94) P7-3 | Read-path enforcement (`USING` + default-deny) |
| [#95](https://github.com/bihaviour/twill-db/issues/95) P7-4 | Write-path enforcement (`WITH CHECK`, `RETURNING`, bypass) |
| [#96](https://github.com/bihaviour/twill-db/issues/96) P7-5 | Security gate + composition (bypass-resistance, branch/PITR, PostgREST) |

Critical path: P7-1 → P7-2 → P7-3 → P7-4 → P7-5 (P7-1/P7-2 parallel; P7-3/P7-4 parallel).
Spec: `pages/specs/17-row-level-security.html`.

## 2. Vector hardening · epic [#89](https://github.com/bihaviour/twill-db/issues/89)

The three deferred items from the Phase 5 vector scope boundaries. The index
stays derived-from-WAL — no side file, no `STORAGE_TRAIT_VERSION` change.

| Sub-issue | Scope | Priority |
|---|---|---|
| [#97](https://github.com/bihaviour/twill-db/issues/97) VH-1 | Page-laid-out vector index (cold-read optimization) | MAY |
| [#98](https://github.com/bihaviour/twill-db/issues/98) VH-2 | Incremental maintenance under delete churn | MAY (highest value) |
| [#99](https://github.com/bihaviour/twill-db/issues/99) VH-3 | Recall tuning (`ef_search`) + documented trade-off | SHOULD |

Mostly independent; suggested order VH-3 (lowest friction) → VH-2 → VH-1.
Specs: `pages/specs/12-capabilities.html`, `pages/specs/phase-5-capabilities.html`.

## 3. Exploratory / parallel tracks · epic [#90](https://github.com/bihaviour/twill-db/issues/90)

Mostly **decisions / spikes** — each closes with a documented decision (and a
prototype or boundary table), not a finished feature. None moves the seam or core.

| Sub-issue | Scope | Weight |
|---|---|---|
| [#100](https://github.com/bihaviour/twill-db/issues/100) EX-1 | WASM build track (Cloudflare Workers + R2) | MAY |
| [#101](https://github.com/bihaviour/twill-db/issues/101) EX-2 | NAPI vs FFI decision (single Bun+Node package) | MAY |
| [#102](https://github.com/bihaviour/twill-db/issues/102) EX-3 | Explicit pgwire subset boundary (matrix + conformance) | SHOULD |
| [#103](https://github.com/bihaviour/twill-db/issues/103) EX-4 | Per-tool hot-row go/no-go (observable contention + runbook) | MUST keep |

EX-3 should land before EX-1 settles its listener shape; EX-4 ties to V-3 below.
Specs: `pages/specs/{11-deployment-targets,08-bun-integration,07-server-mode,10-hot-row-contention}.html`.

## 4. Spec-09 benchmark validation campaign · epic [#91](https://github.com/bihaviour/twill-db/issues/91)

The Twill Bench CLI is complete; what remains is *running* the five experiments
for real (real S3, two missing variants, baselines + CI gating).

| Sub-issue | Scope |
|---|---|
| [#104](https://github.com/bihaviour/twill-db/issues/104) V-1 | Exp 1 latency floor on real object storage (+ variants, gate) |
| [#105](https://github.com/bihaviour/twill-db/issues/105) V-2 | Exp 2 group-commit-window sweep + plateau detection |
| [#106](https://github.com/bihaviour/twill-db/issues/106) V-3 | Exp 3 N-database sharding variant + cross-DB CAS analysis |
| [#107](https://github.com/bihaviour/twill-db/issues/107) V-4 | Exp 5 thundering-herd concurrent cold starts |
| [#108](https://github.com/bihaviour/twill-db/issues/108) V-5 | Baseline capture, archival & CI regression gate (W1/W2 tables) |

V-1 is the spine; V-2/V-3 depend on its baseline; V-4 parallel; V-5 closes it.
Specs: `pages/specs/09-benchmark-plan.html`, `pages/specs/15-twill-bench.html`.

---

*Generated as part of the release-spec review (PR #87). Phases 1–6 are shipped;
the items above are the scoped-for-later remainder.*
