# Homebrew formula for the `twilldb` scaffolding CLI.
#
# This is the canonical source of the formula. It lives in this repo for review
# and manual use; the release workflow (.github/workflows/release-cli.yml)
# renders an identical formula into the tap repo (bihaviour/homebrew-twilldb)
# on every `v*` tag, filling in `url` + `sha256` for that version.
#
# Users install with:
#   brew tap bihaviour/twilldb        # → github.com/bihaviour/homebrew-twilldb
#   brew install twilldb
#
# It builds from source via cargo (no maturity gate, no homebrew-core review),
# so a Rust toolchain is pulled in only as a build dependency.
class Twilldb < Formula
  desc "Project scaffolding CLI for the Twill DB engine"
  homepage "https://github.com/bihaviour/twill-db"
  url "https://github.com/bihaviour/twill-db/archive/refs/tags/v0.4.0.tar.gz"
  # Placeholder — the release workflow computes and substitutes the real digest
  # for each tagged tarball. Replace before a manual `brew install` from source.
  sha256 "0000000000000000000000000000000000000000000000000000000000000000"
  license "BUSL-1.1"
  head "https://github.com/bihaviour/twill-db.git", branch: "main"

  depends_on "rust" => :build

  def install
    # Builds only the CLI crate (it has no workspace dependencies).
    system "cargo", "install", *std_cargo_args(path: "crates/cli")
  end

  test do
    assert_match "twilldb #{version}", shell_output("#{bin}/twilldb version")
    system bin/"twilldb", "new", "smoke"
    assert_predicate testpath/"smoke/package.json", :exist?
  end
end
