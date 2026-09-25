"""Unit tests for verdict.py (docs/ORTSOM_PR_E2E_SPEC.md § 7, § 8, § 9.1)."""

import io
import json
import sys
import tempfile
import unittest
import urllib.error
import zipfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

import verdict as vd  # noqa: E402

REPO = "MostroP2P/mostro"
HEAD = "a" * 40
BASE = "b" * 40
SETTINGS = {
    "ortsom_ref": "v0.3.1",
    "min_scenarios": 4,
    "close_ratio": 0.5,
    "baseline_max_distance": 10,
    "stability_window": 3,
}
FOUR = ["s1", "s2", "s3", "s4"]


def result(name, outcome="passed", detail="", flaky=False):
    return {"name": name, "outcome": outcome, "detail": detail, "flaky": flaky}


def summary(*results):
    return {"results": list(results)}


def passing(*names):
    return summary(*(result(n) for n in names))


def baseline(sha, summ, distance=0, ref="v0.3.1", stack="ok", run_id=1):
    return {
        "distance": distance,
        "run": {"id": run_id, "html_url": f"https://github.com/{REPO}/actions/runs/{run_id}"},
        "meta": {"sha": sha, "ortsom_ref": ref, "stack": stack},
        "summary": summ,
    }


def meta(**over):
    doc = {
        "pr": 7,
        "head_sha": HEAD,
        "base_sha": BASE,
        "ortsom_ref": "v0.3.1",
        "ortsom_ref_overridden": False,
        "registry_stale": False,
        "skipped": None,
        "stack": "ok",
        "suite_exit": 0,
    }
    doc.update(over)
    return doc


def selection(*names, mode="subset", **over):
    doc = {
        "mode": mode,
        "scenarios": list(names),
        "matched_rules": [],
        "uncovered_files": {},
        "unmapped_files": [],
        "ignored_files": [],
    }
    doc.update(over)
    return doc


class SanitizeTest(unittest.TestCase):
    def test_detail_is_one_line_without_backticks_in_a_code_span(self):
        self.assertEqual(vd.code("boom `rm`\nsecond line"), "`boom rm second line`")

    def test_detail_is_cut_to_200_characters(self):
        self.assertEqual(len(vd.code("x" * 500)), 200 + 2)

    def test_a_pipe_cannot_split_a_table_cell(self):
        self.assertEqual(vd.code("a|b"), "`a\\|b`")

    def test_an_empty_detail_is_a_dash(self):
        self.assertEqual(vd.code(""), "—")

    def test_a_comment_marker_is_inert_inside_a_code_span(self):
        self.assertTrue(vd.code(vd.MARKER).startswith("`"))


class GateModifiedTest(unittest.TestCase):
    def test_a_file_under_the_gate_directory_counts(self):
        self.assertTrue(vd.gate_modified([".github/ortsom/map.toml"]))

    def test_an_ortsom_workflow_counts(self):
        self.assertTrue(vd.gate_modified(["src/app/release.rs", ".github/workflows/ortsom-pr.yml"]))

    def test_other_workflows_do_not_count(self):
        self.assertFalse(vd.gate_modified([".github/workflows/rust.yml", "src/main.rs"]))


class ClassifyTest(unittest.TestCase):
    def classify(self, pr, base, history=None, names=("s1",)):
        history = history or [baseline(BASE, base)]
        return {r["name"]: r["class"] for r in vd.classify(list(names), pr, history)}

    def test_a_pass_on_both_sides_is_a_pass(self):
        self.assertEqual(self.classify(passing("s1"), passing("s1")), {"s1": "pass"})

    def test_a_failure_against_a_stable_pass_is_a_regression(self):
        pr = summary(result("s1", "failed", "timeout"))
        self.assertEqual(self.classify(pr, passing("s1")), {"s1": "regression"})

    def test_a_skip_in_the_pr_is_skipped_first(self):
        pr = summary(result("s1", "skipped"))
        self.assertEqual(self.classify(pr, summary()), {"s1": "skipped"})

    def test_a_scenario_absent_from_the_reference_has_no_baseline(self):
        pr = summary(result("s1", "failed"))
        self.assertEqual(self.classify(pr, summary()), {"s1": "no-baseline"})

    def test_a_scenario_not_passing_in_the_reference_is_unstable(self):
        pr = summary(result("s1", "failed"))
        self.assertEqual(self.classify(pr, summary(result("s1", "skipped"))), {"s1": "unstable-on-main"})

    def test_a_failure_anywhere_in_the_history_is_unstable(self):
        history = [
            baseline(BASE, passing("s1")),
            baseline("c" * 40, summary(result("s1", "failed")), distance=1),
        ]
        pr = summary(result("s1", "failed"))
        self.assertEqual(self.classify(pr, None, history), {"s1": "unstable-on-main"})

    def test_a_flaky_pass_in_the_history_is_unstable(self):
        history = [baseline(BASE, summary(result("s1", flaky=True)))]
        self.assertEqual(self.classify(passing("s1"), None, history), {"s1": "unstable-on-main"})

    def test_a_selected_scenario_missing_from_the_pr_run_is_missing(self):
        self.assertEqual(self.classify(summary(), passing("s1")), {"s1": "missing"})

    def test_a_flaky_pass_in_the_pr_is_still_a_pass_and_noted(self):
        rows = vd.classify(["s1"], summary(result("s1", flaky=True)), [baseline(BASE, passing("s1"))])
        self.assertEqual((rows[0]["class"], rows[0]["flaky"]), ("pass", True))


class PrecheckTest(unittest.TestCase):
    def precheck(self, m=None, sel=None, changed=(), summ=None):
        return vd.precheck(m or meta(), sel or selection(*FOUR), list(changed), summ)

    def test_a_complete_ok_run_needs_the_baseline(self):
        self.assertIsNone(self.precheck(summ=passing(*FOUR)))

    def test_a_daemon_that_did_not_start_needs_the_baseline(self):
        self.assertIsNone(self.precheck(meta(stack="daemon_failed", suite_exit=None)))

    def test_a_selection_error_is_inconclusive(self):
        self.assertEqual(self.precheck(meta(stack="selection_error")), ("inconclusive", "selection-error"))

    def test_mode_none_is_not_applicable(self):
        got = self.precheck(meta(stack="not_run"), selection(mode="none"))
        self.assertEqual(got, ("not-applicable", "no-selection"))

    def test_a_stale_registry_is_inconclusive(self):
        got = self.precheck(meta(registry_stale=True), summ=passing(*FOUR))
        self.assertEqual(got, ("inconclusive", "stale-registry"))

    def test_touching_the_gate_is_inconclusive(self):
        got = self.precheck(changed=[".github/ortsom/verdict.py"], summ=passing(*FOUR))
        self.assertEqual(got, ("inconclusive", "gate-modified"))

    def test_stack_failures_are_inconclusive_with_their_name(self):
        for stack, reason in [
            ("no_image", "no-image"),
            ("bad_image", "bad-image"),
            ("build_failed", "build-failed"),
            ("infra_failed", "infra-failed"),
            ("doctor_failed", "doctor-failed"),
        ]:
            with self.subTest(stack=stack):
                self.assertEqual(self.precheck(meta(stack=stack)), ("inconclusive", reason))

    def test_an_unknown_stack_is_inconclusive(self):
        self.assertEqual(self.precheck(meta(stack="weird")), ("inconclusive", "unknown-stack"))

    def test_an_ok_stack_without_a_summary_is_an_incomplete_run(self):
        self.assertEqual(self.precheck(summ=None), ("inconclusive", "incomplete-run"))

    def test_an_interrupted_scenario_is_an_incomplete_run(self):
        summ = summary(*(result(n) for n in FOUR[:3]), result("s4", "interrupted"))
        self.assertEqual(self.precheck(summ=summ), ("inconclusive", "incomplete-run"))


class JudgeTest(unittest.TestCase):
    def judge(self, pr, base=None, m=None, caps=(), names=FOUR, history=None):
        if history is None:
            history = [baseline(BASE, base if base is not None else passing(*names))]
        return vd.judge(m or meta(), list(names), pr, history, SETTINGS, list(caps))

    def failing(self, *names):
        return summary(*(result(n, "failed" if n in names else "passed") for n in FOUR))

    def test_no_reference_baseline_is_inconclusive(self):
        got = self.judge(passing(*FOUR), history=[])
        self.assertEqual((got["verdict"], got["reason"]), ("inconclusive", "no-baseline"))

    def test_every_eligible_scenario_passing_is_a_pass(self):
        got = self.judge(passing(*FOUR))
        self.assertEqual((got["verdict"], got["counts"]["eligible"]), ("pass", 4))

    def test_one_regression_below_the_ratio_is_a_regression(self):
        got = self.judge(self.failing("s1"))
        self.assertEqual((got["verdict"], got["counts"]["regression"]), ("regression", 1))

    def test_half_of_four_regressed_would_close(self):
        got = self.judge(self.failing("s1", "s2"))
        self.assertEqual((got["verdict"], got["reason"]), ("would-close", "ratio"))

    def test_below_the_minimum_eligible_never_would_close(self):
        pr = summary(result("s1", "failed"), result("s2", "failed"), result("s3"))
        got = self.judge(pr, names=["s1", "s2", "s3"])
        self.assertEqual(got["verdict"], "regression")

    def test_missing_scenarios_count_as_regressions(self):
        got = self.judge(passing("s1", "s2"))
        self.assertEqual((got["verdict"], got["counts"]["regression"]), ("would-close", 2))

    def test_unstable_scenarios_are_not_eligible(self):
        base = summary(result("s1", "failed"), result("s2", "failed"), result("s3"), result("s4"))
        got = self.judge(self.failing("s1", "s2"), base)
        self.assertEqual((got["verdict"], got["counts"]["eligible"]), ("pass", 2))

    def test_a_daemon_that_did_not_start_would_close_without_a_minimum(self):
        got = self.judge(None, m=meta(stack="daemon_failed", suite_exit=None), names=["s1"])
        self.assertEqual((got["verdict"], got["reason"]), ("would-close", "daemon-failed"))

    def test_expected_break_caps_would_close_at_regression(self):
        got = self.judge(self.failing("s1", "s2", "s3"), caps=["label"])
        self.assertEqual((got["verdict"], got["reason"]), ("regression", "capped"))

    def test_an_ortsom_override_caps_a_daemon_that_did_not_start(self):
        got = self.judge(None, m=meta(stack="daemon_failed"), caps=["override"])
        self.assertEqual(got["verdict"], "regression")


class FindBaselinesTest(unittest.TestCase):
    WALK = ["m0" * 20, "m1" * 20, "m2" * 20]

    def runs_and_loader(self, entries):
        runs, loaded = [], {}
        for i, (sha, created, m, summ) in enumerate(entries):
            runs.append({"id": i, "head_sha": sha, "created_at": created})
            loaded[i] = None if m is None else (m, summ)
        return runs, lambda run: loaded[run["id"]]

    def ok_meta(self, sha, **over):
        doc = {"sha": sha, "ortsom_ref": "v0.3.1", "stack": "ok"}
        doc.update(over)
        return doc

    def test_the_reference_is_the_newest_baseline_nearest_the_merge_base(self):
        w = self.WALK
        runs, load = self.runs_and_loader([
            (w[1], "2026-09-25T01:00:00Z", self.ok_meta(w[1]), passing("s1")),
            (w[0], "2026-09-25T02:00:00Z", self.ok_meta(w[0]), passing("s1")),
            (w[0], "2026-09-25T08:00:00Z", self.ok_meta(w[0]), passing("s1")),
        ])
        history = vd.find_baselines(w, runs, load, "v0.3.1", 3)
        self.assertEqual([(h["run"]["id"], h["distance"]) for h in history], [(2, 0), (1, 0), (0, 1)])

    def test_another_harness_or_a_failed_stack_is_passed_over(self):
        w = self.WALK
        runs, load = self.runs_and_loader([
            (w[0], "2026-09-25T03:00:00Z", self.ok_meta(w[0], ortsom_ref="v0.3.0"), passing("s1")),
            (w[0], "2026-09-25T02:00:00Z", self.ok_meta(w[0], stack="infra_failed"), None),
            (w[0], "2026-09-25T01:00:00Z", self.ok_meta(w[0]), None),
            (w[1], "2026-09-25T00:00:00Z", self.ok_meta(w[1]), passing("s1")),
        ])
        history = vd.find_baselines(w, runs, load, "v0.3.1", 3)
        self.assertEqual([h["run"]["id"] for h in history], [3])

    def test_the_window_bounds_the_history(self):
        w = self.WALK
        runs, load = self.runs_and_loader([
            (sha, f"2026-09-2{i}T00:00:00Z", self.ok_meta(sha), passing("s1")) for i, sha in enumerate(w)
        ])
        self.assertEqual(len(vd.find_baselines(w, runs, load, "v0.3.1", 2)), 2)

    def test_runs_of_commits_off_the_walk_are_ignored(self):
        runs, load = self.runs_and_loader([("zz" * 20, "2026-09-25T00:00:00Z", self.ok_meta("zz" * 20), passing("s1"))])
        self.assertEqual(vd.find_baselines(self.WALK, runs, load, "v0.3.1", 3), [])

    def test_an_expired_artifact_is_skipped(self):
        w = self.WALK
        runs, load = self.runs_and_loader([(w[0], "2026-09-25T00:00:00Z", None, None)])
        self.assertEqual(vd.find_baselines(w, runs, load, "v0.3.1", 3), [])


class WalkTest(unittest.TestCase):
    def test_the_walk_follows_first_parents_up_to_the_distance(self):
        parents = {"c0": ["c1", "x"], "c1": ["c2"], "c2": ["c3"], "c3": []}

        def get(path, params=""):
            return {"parents": [{"sha": p} for p in parents[path.rsplit("/", 1)[1]]]}

        self.assertEqual(vd.first_parent_walk(get, REPO, "c0", 2), ["c0", "c1", "c2"])
        self.assertEqual(vd.first_parent_walk(get, REPO, "c0", 10), ["c0", "c1", "c2", "c3"])


class BaselineZipTest(unittest.TestCase):
    def zipped(self, files):
        buf = io.BytesIO()
        with zipfile.ZipFile(buf, "w") as z:
            for name, doc in files.items():
                z.writestr(name, json.dumps(doc))
        return buf.getvalue()

    def test_meta_and_summary_are_read(self):
        data = self.zipped({"meta.json": {"sha": BASE}, "summary.json": passing("s1"), "suite.log": "x"})
        self.assertEqual(vd.read_baseline_zip(data), ({"sha": BASE}, passing("s1")))

    def test_a_missing_summary_is_none(self):
        self.assertEqual(vd.read_baseline_zip(self.zipped({"meta.json": {"sha": BASE}})), ({"sha": BASE}, None))

    def test_no_meta_is_no_baseline(self):
        self.assertIsNone(vd.read_baseline_zip(self.zipped({"summary.json": {}})))

    def test_a_broken_zip_is_no_baseline(self):
        self.assertIsNone(vd.read_baseline_zip(b"not a zip"))


BLIND_MAP = {
    "settings": SETTINGS,
    "rule": [
        {"name": "bonds", "paths": ["src/app/bond/**"], "tags": ["bond"]},
        {"name": "rating", "paths": ["src/app/rate_user.rs"], "scenarios": ["s1"]},
        {"name": "scheduler", "paths": ["src/scheduler.rs"], "full": True},
        {"name": "rpc", "paths": ["src/rpc/**"], "uncovered": "no gRPC client"},
    ],
}
BLIND_REGISTRY = [
    {"name": "s1", "tags": ["smoke"]},
    {"name": "bond_a", "tags": ["bond"]},
    {"name": "bond_b", "tags": ["bond"]},
]


class BlindSpotTest(unittest.TestCase):
    def row(self, name, cls):
        return {"name": name, "class": cls, "pr": None, "baseline": None, "detail": "", "flaky": False}

    def spots(self, rows, matched, changed):
        sel = selection(*[r["name"] for r in rows], matched_rules=matched)
        return vd.blind_spots(rows, sel, changed, BLIND_MAP, BLIND_REGISTRY)

    def test_a_rule_whose_scenarios_were_all_skipped_is_a_blind_spot(self):
        rows = [self.row("s1", "pass"), self.row("bond_a", "skipped"), self.row("bond_b", "skipped")]
        got = self.spots(rows, ["bonds", "scheduler"], ["src/app/bond/flow.rs", "src/scheduler.rs"])
        self.assertEqual(got, [{
            "rule": "bonds",
            "files": ["src/app/bond/flow.rs"],
            "scenarios": {"skipped": ["bond_a", "bond_b"]},
        }])

    def test_one_compared_scenario_is_enough(self):
        rows = [self.row("bond_a", "regression"), self.row("bond_b", "skipped")]
        self.assertEqual(self.spots(rows, ["bonds"], ["src/app/bond/flow.rs"]), [])

    def test_unstable_and_new_scenarios_do_not_count_as_compared(self):
        rows = [self.row("bond_a", "unstable-on-main"), self.row("bond_b", "no-baseline")]
        got = self.spots(rows, ["bonds"], ["src/app/bond/db.rs"])
        self.assertEqual(got[0]["scenarios"], {"no-baseline": ["bond_b"], "unstable-on-main": ["bond_a"]})

    def test_full_uncovered_and_unmatched_rules_are_not_blind_spots(self):
        rows = [self.row("s1", "skipped")]
        self.assertEqual(self.spots(rows, ["scheduler", "rpc"], ["src/scheduler.rs", "src/rpc/a.rs"]), [])

    def test_an_unjudged_run_has_no_blind_spots(self):
        # No baseline or a daemon that did not start: no scenario was
        # classified, so none is "not compared" (#988 review).
        self.assertEqual(self.spots([], ["bonds"], ["src/app/bond/flow.rs"]), [])

    def test_without_the_map_there_are_none(self):
        self.assertEqual(vd.blind_spots([self.row("s1", "skipped")], selection("s1", matched_rules=["rating"]),
                                        ["src/app/rate_user.rs"], None, None), [])


class CommentTest(unittest.TestCase):
    CTX = {
        "pr": 7,
        "run_url": f"https://github.com/{REPO}/actions/runs/99",
        "settings": SETTINGS,
    }

    def render(self, outcome, m=None, sel=None):
        return vd.render_comment(outcome, m or meta(), sel or selection(*FOUR), self.CTX)

    def outcome(self, verdict, reason, rows=(), history=None, caps=()):
        counts = vd.count(list(rows))
        return {"verdict": verdict, "reason": reason, "rows": list(rows), "counts": counts,
                "history": history or [baseline(BASE, passing(*FOUR), distance=2, run_id=5)], "caps": list(caps)}

    def row(self, name, cls, pr="passed", base="passed", detail=""):
        return {"name": name, "class": cls, "pr": pr, "baseline": base, "detail": detail, "flaky": False}

    def test_the_marker_opens_the_comment(self):
        self.assertTrue(self.render(self.outcome("pass", "ok")).startswith(vd.MARKER + "\n"))

    def test_would_close_states_shadow_mode_and_the_numbers(self):
        rows = [self.row("s1", "regression", "failed"), self.row("s2", "regression", "failed"),
                self.row("s3", "pass"), self.row("s4", "pass")]
        body = self.render(self.outcome("would-close", "ratio", rows))
        self.assertIn("2 of 4 eligible scenarios regressed (50%, threshold 50%, minimum 4)", body)
        self.assertIn("Shadow mode: this pull request would have been closed. It stays open; "
                      "a maintainer reviews this verdict.", body)

    def test_a_pass_has_no_shadow_line(self):
        self.assertNotIn("Shadow mode", self.render(self.outcome("pass", "ok")))

    def test_the_regression_table_shows_the_sanitized_detail(self):
        rows = [self.row("s1", "regression", "failed", detail="`@x` <b>|</b>\nmore")]
        body = self.render(self.outcome("regression", "ok", rows))
        self.assertIn("| `s1` | failed: `@x <b>\\|</b> more` | passed |", body)

    def test_an_unknown_outcome_is_shown_as_untrusted_text(self):
        rows = [self.row("s1", "regression", "[x](http://e) | y", detail="d")]
        self.assertIn("| `s1` | `[x](http://e) \\| y`: `d` | passed |",
                      self.render(self.outcome("regression", "ok", rows)))

    def test_a_missing_scenario_is_listed_as_not_run(self):
        rows = [self.row("s1", "missing", None)]
        self.assertIn("| `s1` | not run |", self.render(self.outcome("regression", "ok", rows)))

    def test_the_baseline_names_commit_distance_and_run(self):
        body = self.render(self.outcome("pass", "ok"))
        self.assertIn(f"`{BASE[:12]}`, 2 commits before the merge-base", body)
        self.assertIn(f"https://github.com/{REPO}/actions/runs/5", body)

    def test_file_names_are_code_spans(self):
        sel = selection(*FOUR, uncovered_files={"src/rpc/a.rs": "no gRPC client"}, unmapped_files=["x`y.rs"])
        body = self.render(self.outcome("pass", "ok"), sel=sel)
        self.assertIn("`src/rpc/a.rs`: no gRPC client", body)
        self.assertIn("`xy.rs`", body)

    def test_build_failed_says_it_does_not_compile(self):
        body = self.render(self.outcome("inconclusive", "build-failed", history=[]), meta(stack="build_failed"))
        self.assertIn("does not compile", body)

    def test_no_baseline_explains_the_harness_bump(self):
        body = self.render(self.outcome("inconclusive", "no-baseline", history=[]))
        self.assertIn("rebases onto a `main` whose new-harness baseline has finished", body)

    def test_a_blind_spot_is_a_warning_before_the_details(self):
        out = self.outcome("pass", "ok")
        out["blind_spots"] = [{"rule": "bonds", "files": ["src/app/bond/flow.rs", "src/app/bond/db.rs"],
                               "scenarios": {"skipped": ["bond_a", "bond_b"]}}]
        body = self.render(out)
        warning = ("> ⚠️ No scenario of the `bonds` rule was compared (skipped: `bond_a`, `bond_b`), "
                   "so this verdict says nothing about `src/app/bond/flow.rs`, `src/app/bond/db.rs`.")
        self.assertIn(warning, body)
        self.assertLess(body.index(warning), body.index("Daemon `"))

    def test_a_cap_is_named(self):
        rows = [self.row(n, "regression", "failed") for n in FOUR]
        body = self.render(self.outcome("regression", "capped", rows, caps=["label", "override"]))
        self.assertIn("`ortsom:expected-break`", body)
        self.assertIn("`ortsom-ref:`", body)


class FakeGitHub:
    """Records writes; serves a pull request and its comments."""

    def __init__(self, labels=(), comments=(), head=HEAD, draft=False):
        self.pull = {
            "number": 7,
            "state": "open",
            "draft": draft,
            "head": {"sha": head},
            "user": {"login": "alice"},
            "labels": [{"name": n} for n in labels],
        }
        self.comments = list(comments)
        self.calls = []

    def get(self, path, params=""):
        if path.endswith("/pulls/7"):
            return self.pull
        if path.endswith("/issues/7/comments"):
            return self.comments if "page=1" in params else []
        raise AssertionError(path)

    def send(self, method, path, body=None):
        if method == "DELETE" and path.endswith("/labels/ortsom%3Amissing"):
            raise urllib.error.HTTPError(path, 404, "Not Found", {}, None)
        self.calls.append((method, path, body))
        return {}


def bot_comment(cid, body):
    return {"id": cid, "user": {"login": vd.BOT}, "body": body}


class ApplyTest(unittest.TestCase):
    def test_one_verdict_label_replaces_the_others_and_ortsom_run_goes(self):
        gh = FakeGitHub(labels=["ortsom:would-close", "ortsom:run", "quality:ok"])
        vd.apply(gh.get, gh.send, REPO, 7, "ortsom:pass", "body", create=True)
        self.assertIn(("DELETE", f"/repos/{REPO}/issues/7/labels/ortsom%3Awould-close", None), gh.calls)
        self.assertIn(("DELETE", f"/repos/{REPO}/issues/7/labels/ortsom%3Arun", None), gh.calls)
        self.assertIn(("POST", f"/repos/{REPO}/issues/7/labels", {"labels": ["ortsom:pass"]}), gh.calls)
        self.assertNotIn("quality", json.dumps(gh.calls))

    def test_the_sticky_comment_is_edited_in_place(self):
        gh = FakeGitHub(comments=[bot_comment(11, vd.MARKER + "\nold")])
        vd.apply(gh.get, gh.send, REPO, 7, "ortsom:pass", "new", create=True)
        self.assertIn(("PATCH", f"/repos/{REPO}/issues/comments/11", {"body": "new"}), gh.calls)

    def test_a_marker_in_someone_elses_comment_is_ignored(self):
        forged = {"id": 12, "user": {"login": "mallory"}, "body": vd.MARKER}
        gh = FakeGitHub(comments=[forged])
        vd.apply(gh.get, gh.send, REPO, 7, "ortsom:pass", "new", create=True)
        self.assertIn(("POST", f"/repos/{REPO}/issues/7/comments", {"body": "new"}), gh.calls)

    def test_without_create_no_new_comment_appears(self):
        gh = FakeGitHub(labels=["ortsom:pass"])
        vd.apply(gh.get, gh.send, REPO, 7, None, "skipped", create=False)
        self.assertEqual([c[0] for c in gh.calls], ["DELETE"])

    def test_a_label_already_gone_is_not_an_error(self):
        gh = FakeGitHub(labels=["ortsom:missing"])
        vd.remove_label(gh.send, REPO, 7, "ortsom:missing")


class RunTest(unittest.TestCase):
    """The whole job against a fake GitHub and a result directory."""

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.dir = Path(self.tmp.name)

    def write_result(self, m, sel=None, summ=None, changes="M\tsrc/app/release.rs\n"):
        res = self.dir / "result"
        res.mkdir(exist_ok=True)
        (res / "meta.json").write_text(json.dumps(m))
        if sel is not None:
            (res / "selection.json").write_text(json.dumps(sel))
        if summ is not None:
            (res / "summary.json").write_text(json.dumps(summ))
        (res / "changes.txt").write_text(changes)
        return res

    def run_job(self, gh, reason="ok", history=(), res=None, **over):
        args = {
            "pr": 7, "reason": reason, "head_sha": HEAD, "result": res, "out": self.dir / "out",
            "run_url": f"https://github.com/{REPO}/actions/runs/99",
        }
        args.update(over)
        return vd.run(args, REPO, gh.get, gh.send, lambda _m: list(history), SETTINGS)

    def test_a_passing_run_labels_pass_and_writes_the_comment(self):
        res = self.write_result(meta(), selection(*FOUR), passing(*FOUR))
        gh = FakeGitHub()
        out = self.run_job(gh, history=[baseline(BASE, passing(*FOUR))], res=res)
        self.assertEqual(out["verdict"], "pass")
        self.assertIn(("POST", f"/repos/{REPO}/issues/7/labels", {"labels": ["ortsom:pass"]}), gh.calls)
        self.assertEqual(json.loads((self.dir / "out" / "verdict.json").read_text())["verdict"], "pass")
        self.assertTrue((self.dir / "out" / "comment.md").read_text().startswith(vd.MARKER))

    def test_a_newer_head_discards_the_result_untouched(self):
        res = self.write_result(meta(), selection(*FOUR), passing(*FOUR))
        gh = FakeGitHub(head="c" * 40)
        self.assertIsNone(self.run_job(gh, res=res))
        self.assertEqual(gh.calls, [])

    def test_a_pr_that_became_a_draft_is_handled_as_a_skip(self):
        res = self.write_result(meta(), selection(*FOUR), passing(*FOUR))
        gh = FakeGitHub(labels=["ortsom:pass"], draft=True, comments=[bot_comment(11, vd.MARKER)])
        self.run_job(gh, res=res)
        self.assertIn(("DELETE", f"/repos/{REPO}/issues/7/labels/ortsom%3Apass", None), gh.calls)
        patch = [c for c in gh.calls if c[0] == "PATCH"][0]
        self.assertIn("draft", patch[2]["body"])
        self.assertNotIn("POST", [c[0] for c in gh.calls])

    def test_an_authenticated_skip_clears_labels_without_a_new_comment(self):
        gh = FakeGitHub(labels=["ortsom:would-close", "ortsom:run"])
        self.run_job(gh, reason="skipped-label")
        self.assertEqual({c[0] for c in gh.calls}, {"DELETE"})

    def test_a_run_cancelled_by_hand_rewrites_the_comment(self):
        gh = FakeGitHub(labels=["ortsom:regression"], comments=[bot_comment(11, vd.MARKER)])
        self.run_job(gh, reason="cancelled", head_sha=None)
        patch = [c for c in gh.calls if c[0] == "PATCH"][0]
        self.assertIn("cancelled", patch[2]["body"])
        self.assertIn("no verdict is current", patch[2]["body"])

    def test_not_applicable_removes_labels_and_creates_no_comment(self):
        res = self.write_result(meta(stack="not_run", suite_exit=None), selection(mode="none"))
        gh = FakeGitHub(labels=["ortsom:inconclusive"])
        out = self.run_job(gh, res=res)
        self.assertEqual(out["verdict"], "not-applicable")
        self.assertEqual({c[0] for c in gh.calls}, {"DELETE"})

    def test_a_superseded_suite_touches_nothing(self):
        res = self.write_result(meta(stack="superseded", suite_exit=None))
        gh = FakeGitHub()
        self.assertIsNone(self.run_job(gh, res=res))
        self.assertEqual(gh.calls, [])

    def test_a_pass_that_skipped_the_changed_code_says_so(self):
        pr = summary(result("s1"), result("bond_a", "skipped"), result("bond_b", "skipped"))
        base = summary(result("s1"), result("bond_a", "skipped"), result("bond_b", "skipped"))
        sel = selection("s1", "bond_a", "bond_b", matched_rules=["bonds"])
        res = self.write_result(meta(), sel, pr, changes="M\tsrc/app/bond/flow.rs\n")
        gh = FakeGitHub()
        out = vd.run(
            {"pr": 7, "reason": "ok", "head_sha": HEAD, "result": res, "out": self.dir / "out",
             "run_url": "https://example/run"},
            REPO, gh.get, gh.send, lambda _m: [baseline(BASE, base)], SETTINGS,
            gate_map=BLIND_MAP, registry=BLIND_REGISTRY,
        )
        self.assertEqual(out["verdict"], "pass")
        self.assertEqual([b["rule"] for b in out["blind_spots"]], ["bonds"])
        verdict = json.loads((self.dir / "out" / "verdict.json").read_text())
        self.assertEqual(verdict["blind_spots"][0]["files"], ["src/app/bond/flow.rs"])
        self.assertIn("says nothing about `src/app/bond/flow.rs`", (self.dir / "out" / "comment.md").read_text())

    def test_a_daemon_that_did_not_start_reports_no_blind_spot(self):
        sel = selection("s1", "bond_a", "bond_b", matched_rules=["bonds"])
        res = self.write_result(meta(stack="daemon_failed", suite_exit=None), sel,
                                changes="M\tsrc/app/bond/flow.rs\n")
        out = vd.run(
            {"pr": 7, "reason": "ok", "head_sha": HEAD, "result": res, "out": self.dir / "out",
             "run_url": "https://example/run"},
            REPO, FakeGitHub().get, FakeGitHub().send, lambda _m: [baseline(BASE, passing("s1"))], SETTINGS,
            gate_map=BLIND_MAP, registry=BLIND_REGISTRY,
        )
        self.assertEqual((out["verdict"], out["blind_spots"]), ("would-close", []))

    def test_the_expected_break_label_caps_the_verdict(self):
        pr = summary(*(result(n, "failed") for n in FOUR))
        res = self.write_result(meta(), selection(*FOUR), pr)
        gh = FakeGitHub(labels=["ortsom:expected-break"])
        out = self.run_job(gh, history=[baseline(BASE, passing(*FOUR))], res=res)
        self.assertEqual((out["verdict"], out["reason"]), ("regression", "capped"))

    def test_a_result_for_another_head_is_refused(self):
        res = self.write_result(meta(head_sha="d" * 40), selection(*FOUR), passing(*FOUR))
        with self.assertRaises(vd.VerdictError):
            self.run_job(FakeGitHub(), res=res)


if __name__ == "__main__":
    unittest.main()
