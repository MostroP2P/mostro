"""Run the Ortsom settings variants on a stack that is already up
(docs/ORTSOM_PR_E2E_SPEC.md § 4.5). Standard library only.

    run_variants.py (--selection selection.json | --all) --image IMAGE
                    [--with-bonds] --ortsom-dir ortsom --out DIR

For each variant, in order: cancel the orders the previous daemon left,
restart mostrod with the variant's overlay (`ortsom stack restart-daemon`),
run `ortsom doctor`, then run the variant's scenarios. Writes
DIR/variants.json, `{name: {stack, overlay, overlay_sha256, scenarios,
suite_exit}}`, and DIR/<name>/summary.json for every variant whose suite
finished. `stack` is `ok`, `daemon_failed` (restart-daemon exit 4: mostrod
refused the settings), `infra_failed` or `doctor_failed`, as for the main
stack. The ortsom command comes from $ORTSOM, as in the workflows.

Always exits 0 once it has written variants.json: a variant that failed is
a result for the verdict, not an error of this script.
"""

import argparse
import json
import os
import shlex
import shutil
import subprocess
import sys
import tomllib
from pathlib import Path

import select_scenarios

GATE_DIR = Path(__file__).resolve().parent
RESTART_DAEMON_FAILED = 4
# One variant runs a handful of scenarios in about two minutes. These caps
# keep one hung variant from using the step's budget (60 min for five).
SUITE_TIMEOUT_SECS = 15 * 60
STEP_TIMEOUT_SECS = 5 * 60


def variants_to_run(selection=None, gate_map=None, digest=select_scenarios.overlay_digest):
    """The variants a selection.json picked, or every variant of the map."""
    if selection is not None:
        return list(selection.get("variants") or [])
    select_scenarios.validate_variants(gate_map.get("variant", []))
    return [
        {"name": v["name"], "overlay": v["overlay"], "sha256": digest(v["overlay"]), "scenarios": v["scenarios"]}
        for v in gate_map.get("variant", [])
    ]


def stack_state(restart_exit):
    if restart_exit == 0:
        return "ok"
    if restart_exit == RESTART_DAEMON_FAILED:
        return "daemon_failed"
    return "infra_failed"


def run_variant(variant, ortsom, image, with_bonds, gate_dir, run, summaries, daemon_up=True):
    """One variant. `run(argv, timeout)` returns the exit code, or None on a
    timeout; `summaries()` lists the summary.json files Ortsom has written.
    `daemon_up` says whether the daemon before this one is running.
    Returns (record, path of this suite's summary.json or None)."""
    record = {"stack": None, "overlay": variant["overlay"], "overlay_sha256": variant["sha256"],
              "scenarios": variant["scenarios"], "suite_exit": None}
    # The previous daemon's orders die with its database; this only frees
    # what it still holds on the nodes. Best effort, and pointless against
    # a daemon that never started.
    if daemon_up:
        run(ortsom + ["cleanup"], STEP_TIMEOUT_SECS)
    restart = ortsom + ["stack", "restart-daemon", "--mostro-image", image]
    if with_bonds:
        restart.append("--with-bonds")
    restart += ["--settings-overlay", str(gate_dir / variant["overlay"])]
    record["stack"] = stack_state(run(restart, STEP_TIMEOUT_SECS))
    if record["stack"] != "ok":
        return record, None
    if run(ortsom + ["doctor"], STEP_TIMEOUT_SECS) != 0:
        record["stack"] = "doctor_failed"
        return record, None
    before = summaries()
    record["suite_exit"] = run(ortsom + ["run", "--jobs", "1", *variant["scenarios"]], SUITE_TIMEOUT_SECS)
    new = sorted(set(summaries()) - set(before))
    return record, (new[-1] if new else None)


def run_all(variants, ortsom, image, with_bonds, gate_dir, out, run, summaries):
    out.mkdir(parents=True, exist_ok=True)
    records = {}
    write_records(out, records)
    daemon_up = True
    for variant in variants:
        print(f"::group::variant {variant['name']}", flush=True)
        record, summary = run_variant(variant, ortsom, image, with_bonds, gate_dir, run, summaries, daemon_up)
        daemon_up = record["stack"] in ("ok", "doctor_failed")
        if summary is not None:
            (out / variant["name"]).mkdir(exist_ok=True)
            shutil.copyfile(summary, out / variant["name"] / "summary.json")
        records[variant["name"]] = record
        # After each variant: a step killed later keeps what already ran.
        write_records(out, records)
        print(f"variant {variant['name']}: stack {record['stack']}, suite exit {record['suite_exit']}", flush=True)
        print("::endgroup::", flush=True)
    return records


def write_records(out, records):
    tmp = out / "variants.json.tmp"
    tmp.write_text(json.dumps(records, indent=2) + "\n")
    tmp.replace(out / "variants.json")


def subprocess_runner(cwd):
    def run(argv, timeout):
        try:
            return subprocess.run(argv, cwd=cwd, timeout=timeout).returncode
        except subprocess.TimeoutExpired:
            print(f"::warning::timed out after {timeout}s: {shlex.join(argv)}", flush=True)
            return None
    return run


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    which = parser.add_mutually_exclusive_group(required=True)
    which.add_argument("--selection", type=Path)
    which.add_argument("--all", action="store_true")
    parser.add_argument("--image", required=True)
    parser.add_argument("--with-bonds", action="store_true")
    parser.add_argument("--ortsom-dir", type=Path, required=True)
    parser.add_argument("--out", type=Path, required=True)
    args = parser.parse_args(argv)

    if args.all:
        with open(GATE_DIR / "map.toml", "rb") as f:
            variants = variants_to_run(gate_map=tomllib.load(f))
    else:
        variants = variants_to_run(selection=json.loads(args.selection.read_text()))
    ortsom = shlex.split(os.environ["ORTSOM"])
    artifacts = args.ortsom_dir.resolve() / "artifacts"

    def summaries():
        return sorted(str(p) for p in artifacts.glob("*/summary.json"))

    run_all(variants, ortsom, args.image, args.with_bonds, GATE_DIR, args.out,
            subprocess_runner(args.ortsom_dir), summaries)
    return 0


if __name__ == "__main__":
    sys.exit(main())
