# Rule: Rust conventions

- Code must be `cargo fmt`-clean and `cargo clippy --all-targets -D warnings`-clean
  before commit. Match the surrounding style; keep comment density and naming
  consistent with the file you are editing.
- **No panic may cross the FFI boundary.** Every `#[no_mangle] extern "C"`
  function in `crates/engine/src/ffi.rs` is wrapped in `catch_unwind` and returns
  a defined `EngineStatus`; null/invalid handles return `ENGINE_ERR_MISUSE`, a
  caught panic returns `ENGINE_ERR_INTERNAL`. Preserve this for any new export.
- `unsafe` is confined to the FFI layer (raw-pointer handle deref) and the small,
  documented `block_on`. Don't introduce `unsafe` elsewhere; if a hot path seems
  to need it, reconsider the design first.
- Prefer returning `Result` with the project's error types (`EngineError` /
  `StorageError`) over panicking. Use `expect`/`unwrap` only where an invariant
  makes failure truly impossible, and say why.
- Keep dependencies minimal and deliberate. The engine deliberately hand-rolls
  its SQL parser, WAL codec, and base64; don't add a crate to replace them
  without a clear reason.
- The C ABI in `include/engine.h` is hand-maintained to mirror `ffi.rs`. If you
  change a signature or the status enum, update the header and bump
  `ENGINE_ABI_VERSION` in both the header and `lib.rs`.
