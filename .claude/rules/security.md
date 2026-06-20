# Rule: security

- Never commit secrets (API keys, tokens, private keys, cloud credentials).
  Connection strings in code/tests/examples use `file://` only; real bucket
  credentials belong in the environment, never in the repo. The PostToolUse
  security hook and CI secret scan exist to catch slips — treat a hit as a stop.
- Treat data crossing the FFI boundary as untrusted: validate lengths and indices,
  copy borrowed `const char*` out before the owning object advances, and never
  dereference a handle without the null check + `catch_unwind` guard.
- Keep dependencies auditable. CI runs a dependency vulnerability audit; if it
  flags an advisory, upgrade or justify with a documented exception rather than
  silencing it.
- This is a database engine: durability and single-writer fencing are security
  properties, not just features. Don't weaken `append_wal` durability or the fence
  checks for performance.
