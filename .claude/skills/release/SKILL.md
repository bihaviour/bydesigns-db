---
name: release
description: Cut a versioned release for this repo — create the vX.Y.Z tag from the Cargo workspace version and publish a GitHub release with human-readable notes. Use when the user says "/release", "cut a release", "publish a release", or "tag a release".
argument-hint: "[version] [--draft] [--notes \"…\"]"
---

# /release — publish a version release

Tag and publish a release for `bihaviour/bydesigns-db`. CI also auto-tags on
`main` when all gates are green; this skill is the manual / curated path and is
where you write good notes.

## Preconditions (verify, don't assume)
- The working tree is clean and you're on the intended commit (normally `main`,
  or the commit the user names). `git status --porcelain` should be empty.
- The gates pass locally or in CI: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
  `cargo test --workspace`, and the Bun e2e (`cargo build -p bydesigns-engine --release`
  then `cd clients/bun && bun test`). Do not release on red.

## Resolve the version
- Default to the Cargo workspace version:
  `awk -F'"' '/^\[workspace.package\]/{f=1} f&&/^version[[:space:]]*=/{print $2; exit}' Cargo.toml`
- Tag is `v<version>`. If the user passed an explicit version, use it — and if it
  differs from `Cargo.toml`, offer to bump `Cargo.toml` (and `ENGINE_ABI_VERSION`
  if the C ABI changed) in a commit first, since the tag should match the code.
- If the tag already exists, stop and report — never move an existing tag.

## Write human-readable notes
Summarize what changed since the previous tag for a human reader — group as
**Added / Changed / Fixed**, reference notable PRs, and call out any breaking
changes (especially C ABI or `Storage` trait changes). Don't just dump commit
subjects. Seed from `git log <prev-tag>..HEAD --no-merges --pretty='- %s'` (or
`--generate-notes`) and then edit into prose.

## Publish (outward-facing — confirm before running)
Pick a backend like the `/gh` skill: GitHub MCP tools if available, else `gh` CLI.
- Build attachable artifacts: `cargo build -p bydesigns-engine --release` →
  attach `target/release/libengine.so` and `crates/engine/include/engine.h`.
- CLI: `gh release create v<version> --target <sha> --title "v<version>" --notes "<curated notes>" <assets…>`
  (add `--draft` if the user wants to review first).
- MCP: create the release/tag via the GitHub tools with the same title, notes,
  and target commit.
- After publishing, report the release URL.

## Guardrails
- Releasing is public and effectively irreversible — confirm the version, target
  commit, and notes with the user before creating it, unless they've said go.
- Keep the tag and the committed code in sync; don't tag a tree whose version
  field says something else.
