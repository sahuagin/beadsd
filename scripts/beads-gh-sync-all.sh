#!/bin/sh
# beadsd: auto-mirror gh-labeled beads to GitHub issues for every configured
# project (one beads-gh-sync run per github.env entry). Cron-friendly — each
# project is independent; a failure in one is logged and doesn't block the rest.
# Logs only when something changed or failed (quiet on all-unchanged no-ops).
set -u

GH="${BEADS_GITHUB_ENV:-$HOME/.config/beads/github.env}"
LOG="${BEADS_GH_SYNC_LOG:-$HOME/.local/share/beadsd/gh-sync.log}"
SYNC="${BEADS_GH_SYNC:-$HOME/.local/bin/beads-gh-sync}"

mkdir -p "$(dirname "$LOG")" 2>/dev/null || true
[ -f "$GH" ] || exit 0

while IFS='=' read -r proj _rest; do
    case "$proj" in '' | \#*) continue ;; esac   # skip blanks + comments
    out=$("$SYNC" "$proj" 2>&1)
    rc=$?
    if [ "$rc" -ne 0 ]; then
        printf '%s [%s] FAILED rc=%s: %s\n' "$(date '+%F %T')" "$proj" "$rc" "$out" >>"$LOG"
    elif ! printf '%s' "$out" | grep -q '0 created, 0 updated'; then
        printf '%s [%s] %s\n' "$(date '+%F %T')" "$proj" "$(printf '%s' "$out" | tail -1)" >>"$LOG"
    fi
done < "$GH"
