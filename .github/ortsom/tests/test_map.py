"""Checks on the committed map.toml and scenarios.json
(docs/ORTSOM_PR_E2E_SPEC.md § 4.4): every daemon source file is mapped, and
every tag and scenario the map names exists in the pinned Ortsom.
"""

import json
import subprocess
import sys
import tomllib
import unittest
from pathlib import Path

GATE_DIR = Path(__file__).resolve().parents[1]
REPO_ROOT = GATE_DIR.parents[1]

sys.path.insert(0, str(GATE_DIR))

import money_path  # noqa: E402
import select_scenarios as sel  # noqa: E402

MONEY_OWNERS = {"@grunch", "@Catrya", "@arkanoider"}


def load_map():
    with open(GATE_DIR / "map.toml", "rb") as f:
        return tomllib.load(f)


def load_registry():
    return json.loads((GATE_DIR / "scenarios.json").read_text())


def tracked_daemon_files():
    out = subprocess.run(
        ["git", "ls-files", "--", "src", "proto"],
        cwd=REPO_ROOT, check=True, capture_output=True, text=True,
    ).stdout
    return [
        f for f in out.splitlines()
        if f.startswith("proto/") or (f.startswith("src/") and f.endswith(".rs"))
    ]


class MapTest(unittest.TestCase):
    def test_map_is_valid(self):
        sel.validate_map(load_map())

    def test_every_daemon_source_file_matches_a_rule(self):
        files = tracked_daemon_files()
        self.assertTrue(files, "git ls-files found no daemon sources")
        unmapped = sel.unmapped(files, load_map())
        self.assertEqual(
            unmapped, [],
            "add these files to a rule in .github/ortsom/map.toml "
            "(use `uncovered = \"<reason>\"` if Ortsom cannot exercise them)",
        )

    def test_scheduler_changes_run_the_full_suite(self):
        # src/scheduler.rs flushes every outgoing message and runs payment
        # retries, payout reconciliation, dev fee and bond payouts.
        registry = load_registry()
        result = sel.select(["src/scheduler.rs"], load_map(), registry)
        self.assertEqual(result["mode"], "full")

    def test_every_tag_and_scenario_exists_in_the_registry(self):
        sel.check_references(load_map(), load_registry())

    def test_settings_and_config_changes_run_the_settings_variants(self):
        # mostro#1104: the configuration code the stack's own template never
        # exercises runs under the variants' overlays.
        registry = load_registry()
        for changed in (["settings.tpl.toml"], ["src/config/types.rs"]):
            result = sel.select(changed, load_map(), registry)
            names = {v["name"] for v in result["variants"]}
            self.assertTrue({"price-pegged-only", "bond-hold-cltv-298"} <= names, changed)
            self.assertEqual((result["unmapped_files"], result["uncovered_files"]), ([], {}), changed)

    def test_every_overlay_file_is_a_variant_and_every_variant_has_one(self):
        named = {v["overlay"] for v in load_map().get("variant", [])}
        on_disk = {f"variants/{p.name}" for p in (GATE_DIR / "variants").glob("*")}
        self.assertEqual(named, on_disk)

    def test_codeowners_names_the_money_paths(self):
        globs = money_path.validate_globs(load_map())
        owners = {}
        for line in (REPO_ROOT / ".github" / "CODEOWNERS").read_text().splitlines():
            if line.strip() and not line.startswith("#"):
                pattern, *names = line.split()
                owners[pattern] = set(names)
        expected = {"/" + (g[:-2] if g.endswith("/**") else g): MONEY_OWNERS for g in globs}
        self.assertEqual(owners, expected, "CODEOWNERS and [money_path] paths must list the same paths")

    def test_registry_snapshot_is_well_formed(self):
        registry = load_registry()
        names = [s["name"] for s in registry]
        self.assertEqual(names, sorted(set(names)), "scenarios.json must be sorted and unique")
        for s in registry:
            self.assertIsInstance(s["tags"], list, s["name"])


if __name__ == "__main__":
    unittest.main()
