# Rule: git & release workflow

- Never push directly to `main`. Develop on a feature branch and open a PR.
- Use the `/gh` skill to open, merge, or close pull requests (it prefers the
  GitHub MCP tools, falling back to the `gh` CLI). Use the `/release` skill to cut
  a version tag with human-readable notes.
- Releases are versioned from the Cargo workspace version. CI tags a release
  automatically on `main` once code quality, security, complexity, and unit tests
  are all green; the `/release` skill is the manual equivalent.
- Keep commits focused with descriptive messages. Only commit or push when asked.
- Don't create a pull request unless explicitly asked.
- The website + documentation are static HTML under `pages/` (home, `docs/`,
  `specs/`, `release/`, shared `assets/`). They deploy to GitHub Pages from the
  `pages/` directory via `.github/workflows/pages.yml` on push to `main`. There is
  no build step — edit the HTML/CSS/JS directly and preview with `bunx serve pages`.
  Keep `pages/specs/` (design source of truth) and `pages/docs/` (user docs) in sync
  with behaviour changes.
