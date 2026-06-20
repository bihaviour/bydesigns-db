#!/usr/bin/env bash
# PostToolUse security hook: scan the edited file for high-confidence secret
# material (private keys, cloud/provider tokens). A hit blocks (exit 2) and feeds
# the finding back to Claude so the secret is removed before it can be committed.
# Uses `gitleaks` when available; otherwise a focused regex scan. Deliberately
# high-confidence to avoid blocking on ordinary code.
set -uo pipefail

input=$(cat)
f=$(printf '%s' "$input" | jq -r '.tool_input.file_path // .tool_response.filePath // empty' 2>/dev/null || true)
[ -z "${f:-}" ] && exit 0
[ -f "$f" ] || exit 0

# Skip lockfiles and binaries.
case "$f" in
  */Cargo.lock|*.lock|*.png|*.jpg|*.jpeg|*.gif|*.pdf|*.so|*.a|*.dylib|*.db) exit 0 ;;
esac

if command -v gitleaks >/dev/null 2>&1; then
  if ! out=$(gitleaks detect --no-banner --no-git --redact --source "$f" 2>&1); then
    echo "Potential secret detected by gitleaks in ${f}:" >&2
    echo "$out" | tail -n 30 >&2
    echo "Remove the secret (use an env var / config, never commit credentials)." >&2
    exit 2
  fi
  exit 0
fi

# Fallback: focused, high-confidence patterns.
patterns='-----BEGIN [A-Z ]*PRIVATE KEY-----|AKIA[0-9A-Z]{16}|ASIA[0-9A-Z]{16}|gh[pousr]_[A-Za-z0-9]{36,}|github_pat_[A-Za-z0-9_]{40,}|xox[baprs]-[A-Za-z0-9-]{10,}|AIza[0-9A-Za-z_-]{35}|sk-[A-Za-z0-9]{32,}|-----BEGIN OPENSSH PRIVATE KEY-----'
if hits=$(grep -nEI -e "$patterns" "$f" 2>/dev/null); then
  echo "Potential secret detected in ${f}:" >&2
  echo "$hits" | sed -E 's/(.{0,80}).*/\1.../' | head -n 20 >&2
  echo "Remove the credential — never commit secrets; load them from the environment." >&2
  exit 2
fi
exit 0
