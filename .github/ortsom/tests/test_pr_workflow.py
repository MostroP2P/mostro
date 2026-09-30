"""Checks on the build job of ortsom-pr.yml (docs/ORTSOM_PR_E2E_SPEC.md § 6.1):
the selector and the Dockerfile come from base_sha, so a pull request
branched before the gate existed is still built (#945 was reported as not
compiling because its head had no .github/ortsom).

    python3 -m unittest discover -s .github/ortsom/tests
"""

import os
import re
import subprocess
import tempfile
import textwrap
import unittest
from pathlib import Path

GATE_DIR = Path(__file__).resolve().parents[1]
REPO_ROOT = GATE_DIR.parents[1]
WORKFLOW = REPO_ROOT / ".github" / "workflows" / "ortsom-pr.yml"

# No global config: the developer's signing or hooks must not leak in.
GIT_ENV = {
    **os.environ,
    "GIT_CONFIG_GLOBAL": os.devnull,
    "GIT_CONFIG_NOSYSTEM": "1",
    "GIT_AUTHOR_NAME": "t", "GIT_AUTHOR_EMAIL": "t@example.com",
    "GIT_COMMITTER_NAME": "t", "GIT_COMMITTER_EMAIL": "t@example.com",
}


def step_run(name):
    """The `run: |` script of the step called `name`, dedented."""
    lines = WORKFLOW.read_text().splitlines()
    starts = [i for i, line in enumerate(lines)
              if re.fullmatch(rf"\s*- name: {re.escape(name)}\s*", line)]
    if not starts:
        raise AssertionError(f"ortsom-pr.yml has no step named {name!r}")
    step_indent = len(lines[starts[0]]) - len(lines[starts[0]].lstrip())
    body = []
    in_run = False
    for line in lines[starts[0] + 1:]:
        indent = len(line) - len(line.lstrip())
        if line.strip() and indent <= step_indent:
            break
        if in_run:
            if line.strip() and indent <= step_indent + 2:
                break
            body.append(line)
        elif line.strip() == "run: |":
            in_run = True
    if not body:
        raise AssertionError(f"step {name!r} has no `run: |` block")
    return textwrap.dedent("\n".join(body))


def git(repo, *args):
    return subprocess.run(["git", *args], cwd=repo, env=GIT_ENV, check=True,
                          capture_output=True, text=True).stdout.strip()


class GateFromBase(unittest.TestCase):
    def test_gate_is_extracted_from_base_when_head_predates_it(self):
        with tempfile.TemporaryDirectory() as tmp:
            repo, runner_temp = Path(tmp, "repo"), Path(tmp, "runner")
            repo.mkdir()
            runner_temp.mkdir()
            git(repo, "init", "-q")
            (repo / "README.md").write_text("mostro\n")
            git(repo, "add", "-A")
            git(repo, "commit", "-q", "-m", "before the gate")
            head = git(repo, "rev-parse", "HEAD")
            gate = repo / ".github" / "ortsom"
            gate.mkdir(parents=True)
            for f in ("select_scenarios.py", "mostro.Dockerfile",
                      "mostro.Dockerfile.dockerignore"):
                (gate / f).write_text(f"# {f}\n")
            git(repo, "add", "-A")
            git(repo, "commit", "-q", "-m", "the gate lands")
            base = git(repo, "rev-parse", "HEAD")
            git(repo, "checkout", "-q", "--detach", head)
            self.assertFalse(gate.exists())

            github_env = runner_temp / "github_env"
            subprocess.run(
                ["bash", "-e", "-c", step_run("Gate from base")], cwd=repo,
                env={**GIT_ENV, "BASE_SHA": base, "HEAD_SHA": head,
                     "RUNNER_TEMP": str(runner_temp), "GITHUB_ENV": str(github_env)},
                check=True, capture_output=True, text=True,
            )

            exported = dict(line.split("=", 1)
                            for line in github_env.read_text().splitlines())
            extracted = Path(exported["GATE"])
            self.assertTrue((extracted / "select_scenarios.py").is_file())
            self.assertTrue((extracted / "mostro.Dockerfile").is_file())
            self.assertTrue((extracted / "mostro.Dockerfile.dockerignore").is_file())

    def test_select_and_build_use_the_base_gate(self):
        select = step_run("Select")
        build = step_run("Build the daemon image")
        self.assertIn('python3 "$GATE/select_scenarios.py"', select)
        self.assertIn('-f "$GATE/mostro.Dockerfile"', build)
        for script in (select, build):
            self.assertNotIn(".github/ortsom/", script)


if __name__ == "__main__":
    unittest.main()
