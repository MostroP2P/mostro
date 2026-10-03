"""Triage bot entry point: fetch a pull request's facts, run the checks of
triage.py, and label and comment (docs/CONTRIBUTION_QUALITY_SPEC.md § 7.2).

Runs from quality-triage.yml on pull_request_target, from a checkout of
`main`. It never checks out or runs pull request code: the title and the
description come from the event payload as data, everything else from the
API. Standard library only.
"""

import json
import os
import sys
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path

import triage

QUALITY_DIR = Path(__file__).resolve().parent
REPO_ROOT = QUALITY_DIR.parents[1]
BOT_LOGIN = "github-actions[bot]"
CHECK_NAME = "Contribution quality bar"
PER_PAGE = 100

LINKED_ISSUES_QUERY = """
query($owner: String!, $name: String!, $number: Int!) {
  repository(owner: $owner, name: $name) {
    pullRequest(number: $number) {
      closingIssuesReferences(first: 20) {
        nodes {
          number
          repository { nameWithOwner }
          labels(first: 50) { nodes { name } }
        }
      }
    }
  }
}
"""


class GitHub:
    """The few REST and GraphQL calls the bot needs."""

    def __init__(self, repo, token, api="https://api.github.com"):
        self.repo, self.token, self.api = repo, token, api

    def request(self, method, path, body=None, params=None):
        url = self.api + path + ("?" + urllib.parse.urlencode(params) if params else "")
        data = json.dumps(body).encode() if body is not None else None
        req = urllib.request.Request(url, data=data, method=method, headers={
            "Authorization": f"Bearer {self.token}",
            "Accept": "application/vnd.github+json",
            "X-GitHub-Api-Version": "2022-11-28",
        })
        with urllib.request.urlopen(req, timeout=30) as resp:
            payload = resp.read()
        return json.loads(payload) if payload else None

    def paginate(self, path, params=None):
        items, page = [], 1
        while True:
            batch = self.request("GET", path, params={**(params or {}), "per_page": PER_PAGE, "page": page})
            items += batch
            if len(batch) < PER_PAGE:
                return items
            page += 1

    def graphql(self, query, variables):
        data = self.request("POST", "/graphql", {"query": query, "variables": variables})
        if data.get("errors"):
            raise RuntimeError(f"GraphQL errors: {data['errors']}")
        return data["data"]

    def add_labels(self, number, labels):
        self.request("POST", f"/repos/{self.repo}/issues/{number}/labels", {"labels": list(labels)})

    def remove_label(self, number, label):
        try:
            self.request("DELETE", f"/repos/{self.repo}/issues/{number}/labels/{urllib.parse.quote(label)}")
        except urllib.error.HTTPError as e:
            if e.code != 404:  # already gone
                raise

    def list_comments(self, number):
        return self.paginate(f"/repos/{self.repo}/issues/{number}/comments")

    def create_comment(self, number, body):
        self.request("POST", f"/repos/{self.repo}/issues/{number}/comments", {"body": body})

    def update_comment(self, comment_id, body):
        self.request("PATCH", f"/repos/{self.repo}/issues/comments/{comment_id}", {"body": body})

    def delete_comment(self, comment_id):
        self.request("DELETE", f"/repos/{self.repo}/issues/comments/{comment_id}")

    def create_check_run(self, head_sha, conclusion, title, summary):
        self.request("POST", f"/repos/{self.repo}/check-runs", {
            "name": CHECK_NAME, "head_sha": head_sha, "status": "completed",
            "conclusion": conclusion, "output": {"title": title, "summary": summary},
        })


def pull_request_from_event(event):
    pr = event["pull_request"]
    return triage.PullRequest(
        number=pr["number"],
        title=pr.get("title") or "",
        body=pr.get("body") or "",
        author=pr["user"]["login"],
        author_type=pr["user"].get("type", "User"),
        association=pr.get("author_association", "NONE"),
        labels=tuple(label["name"] for label in pr.get("labels", [])),
        head_sha=pr["head"]["sha"],
        draft=bool(pr.get("draft")),
    )


def gather_facts(gh, pull):
    n = pull.number
    files = tuple(
        triage.ChangedFile(
            f["filename"], f.get("additions", 0), f.get("deletions", 0), f.get("patch"),
            f.get("previous_filename"),
        )
        for f in gh.paginate(f"/repos/{gh.repo}/pulls/{n}/files")
    )
    commits = tuple(
        triage.Commit(c["sha"], bool(c["commit"].get("verification", {}).get("verified")))
        for c in gh.paginate(f"/repos/{gh.repo}/pulls/{n}/commits")
    )
    owner, name = gh.repo.split("/")
    data = gh.graphql(LINKED_ISSUES_QUERY, {"owner": owner, "name": name, "number": n})
    nodes = data["repository"]["pullRequest"]["closingIssuesReferences"]["nodes"]
    issues = tuple(
        triage.LinkedIssue(i["number"], tuple(label["name"] for label in i["labels"]["nodes"]))
        for i in nodes if i["repository"]["nameWithOwner"].lower() == gh.repo.lower()
    )
    query = f"repo:{gh.repo} is:pr is:open author:{pull.author}"
    open_prs = gh.request("GET", "/search/issues", params={"q": query, "per_page": 1})["total_count"]
    return triage.Facts(files, commits, issues, max(0, open_prs - 1))


def apply_result(gh, number, head_sha, current_labels, result, cfg):
    """Swap the verdict label, keep one sticky comment, report a check run."""
    want, drop = (
        (cfg["labels"]["ok"], cfg["labels"]["needs_info"]) if result.ok
        else (cfg["labels"]["needs_info"], cfg["labels"]["ok"])
    )
    if want not in current_labels:
        gh.add_labels(number, [want])
    if drop in current_labels:
        gh.remove_label(number, drop)

    mine = [
        c for c in gh.list_comments(number)
        if c["user"]["login"] == BOT_LOGIN and c["body"].startswith(triage.MARKER)
    ]
    if result.ok:
        for c in mine:
            gh.delete_comment(c["id"])
    else:
        body = triage.render_comment(result, gh_repo(gh))
        if not mine:
            gh.create_comment(number, body)
        elif mine[0]["body"] != body:
            gh.update_comment(mine[0]["id"], body)

    # Never `failure`: the check informs, it does not block a merge (§ 7.2).
    conclusion = "success" if result.ok else "neutral"
    title = "Meets the bar" if result.ok else f"{len(result.reasons)} thing(s) to fix"
    try:
        gh.create_check_run(head_sha, conclusion, title, summary(result))
    except urllib.error.HTTPError as e:
        print(f"::warning::could not create the check run: HTTP {e.code}", file=sys.stderr)


def gh_repo(gh):
    return getattr(gh, "repo", "MostroP2P/mostro")


def summary(result):
    rows = ["| check | result |", "|---|---|"]
    for c in result.checks:
        state = "skipped" if c.skipped else ("pass" if c.passed else "; ".join(c.reasons))
        rows.append(f"| `{c.name}` | {state} |")
    if result.exempt_reason:
        rows += ["", f"Content checks skipped: {result.exempt_reason}."]
    return "\n".join(rows)


def main():
    event = json.loads(Path(os.environ["GITHUB_EVENT_PATH"]).read_text())
    pull = pull_request_from_event(event)
    if pull.draft:
        print(f"#{pull.number} is a draft; it is triaged when marked ready for review.")
        return 0

    cfg = triage.load_config(QUALITY_DIR / "config.toml")
    template = triage.template_sections((REPO_ROOT / ".github/pull_request_template.md").read_text())
    gh = GitHub(os.environ["GITHUB_REPOSITORY"], os.environ["GITHUB_TOKEN"])

    result = triage.evaluate(pull, gather_facts(gh, pull), cfg, template)
    apply_result(gh, pull.number, pull.head_sha, frozenset(pull.labels), result, cfg)

    report = f"## Quality triage of #{pull.number}\n\n{summary(result)}\n"
    print(report)
    if os.environ.get("GITHUB_STEP_SUMMARY"):
        with open(os.environ["GITHUB_STEP_SUMMARY"], "a") as f:
            f.write(report)
    return 0


if __name__ == "__main__":
    sys.exit(main())
