# beadsd

A single-writer **beads** service. One long-running process owns one central
`.beads` SQLite DB for one project and is the **sole** process that runs `br`
against it. Agents on any host on the trusted network mutate beads by calling
this service over MCP-over-HTTP instead of sharing the DB on a filesystem.

## Why this exists

`br`'s `.beads/` normally lives *inside* each working repo. Two failure classes
follow:

1. **jj-workspace divergence.** A `/goal` session in an isolated jj workspace (a
   *sibling* of the backing repo) gets only the tracked `issues.jsonl`, not the
   gitignored `beads.db`. `br`'s cwd-walk discovery rebuilds a *divergent* DB, so
   claims/edits land in the wrong place, invisible to the canonical DB.
2. **Cross-host writers.** SQLite/fsqlite file locking is only safe on a *local*
   filesystem. Concurrent writers across a network filesystem (NFS) are not safe.

Moving the DB out of every working repo into one service-owned location, with a
single owning process, deletes both by construction: there is no in-repo DB for
the cwd-walk to find, and exactly one process ever touches the file.

## How it works

```
   clients on the trusted network (Claude Code as an MCP server URL; the `beads`
   CLI from shell scripts; any MCP client)
                 │  rmcp streamable-HTTP  (POST /mcp)
                 ▼
   beadsd  ── one process per project ──────────────────────────────
     • owns ~/src/beads-central/<project>/.beads/beads.db (sole writer)
     • verbs shell out to `br <verb> --db <central> --json`
       (depends only on br's CLI surface — br stays swappable)
     • mutations serialized by an in-process mutex
     • debounced background committer: git-commits issues.jsonl for audit/backup
     • GET /health → "ok"
```

**Reuse posture:** beadsd shells out to the `br` binary; it does not link br or
reimplement issue logic. That honors the operator policy that nothing assumes br
beyond its CLI surface + the `issues.jsonl` export, so upstream `br` merges never
touch this code, and the fork's effective-priority behavior comes along for free.

## Tools (MCP) / verbs (`beads` CLI)

`beads_ready` · `beads_show` · `beads_list` · `beads_create` · `beads_update` ·
`beads_claim` · `beads_unclaim` · `beads_close`

## Binaries

- **`beadsd`** — the service. `beadsd --db <central-db> --listen 0.0.0.0:<port>
  [--repo <central-repo> --commit-interval-secs N] [--br-bin <path>]`. Serves MCP
  at `/mcp` and a `/health` route.
- **`beads`** — thin client for scripts/humans. `beads <verb> [...] --url
  $BEADS_REMOTE`. Exits non-zero (with detail on stderr) when beadsd reports a br
  failure (e.g. a claim conflict), so `if ! beads claim …` works.

## Build

```sh
RUSTC_WRAPPER= cargo build --release
# → target/release/{beadsd,beads}
```

## Quick local smoke

```sh
br init --prefix smk           # in a throwaway dir
beadsd --db "$PWD/.beads/beads.db" --listen 127.0.0.1:7799 &
export BEADS_REMOTE=http://127.0.0.1:7799/mcp
beads create "hello" --type task --priority 2 --actor me
beads ready
```

## Deployment

See [`DEPLOY.md`](DEPLOY.md) for the staged migration (central repo, rc.d
supervision, sprint repoint, cutover) with rollback at each step, and
[`freebsd/rc.d/beadsd`](freebsd/rc.d/beadsd) for the supervision script.
