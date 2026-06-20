#!/usr/bin/env bash
# PostToolUse complexity hook: flag functions in the edited file whose cyclomatic
# complexity exceeds the threshold, so they get refactored early. Uses `lizard`
# when available (CI installs it); otherwise no-ops. Advisory only — it never
# blocks the edit; it injects the finding back to Claude as context.
set -uo pipefail

input=$(cat)
f=$(printf '%s' "$input" | jq -r '.tool_input.file_path // .tool_response.filePath // empty' 2>/dev/null || true)
[ -z "${f:-}" ] && exit 0
[ -f "$f" ] || exit 0

case "$f" in
  *.rs|*.ts|*.tsx|*.js|*.jsx|*.py) ;;
  *) exit 0 ;;
esac

threshold="${BYDESIGNS_CCN_THRESHOLD:-15}"
command -v lizard >/dev/null 2>&1 || exit 0

report=$(lizard -C "$threshold" -w "$f" 2>/dev/null || true)
if [ -n "$report" ]; then
  msg="Complexity check (lizard, CCN > ${threshold}) flagged functions in ${f}:
${report}
Consider splitting these before the change lands; CI enforces the same threshold."
  jq -cn --arg c "$msg" \
    '{hookSpecificOutput:{hookEventName:"PostToolUse",additionalContext:$c}}'
fi
exit 0
