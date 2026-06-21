# Rule: testing & verification

- A behaviour change ships with a test that would fail without it. Engine
  correctness lives in `crates/engine/tests/`, the C ABI surface in
  `crates/engine/tests/ffi.rs`, storage durability/MVCC in the C1–C8 conformance
  suite (`crates/storage/src/conformance.rs`), copy-on-write branching in
  `crates/storage/tests/branching.rs` (both backends), and the scale-to-zero
  lifecycle in `crates/controller/src/tests.rs`.
- The Bun client loads the native library through `bun:ffi`. After any change to
  the C ABI or engine behaviour, **rebuild `libengine` in release and re-run
  `bun test`** — otherwise Bun silently runs against a stale binary:
  `cargo build -p twill-engine --release && (cd clients/bun && bun test)`.
- Durability is non-negotiable: changes to the commit/recovery path must keep the
  crash-safety tests green (C1 durability-after-ack, C5 deterministic recovery,
  and the torn-trailing-frame test in `crates/storage/tests/conformance.rs`).
- Report results faithfully — if a test fails or a step was skipped, say so with
  the output; don't claim green without running it.
