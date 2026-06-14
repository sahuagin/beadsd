#!/usr/bin/env python3
"""beads-gh-triage <project> <list|accept|skip|show> [args]

GATED inbound: GitHub issue -> bead. Nothing is automatic (no DoS by auto-add).
A new human-filed issue sits untriaged until someone decides:

  beads-gh-triage <project> list                  # open issues not yet triaged
  beads-gh-triage <project> show <issue#>          # full issue text
  beads-gh-triage <project> accept <issue#> [--priority P] [--type T] [--title T]
                                                   # create a bead from it, link + label
  beads-gh-triage <project> skip <issue#>          # label `triage-skip`; never resurfaces

"Untriaged" = open, and labeled neither `beads` (already mirrored from/to a bead)
nor `triage-skip`. accept creates the bead through the beadsd service (labeled
`gh` so beads-gh-sync keeps it mirrored), stamps the bead's external_ref, and
labels the issue `beads`.

Config: ~/.config/beads/{remotes.env,github.env} (same as beads-gh-sync).
"""
import argparse
import json
import os
import re
import subprocess
import sys

CONF = os.path.expanduser("~/.config/beads")
BEADS_LABEL = "beads"
SKIP_LABEL = "triage-skip"
OPT_IN_LABEL = "gh"
_ANSI = re.compile(r"\x1b\[[0-9;]*m")


def die(msg):
    print(f"beads-gh-triage: {msg}", file=sys.stderr)
    sys.exit(1)


def load_map(filename, project):
    path = os.path.join(CONF, filename)
    if not os.path.exists(path):
        die(f"missing {path}")
    for line in open(path):
        line = line.strip()
        if line.startswith(f"{project}="):
            return line.split("=", 1)[1]
    die(f"no entry for '{project}' in {filename}")


def gh(args, repo, check=True):
    env = {**os.environ, "NO_COLOR": "1", "GH_PAGER": "cat", "CLICOLOR": "0"}
    r = subprocess.run(["gh", *args, "--repo", repo], capture_output=True, text=True, env=env)
    if check and r.returncode != 0:
        die(f"gh {' '.join(args)} failed (exit {r.returncode}): {r.stderr.strip()}")
    return _ANSI.sub("", r.stdout).strip()


def beads_exec(endpoint, args, check=True):
    r = subprocess.run(["beads", "--url", endpoint, "exec", "--", *args],
                       capture_output=True, text=True)
    if check and r.returncode != 0:
        die(f"beads exec {' '.join(args)} failed (exit {r.returncode}): {r.stderr.strip()}")
    return r.stdout


def ensure_label(repo, name, color, desc):
    subprocess.run(["gh", "label", "create", name, "--color", color, "--description", desc,
                    "--repo", repo], capture_output=True, text=True)


def untriaged(repo):
    out = gh(["issue", "list", "--state", "open", "--limit", "200",
              "--json", "number,title,author,createdAt,labels"], repo)
    issues = json.loads(out) if out else []
    res = []
    for i in issues:
        labels = {l["name"] for l in i.get("labels", [])}
        if BEADS_LABEL in labels or SKIP_LABEL in labels:
            continue
        res.append(i)
    return res


def cmd_list(endpoint, repo, args):
    items = untriaged(repo)
    if not items:
        print(f"{repo}: no untriaged open issues.")
        return
    print(f"{repo}: {len(items)} untriaged open issue(s):")
    for i in items:
        who = (i.get("author") or {}).get("login", "?")
        print(f"  #{i['number']:<5} {i['title'][:70]:<70}  @{who}  {i.get('createdAt','')[:10]}")
    print("\n  -> accept <#> to make a bead, or skip <#> to silence.")


def cmd_show(endpoint, repo, args):
    out = gh(["issue", "view", str(args.number), "--json", "number,title,body,author,state,labels"], repo)
    i = json.loads(out)
    print(f"#{i['number']}  [{i['state']}]  @{(i.get('author') or {}).get('login','?')}  "
          f"labels={[l['name'] for l in i.get('labels',[])]}")
    print(f"title: {i['title']}\n")
    print(i.get("body") or "(no body)")


def cmd_accept(endpoint, repo, args):
    out = gh(["issue", "view", str(args.number), "--json", "number,title,body,labels"], repo)
    i = json.loads(out)
    if any(l["name"] == BEADS_LABEL for l in i.get("labels", [])):
        die(f"#{args.number} already has the '{BEADS_LABEL}' label (already tracked)")
    title = args.title or i["title"]
    body = i.get("body") or ""

    create = ["create", title, "--labels", OPT_IN_LABEL, "--json"]
    if body.strip():
        create += ["--description", body]
    if args.type:
        create += ["--type", args.type]
    if args.priority:
        create += ["--priority", str(args.priority)]
    res = beads_exec(endpoint, create)
    try:
        bead = json.loads(res)
        bead = bead[0] if isinstance(bead, list) else bead
        bid = bead.get("id") or bead.get("issue", {}).get("id")
    except json.JSONDecodeError:
        die(f"could not parse created bead: {res[:200]}")
    if not bid:
        die(f"created bead but found no id in: {res[:200]}")

    ref = f"{repo}#{args.number}"
    beads_exec(endpoint, ["update", bid, "--external-ref", ref])
    gh(["issue", "edit", str(args.number), "--add-label", BEADS_LABEL], repo)
    print(f"accepted #{args.number} -> bead {bid} (labeled '{OPT_IN_LABEL}', issue labeled '{BEADS_LABEL}')")
    print(f"  (next `beads-gh-sync {args.project}` will keep them in sync)")


def cmd_skip(endpoint, repo, args):
    ensure_label(repo, SKIP_LABEL, "cccccc", "Reviewed; intentionally not tracked in beads")
    gh(["issue", "edit", str(args.number), "--add-label", SKIP_LABEL], repo)
    print(f"skipped #{args.number} (labeled '{SKIP_LABEL}'; won't resurface)")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("project")
    sub = ap.add_subparsers(dest="cmd", required=True)
    sub.add_parser("list")
    sp_show = sub.add_parser("show"); sp_show.add_argument("number", type=int)
    sp_acc = sub.add_parser("accept")
    sp_acc.add_argument("number", type=int)
    sp_acc.add_argument("--priority"); sp_acc.add_argument("--type"); sp_acc.add_argument("--title")
    sp_skip = sub.add_parser("skip"); sp_skip.add_argument("number", type=int)
    args = ap.parse_args()

    endpoint = load_map("remotes.env", args.project)
    repo = load_map("github.env", args.project)
    ensure_label(repo, BEADS_LABEL, "1f883d", "Mirrored to/from a beads issue")

    {"list": cmd_list, "show": cmd_show, "accept": cmd_accept, "skip": cmd_skip}[args.cmd](endpoint, repo, args)


if __name__ == "__main__":
    main()
