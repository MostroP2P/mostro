"""Trusted side of the Ortsom gate: tie an `ortsom-pr.yml` run to its pull
request before anything runs against it (docs/ORTSOM_PR_E2E_SPEC.md § 6.2,
§ 8.2, § 9.1). Standard library only.

    resolve_run.py resolve --meta ortsom-pr/meta.json [--run-id N]
    resolve_run.py changes --pr N --head SHA --out changes.txt

`resolve` writes `proceed`, `reason`, `pr`, `head_sha`, `base_sha`, `fork`,
`image`, `ortsom_ref` and `ortsom_ref_overridden` to $GITHUB_OUTPUT. It fails
closed: any check that does not pass sets `proceed=false` with the reason. A
run cancelled by hand yields `reason=cancelled` and its `pr`, for the verdict
job to clear what the last verdict left.
The triggering run comes from the workflow_run event, or from the API with
--run-id (the workflow_dispatch path, for maintainers).

`changes` writes the pull request's changed files, fetched through the API,
as `git diff --name-status` lines for select_scenarios.py. It exits 3 when
the list is incomplete (GitHub lists at most 3000 files) and 4 when the head
moved: a partial list could select less than the pull request touches.
"""

import argparse
import json
import os
import re
import sys
import tomllib
import urllib.parse
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
    for key in ("skipped", "image"):
        if meta[key] is not None and not isinstance(meta[key], str):
            raise ResolveError(f"meta.json: {key} must be a string or null")
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


def checkout_ref(ref):
    """An override as actions/checkout needs it: a bare number is an Ortsom
    pull request (§ 8.2), anything else a branch, tag or commit."""
    return f"refs/pull/{ref}/head" if ref.isdigit() else ref


def current_skip(pull):
    """Why the pull request should not run now, whatever the build said."""
    if pull.get("draft"):
        return "draft"
    if "ortsom:skip" in {label.get("name") for label in pull.get("labels") or []}:
        return "label"
    if (pull.get("user") or {}).get("login") == "dependabot[bot]":
        return "dependabot"
    return None


def decide(run, meta, pull, repo, pinned):
    """resolve's outputs once the run, meta.json and the PR are fetched.
    The association is checked first, skips included: a skip names a PR
    and must be that run's PR before anything acts on it."""
    check_association(run, pull, meta, repo)
    skip = meta["skipped"] or current_skip(pull)
    if skip:
        return {"proceed": "false", "reason": f"skipped-{skip}", "pr": meta["pr"]}
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
        "ortsom_ref": checkout_ref(ref) if ref else pinned,
        "ortsom_ref_overridden": str(ref is not None).lower(),
    }


def collect_changes(get, repo, pr, head):
    """The complete changed-file list as name-status text, or an error: a
    list GitHub cut at its 3000-file ceiling, or one for a head that has
    moved, is not the pull request."""
    files = paginate(get, f"/repos/{repo}/pulls/{pr}/files")
    pull = get(f"/repos/{repo}/pulls/{pr}")
    if (pull.get("head") or {}).get("sha") != head:
        raise Superseded("the pull request's head moved while listing its files")
    if len(files) != pull.get("changed_files"):
        raise ResolveError(
            f"GitHub listed {len(files)} of {pull.get('changed_files')} changed files; "
            "a partial list could select less than the pull request touches"
        )
    return name_status(files)


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


def paginate(get, path):
    items = []
    for page in range(1, MAX_PAGES + 1):
        batch = get(path, f"?per_page={PER_PAGE}&page={page}")
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


def resolve_cancelled(run, get, repo):
    """A cancelled `ortsom-pr.yml` run (§ 9.1). A newer run of the same head
    repository and branch is its successor and will report. Without one it
    was cancelled by hand and may have uploaded nothing, so the pull request
    is found from the run's head and checked without meta.json."""
    head_repo = run.get("head_repository") or {}
    branch = run.get("head_branch") or ""
    runs = get(
        f"/repos/{repo}/actions/workflows/ortsom-pr.yml/runs",
        f"?branch={urllib.parse.quote(branch, safe='')}&event=pull_request&per_page={PER_PAGE}",
    )["workflow_runs"]
    for other in runs:
        if (other.get("id") != run.get("id")
                and (other.get("head_repository") or {}).get("full_name") == head_repo.get("full_name")
                and other.get("created_at", "") > run.get("created_at", "")):
            raise Superseded(f"run {other.get('id')} of the same branch replaced the cancelled one")
    owner = (head_repo.get("owner") or {}).get("login") or ""
    head = urllib.parse.quote(f"{owner}:{branch}", safe="")
    pulls = get(f"/repos/{repo}/pulls", f"?state=open&head={head}&per_page={PER_PAGE}")
    if len(pulls) != 1:
        raise ResolveError(f"{len(pulls)} open pull requests for the cancelled run's head")
    check_association(run, pulls[0], {"head_sha": run.get("head_sha")}, repo)
    return {"proceed": "false", "reason": "cancelled", "pr": pulls[0]["number"]}


def resolve(args, repo, token):
    def get(path, params=""):
        return api(path, token, params)

    if args.run_id:
        run = get(f"/repos/{repo}/actions/runs/{args.run_id}")
    else:
        run = json.loads(Path(os.environ["GITHUB_EVENT_PATH"]).read_text())["workflow_run"]
    if run.get("conclusion") == "cancelled":
        return resolve_cancelled(run, get, repo)
    if run.get("conclusion") != "success":
        raise ResolveError(f"triggering run concluded {run.get('conclusion')!r}")
    raw = Path(args.meta).read_bytes()
    if len(raw) > MAX_META_BYTES:
        raise ResolveError("meta.json is too large")
    meta = validate_meta(json.loads(raw))
    pull = get(f"/repos/{repo}/pulls/{meta['pr']}")
    return decide(run, meta, pull, repo, pinned_ref())


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    sub = parser.add_subparsers(dest="command", required=True)
    p = sub.add_parser("resolve")
    p.add_argument("--meta", required=True)
    p.add_argument("--run-id", type=int)
    c = sub.add_parser("changes")
    c.add_argument("--pr", type=int, required=True)
    c.add_argument("--head", required=True)
    c.add_argument("--out", type=Path, required=True)
    args = parser.parse_args(argv)
    repo, token = os.environ["GITHUB_REPOSITORY"], os.environ["GITHUB_TOKEN"]

    if args.command == "changes":
        try:
            text = collect_changes(lambda path, params="": api(path, token, params), repo, args.pr, args.head)
        except Superseded as e:
            print(f"::notice::{e}")
            return 4
        except ResolveError as e:
            print(f"::warning::{e}")
            return 3
        args.out.write_text(text)
        return 0
    try:
        outputs = resolve(args, repo, token)
    except Superseded as e:
        outputs = {"proceed": "false", "reason": "superseded"}
        print(f"::notice::{e}")
    # ValueError covers bad JSON and bad UTF-8; OSError covers a missing
    # file and HTTPError/URLError from the API.
    except (ResolveError, ValueError, OSError, RecursionError) as e:
        outputs = {"proceed": "false", "reason": "rejected"}
        print(f"::warning::rejected artifact: {e}")
    write_outputs(outputs)
    return 0


if __name__ == "__main__":
    sys.exit(main())
