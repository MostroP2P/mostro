"""Unit tests for money_path.py (docs/ORTSOM_PR_E2E_SPEC.md § 15)."""

import json
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

import money_path as mp  # noqa: E402

REPO = "MostroP2P/mostro"
HEAD = "a" * 40
GLOBS = ["src/lightning/**", "src/app/release.rs"]


class StatusTest(unittest.TestCase):
    def test_no_money_path_is_success_whatever_the_verdict(self):
        for verdict in ("pending", "regression", "none", None):
            state, _ = mp.status(["src/app/cancel.rs", "docs/x.md"], GLOBS, verdict)
            self.assertEqual(state, "success", verdict)

    def test_a_money_path_waits_for_the_verdict(self):
        state, text = mp.status(["src/lightning/mod.rs"], GLOBS, "pending")

        self.assertEqual(state, "pending")
        self.assertIn("waiting", text)

    def test_a_money_path_passes_only_with_ortsom_pass(self):
        self.assertEqual(mp.status(["src/app/release.rs"], GLOBS, "pass")[0], "success")
        for verdict in ("regression", "would-close", "inconclusive", "not-applicable", "none", None):
            state, text = mp.status(["src/app/release.rs"], GLOBS, verdict)
            self.assertEqual(state, "failure", verdict)
            self.assertIn("ortsom:pass", text)

    def test_a_pass_that_compared_nothing_is_not_enough(self):
        state, text = mp.status(["src/app/release.rs"], GLOBS, "pass", eligible=0)

        self.assertEqual(state, "failure")
        self.assertIn("compared no scenario", text)

    def test_a_run_that_did_not_complete_fails_a_money_path_only(self):
        self.assertEqual(mp.status(["src/app/release.rs"], GLOBS, "incomplete")[0], "failure")
        self.assertIn("ortsom:run", mp.status(["src/app/release.rs"], GLOBS, "incomplete")[1])
        self.assertEqual(mp.status(["README.md"], GLOBS, "incomplete")[0], "success")

    def test_the_description_fits_githubs_limit(self):
        _, text = mp.status(["src/lightning/" + "x" * 300 + ".rs"], GLOBS, "would-close")

        self.assertLessEqual(len(text), 140)

    def test_both_names_of_a_rename_count(self):
        files = mp.changed_files([{"filename": "src/app/x.rs", "previous_filename": "src/app/release.rs",
                                   "status": "renamed"}])

        self.assertEqual(mp.money_files(files, GLOBS), ["src/app/release.rs"])


class FakeGitHub:
    def __init__(self, files, head=HEAD, changed=None, statuses=(), state="open"):
        self.files, self.head, self.calls = files, head, []
        self.changed = len(files) if changed is None else changed
        self.statuses, self.state = list(statuses), state

    def get(self, path, params=""):
        if path.endswith("/files"):
            return self.files if "page=1" in params else []
        if path.endswith("/status"):
            return {"statuses": [{"context": c} for c in self.statuses]}
        pull = {"number": 7, "state": self.state, "head": {"sha": self.head}}
        if path.endswith("/pulls"):
            return [pull]  # the commit's list: no changed_files
        return {**pull, "changed_files": self.changed}

    def send(self, method, path, body=None):
        self.calls.append((method, path, body))
        return {}


class PostTest(unittest.TestCase):
    def post(self, gh, verdict="pending", head=HEAD, eligible=None):
        return mp.post(gh.get, gh.send, REPO, 7, head, verdict, GLOBS, "https://example/run", eligible)

    def test_posts_the_status_on_the_pull_requests_head(self):
        gh = FakeGitHub([{"filename": "src/lightning/mod.rs", "status": "modified"}])

        self.post(gh, "regression")

        self.assertEqual(gh.calls, [("POST", f"/repos/{REPO}/statuses/{HEAD}", {
            "state": "failure", "context": mp.CONTEXT, "target_url": "https://example/run",
            "description": mp.status(["src/lightning/mod.rs"], GLOBS, "regression")[1]})])

    def test_a_head_that_moved_gets_nothing(self):
        gh = FakeGitHub([{"filename": "src/lightning/mod.rs", "status": "modified"}], head="b" * 40)

        self.post(gh)

        self.assertEqual(gh.calls, [])

    def test_without_an_expected_head_the_current_one_is_used(self):
        gh = FakeGitHub([{"filename": "README.md", "status": "modified"}], head="c" * 40)

        self.post(gh, "none", head=None)

        self.assertEqual(gh.calls[0][1], f"/repos/{REPO}/statuses/{'c' * 40}")
        self.assertEqual(gh.calls[0][2]["state"], "success")

    def test_an_incomplete_file_list_is_treated_as_a_money_path(self):
        gh = FakeGitHub([{"filename": "README.md", "status": "modified"}], changed=3001)

        self.post(gh, "regression")

        self.assertEqual(gh.calls[0][2]["state"], "failure")
        self.assertIn("Could not list", gh.calls[0][2]["description"])


class FillGapTest(unittest.TestCase):
    def fill(self, gh):
        return mp.fill_gap(gh.get, gh.send, REPO, HEAD, GLOBS, "https://example/run")

    def test_a_money_path_without_a_status_gets_a_failure(self):
        gh = FakeGitHub([{"filename": "src/lightning/mod.rs", "status": "modified"}])

        self.fill(gh)

        self.assertEqual(gh.calls[0][1], f"/repos/{REPO}/statuses/{HEAD}")
        self.assertEqual(gh.calls[0][2]["state"], "failure")
        self.assertIn("ortsom:run", gh.calls[0][2]["description"])

    def test_any_other_pull_request_without_a_status_gets_a_success(self):
        gh = FakeGitHub([{"filename": "README.md", "status": "modified"}])

        self.fill(gh)

        self.assertEqual(gh.calls[0][2]["state"], "success")

    def test_an_existing_status_is_never_overwritten(self):
        # An unrelated label event on a pull request that already passed.
        gh = FakeGitHub([{"filename": "src/lightning/mod.rs", "status": "modified"}], statuses=[mp.CONTEXT])

        self.fill(gh)

        self.assertEqual(gh.calls, [])

    def test_only_an_open_pull_request_at_that_head(self):
        for gh in (FakeGitHub([], head="b" * 40), FakeGitHub([], state="closed")):
            self.fill(gh)
            self.assertEqual(gh.calls, [])


class VerdictFileTest(unittest.TestCase):
    def test_reads_the_verdict_and_its_eligible_count(self):
        with tempfile.TemporaryDirectory() as d:
            path = Path(d) / "verdict.json"
            self.assertIsNone(mp.read_verdict(path))
            path.write_text(json.dumps({"verdict": "pass", "counts": {"eligible": 3}}))
            self.assertEqual(mp.read_verdict(path), ("pass", 3))
            path.write_text(json.dumps({"verdict": "none"}))
            self.assertEqual(mp.read_verdict(path), ("none", None))


class MapTest(unittest.TestCase):
    def test_the_maps_money_paths_are_globs(self):
        self.assertTrue(mp.load_globs())
        with self.assertRaises(mp.MoneyPathError):
            mp.validate_globs({"money_path": {"paths": []}})
        with self.assertRaises(mp.MoneyPathError):
            mp.validate_globs({})


if __name__ == "__main__":
    unittest.main()
