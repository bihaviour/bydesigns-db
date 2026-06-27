# Homebrew distribution for `twilldb`

The `twilldb` scaffolding CLI is distributed through a **Homebrew tap** — a
self-hosted formula repo. A tap has no notability/maturity gate and needs no
registration with Homebrew, unlike `homebrew-core` (which also wouldn't accept
the project's `BUSL-1.1` license). Users install with:

```bash
brew tap bihaviour/twilldb     # → github.com/bihaviour/homebrew-twilldb
brew install twilldb
```

> Homebrew resolves a tapped formula as `user/repo/formula`, so a single-token
> `brew install bihaviour/twilldb` is *not* valid. Either tap first (above), or
> use the fully-qualified one-liner `brew install bihaviour/twilldb/twilldb`.

## One-time setup

1. **Create the tap repo.** A public GitHub repo named exactly
   `bihaviour/homebrew-twilldb` (the `homebrew-` prefix is stripped in commands,
   which is why the tap reads as `bihaviour/twilldb`). No other registration.

2. **Seed it** with `Formula/twilldb.rb` — copy [`twilldb.rb`](./twilldb.rb)
   from this directory and fill in the real `sha256` (see below). After that the
   release workflow keeps it current automatically.

3. **Add a push token.** Create a fine-grained PAT with **contents: write** on
   `bihaviour/homebrew-twilldb`, and store it as the secret `HOMEBREW_TAP_TOKEN`
   on this repo (or the org). The release workflow uses it to push the refreshed
   formula.

## How releases stay in sync

`.github/workflows/release-cli.yml` runs on every `v*` tag (or via manual
dispatch). It:

1. downloads the GitHub source tarball for the tag,
2. computes its `sha256`,
3. renders `Formula/twilldb.rb` with the tag's `url` + `sha256`, and
4. commits and pushes it to the tap repo.

The formula **builds from source via cargo** (`depends_on "rust" => :build`),
which is the simplest robust option for v1 — no per-platform binary matrix to
maintain. If build-time on the user's machine becomes a concern, switch to
prebuilt **bottles**: add a binary-build matrix to the workflow, upload the
artifacts to the GitHub Release, and emit `bottle do … end` blocks in the
formula instead of the `install`-from-source step.

## Computing a sha256 manually

```bash
curl -fsSL https://github.com/bihaviour/twill-db/archive/refs/tags/v0.4.0.tar.gz \
  | shasum -a 256
```

## Other channels (not set up here)

The CLI is a plain Rust binary, so the same release can also feed
`cargo install twilldb-cli`, a Scoop bucket (Windows), and a
`curl | sh` install script over GitHub Release binaries. Those are intentionally
out of scope for this first pass — see the CLI spec page for the roadmap.
