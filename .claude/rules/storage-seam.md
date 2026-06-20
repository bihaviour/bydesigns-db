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
- Keep the trait **async and signature-stable**; Phase 2's `ObjectStorage` must
  drop in without changing the signatures the engine calls. Bump
  `STORAGE_TRAIT_VERSION` for any signature/contract change, and keep the full
  C1–C8 conformance suite green (`crates/storage/src/conformance.rs`).
- Never leak backend-specific concepts (S3 keys, file offsets, LSM layer ids)
  into the trait surface.
