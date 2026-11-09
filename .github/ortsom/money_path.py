"""The `ortsom/money-path` commit status (docs/ORTSOM_PR_E2E_SPEC.md § 15).
Standard library only.

    money_path.py --pr N [--head SHA] (--pending | --verdict-file verdict.json)
    money_path.py --fill-gap SHA

A pull request that touches a money path (`[money_path] paths` in map.toml)
may merge only with `ortsom:pass`. This posts one commit status on the pull
request's head: success when no money path changed, or when one did and
the verdict is `pass`; pending while the verdict is not in; failure for any
other verdict, a skip included. Branch protection requires the status.

`--pending` is resolve's call, as soon as the run is tied to its pull
request; `--verdict-file` is the verdict job's, with the verdict.json it
wrote (no file: a newer run reports, nothing is posted). `--head`, when
given, must still be the pull request's head. `--fill-gap` is resolve's
call when it rejected the run: every open pull request at that commit
that has no status yet gets one (failure for a money path, success
otherwise), so a run that never completed cannot leave the required check
missing; a status already there is never overwritten.
"""

import argparse
import json
import os
import sys
import tomllib
import urllib.request
from pathlib import Path

from resolve_run import api, paginate
from select_scenarios import glob_match

GATE_DIR = Path(__file__).resolve().parent
CONTEXT = "ortsom/money-path"
DESCRIPTION_MAX = 140


class MoneyPathError(Exception):
    """map.toml has no usable [money_path]."""


def validate_globs(gate_map):
    paths = (gate_map.get("money_path") or {}).get("paths")
    if not isinstance(paths, list) or not paths or not all(isinstance(p, str) and p for p in paths):
        raise MoneyPathError("map.toml needs [money_path] paths, a non-empty list of globs")
    return paths


def load_globs(path=GATE_DIR / "map.toml"):
    with open(path, "rb") as f:
        return validate_globs(tomllib.load(f))


def changed_files(entries):
    """Both names of a rename from API `pulls/{n}/files` entries."""
    names = set()
    for e in entries:
        names.add(e["filename"])
        if e.get("previous_filename"):
            names.add(e["previous_filename"])
    return sorted(names)


def money_files(files, globs):
    return [f for f in files if any(glob_match(g, f) for g in globs)]


def status(files, globs, verdict, eligible=None):
    """(state, description) for a pull request changing `files`. `verdict`
    is `pending`, `incomplete` (the run never reached a verdict), a verdict
    of § 7.3, or `none`/None when there is none. `eligible` is the verdict's
    count of compared scenarios: a pass that compared none is not one."""
    touched = money_files(files, globs)
    if not touched:
        return "success", "No money path changed"
    if verdict == "pending":
        return "pending", "Money path changed: waiting for the Ortsom verdict"
    if verdict == "incomplete":
        return "failure", "Money path changed: the Ortsom run did not complete; push or add ortsom:run"
    if verdict == "pass" and eligible == 0:
        return "failure", "Money path changed: the Ortsom pass compared no scenario with main"
    if verdict == "pass":
        return "success", "Money path changed and Ortsom passed"
    text = f"Money path changed: needs ortsom:pass, the verdict is {verdict or 'none'}"
    return "failure", text[:DESCRIPTION_MAX]


def state_for(get, repo, pull, verdict, globs, eligible=None):
    entries = paginate(get, f"/repos/{repo}/pulls/{pull['number']}/files")
    if len(entries) == pull.get("changed_files"):
        return status(changed_files(entries), globs, verdict, eligible)
    # GitHub cuts the list at 3000 files: fail closed.
    state = status(["<every file>"], ["**"], verdict, eligible)[0]
    return state, "Could not list every changed file; treated as a money path"


def send_status(send, repo, sha, state, text, target_url):
    body = {"state": state, "context": CONTEXT, "description": text, "target_url": target_url}
    send("POST", f"/repos/{repo}/statuses/{sha}", body)
    print(f"{CONTEXT} on {sha[:12]}: {state} ({text})")


def post(get, send, repo, pr, head, verdict, globs, target_url, eligible=None):
    pull = get(f"/repos/{repo}/pulls/{pr}")
    current = (pull.get("head") or {}).get("sha")
    if head and current != head:
        print(f"::notice::#{pr} has a newer head; its own run posts {CONTEXT}")
        return None
    pull = {**pull, "number": pr}
    state, text = state_for(get, repo, pull, verdict, globs, eligible)
    send_status(send, repo, current, state, text, target_url)
    return state


def fill_gap(get, send, repo, sha, globs, target_url):
    """A status for each open pull request at `sha` that has none yet."""
    existing = get(f"/repos/{repo}/commits/{sha}/status").get("statuses") or []
    if any(s.get("context") == CONTEXT for s in existing):
        return
    for pull in get(f"/repos/{repo}/commits/{sha}/pulls"):
        if pull.get("state") != "open" or (pull.get("head") or {}).get("sha") != sha:
            continue
        state, text = state_for(get, repo, pull, "incomplete", globs)
        send_status(send, repo, sha, state, text, target_url)

def read_verdict(path):
    """(verdict, eligible scenario count) from verdict.json, or None."""
    path = Path(path)
    if not path.is_file():
        return None
    doc = json.loads(path.read_text())
    return doc.get("verdict"), (doc.get("counts") or {}).get("eligible")


def request(method, path, token, body=None):
    req = urllib.request.Request(
        f"https://api.github.com{path}",
        data=json.dumps(body).encode() if body is not None else None,
        method=method,
        headers={
            "Authorization": f"Bearer {token}",
            "Accept": "application/vnd.github+json",
            "X-GitHub-Api-Version": "2022-11-28",
        },
    )
    with urllib.request.urlopen(req, timeout=30) as resp:
        raw = resp.read()
    return json.loads(raw) if raw else {}


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--pr", type=int)
    parser.add_argument("--head")
    which = parser.add_mutually_exclusive_group(required=True)
    which.add_argument("--pending", action="store_true")
    which.add_argument("--verdict-file", type=Path)
    which.add_argument("--fill-gap", metavar="SHA")
    args = parser.parse_args(argv)
    repo, token = os.environ["GITHUB_REPOSITORY"], os.environ["GITHUB_TOKEN"]
    target = f"{os.environ.get('GITHUB_SERVER_URL', 'https://github.com')}/{repo}/actions/runs/{os.environ.get('GITHUB_RUN_ID', '')}"

    def get(path, params=""):
        return api(path, token, params)

    def send(method, path, body=None):
        return request(method, path, token, body)

    if args.fill_gap:
        fill_gap(get, send, repo, args.fill_gap, load_globs(), target)
        return 0
    if args.pr is None:
        parser.error("--pr is required with --pending and --verdict-file")
    verdict, eligible = "pending", None
    if args.verdict_file:
        read = read_verdict(args.verdict_file)
        if read is None:
            print(f"::notice::no verdict was written; a newer run posts {CONTEXT}")
            return 0
        verdict, eligible = read
    post(get, send, repo, args.pr, args.head, verdict, load_globs(), target, eligible)
    return 0


if __name__ == "__main__":
    sys.exit(main())
