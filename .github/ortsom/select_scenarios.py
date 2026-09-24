"""Choose the Ortsom scenarios a pull request runs from the files it changes.

Implements docs/ORTSOM_PR_E2E_SPEC.md § 4. Standard library only, so it runs
on any GitHub runner without installing anything:

    git diff --name-status BASE...HEAD > changes.txt
    python3 .github/ortsom/select_scenarios.py --changes changes.txt --out selection.json

Exit codes: 0 selection written, 2 the map or the change list is invalid (a
tag or scenario the pinned Ortsom does not have, a malformed rule). Nothing is
written on exit 2; the caller reports the message as an inconclusive verdict.

Named select_scenarios rather than select: a select.py next to the script
would shadow the standard library's select module, which subprocess imports.
"""

import argparse
import json
import re
import sys
import tomllib
from pathlib import Path

GATE_DIR = Path(__file__).resolve().parent

RULE_KEYS = {"name", "paths", "ignore", "full", "uncovered", "tags", "scenarios"}


class SelectionError(Exception):
    """The map, the registry or the change list cannot produce a selection."""


def glob_to_regex(pattern):
    """Translate a § 4.2 glob: `*` and `?` stay within one path segment,
    `**` spans any number of them (zero included), everything else is literal."""
    out = []
    i = 0
    while i < len(pattern):
        if pattern.startswith("**/", i):
            out.append("(?:.*/)?")
            i += 3
        elif pattern.startswith("**", i):
            out.append(".*")
            i += 2
        elif pattern[i] == "*":
            out.append("[^/]*")
            i += 1
        elif pattern[i] == "?":
            out.append("[^/]")
            i += 1
        else:
            out.append(re.escape(pattern[i]))
            i += 1
    return re.compile("".join(out))


def glob_match(pattern, path):
    return glob_to_regex(pattern).fullmatch(path) is not None


def parse_name_status(text):
    """Changed files from `git diff --name-status`: both names of a rename,
    only the new name of a copy. Sorted and deduplicated."""
    files = set()
    for line in text.splitlines():
        if not line.strip():
            continue
        fields = line.split("\t")
        status = fields[0]
        if len(fields) < 2 or not status:
            raise SelectionError(f"malformed --name-status line: {line!r}")
        if status[0] == "R":
            files.update(fields[1:3])
        elif status[0] == "C":
            files.add(fields[-1])
        else:
            files.add(fields[1])
    return sorted(files)


def rule_kind(rule):
    kinds = []
    if rule.get("ignore") is True:
        kinds.append("ignore")
    if rule.get("full") is True:
        kinds.append("full")
    if "uncovered" in rule:
        kinds.append("uncovered")
    if "tags" in rule or "scenarios" in rule:
        kinds.append("select")
    return kinds


def validate_map(gate_map):
    if not isinstance(gate_map, dict):
        raise SelectionError("map.toml must be a table")
    settings = gate_map.get("settings", {})
    if not isinstance(settings, dict):
        raise SelectionError("[settings] must be a table")
    rules = gate_map.get("rule", [])
    if not isinstance(rules, list) or not all(isinstance(r, dict) for r in rules):
        raise SelectionError("rule must be an array of [[rule]] tables")
    if not isinstance(settings.get("ortsom_ref"), str) or not settings["ortsom_ref"]:
        raise SelectionError("[settings] needs a non-empty ortsom_ref")
    if not isinstance(settings.get("always_tags", []), list):
        raise SelectionError("[settings] always_tags must be a list")
    seen = set()
    for rule in rules:
        name = rule.get("name")
        if not isinstance(name, str) or not name:
            raise SelectionError(f"rule without a name: {rule!r}")
        if name in seen:
            raise SelectionError(f"duplicate rule name: {name!r}")
        seen.add(name)
        unknown = set(rule) - RULE_KEYS
        if unknown:
            raise SelectionError(f"rule {name!r}: unknown keys {sorted(unknown)}")
        paths = rule.get("paths")
        if not isinstance(paths, list) or not paths or not all(isinstance(p, str) for p in paths):
            raise SelectionError(f"rule {name!r}: paths must be a non-empty list of globs")
        kinds = rule_kind(rule)
        if len(kinds) != 1:
            raise SelectionError(
                f"rule {name!r} must be exactly one of ignore, full, uncovered "
                f"or tags/scenarios; it is {kinds or 'none'}"
            )


def check_references(gate_map, registry):
    """Every tag and scenario name the map uses exists in the pinned Ortsom."""
    names = {s["name"] for s in registry}
    tags = {t for s in registry for t in s["tags"]}
    problems = [
        f"always_tags: unknown tag {t!r}"
        for t in gate_map["settings"].get("always_tags", []) if t not in tags
    ]
    for rule in gate_map.get("rule", []):
        problems += [f"rule {rule['name']!r}: unknown tag {t!r}" for t in rule.get("tags", []) if t not in tags]
        problems += [
            f"rule {rule['name']!r}: unknown scenario {n!r}" for n in rule.get("scenarios", []) if n not in names
        ]
    if problems:
        raise SelectionError(
            "map.toml does not match scenarios.json (stale map or snapshot): " + "; ".join(problems)
        )


def matching_rules(path, rules):
    return [r for r in rules if any(glob_match(p, path) for p in r["paths"])]


def unmapped(files, gate_map):
    """Files no rule of any kind matches (§ 4.4 coverage test)."""
    rules = gate_map.get("rule", [])
    return sorted(f for f in files if not matching_rules(f, rules))


def select(files, gate_map, registry):
    """The selection.json document of § 4.3 for these changed files."""
    validate_map(gate_map)
    check_references(gate_map, registry)
    rules = gate_map.get("rule", [])

    ignored, remaining = [], []
    for f in sorted(set(files)):
        hits = matching_rules(f, rules)
        (ignored if any(r.get("ignore") is True for r in hits) else remaining).append(f)

    result = {
        "mode": "none",
        "scenarios": [],
        "matched_rules": [],
        "uncovered_files": {},
        "unmapped_files": [],
        "ignored_files": ignored,
    }
    if not remaining:
        return result

    matched = []
    for f in remaining:
        hits = matching_rules(f, rules)
        if not hits:
            result["unmapped_files"].append(f)
            continue
        matched += [r for r in hits if r not in matched]
        if all("uncovered" in r for r in hits):
            result["uncovered_files"][f] = "; ".join(r["uncovered"] for r in hits)
    result["matched_rules"] = [r["name"] for r in rules if r in matched]

    if any(r.get("full") is True for r in matched):
        result["mode"] = "full"
        result["scenarios"] = sorted(s["name"] for s in registry)
        return result

    tags = set(gate_map["settings"].get("always_tags", []))
    names = set()
    for r in matched:
        tags.update(r.get("tags", []))
        names.update(r.get("scenarios", []))
    names.update(s["name"] for s in registry if tags.intersection(s["tags"]))
    result["mode"] = "subset"
    result["scenarios"] = sorted(names)
    return result


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--map", type=Path, default=GATE_DIR / "map.toml")
    parser.add_argument("--scenarios", type=Path, default=GATE_DIR / "scenarios.json")
    parser.add_argument("--changes", type=Path, required=True, help="`git diff --name-status` output")
    parser.add_argument("--out", type=Path, required=True)
    args = parser.parse_args(argv)

    try:
        with open(args.map, "rb") as f:
            gate_map = tomllib.load(f)
        registry = json.loads(args.scenarios.read_text())
        files = parse_name_status(args.changes.read_text())
        selection = select(files, gate_map, registry)
    except (SelectionError, tomllib.TOMLDecodeError, json.JSONDecodeError, KeyError) as e:
        print(f"select_scenarios: {e}", file=sys.stderr)
        return 2

    args.out.write_text(json.dumps(selection, indent=2) + "\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())
