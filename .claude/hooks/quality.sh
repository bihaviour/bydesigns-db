#!/usr/bin/env bash
# PostToolUse code-quality hook: auto-format the file Claude just edited so the
# tree stays `cargo fmt`-clean. Non-blocking and best-effort — if a formatter is
# missing or the file is mid-edit and unparsable, it simply does nothing.
set -uo pipefail

input=$(cat)
f=$(printf '%s' "$input" | jq -r '.tool_input.file_path // .tool_response.filePath // empty' 2>/dev/null || true)
[ -z "${f:-}" ] && exit 0
[ -f "$f" ] || exit 0

case "$f" in
  *.rs)
    if command -v rustfmt >/dev/null 2>&1; then
      rustfmt --edition 2021 "$f" >/dev/null 2>&1 || true
    fi
    ;;
esac
exit 0
