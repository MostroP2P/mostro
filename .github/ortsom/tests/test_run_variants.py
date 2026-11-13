"""Unit tests for run_variants.py (docs/ORTSOM_PR_E2E_SPEC.md § 4.5)."""

import json
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

import run_variants as rv  # noqa: E402

ORTSOM = ["ortsom", "--config", "ortsom.regtest.toml"]
PEGGED = {"name": "price-pegged-only", "overlay": "variants/price-pegged-only.toml",
          "sha256": "abc", "scenarios": ["market_price_order"]}
CLTV = {"name": "bond-hold-cltv-298", "overlay": "variants/bond-hold-cltv-298.toml",
        "sha256": "def", "scenarios": ["bond_released_on_success", "happy_sell"]}


class FakeOrtsom:
    """Exit codes per subcommand, and a summary.json written by every run."""

    def __init__(self, root, restart=0, doctor=0, suite=0):
        self.root, self.calls, self.written = Path(root), [], []
        self.codes = {"restart-daemon": restart, "doctor": doctor, "run": suite, "cleanup": 0}

    def run(self, argv, timeout):
        self.calls.append(argv)
        verb = "restart-daemon" if "restart-daemon" in argv else argv[len(ORTSOM)]
        code = self.codes[verb]
        if verb == "run" and code is not None:
            run_dir = self.root / f"run-{len(self.written)}"
            run_dir.mkdir()
            (run_dir / "summary.json").write_text(json.dumps({"results": [], "argv": argv}))
            self.written.append(str(run_dir / "summary.json"))
        return code

    def summaries(self):
        return list(self.written)

    def verbs(self):
        return [c[len(ORTSOM)] for c in self.calls]


class RunVariantsTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.out = Path(self.tmp.name) / "out"

    def run_all(self, fake, variants=(PEGGED,), with_bonds=True):
        return rv.run_all(list(variants), ORTSOM, "ortsom-pr/mostro:sha", with_bonds, Path("/gate"),
                          self.out, fake.run, fake.summaries)

    def test_restarts_with_the_overlay_then_runs_the_variants_scenarios(self):
        fake = FakeOrtsom(self.tmp.name)

        records = self.run_all(fake)

        restart = next(c for c in fake.calls if "restart-daemon" in c)
        self.assertEqual(restart, ORTSOM + [
            "stack", "restart-daemon", "--mostro-image", "ortsom-pr/mostro:sha", "--with-bonds",
            "--settings-overlay", "/gate/variants/price-pegged-only.toml"])
        self.assertIn(ORTSOM + ["run", "--jobs", "1", "market_price_order"], fake.calls)
        self.assertEqual(records["price-pegged-only"], {
            "stack": "ok", "overlay": "variants/price-pegged-only.toml", "overlay_sha256": "abc",
            "scenarios": ["market_price_order"], "suite_exit": 0})
        summary = json.loads((self.out / "price-pegged-only" / "summary.json").read_text())
        self.assertIn("market_price_order", summary["argv"])

    def test_without_bonds_the_flag_is_left_out(self):
        fake = FakeOrtsom(self.tmp.name)

        self.run_all(fake, with_bonds=False)

        restart = next(c for c in fake.calls if "restart-daemon" in c)
        self.assertNotIn("--with-bonds", restart)

    def test_a_daemon_that_refuses_the_settings_is_daemon_failed_and_runs_nothing(self):
        fake = FakeOrtsom(self.tmp.name, restart=4)

        records = self.run_all(fake)

        self.assertEqual(records["price-pegged-only"]["stack"], "daemon_failed")
        self.assertNotIn("run", fake.verbs())
        self.assertNotIn("doctor", fake.verbs())
        self.assertFalse((self.out / "price-pegged-only").exists())

    def test_any_other_restart_failure_is_infra(self):
        for code in (1, 3, None):
            with self.subTest(code=code):
                fake = FakeOrtsom(self.tmp.name, restart=code)
                self.assertEqual(self.run_all(fake)["price-pegged-only"]["stack"], "infra_failed")

    def test_a_failing_doctor_is_doctor_failed(self):
        fake = FakeOrtsom(self.tmp.name, doctor=1)

        self.assertEqual(self.run_all(fake)["price-pegged-only"]["stack"], "doctor_failed")
        self.assertNotIn("run", fake.verbs())

    def test_a_suite_that_timed_out_leaves_no_summary(self):
        fake = FakeOrtsom(self.tmp.name, suite=None)

        records = self.run_all(fake)

        self.assertEqual((records["price-pegged-only"]["stack"], records["price-pegged-only"]["suite_exit"]),
                         ("ok", None))
        self.assertFalse((self.out / "price-pegged-only" / "summary.json").exists())

    def test_each_variant_keeps_its_own_summary_and_all_are_recorded(self):
        fake = FakeOrtsom(self.tmp.name, suite=1)

        records = self.run_all(fake, variants=(PEGGED, CLTV))

        self.assertEqual(list(records), ["price-pegged-only", "bond-hold-cltv-298"])
        cltv = json.loads((self.out / "bond-hold-cltv-298" / "summary.json").read_text())
        self.assertIn("bond_released_on_success", cltv["argv"])
        written = json.loads((self.out / "variants.json").read_text())
        self.assertEqual(written, records)
        self.assertEqual(fake.verbs().count("cleanup"), 2, "orders freed before each restart")

    def test_no_cleanup_against_a_daemon_that_never_started(self):
        fake = FakeOrtsom(self.tmp.name, restart=4)

        self.run_all(fake, variants=(PEGGED, CLTV))

        self.assertEqual(fake.verbs().count("cleanup"), 1, "only before the first restart")

    def test_the_record_is_written_after_each_variant(self):
        seen = []
        fake = FakeOrtsom(self.tmp.name)
        original = fake.run

        def run(argv, timeout):
            if "restart-daemon" in argv and (self.out / "variants.json").exists():
                seen.append(json.loads((self.out / "variants.json").read_text()))
            return original(argv, timeout)

        rv.run_all([PEGGED, CLTV], ORTSOM, "img", True, Path("/gate"), self.out, run, fake.summaries)

        self.assertEqual([list(s) for s in seen], [[], ["price-pegged-only"]])

    def test_no_variant_writes_an_empty_record(self):
        records = self.run_all(FakeOrtsom(self.tmp.name), variants=())

        self.assertEqual(records, {})
        self.assertEqual(json.loads((self.out / "variants.json").read_text()), {})

    def test_the_selection_picks_the_variants_and_all_takes_the_maps(self):
        selection = {"variants": [PEGGED]}
        gate_map = {"variant": [{"name": "price-legacy", "overlay": "variants/price-legacy.toml",
                                 "why": "legacy", "paths": ["src/price/**"], "scenarios": ["market_price_order"]}]}

        self.assertEqual(rv.variants_to_run(selection=selection), [PEGGED])
        self.assertEqual(rv.variants_to_run(selection={}), [])
        self.assertEqual(rv.variants_to_run(gate_map=gate_map, digest=lambda rel: f"sha:{rel}"), [{
            "name": "price-legacy", "overlay": "variants/price-legacy.toml",
            "sha256": "sha:variants/price-legacy.toml", "scenarios": ["market_price_order"]}])


if __name__ == "__main__":
    unittest.main()
