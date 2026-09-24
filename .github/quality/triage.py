"""Checks of the contribution quality bar (docs/CONTRIBUTION_QUALITY_SPEC.md § 7).

Pure functions over data the GitHub API returns; run_triage.py fetches that
data and applies the result. Nothing here reads the network or runs pull
request code. Standard library only.
"""

import re
import tomllib
from dataclasses import dataclass

MARKER = "<!-- quality-triage -->"

TYPES = {"fix", "feat", "refactor", "docs", "test", "chore", "perf", "ci"}

# § 9.2: the closed vocabulary of an `ortsom-steps` block.
STEP_ACTIONS = {
    "create_sell_order", "create_buy_order", "take_sell", "take_buy", "provide_invoice",
    "mark_fiat_sent", "release", "open_dispute", "cancel", "rate", "pay_hold_invoice",
    "take_dispute", "settle",
}
STEP_EXPECTATIONS = {
    "status", "message_received", "dispute_status", "payment_received", "settled",
    "payment_failed", "transition_path", "absent_from_book", "status_absent",
}

CONTENT_CHECKS = ("issue", "template", "manual-testing", "fix-has-test")

TEST_ATTRIBUTE = re.compile(r"^\+\s*#\[(tokio::)?test\b")
STEP_LINE = re.compile(r"^\s{0,3}(\d+)[.)](?:\s+|$)")
FENCE = re.compile(r"^\s{0,3}(`{3,}|~{3,})\s*([\w-]*)\s*$")
COMMENT = re.compile(r"<!--.*?-->", re.S)
TITLE_TYPE = re.compile(r"^\s*([a-z]+)(?:\([^)]*\))?!?:")


@dataclass(frozen=True)
class PullRequest:
    number: int
    title: str
    body: str
    author: str
    author_type: str
    association: str
    labels: tuple
    head_sha: str
    draft: bool


@dataclass(frozen=True)
class ChangedFile:
    filename: str
    additions: int
    deletions: int
    patch: object  # str, or None when GitHub omits a large diff
    previous_filename: object = None  # the old name of a rename


@dataclass(frozen=True)
class Commit:
    sha: str
    verified: bool


@dataclass(frozen=True)
class LinkedIssue:
    number: int
    labels: tuple


@dataclass(frozen=True)
class Facts:
    files: tuple
    commits: tuple
    linked_issues: tuple
    other_open_prs: int


@dataclass(frozen=True)
class Check:
    name: str
    passed: bool
    reasons: tuple = ()
    skipped: bool = False


@dataclass(frozen=True)
class Result:
    checks: tuple
    exempt_reason: object  # str or None

    @property
    def ok(self):
        return all(c.passed for c in self.checks)

    @property
    def reasons(self):
        return tuple(r for c in self.checks for r in c.reasons)


def load_config(path):
    with open(path, "rb") as f:
        return tomllib.load(f)


# --- Markdown -----------------------------------------------------------------


def outside_fences(lines):
    """Yield (line, inside_fence) so headings in code blocks are not sections."""
    fence = None
    for line in lines:
        m = FENCE.match(line)
        if fence is None and m:
            fence = m.group(1)
            yield line, True
        elif fence is not None:
            if line.strip().startswith(fence) and set(line.strip()) == {fence[0]}:
                fence = None
            yield line, True
        else:
            yield line, False


def split_sections(text, level=2):
    """Map each heading of exactly `level` to the text under it (first wins)."""
    prefix = "#" * level + " "
    sections, name, buf = {}, None, []
    for line, fenced in outside_fences(text.replace("\r\n", "\n").split("\n")):
        if not fenced and line.startswith(prefix):
            if name is not None and name not in sections:
                sections[name] = "\n".join(buf)
            name, buf = line[len(prefix):].strip(), []
        else:
            buf.append(line)
    if name is not None and name not in sections:
        sections[name] = "\n".join(buf)
    return sections


def normalize(text):
    """Text without HTML comments, blank lines or surrounding whitespace."""
    lines = (line.strip() for line in COMMENT.sub("", text).split("\n"))
    return "\n".join(line for line in lines if line)


def plain(text):
    """Lowercase text without Markdown emphasis or code marks."""
    return re.sub(r"[*_`]", "", text).lower()


def template_sections(template_text):
    """Each `##` heading of the template and its placeholder, normalized."""
    return {name: normalize(body) for name, body in split_sections(template_text).items()}


def parse_steps(text):
    """Numbered steps, each with its continuation lines."""
    steps = []
    for line in normalize(text).split("\n"):
        if STEP_LINE.match(line):
            steps.append(line)
        elif steps:
            steps[-1] += "\n" + line
    return steps


def pr_type(sections, title):
    words = normalize(sections.get("Type", "")).split()
    if words:
        declared = re.sub(r"[`*_]", "", words[0]).lower()
        if declared in TYPES:
            return declared
    m = TITLE_TYPE.match(title.lower())
    return m.group(1) if m and m.group(1) in TYPES else None


def steps_blocks(body):
    blocks, current = [], None
    for line in body.replace("\r\n", "\n").split("\n"):
        m = FENCE.match(line)
        if current is None and m and m.group(2) == "ortsom-steps":
            current = (m.group(1), [])
        elif current is not None:
            if line.strip().startswith(current[0]) and set(line.strip()) == {current[0][0]}:
                blocks.append("\n".join(current[1]))
                current = None
            else:
                current[1].append(line)
    return blocks


# --- Checks -------------------------------------------------------------------


def check_issue(facts, cfg):
    accepted = cfg["labels"]["accepted"]
    if any(accepted in issue.labels for issue in facts.linked_issues):
        return Check("issue", True)
    return Check("issue", False, ("No accepted issue is linked",))


def check_template(sections, template):
    reasons = tuple(
        f"Section `{name}` is missing or empty"
        for name, placeholder in template.items()
        if name not in sections or normalize(sections[name]) in ("", placeholder)
    )
    return Check("template", not reasons, reasons)


def check_manual_testing(sections, is_fix):
    steps = parse_steps(split_sections(sections.get("Manual testing", ""), level=3).get("Steps", ""))
    reasons = []
    if len(steps) < 2:
        reasons.append("Manual testing: **Steps** needs at least 2 numbered steps")
    reasons += [
        f"Manual testing: step {i} has no `Expected:` line"
        for i, step in enumerate(steps, 1)
        if not re.search(r"\bexpected\s*:", plain(step))
    ]
    if is_fix and not any("(fails on main)" in plain(step) for step in steps):
        reasons.append("Manual testing: no step is marked `(fails on main)`")
    return Check("manual-testing", not reasons, tuple(reasons))


def validate_step(i, step):
    errors = []
    is_action = "actor" in step or "do" in step
    is_expectation = "expect" in step
    if is_action == is_expectation:
        return [f"step {i} must be an action (`actor` and `do`) or an expectation (`expect`), not "
                + ("both" if is_action else "neither")]
    if is_action:
        if not isinstance(step.get("actor"), str) or not isinstance(step.get("do"), str):
            errors.append(f"step {i}: `actor` and `do` must both be strings")
        elif step["do"] not in STEP_ACTIONS:
            errors.append(f"step {i}: unknown action `{step['do']}`")
    elif step["expect"] not in STEP_EXPECTATIONS:
        errors.append(f"step {i}: unknown expectation `{step['expect']}`")
    if not isinstance(step.get("fails_on_main", False), bool):
        errors.append(f"step {i}: `fails_on_main` must be true or false")
    errors += [
        f"step {i}: `{key}` must be a string, number or boolean"
        for key, value in step.items()
        if not isinstance(value, (str, int, float, bool))
    ]
    return errors


def steps_errors(block):
    try:
        doc = tomllib.loads(block)
    except tomllib.TOMLDecodeError as e:
        return [f"not valid TOML ({e})"]
    steps = doc.get("step")
    if set(doc) != {"step"} or not isinstance(steps, list) or not steps:
        return ["expected one or more `[[step]]` tables and nothing else"]
    errors = [e for i, step in enumerate(steps, 1) for e in validate_step(i, step)]
    if sum(1 for s in steps if s.get("fails_on_main") is True) > 1:
        errors.append("only one step may set `fails_on_main`")
    return errors


def check_steps_syntax(body):
    """Structural check of § 9.2 until `ortsom steps check` exists (§ 9.4)."""
    blocks = steps_blocks(body)
    if not blocks:
        return Check("steps-syntax", True)
    errors = ["more than one `ortsom-steps` block"] if len(blocks) > 1 else steps_errors(blocks[0])
    reasons = tuple(f"`ortsom-steps`: {e}" for e in errors)
    return Check("steps-syntax", not reasons, reasons)


def check_fix_has_test(files, is_fix):
    if not is_fix:
        return Check("fix-has-test", True)
    adds_test = any(
        TEST_ATTRIBUTE.match(line)
        for f in files if f.patch
        for line in f.patch.split("\n")
    )
    # GitHub omits the patch of a large file; a test there cannot be ruled out.
    unknown = any(f.patch is None and f.filename.endswith(".rs") for f in files)
    if adds_test or unknown:
        return Check("fix-has-test", True)
    return Check("fix-has-test", False, ("A fix needs a regression test",))


def check_signed(commits):
    unsigned = [c.sha[:7] for c in commits if not c.verified]
    if not unsigned:
        return Check("signed", True)
    return Check("signed", False, ("Commits " + ", ".join(f"`{s}`" for s in unsigned) + " are not signed",))


def check_size(files, first_time, cfg):
    limit = cfg["limits"]["max_first_pr_lines"]
    excluded = set(cfg["limits"]["size_exclude"])
    lines = sum(
        f.additions + f.deletions for f in files
        if f.filename not in excluded and f.filename.rsplit("/", 1)[-1] not in excluded
    )
    if not first_time or lines <= limit:
        return Check("size", True)
    return Check("size", False, (
        f"First pull requests are limited to {limit} changed lines ({lines} here); "
        "split it or discuss the scope in the issue",
    ))


def check_open_prs(other_open_prs, first_time, cfg):
    if not first_time or other_open_prs <= cfg["limits"]["max_open_prs_new"]:
        return Check("open-prs", True)
    return Check("open-prs", False, ("Please wait until your open pull request is reviewed",))


# --- Evaluation ---------------------------------------------------------------


def exemption(pull, facts, cfg):
    """Why the content checks are skipped (§ 7.3), or None."""
    if pull.association in cfg["authors"]["maintainers"]:
        return "maintainer"
    # Both names of a rename: moving code to a .md name deletes code.
    names = [n for f in facts.files for n in (f.filename, f.previous_filename) if n]
    if names and all(n.endswith(".md") for n in names):
        return "Markdown only"
    if cfg["labels"]["exempt"] in pull.labels:
        return "quality:exempt label"
    return None


def is_bot(pull, cfg):
    return pull.author_type == "Bot" or pull.author in cfg["authors"]["bots"]


def evaluate(pull, facts, cfg, template):
    body = pull.body.encode()[: cfg["limits"]["max_body_bytes"]].decode(errors="ignore")
    sections = split_sections(body)
    is_fix = pr_type(sections, pull.title) == "fix"
    first_time = pull.association in cfg["authors"]["first_time"]
    checks = (
        check_issue(facts, cfg),
        check_template(sections, template),
        check_manual_testing(sections, is_fix),
        check_steps_syntax(body),
        check_fix_has_test(facts.files, is_fix),
        check_signed(facts.commits),
        check_size(facts.files, first_time, cfg),
        check_open_prs(facts.other_open_prs, first_time, cfg),
    )
    if is_bot(pull, cfg):
        return Result(tuple(Check(c.name, True, skipped=True) for c in checks), "bot")
    reason = exemption(pull, facts, cfg)
    if reason:
        checks = tuple(Check(c.name, True, skipped=True) if c.name in CONTENT_CHECKS else c for c in checks)
    return Result(checks, reason)


def render_comment(result, repo="MostroP2P/mostro"):
    bar = f"https://github.com/{repo}/blob/main/CONTRIBUTING.md#contribution-quality-bar"
    lines = [
        MARKER,
        "### Contribution quality bar",
        "",
        f"This pull request does not meet the [contribution quality bar]({bar}) yet:",
        "",
        *(f"- {r}" for r in result.reasons),
        "",
        "Edit the description or push new commits and this comment updates itself; "
        "it is removed once every check passes.",
        "",
        "<sub>Label-only for now: nothing is closed automatically. A maintainer reviews each case.</sub>",
    ]
    return "\n".join(lines) + "\n"
