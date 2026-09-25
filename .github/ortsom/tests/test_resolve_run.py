"""Unit tests for resolve_run.py (docs/ORTSOM_PR_E2E_SPEC.md § 6.2, § 8.2, § 9.1)."""

import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

import resolve_run as rr  # noqa: E402

REPO = "MostroP2P/mostro"
HEAD = "a" * 40
BASE = "b" * 40


def meta(**over):
    doc = {"pr": 7, "head_sha": HEAD, "base_sha": BASE, "skipped": None, "image": "built"}
    doc.update(over)
    return doc


def run(**over):
    doc = {
        "event": "pull_request",
        "path": ".github/workflows/ortsom-pr.yml",
        "conclusion": "success",
        "head_sha": HEAD,
        "head_branch": "fix/thing",
        "head_repository": {"full_name": "alice/mostro"},
    }
    doc.update(over)
    return doc


def pull(**over):
    doc = {
        "number": 7,
        "state": "open",
        "base": {"ref": "main", "repo": {"full_name": REPO}},
        "head": {"sha": HEAD, "ref": "fix/thing", "repo": {"full_name": "alice/mostro"}},
        "body": "",
    }
    doc.update(over)
    return doc


class ValidateMetaTest(unittest.TestCase):
    def test_a_built_image_is_valid(self):
        self.assertEqual(rr.validate_meta(meta())["pr"], 7)

    def test_a_skip_carries_its_reason_and_no_image(self):
        rr.validate_meta(meta(skipped="draft", image=None))

    def test_unknown_field_is_rejected(self):
        with self.assertRaisesRegex(rr.ResolveError, "fields"):
            rr.validate_meta({**meta(), "extra": 1})

    def test_missing_field_is_rejected(self):
        doc = meta()
        del doc["image"]
        with self.assertRaisesRegex(rr.ResolveError, "fields"):
            rr.validate_meta(doc)

    def test_bad_sha_is_rejected(self):
        with self.assertRaisesRegex(rr.ResolveError, "head_sha"):
            rr.validate_meta(meta(head_sha="HEAD; rm -rf /"))

    def test_uppercase_sha_is_rejected(self):
        with self.assertRaisesRegex(rr.ResolveError, "base_sha"):
            rr.validate_meta(meta(base_sha="B" * 40))

    def test_pr_must_be_a_positive_integer(self):
        for bad in (0, -1, "7", True, 7.0):
            with self.assertRaisesRegex(rr.ResolveError, "pr"):
                rr.validate_meta(meta(pr=bad))

    def test_unknown_image_state_is_rejected(self):
        with self.assertRaisesRegex(rr.ResolveError, "image"):
            rr.validate_meta(meta(image="maybe"))

    def test_unknown_skip_reason_is_rejected(self):
        with self.assertRaisesRegex(rr.ResolveError, "skipped"):
            rr.validate_meta(meta(skipped="because", image=None))

    def test_a_run_that_was_not_skipped_needs_an_image_state(self):
        with self.assertRaisesRegex(rr.ResolveError, "image"):
            rr.validate_meta(meta(image=None))


class AssociationTest(unittest.TestCase):
    def check(self, run_doc=None, pull_doc=None, meta_doc=None):
        rr.check_association(run_doc or run(), pull_doc or pull(), meta_doc or meta(), REPO)

    def test_a_matching_run_passes(self):
        self.check()

    def test_the_run_must_come_from_ortsom_pr_on_pull_request(self):
        with self.assertRaisesRegex(rr.ResolveError, "event"):
            self.check(run_doc=run(event="push"))
        with self.assertRaisesRegex(rr.ResolveError, "workflow"):
            self.check(run_doc=run(path=".github/workflows/ci.yml"))

    def test_the_pull_request_must_target_main_here(self):
        with self.assertRaisesRegex(rr.ResolveError, "base"):
            self.check(pull_doc=pull(base={"ref": "dev", "repo": {"full_name": REPO}}))
        with self.assertRaisesRegex(rr.ResolveError, "base"):
            self.check(pull_doc=pull(base={"ref": "main", "repo": {"full_name": "evil/mostro"}}))

    def test_head_repository_and_branch_must_match_the_run(self):
        other = pull(head={"sha": HEAD, "ref": "fix/thing", "repo": {"full_name": "mallory/mostro"}})
        with self.assertRaisesRegex(rr.ResolveError, "head repository"):
            self.check(pull_doc=other)
        other = pull(head={"sha": HEAD, "ref": "other", "repo": {"full_name": "alice/mostro"}})
        with self.assertRaisesRegex(rr.ResolveError, "head branch"):
            self.check(pull_doc=other)

    def test_a_newer_push_supersedes_the_run(self):
        newer = pull(head={"sha": "c" * 40, "ref": "fix/thing", "repo": {"full_name": "alice/mostro"}})
        with self.assertRaises(rr.Superseded):
            self.check(pull_doc=newer)

    def test_meta_head_must_match_the_run(self):
        with self.assertRaisesRegex(rr.ResolveError, "head_sha"):
            self.check(meta_doc=meta(head_sha="d" * 40))

    def test_a_closed_pull_request_is_not_run(self):
        with self.assertRaisesRegex(rr.ResolveError, "open"):
            self.check(pull_doc=pull(state="closed"))

    def test_a_deleted_fork_is_rejected(self):
        gone = pull(head={"sha": HEAD, "ref": "fix/thing", "repo": None})
        with self.assertRaisesRegex(rr.ResolveError, "head repository"):
            self.check(pull_doc=gone)


class OrtsomRefTest(unittest.TestCase):
    def test_no_line_means_no_override(self):
        self.assertEqual(rr.ortsom_ref_override("Fixes the release flow."), (None, None))

    def test_a_branch_name_is_accepted(self):
        self.assertEqual(rr.ortsom_ref_override("text\nortsom-ref: feat/new-action\nmore"), ("feat/new-action", None))

    def test_crlf_and_trailing_spaces_are_tolerated(self):
        self.assertEqual(rr.ortsom_ref_override("ortsom-ref: v0.4.0  \r\n"), ("v0.4.0", None))

    def test_a_ref_that_could_be_an_option_or_injection_is_refused(self):
        for bad in ("--upload-pack=x", "a;b", "$(id)", "a b", "../x", "x" * 101):
            ref, warning = rr.ortsom_ref_override(f"ortsom-ref: {bad}")
            self.assertIsNone(ref, bad)
            self.assertIn("ignored", warning)

    def test_the_line_inside_a_code_block_still_counts_as_written(self):
        # The author wrote it; § 8.2 does not distinguish, and a false
        # override only caps the verdict, never hides a regression.
        self.assertEqual(rr.ortsom_ref_override("```\nortsom-ref: x\n```")[0], "x")


class DecideTest(unittest.TestCase):
    """resolve's decision once the run, meta.json and the PR are fetched."""

    def decide(self, run_doc=None, meta_doc=None, pull_doc=None):
        return rr.decide(run_doc or run(), meta_doc or meta(), pull_doc or pull(), REPO, "v0.3.1")

    def test_a_built_run_proceeds_with_the_pinned_harness(self):
        out = self.decide()
        self.assertEqual((out["proceed"], out["ortsom_ref"], out["ortsom_ref_overridden"]), ("true", "v0.3.1", "false"))
        self.assertEqual(out["fork"], "true")

    def test_a_same_repository_head_is_not_a_fork(self):
        same = pull(head={"sha": HEAD, "ref": "fix/thing", "repo": {"full_name": REPO}})
        out = self.decide(run_doc=run(head_repository={"full_name": REPO}), pull_doc=same)
        self.assertEqual(out["fork"], "false")

    def test_a_skip_is_checked_against_its_pull_request_first(self):
        # A PR can upload a well-formed skip meta.json naming another PR.
        forged = meta(pr=99, skipped="label", image=None)
        victim = pull(number=99, head={"sha": "e" * 40, "ref": "other", "repo": {"full_name": "bob/mostro"}})
        with self.assertRaises(rr.ResolveError):
            self.decide(meta_doc=forged, pull_doc=victim)

    def test_an_authenticated_skip_does_not_proceed_and_names_its_pr(self):
        out = self.decide(meta_doc=meta(skipped="draft", image=None))
        self.assertEqual((out["proceed"], out["reason"], out["pr"]), ("false", "skipped-draft", 7))

    def test_a_pr_that_became_a_draft_since_the_build_is_skipped(self):
        out = self.decide(pull_doc=pull(draft=True))
        self.assertEqual((out["proceed"], out["reason"]), ("false", "skipped-draft"))

    def test_a_pr_that_got_ortsom_skip_since_the_build_is_skipped(self):
        out = self.decide(pull_doc=pull(labels=[{"name": "bug"}, {"name": "ortsom:skip"}]))
        self.assertEqual((out["proceed"], out["reason"]), ("false", "skipped-label"))

    def test_a_numeric_override_is_an_ortsom_pull_request(self):
        out = self.decide(pull_doc=pull(body="ortsom-ref: 123"))
        self.assertEqual((out["ortsom_ref"], out["ortsom_ref_overridden"]), ("refs/pull/123/head", "true"))

    def test_a_named_override_is_passed_as_is(self):
        out = self.decide(pull_doc=pull(body="ortsom-ref: feat/new-action"))
        self.assertEqual(out["ortsom_ref"], "feat/new-action")


class CollectChangesTest(unittest.TestCase):
    def fake_api(self, files, changed_files, head=HEAD):
        def api(path, params=""):
            if path.endswith("/files"):
                page = int(params.rsplit("&page=", 1)[1])
                return files[(page - 1) * rr.PER_PAGE: page * rr.PER_PAGE]
            return {"changed_files": changed_files, "head": {"sha": head}}
        return api

    def test_a_complete_list_becomes_name_status(self):
        files = [{"filename": "src/db.rs", "status": "modified"}]
        text = rr.collect_changes(self.fake_api(files, 1), REPO, 7, HEAD)
        self.assertEqual(text, "M\tsrc/db.rs\n")

    def test_a_list_cut_at_the_api_ceiling_is_refused(self):
        # GitHub lists 3000 files at most; the 3001st (src/db.rs here) would
        # make the selection `full`, the visible prefix makes it `none`.
        files = [{"filename": f"docs/generated/{i}.md", "status": "added"} for i in range(3000)]
        files.append({"filename": "src/db.rs", "status": "modified"})
        with self.assertRaisesRegex(rr.ResolveError, "3001"):
            rr.collect_changes(self.fake_api(files, 3001), REPO, 7, HEAD)

    def test_a_head_that_moved_is_refused(self):
        files = [{"filename": "src/db.rs", "status": "modified"}]
        with self.assertRaises(rr.Superseded):
            rr.collect_changes(self.fake_api(files, 1, head="f" * 40), REPO, 7, HEAD)


class ChangesTest(unittest.TestCase):
    def test_api_files_become_name_status_lines(self):
        files = [
            {"filename": "src/a.rs", "status": "modified"},
            {"filename": "src/b.rs", "status": "added"},
            {"filename": "src/c.rs", "status": "removed"},
            {"filename": "src/new.rs", "status": "renamed", "previous_filename": "src/old.rs"},
            {"filename": "src/copy.rs", "status": "copied", "previous_filename": "src/a.rs"},
            {"filename": "src/d.rs", "status": "changed"},
        ]
        self.assertEqual(rr.name_status(files), (
            "M\tsrc/a.rs\nA\tsrc/b.rs\nD\tsrc/c.rs\nR100\tsrc/old.rs\tsrc/new.rs\n"
            "C100\tsrc/a.rs\tsrc/copy.rs\nM\tsrc/d.rs\n"
        ))

    def test_a_tab_or_newline_in_a_file_name_is_refused(self):
        with self.assertRaises(rr.ResolveError):
            rr.name_status([{"filename": "src/a\tb.rs", "status": "modified"}])
        with self.assertRaises(rr.ResolveError):
            rr.name_status([{"filename": "src/a\nM\tsrc/db.rs", "status": "modified"}])


if __name__ == "__main__":
    unittest.main()
