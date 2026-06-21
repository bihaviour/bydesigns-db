---
name: gh
description: Manage GitHub pull requests for this repo — open, merge, or close a PR (and check its status). Prefers the GitHub MCP tools when available, otherwise falls back to the `gh` CLI. Use when the user says "/gh", "open a PR", "merge the PR", "close the PR", or asks about PR status.
argument-hint: "open|merge|close|status [pr-number] [notes…]"
---

# /gh — pull request operations

Drive PR lifecycle for `bihaviour/twill-db`. The first argument is the action.

## Pick a backend (in this order)

1. **GitHub MCP tools** (`mcp__github__*`) if present — preferred in remote/web sessions. Load them with ToolSearch first (e.g. `select:mcp__github__create_pull_request,mcp__github__merge_pull_request,mcp__github__pull_request_read,mcp__github__update_pull_request`).
2. **`gh` CLI** if MCP is unavailable and `gh` is on PATH and authenticated (`gh auth status`).

If neither is available, say so and stop — don't guess.

## Actions

### open
Open a PR from the current branch.
- Determine the head branch (`git rev-parse --abbrev-ref HEAD`) and base (default `main`).
- Refuse if head == base, or if there are uncommitted changes the user expects included — surface that first.
- Title: a concise summary of the branch's changes. Body: what changed and why, plus testing notes. End the body with the repo's standard PR attribution.
- MCP: `mcp__github__create_pull_request`. CLI: `gh pr create --base main --head <branch> --title … --body …`.
- After opening, report the PR URL and offer to watch it for CI/review (see the PR-activity guidance).

### merge
Merge an existing PR. **This is outward-facing and hard to reverse — confirm the PR number and merge method with the user before doing it** unless they already said to proceed.
- Default method: squash (keep history linear) unless the user asks otherwise.
- Pre-check: CI is green and the PR is mergeable (`mcp__github__pull_request_read` method `get_status`/`get_check_runs`, or `gh pr checks`). If checks are red or pending, report that and ask before merging.
- MCP: `mcp__github__merge_pull_request`. CLI: `gh pr merge <n> --squash --delete-branch`.

### close
Close a PR **without merging**. Confirm first (it's a visible action). Optionally post a brief reason as a comment.
- MCP: `mcp__github__update_pull_request` with `state: closed`. CLI: `gh pr close <n> --comment "<reason>"`.

### status
Read-only: summarize the PR's state, CI checks, review threads, and mergeability. No confirmation needed.
- MCP: `mcp__github__pull_request_read` (`get`, `get_status`, `get_check_runs`, `get_review_comments`). CLI: `gh pr view <n>` + `gh pr checks <n>`.

## Guardrails
- Repo scope is `bihaviour/twill-db`; don't touch other repos.
- Never open a PR unless explicitly asked.
- Be frugal with PR comments — only when genuinely useful.
- If a PR number isn't given for merge/close/status, resolve it from the current branch (`gh pr view --json number` or `mcp__github__list_pull_requests` filtered by head), and confirm which PR you mean.
