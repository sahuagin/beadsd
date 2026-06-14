#!/usr/bin/env python3
"""beads-gh-sync <project> [--dry-run] [--verbose]

Mirror the OPT-IN beads of a project (those labeled `gh`) to GitHub issues, 1
issue per bead. beads is the source of truth (outbound only):

  - a `gh`-labeled bead with no external_ref  -> create an issue (labeled `beads`),
    write `<owner/repo>#<n>` back into the bead's external_ref
  - a `gh`-labeled bead with an external_ref   -> edit the issue's title/body and
    open/closed state to match the bead (only when something actually changed)

Reads + writes beads through the beadsd service (the single writer) via
`beads --url <endpoint> exec`; talks to GitHub via `gh`. NOT in beadsd's write
path, so a GitHub outage never blocks beads. Safe to re-run (idempotent).

Config:
  ~/.config/beads/remotes.env   project=<beadsd mcp url>     (which service)
  ~/.config/beads/github.env    project=<owner/repo>         (which gh repo)
"""
import argparse
import json
import os
import re
import subprocess
import sys

CONF = os.path.expanduser("~/.config/beads")
BEADS_LABEL = "beads"          # GH label marking issues created from beads (loop-breaker)
BEADS_OPT_IN_LABEL = "gh"      # bead label that opts a bead in to mirroring


def die(msg):
    print(f"beads-gh-sync: {msg}", file=sys.stderr)
    sys.exit(1)


def load_map(filename, project):
    path = os.path.join(CONF, filename)
    if not os.path.exists(path):
        die(f"missing {path}")
    for line in open(path):
        line = line.strip()
        if line.startswith(f"{project}="):
            return line.split("=", 1)[1]
    return None


def beads_exec(endpoint, args, check=True):
    """Run `beads --url <endpoint> exec -- <args>`; return (stdout, code)."""
    r = subprocess.run(
        ["beads", "--url", endpoint, "exec", "--", *args],
        capture_output=True, text=True,
    )
    if check and r.returncode != 0:
        die(f"beads exec {' '.join(args)} failed (exit {r.returncode}): {r.stderr.strip()}")
    return r.stdout, r.returncode


_ANSI = re.compile(r"\x1b\[[0-9;]*m")


def gh(args, repo, check=True, capture=True):
    # gh may be configured with color=always, which corrupts --json output;
    # force NO_COLOR and strip any residual ANSI so json.loads is safe.
    env = {**os.environ, "NO_COLOR": "1", "GH_PAGER": "cat", "CLICOLOR": "0"}
    r = subprocess.run(
        ["gh", *args, "--repo", repo], capture_output=capture, text=True, env=env
    )
    if check and r.returncode != 0:
        die(f"gh {' '.join(args)} failed (exit {r.returncode}): {r.stderr.strip()}")
    return _ANSI.sub("", r.stdout).strip()


def ensure_label(repo, name, color, desc, dry):
    # idempotent; gh label create errors if it exists, which we ignore.
    if dry:
        return
    subprocess.run(
        ["gh", "label", "create", name, "--color", color, "--description", desc, "--repo", repo],
        capture_output=True, text=True,
    )


def ref_link(ref, repo):
    """'owner/repo#n' -> '#n' if same repo (GitHub auto-links), else 'owner/repo#n'."""
    if not ref or "#" not in ref:
        return None
    r, _, n = ref.partition("#")
    return f"#{n}" if r == repo else f"{r}#{n}"


def build_related(bead, endpoint, id2ref, repo):
    """A markdown 'Related' block linking parent / blocked-by / blocks / sub-tasks
    to their mirrored issues (#n), falling back to the bead id when unmirrored.
    Empty unless the bead actually has relationships (cheap: gated on counts)."""
    if (bead.get("dependency_count") or 0) + (bead.get("dependent_count") or 0) == 0:
        return ""
    out, _ = beads_exec(endpoint, ["show", bead["id"], "--json"])
    try:
        full = json.loads(out)
        full = full[0] if isinstance(full, list) else full
    except json.JSONDecodeError:
        return ""

    def refs(items, dtype):
        return [ref_link(id2ref.get(d.get("id")), repo) or f"`{d.get('id')}`"
                for d in (items or []) if d.get("dependency_type") == dtype]

    deps, depnts = full.get("dependencies") or [], full.get("dependents") or []
    rows = [
        ("Parent", refs(deps, "parent-child")),
        ("Blocked by", refs(deps, "blocks")),
        ("Blocks", refs(depnts, "blocks")),
        ("Sub-tasks", refs(depnts, "parent-child")),
    ]
    lines = [f"- {label}: {', '.join(v)}" for label, v in rows if v]
    return "\n\n**Related**\n" + "\n".join(lines) if lines else ""


def issue_body(bead, related=""):
    desc = (bead.get("description") or "").rstrip()
    bid = bead["id"]
    footer = f"\n\n---\n*Tracked in beads as `{bid}` — edit there, not here.*\n<!-- beads-id: {bid} -->"
    return f"{desc}{related}{footer}"


def desired_state(bead):
    return "closed" if bead.get("status") in ("closed", "tombstone") else "open"


def parse_issue_number(create_output):
    # gh issue create prints the issue URL, e.g. https://github.com/o/r/issues/42
    m = re.search(r"/issues/(\d+)", create_output)
    return int(m.group(1)) if m else None


def fetch_beads(endpoint):
    out, _ = beads_exec(endpoint, [
        "list", "--label", BEADS_OPT_IN_LABEL,
        "--status", "open", "--status", "in_progress", "--status", "blocked",
        "--status", "deferred", "--status", "closed",
        "--json",
    ])
    try:
        data = json.loads(out)
    except json.JSONDecodeError:
        die(f"could not parse br list json: {out[:200]}")
    return data["issues"] if isinstance(data, dict) and "issues" in data else data


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("project")
    ap.add_argument("--dry-run", action="store_true")
    ap.add_argument("--verbose", "-v", action="store_true")
    args = ap.parse_args()

    endpoint = load_map("remotes.env", args.project) or die(f"no beadsd endpoint for '{args.project}' in remotes.env")
    repo = load_map("github.env", args.project) or die(f"no gh repo for '{args.project}' in github.env")

    ensure_label(repo, BEADS_LABEL, "1f883d", "Mirrored from a beads issue", args.dry_run)

    beads = fetch_beads(endpoint)
    # bead id -> its mirrored issue ref, for cross-linking related issues.
    id2ref = {b["id"]: (b.get("external_ref") or "").strip()
              for b in beads if (b.get("external_ref") or "").strip()}
    print(f"{args.project}: {len(beads)} bead(s) labeled '{BEADS_OPT_IN_LABEL}' -> {repo}"
          + (" [dry-run]" if args.dry_run else ""))

    created = updated = closed = unchanged = 0
    for b in beads:
        bid = b["id"]
        ref = (b.get("external_ref") or "").strip()
        want_title = b.get("title") or bid
        want_body = issue_body(b, build_related(b, endpoint, id2ref, repo))
        want_state = desired_state(b)
        prefix = f"  {bid}"

        if not ref or "#" not in ref:
            # create
            if args.dry_run:
                print(f"{prefix}: would CREATE issue in {repo} (state={want_state})")
                created += 1
                continue
            out = gh(["issue", "create", "--title", want_title, "--body", want_body,
                      "--label", BEADS_LABEL], repo)
            num = parse_issue_number(out)
            if not num:
                die(f"{bid}: could not parse issue number from: {out}")
            newref = f"{repo}#{num}"
            beads_exec(endpoint, ["update", bid, "--external-ref", newref])
            if want_state == "closed":
                gh(["issue", "close", str(num)], repo)
            print(f"{prefix}: created {newref}")
            created += 1
            continue

        # existing -> reconcile
        rrepo, _, numstr = ref.partition("#")
        num = numstr
        view = gh(["issue", "view", num, "--json", "title,body,state"], rrepo, check=False)
        if not view:
            print(f"{prefix}: WARNING external_ref {ref} not viewable (deleted?); skipping", file=sys.stderr)
            continue
        cur = json.loads(view)
        cur_state = cur.get("state", "").lower()
        changes = []
        if cur.get("title") != want_title:
            changes.append("title")
        if (cur.get("body") or "").strip() != want_body.strip():
            changes.append("body")
        state_change = (cur_state != want_state)

        if not changes and not state_change:
            unchanged += 1
            if args.verbose:
                print(f"{prefix}: unchanged ({ref})")
            continue
        if args.dry_run:
            print(f"{prefix}: would EDIT {ref} ({', '.join(changes) or 'state'}"
                  + (f", {cur_state}->{want_state}" if state_change else "") + ")")
            updated += 1
            continue
        if changes:
            gh(["issue", "edit", num, "--title", want_title, "--body", want_body], rrepo)
        if state_change:
            gh(["issue", "close" if want_state == "closed" else "reopen", num], rrepo)
            if want_state == "closed":
                closed += 1
        print(f"{prefix}: updated {ref} ({', '.join(changes) or 'state'}"
              + (f", {cur_state}->{want_state}" if state_change else "") + ")")
        updated += 1

    print(f"done: {created} created, {updated} updated, {unchanged} unchanged.")


if __name__ == "__main__":
    main()
