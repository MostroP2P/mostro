"""Trusted side of the Ortsom gate: tie an `ortsom-pr.yml` run to its pull
request before anything runs against it (docs/ORTSOM_PR_E2E_SPEC.md § 6.2,
§ 8.2, § 9.1). Standard library only.

    resolve_run.py resolve --meta ortsom-pr/meta.json [--run-id N]
    resolve_run.py changes --pr N --out changes.txt

`resolve` writes `proceed`, `reason`, `pr`, `head_sha`, `base_sha`, `fork`,
`image`, `ortsom_ref` and `ortsom_ref_overridden` to $GITHUB_OUTPUT. It fails
closed: any check that does not pass sets `proceed=false` with the reason.
The triggering run comes from the workflow_run event, or from the API with
--run-id (the workflow_dispatch path, for maintainers).

`changes` writes the pull request's changed files, fetched through the API,
as `git diff --name-status` lines for select_scenarios.py.
"""

import argparse
import json
import os
import re
import sys
import tomllib
import urllib.request
from pathlib import Path

GATE_DIR = Path(__file__).resolve().parent
PR_WORKFLOW = ".github/workflows/ortsom-pr.yml"
META_FIELDS = {"pr", "head_sha", "base_sha", "skipped", "image"}
IMAGE_STATES = {"built", "build_failed", "not-needed"}
SKIP_REASONS = {"draft", "label", "dependabot"}
SHA = re.compile(r"^[0-9a-f]{40}$")
REF = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._/-]{0,99}$")
REF_LINE = re.compile(r"^ortsom-ref:[ \t]*(.*?)[ \t]*\r?$", re.M)
STATUS = {"added": "A", "removed": "D", "modified": "M", "changed": "M", "unchanged": "M"}
MAX_META_BYTES = 4096
PER_PAGE = 100
# GitHub lists at most 3000 files for a pull request.
MAX_PAGES = 30


class ResolveError(Exception):
    """The run cannot be tied to a pull request; nothing runs."""


class Superseded(ResolveError):
    """A newer push replaced the run's head; the newer run reports."""


def validate_meta(meta):
    """The untrusted `ortsom-pr` meta.json, field by field (§ 9.1)."""
    if not isinstance(meta, dict) or set(meta) != META_FIELDS:
        raise ResolveError(f"meta.json must have exactly the fields {sorted(META_FIELDS)}")
    pr = meta["pr"]
    if type(pr) is not int or pr <= 0:
        raise ResolveError("meta.json: pr must be a positive integer")
    for key in ("head_sha", "base_sha"):
        if not isinstance(meta[key], str) or not SHA.match(meta[key]):
            raise ResolveError(f"meta.json: {key} is not a 40-character lowercase hex SHA")
    if meta["skipped"] is not None and meta["skipped"] not in SKIP_REASONS:
        raise ResolveError("meta.json: skipped has an unknown reason")
    if meta["skipped"] is None and meta["image"] not in IMAGE_STATES:
        raise ResolveError("meta.json: image has an unknown state")
    if meta["skipped"] is not None and meta["image"] is not None:
        raise ResolveError("meta.json: a skipped run has no image")
    return meta


def check_association(run, pull, meta, repo):
    """The pull request belongs to the triggering run (§ 9.1)."""
    if run.get("event") != "pull_request":
        raise ResolveError(f"triggering run event is {run.get('event')!r}, not pull_request")
    if run.get("path") != PR_WORKFLOW:
        raise ResolveError(f"triggering workflow is {run.get('path')!r}, not {PR_WORKFLOW}")
    if pull.get("state") != "open":
        raise ResolveError("the pull request is not open")
    base = pull.get("base") or {}
    if base.get("ref") != "main" or (base.get("repo") or {}).get("full_name") != repo:
        raise ResolveError("the pull request's base is not main of this repository")
    head = pull.get("head") or {}
    head_repo = (head.get("repo") or {}).get("full_name")
    if not head_repo or head_repo != (run.get("head_repository") or {}).get("full_name"):
        raise ResolveError("head repository does not match the triggering run")
    if head.get("ref") != run.get("head_branch"):
        raise ResolveError("head branch does not match the triggering run")
    if meta["head_sha"] != run.get("head_sha"):
        raise ResolveError("meta.json head_sha does not match the triggering run")
    if head.get("sha") != run.get("head_sha"):
        raise Superseded("the pull request has a newer head than this run")


def ortsom_ref_override(body):
    """The § 8.2 `ortsom-ref:` line: (ref, None), (None, None) when absent,
    or (None, warning) when the value is not an acceptable ref."""
    m = REF_LINE.search(body or "")
    if not m:
        return None, None
    ref = m.group(1)
    if REF.match(ref) and ".." not in ref:
        return ref, None
    return None, "ortsom-ref line ignored: not a plain branch, tag or commit name"


def name_status(files):
    """API `pulls/{n}/files` entries as `git diff --name-status` lines."""
    lines = []
    for f in files:
        names = [f["filename"]]
        code = STATUS.get(f["status"], "M")
        if f["status"] in ("renamed", "copied"):
            names = [f["previous_filename"], f["filename"]]
            code = "R100" if f["status"] == "renamed" else "C100"
        if any("\t" in n or "\n" in n or "\r" in n for n in names):
            raise ResolveError("a changed file name contains a tab or a line break")
        lines.append("\t".join([code, *names]))
    return "".join(line + "\n" for line in lines)


def pinned_ref():
    with open(GATE_DIR / "map.toml", "rb") as f:
        return tomllib.load(f)["settings"]["ortsom_ref"]


def api(path, token, params=""):
    req = urllib.request.Request(
        f"https://api.github.com{path}{params}",
        headers={
            "Authorization": f"Bearer {token}",
            "Accept": "application/vnd.github+json",
            "X-GitHub-Api-Version": "2022-11-28",
        },
    )
    with urllib.request.urlopen(req, timeout=30) as resp:
        return json.loads(resp.read())


def paginate(path, token):
    items = []
    for page in range(1, MAX_PAGES + 1):
        batch = api(path, token, f"?per_page={PER_PAGE}&page={page}")
        items += batch
        if len(batch) < PER_PAGE:
            break
    return items


def write_outputs(values):
    text = "".join(f"{k}={v}\n" for k, v in values.items())
    print(text, end="")
    if os.environ.get("GITHUB_OUTPUT"):
        with open(os.environ["GITHUB_OUTPUT"], "a") as f:
            f.write(text)


def resolve(args, repo, token):
    if args.run_id:
        run = api(f"/repos/{repo}/actions/runs/{args.run_id}", token)
    else:
        run = json.loads(Path(os.environ["GITHUB_EVENT_PATH"]).read_text())["workflow_run"]
    if run.get("conclusion") != "success":
        raise ResolveError(f"triggering run concluded {run.get('conclusion')!r}")
    raw = Path(args.meta).read_bytes()
    if len(raw) > MAX_META_BYTES:
        raise ResolveError("meta.json is too large")
    meta = validate_meta(json.loads(raw))
    if meta["skipped"]:
        return {"proceed": "false", "reason": f"skipped-{meta['skipped']}", "pr": meta["pr"]}
    pull = api(f"/repos/{repo}/pulls/{meta['pr']}", token)
    check_association(run, pull, meta, repo)
    ref, warning = ortsom_ref_override(pull.get("body"))
    if warning:
        print(f"::warning::{warning}")
    return {
        "proceed": "true",
        "reason": "ok",
        "pr": meta["pr"],
        "head_sha": meta["head_sha"],
        "base_sha": meta["base_sha"],
        "fork": str(pull["head"]["repo"]["full_name"] != repo).lower(),
        "image": meta["image"],
        "ortsom_ref": ref or pinned_ref(),
        "ortsom_ref_overridden": str(ref is not None).lower(),
    }


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    sub = parser.add_subparsers(dest="command", required=True)
    p = sub.add_parser("resolve")
    p.add_argument("--meta", required=True)
    p.add_argument("--run-id", type=int)
    c = sub.add_parser("changes")
    c.add_argument("--pr", type=int, required=True)
    c.add_argument("--out", type=Path, required=True)
    args = parser.parse_args(argv)
    repo, token = os.environ["GITHUB_REPOSITORY"], os.environ["GITHUB_TOKEN"]

    if args.command == "changes":
        files = paginate(f"/repos/{repo}/pulls/{args.pr}/files", token)
        args.out.write_text(name_status(files))
        return 0
    try:
        outputs = resolve(args, repo, token)
    except Superseded as e:
        outputs = {"proceed": "false", "reason": "superseded"}
        print(f"::notice::{e}")
    except (ResolveError, json.JSONDecodeError, FileNotFoundError) as e:
        outputs = {"proceed": "false", "reason": "rejected"}
        print(f"::warning::rejected artifact: {e}")
    write_outputs(outputs)
    return 0


if __name__ == "__main__":
    sys.exit(main())
