# Rule: protect the storage seam

The pluggable `Storage` trait (`crates/storage/src/lib.rs`) is the architectural
seam that makes the engine both embeddable and storage-disaggregated. It is the
single most load-bearing artifact in the project.

- The engine MUST reach durable state **only** through `trait Storage`. No engine
  code path may open a file, socket, or cloud SDK directly.
- `append_wal` returns the commit LSN **only after the records are durable**.
  Never acknowledge a commit from an in-memory buffer. Group commit / caching may
  hide read latency but never commit latency.
- `get_page(id, lsn)` MUST return the greatest version with `version-LSN <= lsn`
  (the MVCC read floor). LSNs are strictly monotonic, gap-free, never reused.
- Keep the trait **async and signature-stable**; alternate backends must drop in
  without changing the signatures the engine calls (`ObjectStorage` did in Phase 2;
  branching was additive in Phase 4). Bump `STORAGE_TRAIT_VERSION` for any
  signature/contract change (currently `2`), and keep the full C1–C8 conformance
  suite green (`crates/storage/src/conformance.rs`).
- Never leak backend-specific concepts (S3 keys, file offsets, LSM layer ids)
  into the trait surface.
- **Branching lives at the seam, not in the engine.** A branch is a
  `BranchStorage` (parent read-through below the fork LSN + a private write
  overlay), composed backend-agnostically so the *same* copy-on-write semantics
  serve `file://` and object stores. Keep branch write-isolation intact: a
  branch's writes MUST NOT touch the base or any sibling, and creating a branch
  MUST copy no pages (`crates/storage/tests/branching.rs` gates this).
- **The single-writer lease is durable.** `append_wal` is fenced by the monotonic
  CAS epoch (a fresh `acquire_fence` fences every prior token); `renew_fence`
  durably re-stamps the lease and `release_fence` frees it cleanly. Don't weaken
  the epoch fence for performance — it is the split-brain guard.
