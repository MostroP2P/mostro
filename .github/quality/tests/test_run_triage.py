"""Tests for what the triage bot does to a pull request (§ 7.2), against a
fake GitHub client."""

import json
import sys
import unittest
from pathlib import Path

QUALITY_DIR = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(QUALITY_DIR))

import run_triage  # noqa: E402
import triage  # noqa: E402

CONFIG = triage.load_config(QUALITY_DIR / "config.toml")
BOT = "github-actions[bot]"


def result(ok):
    check = triage.Check("issue", ok, () if ok else ("No accepted issue is linked",))
    return triage.Result(checks=(check,), exempt_reason=None)


class FakeGitHub:
    def __init__(self, comments=(), labels=()):
        self.comments = [dict(c) for c in comments]
        self.labels = set(labels)
        self.calls = []
        self.check_runs = []

    def add_labels(self, number, labels):
        self.calls.append(("add_labels", number, tuple(labels)))
        self.labels.update(labels)

    def remove_label(self, number, label):
        self.calls.append(("remove_label", number, label))
        self.labels.discard(label)

    def list_comments(self, number):
        return list(self.comments)

    def create_comment(self, number, body):
        self.calls.append(("create_comment", number))
        self.comments.append({"id": 99, "body": body, "user": {"login": BOT}})

    def update_comment(self, comment_id, body):
        self.calls.append(("update_comment", comment_id))
        for c in self.comments:
            if c["id"] == comment_id:
                c["body"] = body

    def delete_comment(self, comment_id):
        self.calls.append(("delete_comment", comment_id))
        self.comments = [c for c in self.comments if c["id"] != comment_id]

    def create_check_run(self, head_sha, conclusion, title, summary):
        self.check_runs.append((head_sha, conclusion, title))


def apply(gh, res, labels=()):
    run_triage.apply_result(gh, 7, "a" * 40, frozenset(labels), res, CONFIG)


class ApplyTest(unittest.TestCase):
    def test_failure_labels_comments_and_reports_neutral(self):
        gh = FakeGitHub()
        apply(gh, result(False))
        self.assertEqual(gh.labels, {"quality:needs-info"})
        self.assertEqual(len(gh.comments), 1)
        self.assertIn("No accepted issue is linked", gh.comments[0]["body"])
        self.assertEqual(gh.check_runs[0][1], "neutral")

    def test_failure_edits_the_existing_comment_in_place(self):
        old = {"id": 5, "body": triage.MARKER + "\nold reasons", "user": {"login": BOT}}
        gh = FakeGitHub(comments=[old], labels=["quality:needs-info"])
        apply(gh, result(False), labels=["quality:needs-info"])
        self.assertIn(("update_comment", 5), gh.calls)
        self.assertEqual(len(gh.comments), 1)
        self.assertNotIn(("add_labels", 7, ("quality:needs-info",)), gh.calls)

    def test_unchanged_comment_is_not_rewritten(self):
        gh = FakeGitHub()
        apply(gh, result(False))
        gh.calls.clear()
        apply(gh, result(False), labels=["quality:needs-info"])
        self.assertEqual(gh.calls, [])

    def test_success_swaps_the_label_and_deletes_the_comment(self):
        old = {"id": 5, "body": triage.MARKER + "\nold", "user": {"login": BOT}}
        gh = FakeGitHub(comments=[old], labels=["quality:needs-info"])
        apply(gh, result(True), labels=["quality:needs-info"])
        self.assertEqual(gh.labels, {"quality:ok"})
        self.assertEqual(gh.comments, [])
        self.assertEqual(gh.check_runs[0][1], "success")

    def test_someone_elses_comment_with_the_marker_is_left_alone(self):
        forged = {"id": 6, "body": triage.MARKER + "\nhi", "user": {"login": "mallory"}}
        gh = FakeGitHub(comments=[forged])
        apply(gh, result(True))
        self.assertEqual(gh.comments, [forged])


class PullRequestFromEventTest(unittest.TestCase):
    def test_reads_the_fields_the_checks_need(self):
        event = {"pull_request": {
            "number": 12, "title": "fix: x", "body": None, "draft": False,
            "user": {"login": "alice", "type": "User"}, "author_association": "NONE",
            "labels": [{"name": "bug"}], "head": {"sha": "f" * 40},
        }}
        pull = run_triage.pull_request_from_event(json.loads(json.dumps(event)))
        self.assertEqual(pull.body, "")
        self.assertEqual(pull.labels, ("bug",))
        self.assertEqual(pull.association, "NONE")


if __name__ == "__main__":
    unittest.main()
