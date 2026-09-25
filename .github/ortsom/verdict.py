"""Verdict of the Ortsom gate for one pull request run
(docs/ORTSOM_PR_E2E_SPEC.md § 7, § 8, § 9.1). Standard library only.

    verdict.py --pr N --reason R [--head-sha SHA] [--result DIR] --out DIR

`--reason` is resolve_run.py's: `ok` reads the `ortsom-pr-result` artifact
in DIR, finds the baseline of `main` it compares against, classifies every
selected scenario and applies the verdict, one `ortsom:*` label and a
sticky comment. `skipped-*` and `cancelled` only clear the verdict labels
and, if a verdict comment exists, rewrite it to say no verdict is current.
Shadow mode: nothing is ever closed. Writes verdict.json and comment.md to
the --out directory.
"""

import argparse
import io
import json
import os
import re
import sys
import tomllib
import urllib.error
import urllib.parse
import urllib.request
import zipfile
from pathlib import Path

from resolve_run import api, current_skip
from select_scenarios import parse_name_status

GATE_DIR = Path(__file__).resolve().parent
BASELINE_WORKFLOW = "ortsom-baseline.yml"
BASELINE_ARTIFACT = "ortsom-baseline"
MARKER = "<!-- ortsom-verdict -->"
BOT = "github-actions[bot]"
VERDICT_LABELS = {
    "pass": "ortsom:pass",
    "regression": "ortsom:regression",
    "would-close": "ortsom:would-close",
    "inconclusive": "ortsom:inconclusive",
}
RUN_LABEL = "ortsom:run"
EXPECTED_BREAK = "ortsom:expected-break"
GATE_PATHS = (re.compile(r"^\.github/ortsom/"), re.compile(r"^\.github/workflows/ortsom-[^/]*\.yml$"))
STACK_REASONS = {
    "no_image": "no-image",
    "bad_image": "bad-image",
    "build_failed": "build-failed",
    "infra_failed": "infra-failed",
    "doctor_failed": "doctor-failed",
}
OUTCOMES = {"passed", "failed", "skipped", "interrupted"}
DETAIL_MAX = 200
FILES_MAX = 30
COMMENT_MAX = 60000
MAX_ARTIFACT_BYTES = 64 * 1024 * 1024
BASELINE_RUN_PAGES = 3
PER_PAGE = 100


class VerdictError(Exception):
    """The result cannot be judged; the job fails and nothing is labelled."""


# --- Untrusted text ----------------------------------------------------------


def code(text):
    """Text the pull request controls (a scenario detail from its daemon, a
    file name) as one code span: one line, no backticks, cut to 200
    characters, `|` escaped for tables. It cannot open a link, a mention,
    HTML or a comment marker (§ 7.4)."""
    text = " ".join(str(text).replace("`", "").split())[:DETAIL_MAX]
    return f"`{text.replace('|', chr(92) + '|')}`" if text else "—"


def outcome_text(outcome):
    """A scenario outcome as the table shows it. Ortsom writes one of four
    words; anything else is shown as untrusted text."""
    return outcome if outcome in OUTCOMES else code(outcome)


def gate_modified(files):
    return any(p.match(f) for f in files for p in GATE_PATHS)


# --- Classification (§ 7.2, § 7.3) -------------------------------------------


def results_by_name(summary):
    results = (summary or {}).get("results") or []
    return {r["name"]: r for r in results if isinstance(r, dict) and "name" in r}


def complete(summary):
    """Ortsom writes summary.json once, after the last scenario: its presence
    marks a finished run, unless a scenario was interrupted (§ 7.2)."""
    if not isinstance(summary, dict) or not isinstance(summary.get("results"), list):
        return False
    return all(r.get("outcome") != "interrupted" for r in results_by_name(summary).values())


def unstable(result):
    return result is not None and (result.get("flaky") or result.get("outcome") in ("failed", "interrupted"))


def classify(scenarios, pr_summary, history):
    """One row per selected scenario, classified against the reference
    baseline (history[0]) and the whole history."""
    pr = results_by_name(pr_summary)
    reference = results_by_name(history[0]["summary"])
    runs = [results_by_name(h["summary"]) for h in history]
    rows = []
    for name in scenarios:
        p, b = pr.get(name), reference.get(name)
        if p is not None and p.get("outcome") == "skipped":
            cls = "skipped"
        elif b is None:
            cls = "no-baseline"
        elif b.get("outcome") != "passed" or any(unstable(r.get(name)) for r in runs):
            cls = "unstable-on-main"
        elif p is None:
            cls = "missing"
        elif p.get("outcome") == "passed":
            cls = "pass"
        else:
            cls = "regression"
        rows.append({
            "name": name,
            "class": cls,
            "pr": p.get("outcome") if p else None,
            "baseline": b.get("outcome") if b else None,
            "detail": (p or {}).get("detail") or "",
            "flaky": bool(p and p.get("flaky")),
        })
    return rows


def count(rows):
    passed = sum(r["class"] == "pass" for r in rows)
    regressed = sum(r["class"] in ("regression", "missing") for r in rows)
    eligible = passed + regressed
    return {"pass": passed, "regression": regressed, "eligible": eligible,
            "ratio": regressed / eligible if eligible else 0.0}


def precheck(meta, selection, changed, pr_summary):
    """The verdicts of § 7.3 that need no baseline, in the spec's order:
    (verdict, reason), or None when the baseline decides."""
    stack = meta.get("stack")
    if stack == "selection_error" or not isinstance(selection, dict):
        return ("inconclusive", "selection-error")
    if stack == "not_run" or selection.get("mode") == "none":
        return ("not-applicable", "no-selection")
    if meta.get("registry_stale"):
        return ("inconclusive", "stale-registry")
    if gate_modified(changed):
        return ("inconclusive", "gate-modified")
    if stack in STACK_REASONS:
        return ("inconclusive", STACK_REASONS[stack])
    if stack not in ("ok", "daemon_failed"):
        return ("inconclusive", "unknown-stack")
    if stack == "ok" and not complete(pr_summary):
        return ("inconclusive", "incomplete-run")
    return None


def judge(meta, scenarios, pr_summary, history, settings, caps):
    """The rest of § 7.3 once the history is known. `caps` names what caps
    the verdict at `regression`: "label" (ortsom:expected-break) and
    "override" (an ortsom-ref: line)."""
    out = {"rows": [], "counts": count([]), "history": history, "caps": caps}
    if not history:
        return {**out, "verdict": "inconclusive", "reason": "no-baseline"}
    if meta.get("stack") == "daemon_failed":
        # Not a sample of scenarios: no minimum, no ratio (§ 7.3).
        would_close, reason = True, "daemon-failed"
    else:
        rows = classify(scenarios, pr_summary, history)
        out.update(rows=rows, counts=count(rows))
        c = out["counts"]
        would_close = c["eligible"] >= settings["min_scenarios"] and c["ratio"] >= settings["close_ratio"]
        reason = "ratio"
    if would_close:
        return {**out, "verdict": "regression" if caps else "would-close", "reason": "capped" if caps else reason}
    if out["counts"]["regression"]:
        return {**out, "verdict": "regression", "reason": "below-threshold"}
    return {**out, "verdict": "pass", "reason": "ok"}


# --- Baselines (§ 7.1) -------------------------------------------------------


def first_parent_walk(get, repo, start, max_distance):
    """`start` and up to `max_distance` first parents, nearest first."""
    shas = [start]
    while len(shas) <= max_distance:
        parents = get(f"/repos/{repo}/commits/{shas[-1]}").get("parents") or []
        if not parents:
            break
        shas.append(parents[0]["sha"])
    return shas


def baseline_runs(get, repo):
    """Completed baseline runs on main, as the Actions API lists them."""
    runs = []
    for page in range(1, BASELINE_RUN_PAGES + 1):
        batch = get(
            f"/repos/{repo}/actions/workflows/{BASELINE_WORKFLOW}/runs",
            f"?branch=main&status=completed&per_page={PER_PAGE}&page={page}",
        )["workflow_runs"]
        runs += batch
        if len(batch) < PER_PAGE:
            break
    return runs


def read_baseline_zip(data):
    """(meta, summary or None) from an `ortsom-baseline` artifact archive,
    or None when it holds no readable meta.json."""
    try:
        with zipfile.ZipFile(io.BytesIO(data)) as z:
            names = set(z.namelist())
            if "meta.json" not in names:
                return None
            meta = json.loads(z.read("meta.json"))
            summary = json.loads(z.read("summary.json")) if "summary.json" in names else None
    except (zipfile.BadZipFile, ValueError, KeyError):
        return None
    return meta, summary


def load_baseline(get, download, repo, run):
    listing = get(f"/repos/{repo}/actions/runs/{run['id']}/artifacts", f"?name={BASELINE_ARTIFACT}")
    artifacts = [a for a in listing.get("artifacts") or [] if not a.get("expired")]
    if not artifacts:
        return None
    return read_baseline_zip(download(artifacts[0]["archive_download_url"]))


def find_baselines(walk, runs, load, pinned_ref, window):
    """The history of § 7.1, newest first, at most `window` long: baselines
    of commits on the walk, measured with the pinned Ortsom, whose stack came
    up and whose suite finished. The first is the reference baseline. Each
    entry is {distance, run, meta, summary}."""
    by_sha = {}
    for run in sorted(runs, key=lambda r: r["created_at"], reverse=True):
        by_sha.setdefault(run["head_sha"], []).append(run)
    history = []
    for distance, sha in enumerate(walk):
        for run in by_sha.get(sha, []):
            loaded = load(run)
            if loaded is None:
                continue
            meta, summary = loaded
            if (meta.get("sha") != sha or meta.get("ortsom_ref") != pinned_ref
                    or meta.get("stack") != "ok" or not complete(summary)):
                continue
            history.append({"distance": distance, "run": run, "meta": meta, "summary": summary})
            if len(history) == window:
                return history
    return history


# --- The comment (§ 7.4) -----------------------------------------------------

HEADLINES = {
    "pass": "✅ pass",
    "regression": "⚠️ regression",
    "would-close": "⛔ would close",
    "inconclusive": "❔ inconclusive",
    "not-applicable": "➖ not applicable",
}
INCONCLUSIVE = {
    "selection-error": "The trusted scenario selection failed: `main`'s map rejected the changed files, "
                       "or GitHub listed fewer changed files than the pull request has.",
    "stale-registry": "The Ortsom scenario list differs from `.github/ortsom/scenarios.json` on `main`; "
                      "the snapshot must be updated with the harness.",
    "gate-modified": "This pull request changes the gate itself (`.github/ortsom/` or an `ortsom-*.yml` "
                     "workflow), so its image may not have been built the way the baseline's was.",
    "no-image": "The build side decided no image was needed, but `main`'s map selects scenarios "
                "for these changes.",
    "bad-image": "The image the build handed over was not exactly the expected one, so it was not run.",
    "build-failed": "The pull request does not compile: the image build in `Ortsom PR` failed.",
    "infra-failed": "The regtest stack did not come up, for reasons outside the daemon.",
    "doctor-failed": "`ortsom doctor` failed after the stack came up.",
    "incomplete-run": "The suite did not finish (a timeout, a cancellation or a lost runner).",
    "unknown-stack": "The run reported a stack state this version of the gate does not know.",
}
SKIP_TEXT = {
    "skipped-draft": "this pull request is a draft",
    "skipped-label": "this pull request carries `ortsom:skip`",
    "skipped-dependabot": "this pull request is Dependabot's",
}
SECTIONS = [
    ("pass", "Passed"),
    ("skipped", "Skipped by Ortsom"),
    ("unstable-on-main", "Unstable on `main` (not compared)"),
    ("no-baseline", "Not in the baseline (not compared)"),
]
HELP = ("Re-run: push a commit or add `ortsom:run`. Escape hatches: `ortsom:skip`, `ortsom:expected-break`, "
        "or an `ortsom-ref:` line in the description (docs/ORTSOM_PR_E2E_SPEC.md § 8).")


def sentence(outcome, meta, settings):
    verdict, c = outcome["verdict"], outcome["counts"]
    if verdict == "inconclusive":
        if outcome["reason"] == "no-baseline":
            return (f"No baseline of `main` measured with Ortsom `{settings['ortsom_ref']}` exists within "
                    f"{settings['baseline_max_distance']} commits of the merge-base. After `ortsom_ref` is "
                    "bumped on `main` this lasts until the pull request merges or rebases onto a `main` "
                    "whose new-harness baseline has finished.")
        return INCONCLUSIVE.get(outcome["reason"], INCONCLUSIVE["unknown-stack"])
    if verdict == "not-applicable":
        return "No scenario applies: the map ignores every changed file."
    if meta.get("stack") == "daemon_failed":
        return "The daemon built from this pull request did not start, while the same stack starts on `main`."
    if verdict == "pass" and not c["regression"]:
        if not c["eligible"]:
            return "No scenario was eligible for comparison: each was skipped, unstable on `main` or new."
        return f"All {c['eligible']} eligible scenarios passed."
    return (f"{c['regression']} of {c['eligible']} eligible scenarios regressed ({c['ratio']:.0%}, "
            f"threshold {settings['close_ratio']:.0%}, minimum {settings['min_scenarios']}).")


def file_lines(files):
    lines = list(files[:FILES_MAX])
    if len(files) > FILES_MAX:
        lines.append(f"- … and {len(files) - FILES_MAX} more")
    return lines


def render_comment(outcome, meta, selection, ctx):
    settings, verdict = ctx["settings"], outcome["verdict"]
    lines = [MARKER, f"### Ortsom e2e: {HEADLINES[verdict]}", "", sentence(outcome, meta, settings)]
    if outcome["caps"]:
        why = []
        if "label" in outcome["caps"]:
            why.append("the `ortsom:expected-break` label")
        if "override" in outcome["caps"]:
            why.append("an `ortsom-ref:` override, which compares against a baseline measured with another Ortsom")
        lines += ["", f"The verdict is capped at regression by {' and '.join(why)}."]
    if verdict == "would-close":
        lines += ["", "> Shadow mode: this pull request would have been closed. It stays open; "
                      "a maintainer reviews this verdict."]

    lines += ["", f"Daemon `{meta.get('head_sha', '')[:12]}`, Ortsom `{meta.get('ortsom_ref', '')}`"
                  + (" (override)" if meta.get("ortsom_ref_overridden") else "")
                  + f", selection `{selection.get('mode', '?')}`."]
    if outcome["history"]:
        ref = outcome["history"][0]
        d = ref["distance"]
        where = "at the merge-base" if d == 0 else f"{d} commit{'s' if d != 1 else ''} before the merge-base"
        lines.append(f"Baseline: `main` at `{ref['meta']['sha'][:12]}`, {where} "
                     f"([run]({ref['run']['html_url']})).")

    regressions = [r for r in outcome["rows"] if r["class"] in ("regression", "missing")]
    if regressions:
        lines += ["", "| Regressed scenario | This pull request | Baseline |", "|---|---|---|"]
        for r in regressions:
            cell = "not run" if r["class"] == "missing" else outcome_text(r["pr"])
            if r["class"] != "missing" and r["detail"]:
                cell += f": {code(r['detail'])}"
            lines.append(f"| {code(r['name'])} | {cell} | {outcome_text(r['baseline'])} |")
    for cls, title in SECTIONS:
        rows = [r for r in outcome["rows"] if r["class"] == cls]
        if rows:
            names = ", ".join(code(r["name"]) + (" (flaky)" if r["flaky"] else "") for r in rows)
            lines += ["", f"<details><summary>{title} ({len(rows)})</summary>", "", names, "", "</details>"]

    uncovered = [f"- {code(f)}: {why}" for f, why in sorted((selection.get("uncovered_files") or {}).items())]
    unmapped = [f"- {code(f)}" for f in selection.get("unmapped_files") or []]
    if uncovered:
        lines += ["", "Changed code no Ortsom scenario exercises:", ""] + file_lines(uncovered)
    if unmapped:
        lines += ["", "Changed files the map does not know:", ""] + file_lines(unmapped)

    lines += ["", f"[This run]({ctx['run_url']}) · [artifacts]({ctx['run_url']}#artifacts). {HELP}"]
    return "\n".join(lines)[:COMMENT_MAX] + "\n"


def render_notice(reason):
    if reason == "cancelled":
        text = ("The last Ortsom run was cancelled; no verdict is current. "
                "Push a commit or add `ortsom:run` to run it again.")
    else:
        text = (f"Ortsom is skipped: {SKIP_TEXT.get(reason, 'the gate is skipped')}. "
                "No verdict is current; the gate runs again once that changes.")
    return f"{MARKER}\n### Ortsom e2e: skipped\n\n{text}\n"


# --- GitHub ------------------------------------------------------------------


def remove_label(send, repo, pr, name):
    try:
        send("DELETE", f"/repos/{repo}/issues/{pr}/labels/{urllib.parse.quote(name, safe='')}")
    except urllib.error.HTTPError as e:
        if e.code != 404:
            raise


def sticky_comment(get, repo, pr):
    """The verdict comment: the marker opens it and the Actions bot wrote it.
    A marker in anyone else's comment is ignored (§ 7.4)."""
    for page in range(1, 31):
        batch = get(f"/repos/{repo}/issues/{pr}/comments", f"?per_page={PER_PAGE}&page={page}")
        for c in batch:
            if (c.get("user") or {}).get("login") == BOT and (c.get("body") or "").startswith(MARKER):
                return c
        if len(batch) < PER_PAGE:
            return None
    return None


def apply(get, send, repo, pr, label, body, create):
    """Leave exactly `label` (or no verdict label) and no `ortsom:run`, then
    edit the sticky comment, or post it when `create`."""
    pull = get(f"/repos/{repo}/pulls/{pr}")
    present = {lbl.get("name") for lbl in pull.get("labels") or []}
    for name in sorted(present & (set(VERDICT_LABELS.values()) | {RUN_LABEL})):
        if name != label:
            remove_label(send, repo, pr, name)
    if label and label not in present:
        send("POST", f"/repos/{repo}/issues/{pr}/labels", {"labels": [label]})
    existing = sticky_comment(get, repo, pr)
    if existing:
        send("PATCH", f"/repos/{repo}/issues/comments/{existing['id']}", {"body": body})
    elif create:
        send("POST", f"/repos/{repo}/issues/{pr}/comments", {"body": body})


# --- The job -----------------------------------------------------------------


def read_json(path):
    return json.loads(path.read_text()) if path.is_file() else None


def outcome_json(outcome):
    doc = {k: v for k, v in outcome.items() if k != "history"}
    doc["history"] = [{"run_id": h["run"]["id"], "sha": h["meta"]["sha"], "distance": h["distance"]}
                      for h in outcome.get("history") or []]
    return doc


def write_out(out, outcome, body):
    out = Path(out)
    out.mkdir(parents=True, exist_ok=True)
    (out / "verdict.json").write_text(json.dumps(outcome_json(outcome), indent=2) + "\n")
    (out / "comment.md").write_text(body)


def run(args, repo, get, send, find_history, settings):
    """The verdict job. Returns the outcome it applied, or None when it left
    the pull request untouched."""
    pr, reason = args["pr"], args["reason"]
    pull = get(f"/repos/{repo}/pulls/{pr}")
    if reason == "ok":
        # § 9.1: re-read the pull request before applying anything.
        if (pull.get("head") or {}).get("sha") != args["head_sha"]:
            print(f"::notice::#{pr} has a newer head; the newer run reports")
            return None
        skip = current_skip(pull)
        if skip:
            reason = f"skipped-{skip}"
    if reason != "ok":
        body = render_notice(reason)
        apply(get, send, repo, pr, None, body, create=False)
        outcome = {"verdict": "none", "reason": reason, "rows": [], "counts": count([]), "history": [], "caps": []}
        write_out(args["out"], outcome, body)
        return outcome

    res = Path(args["result"])
    meta = read_json(res / "meta.json")
    if not isinstance(meta, dict) or meta.get("pr") != pr or meta.get("head_sha") != args["head_sha"]:
        raise VerdictError("ortsom-pr-result does not belong to this pull request and head")
    if meta.get("stack") == "superseded":
        print(f"::notice::#{pr}'s head moved during the run; the newer run reports")
        return None
    selection = read_json(res / "selection.json")
    changes = res / "changes.txt"
    changed = parse_name_status(changes.read_text()) if changes.is_file() else []
    pr_summary = read_json(res / "summary.json")

    labels = {lbl.get("name") for lbl in pull.get("labels") or []}
    caps = (["label"] if EXPECTED_BREAK in labels else []) + (["override"] if meta.get("ortsom_ref_overridden") else [])
    early = precheck(meta, selection, changed, pr_summary)
    if early:
        outcome = {"verdict": early[0], "reason": early[1], "rows": [], "counts": count([]),
                   "history": [], "caps": caps}
    else:
        outcome = judge(meta, selection["scenarios"], pr_summary, find_history(meta), settings, caps)

    body = render_comment(outcome, meta, selection or {}, {"pr": pr, "run_url": args["run_url"], "settings": settings})
    write_out(args["out"], outcome, body)
    label = VERDICT_LABELS.get(outcome["verdict"])
    apply(get, send, repo, pr, label, body, create=label is not None)
    return outcome


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


def download(url, token):
    """An artifact archive. The token must not follow the redirect to the
    storage host, hence an unredirected header."""
    req = urllib.request.Request(url)
    req.add_unredirected_header("Authorization", f"Bearer {token}")
    with urllib.request.urlopen(req, timeout=60) as resp:
        data = resp.read(MAX_ARTIFACT_BYTES + 1)
    if len(data) > MAX_ARTIFACT_BYTES:
        raise VerdictError("baseline artifact is too large")
    return data


def history_finder(get, fetch, repo, settings):
    def find(meta):
        compare = get(f"/repos/{repo}/compare/main...{meta['head_sha']}", "?per_page=1")
        walk = first_parent_walk(get, repo, compare["merge_base_commit"]["sha"], settings["baseline_max_distance"])
        return find_baselines(walk, baseline_runs(get, repo), lambda r: load_baseline(get, fetch, repo, r),
                              settings["ortsom_ref"], settings["stability_window"])
    return find


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--pr", type=int, required=True)
    parser.add_argument("--reason", required=True)
    parser.add_argument("--head-sha")
    parser.add_argument("--result", type=Path)
    parser.add_argument("--out", type=Path, required=True)
    args = parser.parse_args(argv)
    repo, token = os.environ["GITHUB_REPOSITORY"], os.environ["GITHUB_TOKEN"]
    run_url = f"{os.environ.get('GITHUB_SERVER_URL', 'https://github.com')}/{repo}/actions/runs/{os.environ.get('GITHUB_RUN_ID', '')}"
    with open(GATE_DIR / "map.toml", "rb") as f:
        settings = tomllib.load(f)["settings"]

    def get(path, params=""):
        return api(path, token, params)

    def send(method, path, body=None):
        return request(method, path, token, body)

    job = {"pr": args.pr, "reason": args.reason, "head_sha": args.head_sha,
           "result": args.result, "out": args.out, "run_url": run_url}
    outcome = run(job, repo, get, send, history_finder(get, lambda url: download(url, token), repo, settings), settings)
    if outcome and os.environ.get("GITHUB_STEP_SUMMARY"):
        with open(os.environ["GITHUB_STEP_SUMMARY"], "a") as f:
            f.write((Path(args.out) / "comment.md").read_text().replace(MARKER + "\n", ""))
    return 0


if __name__ == "__main__":
    sys.exit(main())
