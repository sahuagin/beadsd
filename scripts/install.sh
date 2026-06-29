#!/bin/sh
# install.sh — build (release) and install the beadsd binaries to ~/.local/bin.
#
# Atomic and backup-free by design: install(1) writes to a temp file and
# renames, so an interrupted install never corrupts the live binary, and NO
# `.bak`/`.prev` cruft is left on $PATH. The rollback is git, not a copy on
# disk: `git checkout <sha> && scripts/install.sh`.
#
# Usage:
#   scripts/install.sh             # client only (beads)
#   scripts/install.sh --server    # also install beadsd (the service binary)
#
# Env:
#   DESTDIR   install dir            (default: ~/.local/bin)
#   PROFILE   cargo profile          (default: release)
set -eu

DEST="${DESTDIR:-$HOME/.local/bin}"
PROFILE="${PROFILE:-release}"

bins="beads"
[ "${1:-}" = "--server" ] && bins="beads beadsd"

# Run from the repo root regardless of the caller's cwd.
cd "$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)"

profile_flag=""
[ "$PROFILE" = "release" ] && profile_flag="--release"

for b in $bins; do
    printf '==> cargo build %s --bin %s\n' "$profile_flag" "$b"
    cargo build $profile_flag --bin "$b"
done

mkdir -p "$DEST"
for b in $bins; do
    printf '==> install %s -> %s/%s\n' "$b" "$DEST" "$b"
    install -m 0755 "target/$PROFILE/$b" "$DEST/$b"
done

printf 'done. (rollback: git checkout <sha> && scripts/install.sh)\n'
