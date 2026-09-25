"""Unit tests for select_scenarios.py (docs/ORTSOM_PR_E2E_SPEC.md § 4).

Run from the repository root:

    python3 -m unittest discover -s .github/ortsom/tests
"""

import json
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

import select_scenarios as sel  # noqa: E402

REGISTRY = [
    {"name": "cancel_before_taken", "tags": ["smoke", "cancellation"]},
    {"name": "cooperative_cancel", "tags": ["cancellation"]},
    {"name": "dispute_by_buyer", "tags": ["dispute"]},
    {"name": "happy_buy", "tags": ["smoke", "happy-path"]},
    {"name": "happy_sell", "tags": ["smoke", "happy-path"]},
    {"name": "range_order", "tags": ["happy-path"]},
]

SMOKE = ["cancel_before_taken", "happy_buy", "happy_sell"]


def make_map(*rules, always_tags=("smoke",)):
    return {
        "settings": {"ortsom_ref": "v0.3.0", "always_tags": list(always_tags)},
        "rule": list(rules),
    }


DOCS = {"name": "docs", "paths": ["**/*.md", "docs/**"], "ignore": True}
TRADE = {"name": "trade flow", "paths": ["src/app/release.rs"], "tags": ["happy-path"]}
CANCEL = {"name": "cancellation", "paths": ["src/app/cancel.rs"], "tags": ["cancellation"]}
PRICING = {"name": "pricing", "paths": ["src/price/**"], "scenarios": ["range_order"]}
CORE = {"name": "core", "paths": ["src/db.rs", "migrations/**"], "full": True}
RPC = {"name": "admin RPC", "paths": ["src/rpc/**"], "uncovered": "no gRPC client"}


class GlobTest(unittest.TestCase):
    def test_single_star_stays_within_one_segment(self):
        self.assertTrue(sel.glob_match("src/app/admin_*.rs", "src/app/admin_settle.rs"))
        self.assertFalse(sel.glob_match("src/*.rs", "src/app/release.rs"))

    def test_double_star_matches_any_depth(self):
        self.assertTrue(sel.glob_match("docs/**", "docs/a/b/c.md"))
        self.assertTrue(sel.glob_match("src/app/bond/**", "src/app/bond/mod.rs"))

    def test_leading_double_star_matches_zero_segments(self):
        self.assertTrue(sel.glob_match("**/*.md", "README.md"))
        self.assertTrue(sel.glob_match("**/*.md", "docs/x/README.md"))

    def test_question_mark_is_one_char_but_not_slash(self):
        self.assertTrue(sel.glob_match("src/a?.rs", "src/ab.rs"))
        self.assertFalse(sel.glob_match("src/a?.rs", "src/a/.rs"))

    def test_other_characters_are_literal(self):
        self.assertTrue(sel.glob_match("Cargo.lock", "Cargo.lock"))
        self.assertFalse(sel.glob_match("Cargo.lock", "Cargo_lock"))
        self.assertFalse(sel.glob_match("src/[ab].rs", "src/a.rs"))

    def test_match_is_anchored_at_both_ends(self):
        self.assertFalse(sel.glob_match("src/db.rs", "src/db.rs.orig"))
        self.assertFalse(sel.glob_match("db.rs", "src/db.rs"))


class ParseNameStatusTest(unittest.TestCase):
    def test_keeps_both_names_of_a_rename(self):
        text = "M\tsrc/db.rs\nR087\tsrc/old.rs\tsrc/new.rs\nA\tdocs/x.md\nD\tsrc/gone.rs\n"
        self.assertEqual(
            sel.parse_name_status(text),
            ["docs/x.md", "src/db.rs", "src/gone.rs", "src/new.rs", "src/old.rs"],
        )

    def test_copy_counts_only_the_new_file(self):
        self.assertEqual(sel.parse_name_status("C100\tsrc/a.rs\tsrc/b.rs\n"), ["src/b.rs"])

    def test_ignores_blank_lines_and_deduplicates(self):
        self.assertEqual(sel.parse_name_status("\nM\ta.rs\n\nM\ta.rs\n"), ["a.rs"])

    def test_rejects_a_malformed_line(self):
        with self.assertRaises(sel.SelectionError):
            sel.parse_name_status("src/db.rs\n")


class SelectTest(unittest.TestCase):
    def run_select(self, files, *rules, always_tags=("smoke",)):
        return sel.select(files, make_map(*rules, always_tags=always_tags), REGISTRY)

    def test_files_of_the_gate_itself_are_listed(self):
        files = [".github/ortsom/verdict.py", ".github/workflows/ortsom-pr.yml",
                 ".github/workflows/rust.yml", "src/app/release.rs"]
        result = self.run_select(files, TRADE)
        self.assertEqual(result["gate_files"], [".github/ortsom/verdict.py", ".github/workflows/ortsom-pr.yml"])

    def test_no_gate_file_is_an_empty_list(self):
        self.assertEqual(self.run_select(["src/app/release.rs"], TRADE)["gate_files"], [])

    def test_a_gate_file_under_an_ignore_rule_is_still_listed(self):
        gate_docs = {"name": "docs", "paths": ["**/*.md"], "ignore": True}
        result = self.run_select([".github/ortsom/README.md"], gate_docs)
        self.assertEqual((result["mode"], result["gate_files"]), ("none", [".github/ortsom/README.md"]))

    def test_only_ignored_files_select_nothing(self):
        result = self.run_select(["README.md", "docs/a.md"], DOCS, TRADE)
        self.assertEqual(result["mode"], "none")
        self.assertEqual(result["scenarios"], [])
        self.assertEqual(result["ignored_files"], ["README.md", "docs/a.md"])

    def test_no_changed_files_select_nothing(self):
        self.assertEqual(self.run_select([], DOCS)["mode"], "none")

    def test_full_rule_selects_every_scenario(self):
        result = self.run_select(["src/db.rs", "src/app/release.rs"], CORE, TRADE)
        self.assertEqual(result["mode"], "full")
        self.assertEqual(result["scenarios"], sorted(s["name"] for s in REGISTRY))
        self.assertEqual(result["matched_rules"], ["core", "trade flow"])

    def test_subset_is_union_of_always_tags_rule_tags_and_names(self):
        result = self.run_select(["src/app/cancel.rs", "src/price/mod.rs"], CANCEL, PRICING)
        self.assertEqual(result["mode"], "subset")
        self.assertEqual(
            result["scenarios"],
            ["cancel_before_taken", "cooperative_cancel", "happy_buy", "happy_sell", "range_order"],
        )
        self.assertEqual(result["matched_rules"], ["cancellation", "pricing"])

    def test_ignored_files_do_not_hide_code_changes(self):
        result = self.run_select(["docs/a.md", "src/app/release.rs"], DOCS, TRADE)
        self.assertEqual(result["mode"], "subset")
        self.assertEqual(result["ignored_files"], ["docs/a.md"])
        self.assertEqual(result["scenarios"], ["cancel_before_taken", "happy_buy", "happy_sell", "range_order"])

    def test_uncovered_file_runs_smoke_and_keeps_its_reason(self):
        result = self.run_select(["src/rpc/service.rs"], RPC, TRADE)
        self.assertEqual(result["mode"], "subset")
        self.assertEqual(result["scenarios"], SMOKE)
        self.assertEqual(result["uncovered_files"], {"src/rpc/service.rs": "no gRPC client"})
        self.assertEqual(result["unmapped_files"], [])

    def test_unmapped_file_runs_smoke_and_is_reported(self):
        result = self.run_select(["src/new_module.rs"], TRADE)
        self.assertEqual(result["mode"], "subset")
        self.assertEqual(result["scenarios"], SMOKE)
        self.assertEqual(result["unmapped_files"], ["src/new_module.rs"])
        self.assertEqual(result["matched_rules"], [])

    def test_file_matched_by_a_selecting_rule_is_not_uncovered(self):
        both = {"name": "rpc tests", "paths": ["src/rpc/**"], "tags": ["dispute"]}
        result = self.run_select(["src/rpc/service.rs"], RPC, both)
        self.assertEqual(result["uncovered_files"], {})
        self.assertIn("dispute_by_buyer", result["scenarios"])

    def test_ignore_wins_over_other_rules(self):
        result = self.run_select(["docs/db.md"], DOCS, {"name": "all", "paths": ["**"], "full": True})
        self.assertEqual(result["mode"], "none")

    def test_unknown_tag_is_an_error(self):
        stale = {"name": "stale", "paths": ["src/x.rs"], "tags": ["no-such-tag"]}
        with self.assertRaisesRegex(sel.SelectionError, "no-such-tag"):
            self.run_select(["src/x.rs"], stale)

    def test_unknown_scenario_is_an_error(self):
        stale = {"name": "stale", "paths": ["src/x.rs"], "scenarios": ["renamed_away"]}
        with self.assertRaisesRegex(sel.SelectionError, "renamed_away"):
            self.run_select(["src/x.rs"], stale)

    def test_unknown_always_tag_is_an_error(self):
        with self.assertRaisesRegex(sel.SelectionError, "gone"):
            self.run_select(["src/x.rs"], TRADE, always_tags=("gone",))


class ValidateMapTest(unittest.TestCase):
    def test_accepts_every_rule_kind(self):
        sel.validate_map(make_map(DOCS, TRADE, PRICING, CORE, RPC))

    def test_rule_without_paths_is_rejected(self):
        with self.assertRaisesRegex(sel.SelectionError, "paths"):
            sel.validate_map(make_map({"name": "x", "tags": ["smoke"]}))

    def test_rule_with_two_kinds_is_rejected(self):
        bad = {"name": "x", "paths": ["a"], "full": True, "tags": ["smoke"]}
        with self.assertRaisesRegex(sel.SelectionError, "exactly one"):
            sel.validate_map(make_map(bad))

    def test_rule_with_no_kind_is_rejected(self):
        with self.assertRaisesRegex(sel.SelectionError, "exactly one"):
            sel.validate_map(make_map({"name": "x", "paths": ["a"]}))

    def test_unknown_key_is_rejected(self):
        bad = {"name": "x", "paths": ["a"], "tag": ["smoke"]}
        with self.assertRaisesRegex(sel.SelectionError, "tag"):
            sel.validate_map(make_map(bad))

    def test_duplicate_rule_names_are_rejected(self):
        with self.assertRaisesRegex(sel.SelectionError, "duplicate"):
            sel.validate_map(make_map(TRADE, dict(TRADE)))

    def test_non_table_settings_is_rejected(self):
        with self.assertRaisesRegex(sel.SelectionError, "settings"):
            sel.validate_map({"settings": "bad", "rule": [TRADE]})

    def test_non_list_rules_are_rejected(self):
        with self.assertRaisesRegex(sel.SelectionError, "rule"):
            sel.validate_map({"settings": {"ortsom_ref": "v0.3.0"}, "rule": "bad"})

    def test_non_table_rule_is_rejected(self):
        with self.assertRaisesRegex(sel.SelectionError, "rule"):
            sel.validate_map(make_map("bad"))

    def test_missing_ortsom_ref_is_rejected(self):
        with self.assertRaisesRegex(sel.SelectionError, "ortsom_ref"):
            sel.validate_map({"settings": {"always_tags": []}, "rule": [TRADE]})


class MainTest(unittest.TestCase):
    def test_writes_selection_json(self):
        with tempfile.TemporaryDirectory() as tmp:
            tmp = Path(tmp)
            (tmp / "map.toml").write_text(
                'rule = [{ name = "trade flow", paths = ["src/app/release.rs"], tags = ["happy-path"] }]\n'
                '[settings]\nortsom_ref = "v0.3.0"\nalways_tags = ["smoke"]\n'
            )
            (tmp / "scenarios.json").write_text(json.dumps(REGISTRY))
            (tmp / "changes.txt").write_text("M\tsrc/app/release.rs\n")
            code = sel.main([
                "--map", str(tmp / "map.toml"),
                "--scenarios", str(tmp / "scenarios.json"),
                "--changes", str(tmp / "changes.txt"),
                "--out", str(tmp / "selection.json"),
            ])
            self.assertEqual(code, 0)
            result = json.loads((tmp / "selection.json").read_text())
            self.assertEqual(result["mode"], "subset")
            self.assertEqual(result["matched_rules"], ["trade flow"])

    def test_malformed_settings_exits_2(self):
        with tempfile.TemporaryDirectory() as tmp:
            tmp = Path(tmp)
            (tmp / "map.toml").write_text('settings = "bad"\n')
            (tmp / "scenarios.json").write_text(json.dumps(REGISTRY))
            (tmp / "changes.txt").write_text("M\ta.rs\n")
            code = sel.main([
                "--map", str(tmp / "map.toml"),
                "--scenarios", str(tmp / "scenarios.json"),
                "--changes", str(tmp / "changes.txt"),
                "--out", str(tmp / "selection.json"),
            ])
            self.assertEqual(code, 2)

    def test_stale_map_exits_2_without_writing(self):
        with tempfile.TemporaryDirectory() as tmp:
            tmp = Path(tmp)
            (tmp / "map.toml").write_text(
                'rule = [{ name = "x", paths = ["a.rs"], tags = ["gone"] }]\n'
                '[settings]\nortsom_ref = "v0.3.0"\nalways_tags = []\n'
            )
            (tmp / "scenarios.json").write_text(json.dumps(REGISTRY))
            (tmp / "changes.txt").write_text("M\ta.rs\n")
            out = tmp / "selection.json"
            code = sel.main([
                "--map", str(tmp / "map.toml"),
                "--scenarios", str(tmp / "scenarios.json"),
                "--changes", str(tmp / "changes.txt"),
                "--out", str(out),
            ])
            self.assertEqual(code, 2)
            self.assertFalse(out.exists())


if __name__ == "__main__":
    unittest.main()
