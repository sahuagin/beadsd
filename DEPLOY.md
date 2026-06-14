# beadsd deployment & migration runbook

Staged, reversible cutover from in-repo `.beads/` to a central single-writer
service. Only **mu** and **agent_tools** have beads today; the other reserved
projects are materialized lazily. Each stage is independently reversible; the
first non-free rollback is flagged.

> Outward-facing / coordinated steps (creating the GitHub repo, moving
> production beads, installing system services, editing live `~/.local`
> scripts) should be done deliberately, ideally when concurrent sessions that
> use `sprint-start` are quiet.

## Ports (suggested)

| project     | beadsd listen      | BEADS_REMOTE                       |
|-------------|--------------------|------------------------------------|
| mu          | `0.0.0.0:7771`     | `http://<host>:7771/mcp`           |
| agent_tools | `0.0.0.0:7772`     | `http://<host>:7772/mcp`           |

## Stage 0 — build & prove (no client cutover)

1. Build: `cd ~/src/beadsd && RUSTC_WRAPPER= cargo build --release`; install
   `target/release/{beadsd,beads}` to `~/.local/bin/`.
2. Create `sahuagin/beads-central` (private recommended — agent_tools beads are
   not in public_github). Clone to `~/src/public_github/beads-central/`.
3. **Plain-copy** (history not preserved) the live DBs into subdirs:
   ```sh
   mkdir -p ~/src/public_github/beads-central/mu ~/src/public_github/beads-central/agent_tools
   cp -r ~/src/public_github/mu/.beads        ~/src/public_github/beads-central/mu/.beads
   cp -r ~/src/agent_tools/.beads             ~/src/public_github/beads-central/agent_tools/.beads
   cd ~/src/public_github/beads-central && git init && git add -A && git commit -m "import mu + agent_tools beads"
   ```
4. Run a beadsd **in the foreground** against each subdir DB; smoke from the
   host and a second machine:
   ```sh
   beadsd --db ~/src/public_github/beads-central/mu/.beads/beads.db --listen 0.0.0.0:7771 &
   BEADS_REMOTE=http://localhost:7771/mcp beads ready
   ```
   Verify prefixes (`mu-`, `at-`) intact and `br doctor --db <central>` clean.
   - **Rollback:** kill it. In-repo `.beads/` are untouched. Zero blast radius.

## Stage 1 — supervise (rc.d)

5. Install per-instance config (db, ip:port, repo all live here — see
   `config/*.example.toml`):
   ```sh
   mkdir -p ~/.config/beadsd
   cp ~/src/beadsd/config/config.example.toml       ~/.config/beadsd/config.toml
   cp ~/src/beadsd/config/mu.example.toml            ~/.config/beadsd/mu.toml
   cp ~/src/beadsd/config/agent_tools.example.toml   ~/.config/beadsd/agent_tools.toml
   # edit paths/ports if they differ from the defaults
   ```
   `br_bin` in `config.toml` MUST be absolute: the rc.d-supervised daemon runs
   with a minimal PATH that does not include `~/.local/bin`, so a bare `"br"`
   fails with `br_spawn_failed: No such file or directory`.
6. Install the supervision script + per-project instances and enable them. The
   rc.d derives each instance's config from its name (`beadsd_mu` → `mu.toml`):
   ```sh
   doas install -m 0555 ~/src/beadsd/freebsd/rc.d/beadsd /usr/local/etc/rc.d/beadsd
   doas ln -s /usr/local/etc/rc.d/beadsd /usr/local/etc/rc.d/beadsd_mu
   doas ln -s /usr/local/etc/rc.d/beadsd /usr/local/etc/rc.d/beadsd_agent_tools
   # /etc/rc.conf — only enable + user per instance:
   #   beadsd_mu_enable="YES"
   #   beadsd_mu_user="tcovert"
   #   beadsd_agent_tools_enable="YES"
   #   beadsd_agent_tools_user="tcovert"
   ```
7. `doas service beadsd_mu start && doas service beadsd_agent_tools start`; re-run the
   Stage-0 smoke against the supervised endpoints; confirm `/health` and that a
   mutation produces a `beadsd: snapshot <project>` commit in the central repo.
   - **Rollback:** `service beadsd_* stop`; set `*_enable="NO"`.

## Stage 2 — repoint sprint (agent_tools first, env-gated)

The claim path in `sprint-start` becomes service-aware **only when an endpoint is
configured for the repo** — otherwise it uses the existing local-`--db` path, so
this is additive and safe to land before cutover.

8. Create the endpoint map `~/.config/beads/remotes.env` (one `repo=url` per line):
   ```sh
   agent_tools=http://localhost:7772/mcp
   # mu added in Stage 3
   ```
9. Add a resolver to `~/.local/lib/sprint-lib.sh`:
   ```sh
   # beads_endpoint <repo> -> the beadsd MCP url for that project, or empty.
   # Centralized projects route claims through beadsd; others use the local DB.
   beads_endpoint() {
       f="${XDG_CONFIG_HOME:-$HOME/.config}/beads/remotes.env"
       [ -f "$f" ] || return 0
       sed -n "s/^$1=//p" "$f" | head -n1
   }
   ```
10. Replace the claim block in `sprint-start` (currently lines ~67–88) with:
    ```sh
    # --- claim (bead mode only) ---
    if [ -n "$BEAD" ]; then
        ENDPOINT=$(beads_endpoint "$REPO")
        if [ -n "$ENDPOINT" ]; then
            # Centralized: claim through the single-writer service. The `beads`
            # client exits non-zero on a claim conflict (already held).
            if ! out=$(beads --url "$ENDPOINT" claim "$BEAD" --actor "$ACTOR" 2>&1); then
                # Idempotent resume: if WE already hold it, that's fine.
                held=$(beads --url "$ENDPOINT" show "$BEAD" 2>/dev/null | \
                    python3 -c "import json,sys;i=json.load(sys.stdin);i=i if isinstance(i,dict) else i[0];print(i.get('assignee') or '')" 2>/dev/null || true)
                if [ "$held" = "$ACTOR" ]; then
                    printf 'sprint-start: %s already claimed by this session — resuming.\n' "$BEAD" >&2
                else
                    die "claim failed: $out"
                fi
            fi
        else
            # Legacy local-DB path (unchanged).
            DB="$ROOT/.beads/beads.db"
            [ -f "$DB" ] || die "repo $REPO has no .beads DB ($DB); use --no-bead <label>"
            br show "$BEAD" --db "$DB" >/dev/null 2>&1 || die "bead $BEAD not found in canonical DB"
            held=$(br show "$BEAD" --db "$DB" --json 2>/dev/null | \
                python3 -c "import json,sys;i=json.load(sys.stdin);i=i if isinstance(i,dict) else i[0];print(i.get('assignee') or '')" 2>/dev/null || true)
            if [ -n "$held" ] && [ "$held" != "$ACTOR" ]; then
                die "bead $BEAD already claimed by '$held' — pick another (br ready --db $DB)"
            fi
            if [ "$held" = "$ACTOR" ]; then
                printf 'sprint-start: %s already claimed by this session — resuming.\n' "$BEAD" >&2
            elif ! out=$(br update "$BEAD" --claim --actor "$ACTOR" --db "$DB" 2>&1); then
                die "claim failed: $out"
            fi
        fi
    fi
    ```
11. Mirror in `sprint-end`'s release block: if `beads_endpoint "$REPO"` is set,
    release via `beads --url "$ENDPOINT" unclaim "$BEAD" --actor "$ACTOR"` (or
    `close` with `--close`); else the existing `br ... --db "$DB"` path. Keep the
    ownership pre-check and the actor-sidecar logic unchanged.
12. Run a real `sprint-start <at-bead> … && sprint-end` against agent_tools.
    - **Rollback:** remove the `agent_tools=` line from `remotes.env` → reverts to
      the local-DB path. The in-repo `.beads/` still exists as the fallback.

## Stage 3 — repoint mu, then make the service canonical

13. Add `mu=http://localhost:7771/mcp` to `remotes.env`; exercise a real mu sprint.
14. Once both are trusted, delete the now-obsolete `backing_root()` /
    `canonical_db()` machinery from `sprint-lib.sh` (its only job was resolving
    the per-workspace canonical `--db`, which no longer exists) and drop the
    legacy local-DB branch from the claim/release blocks.
    - **Rollback:** restore the deleted functions from git.

## Stage 4 — remove `.beads/` from the hosting repos (LAST; first non-free rollback)

15. Only after days of trusted operation: `git rm -r .beads/` in `mu` and
    `agent_tools`, commit. The central checkout is now the only `.beads/`; the
    cwd-walk has nothing to diverge to.
    - **Rollback is NOT free here:** the central DB has moved *ahead* of the
      in-repo copy, so undo = re-export from the central DB
      (`br sync --flush-only --db <central>` into a restored `.beads/`), not a
      plain revert. That's why this stage is last.

## Reserved projects (warden, agentic-bench, claude-proxy)

Do not pre-create empty DBs. When the first bead for one is filed, `br init` its
subdir in the central checkout, add a `beadsd_<name>` rc.d instance + port + a
`remotes.env` line. Treat the names as reserved namespaces, materialized on demand.

## Invariants / guardrails

- **No human or stray `br` ever touches `~/src/public_github/beads-central`** — it is the
  service's private working tree. That is now the *only* way to reintroduce
  divergence.
- beadsd must be pinned to its `--db` and never allowed to cwd-walk into another
  repo's `.beads/`.
- Trusted-network exposure: beadsd binds the network with no auth, by design
  (operator decision). Keep it on the trusted network only.
