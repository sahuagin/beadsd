#!/bin/sh
# beadsd: mirror the beads-central audit snapshots to GitHub.
#
# The per-project beadsd committers commit issues.jsonl locally; this is the
# SINGLE pusher, so the push is always a fast-forward (only the local committers
# add commits). Cheap: it does nothing (no network) unless there are unpushed
# commits. Intended to run from cron (every few minutes).
set -u

REPO="${BEADS_CENTRAL_REPO:-$HOME/src/public_github/beads-central}"
LOG="${BEADS_CENTRAL_PUSH_LOG:-$HOME/.local/share/beadsd/push-central.log}"

mkdir -p "$(dirname "$LOG")" 2>/dev/null || true
cd "$REPO" 2>/dev/null || { echo "$(date '+%F %T') repo missing: $REPO" >>"$LOG"; exit 1; }

# Only touch the network if local main is ahead of origin/main.
ahead=$(git rev-list --count origin/main..main 2>/dev/null || echo 0)
[ "${ahead:-0}" -gt 0 ] || exit 0

if out=$(GIT_SSH_COMMAND='ssh -o BatchMode=yes' git push origin main 2>&1); then
    echo "$(date '+%F %T') pushed ${ahead} commit(s)" >>"$LOG"
else
    echo "$(date '+%F %T') push failed: ${out}" >>"$LOG"
    exit 1
fi
