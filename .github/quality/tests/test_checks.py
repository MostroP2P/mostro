"""Unit tests for the triage checks (docs/CONTRIBUTION_QUALITY_SPEC.md § 7).

Run from the repository root:

    python3 -m unittest discover -s .github/quality/tests
"""

import sys
import unittest
from pathlib import Path

QUALITY_DIR = Path(__file__).resolve().parents[1]
REPO_ROOT = QUALITY_DIR.parents[1]
sys.path.insert(0, str(QUALITY_DIR))

import triage  # noqa: E402

CONFIG = triage.load_config(QUALITY_DIR / "config.toml")
TEMPLATE = triage.template_sections((REPO_ROOT / ".github/pull_request_template.md").read_text())

GOOD_BODY = """\
## Linked issue

Closes #123

## Type

fix

## What changes and why

A seller's release during a dispute closed it as `settled`. It now closes it
as `released`, so clients show who ended the dispute.

## Affected flow

`release` while the order is `dispute`; `src/app/release.rs`.

## Blast radius

`admin-settle` also writes the dispute status; it is unchanged (step 4).

## Manual testing

### Setup

Ortsom regtest stack: `ortsom stack up --ref my-branch`. Default settings.

### Steps

1. Seller creates a sell order for 10,000 sats / 10 USD.
   Expected: kind 38383 with `s=pending`.
2. Buyer takes it; seller pays the hold invoice.
   Expected: kind 38383 with `s=active`.
3. Seller sends `release`. **(fails on `main`)**
   Expected: the dispute moves to `s=released`.
   On `main`: it moves to `s=settled`.
4. Regression: the solver settles another dispute.
   Expected: unchanged from `main`, `s=settled`.

### Not covered

Cooperative cancel during a dispute.

## Automated tests

`closes_dispute_as_released_on_release` fails on `main`.

## Checklist

- [x] `cargo fmt`, `cargo clippy --all-targets --all-features` and `cargo test` pass locally
- [x] Commits are signed

## AI assistance

None.
"""

TEST_PATCH = "@@ -1,3 +1,9 @@\n+    #[tokio::test]\n+    async fn closes_dispute_as_released() {\n+    }\n"


def pr(body=GOOD_BODY, title="fix: close a released dispute as released", association="CONTRIBUTOR",
       author="alice", labels=()):
    return triage.PullRequest(
        number=7, title=title, body=body, author=author, author_type="User",
        association=association, labels=tuple(labels), head_sha="a" * 40, draft=False,
    )


def facts(files=None, commits=None, issues=None, other_open_prs=0):
    return triage.Facts(
        files=tuple(files if files is not None else [
            triage.ChangedFile("src/app/release.rs", 30, 2, TEST_PATCH),
        ]),
        commits=tuple(commits if commits is not None else [triage.Commit("b" * 40, True)]),
        linked_issues=tuple(issues if issues is not None else [triage.LinkedIssue(123, ("status: accepted",))]),
        other_open_prs=other_open_prs,
    )


def failing(result):
    return {c.name: c.reasons for c in result.checks if not c.passed}


def evaluate(pull=None, fact=None):
    return triage.evaluate(pull or pr(), fact or facts(), CONFIG, TEMPLATE)


class TemplateTest(unittest.TestCase):
    def test_real_template_has_the_spec_sections(self):
        self.assertEqual(list(TEMPLATE), [
            "Linked issue", "Type", "What changes and why", "Affected flow", "Blast radius",
            "Manual testing", "Automated tests", "Checklist", "AI assistance",
        ])

    def test_a_complete_description_passes_every_check(self):
        result = evaluate()
        self.assertEqual(failing(result), {})
        self.assertTrue(result.ok)

    def test_the_untouched_template_fails_every_section(self):
        body = (REPO_ROOT / ".github/pull_request_template.md").read_text()
        reasons = failing(evaluate(pr(body=body, title="feat: x")))["template"]
        self.assertEqual(len(reasons), 9)
        self.assertIn("Section `Linked issue` is missing or empty", reasons)

    def test_missing_section_is_reported(self):
        body = GOOD_BODY.replace("## Blast radius", "## Something else")
        self.assertEqual(
            failing(evaluate(pr(body=body)))["template"],
            ("Section `Blast radius` is missing or empty",),
        )

    def test_comment_only_section_counts_as_empty(self):
        body = GOOD_BODY.replace("None.\n", "<!-- Did you use AI tools? -->\n")
        self.assertIn("Section `AI assistance` is missing or empty", failing(evaluate(pr(body=body)))["template"])

    def test_windows_line_endings_are_accepted(self):
        self.assertEqual(failing(evaluate(pr(body=GOOD_BODY.replace("\n", "\r\n")))), {})

    def test_heading_inside_a_code_fence_is_not_a_section(self):
        body = GOOD_BODY.replace("## Blast radius\n", "```text\n## Blast radius\n```\n")
        self.assertIn("Section `Blast radius` is missing or empty", failing(evaluate(pr(body=body)))["template"])

    def test_body_beyond_the_limit_is_not_read(self):
        padding = "x" * CONFIG["limits"]["max_body_bytes"]
        self.assertIn("template", failing(evaluate(pr(body=padding + GOOD_BODY))))


class ManualTestingTest(unittest.TestCase):
    def test_one_step_is_not_enough(self):
        start = GOOD_BODY.index("2. Buyer")
        end = GOOD_BODY.index("### Not covered")
        body = GOOD_BODY[:start] + GOOD_BODY[end:]
        body = body.replace("1. Seller creates", "1. Seller sends `release` **(fails on main)**, creates")
        reasons = failing(evaluate(pr(body=body)))["manual-testing"]
        self.assertEqual(reasons, ("Manual testing: **Steps** needs at least 2 numbered steps",))

    def test_step_without_expected_line(self):
        body = GOOD_BODY.replace("   Expected: kind 38383 with `s=active`.\n", "")
        reasons = failing(evaluate(pr(body=body)))["manual-testing"]
        self.assertEqual(reasons, ("Manual testing: step 2 has no `Expected:` line",))

    def test_bold_expected_counts(self):
        body = GOOD_BODY.replace("Expected: kind 38383 with `s=active`", "**Expected:** kind 38383 with `s=active`")
        self.assertEqual(failing(evaluate(pr(body=body))), {})

    def test_fix_needs_a_step_that_fails_on_main(self):
        body = GOOD_BODY.replace(" **(fails on `main`)**", "")
        reasons = failing(evaluate(pr(body=body)))["manual-testing"]
        self.assertEqual(reasons, ("Manual testing: no step is marked `(fails on main)`",))

    def test_feature_needs_no_failing_step(self):
        body = GOOD_BODY.replace(" **(fails on `main`)**", "").replace("\nfix\n", "\nfeat\n")
        self.assertEqual(failing(evaluate(pr(body=body, title="feat: x"))), {})

    def test_steps_numbered_with_parenthesis(self):
        body = GOOD_BODY.replace("1. Seller", "1) Seller").replace("2. Buyer", "2) Buyer")
        self.assertEqual(failing(evaluate(pr(body=body))), {})


class TypeTest(unittest.TestCase):
    def test_type_comes_from_the_template_field(self):
        self.assertEqual(triage.pr_type(triage.split_sections("## Type\n\n`fix`\n"), "feat: x"), "fix")

    def test_title_prefix_is_the_fallback(self):
        self.assertEqual(triage.pr_type({}, "fix(dispute)!: x"), "fix")
        self.assertEqual(triage.pr_type({}, "Improve things"), None)


class FixHasTestTest(unittest.TestCase):
    def test_fix_without_a_test_fails(self):
        files = [triage.ChangedFile("src/app/release.rs", 3, 1, "@@ -1 +1 @@\n+    let x = 1;\n")]
        self.assertEqual(failing(evaluate(fact=facts(files=files)))["fix-has-test"], ("A fix needs a regression test",))

    def test_plain_test_attribute_counts(self):
        files = [triage.ChangedFile("src/util.rs", 3, 0, "@@ -1 +1 @@\n+    #[test]\n+    fn x() {}\n")]
        self.assertNotIn("fix-has-test", failing(evaluate(fact=facts(files=files))))

    def test_removed_test_does_not_count(self):
        files = [triage.ChangedFile("src/util.rs", 0, 2, "@@ -1 +1 @@\n-    #[test]\n-    fn x() {}\n")]
        self.assertIn("fix-has-test", failing(evaluate(fact=facts(files=files))))

    def test_unknown_patch_is_not_held_against_the_author(self):
        files = [triage.ChangedFile("src/huge.rs", 5000, 0, None)]
        self.assertNotIn("fix-has-test", failing(evaluate(fact=facts(files=files))))

    def test_feature_needs_no_test_here(self):
        body = GOOD_BODY.replace("\nfix\n", "\nfeat\n")
        files = [triage.ChangedFile("src/app/release.rs", 3, 1, "@@ -1 +1 @@\n+ x\n")]
        self.assertNotIn("fix-has-test", failing(evaluate(pr(body=body, title="feat: x"), facts(files=files))))


class IssueTest(unittest.TestCase):
    def test_no_linked_issue(self):
        self.assertEqual(failing(evaluate(fact=facts(issues=[])))["issue"], ("No accepted issue is linked",))

    def test_linked_issue_not_accepted(self):
        issues = [triage.LinkedIssue(5, ("bug",))]
        self.assertIn("issue", failing(evaluate(fact=facts(issues=issues))))

    def test_any_accepted_issue_is_enough(self):
        issues = [triage.LinkedIssue(5, ("bug",)), triage.LinkedIssue(6, ("status: accepted",))]
        self.assertNotIn("issue", failing(evaluate(fact=facts(issues=issues))))


class SignedSizeOpenPrsTest(unittest.TestCase):
    def test_unsigned_commits_are_named(self):
        commits = [triage.Commit("1234567890" + "0" * 30, False), triage.Commit("b" * 40, True)]
        self.assertEqual(
            failing(evaluate(fact=facts(commits=commits)))["signed"],
            ("Commits `1234567` are not signed",),
        )

    def test_first_pr_over_the_line_limit(self):
        files = [triage.ChangedFile("src/app/release.rs", 390, 20, TEST_PATCH)]
        reasons = failing(evaluate(pr(association="FIRST_TIME_CONTRIBUTOR"), facts(files=files)))["size"]
        self.assertIn("400", reasons[0])

    def test_lockfiles_do_not_count_toward_size(self):
        files = [
            triage.ChangedFile("src/app/release.rs", 100, 0, TEST_PATCH),
            triage.ChangedFile("Cargo.lock", 900, 900, None),
        ]
        self.assertNotIn("size", failing(evaluate(pr(association="NONE"), facts(files=files))))

    def test_size_limit_is_only_for_first_time_contributors(self):
        files = [triage.ChangedFile("src/app/release.rs", 2000, 0, TEST_PATCH)]
        self.assertNotIn("size", failing(evaluate(fact=facts(files=files))))

    def test_first_timer_with_another_open_pr_passes(self):
        self.assertNotIn("open-prs", failing(evaluate(pr(association="FIRST_TIMER"), facts(other_open_prs=1))))

    def test_first_timer_with_two_other_open_prs_fails(self):
        reasons = failing(evaluate(pr(association="FIRST_TIMER"), facts(other_open_prs=2)))["open-prs"]
        self.assertEqual(reasons, ("Please wait until your open pull request is reviewed",))


class ExemptionTest(unittest.TestCase):
    EMPTY = "Just a quick change."

    def content_checks_skipped(self, result):
        return {c.name for c in result.checks if c.skipped} >= {"issue", "template", "manual-testing", "fix-has-test"}

    def test_maintainer_skips_content_checks_but_not_signing(self):
        commits = [triage.Commit("c" * 40, False)]
        result = evaluate(pr(body=self.EMPTY, association="MEMBER"), facts(issues=[], commits=commits))
        self.assertTrue(self.content_checks_skipped(result))
        self.assertEqual(list(failing(result)), ["signed"])
        self.assertEqual(result.exempt_reason, "maintainer")

    def test_markdown_only_change_is_exempt(self):
        files = [triage.ChangedFile("docs/x.md", 5, 0, "+x\n")]
        result = evaluate(pr(body=self.EMPTY), facts(files=files, issues=[]))
        self.assertTrue(result.ok)
        self.assertEqual(result.exempt_reason, "Markdown only")

    def test_renaming_code_to_markdown_is_not_markdown_only(self):
        files = [triage.ChangedFile("notes.md", 0, 0, None, previous_filename="src/app/release.rs")]
        result = evaluate(pr(body=self.EMPTY), facts(files=files, issues=[]))
        self.assertIsNone(result.exempt_reason)
        self.assertFalse(result.ok)

    def test_exempt_label(self):
        result = evaluate(pr(body=self.EMPTY, labels=["quality:exempt"]), facts(issues=[]))
        self.assertTrue(result.ok)

    def test_bot_skips_everything(self):
        commits = [triage.Commit("c" * 40, False)]
        result = evaluate(pr(body="", author="dependabot[bot]", association="NONE"), facts(commits=commits, issues=[]))
        self.assertTrue(result.ok)
        self.assertTrue(all(c.skipped for c in result.checks))


class StepsSyntaxTest(unittest.TestCase):
    def body_with(self, block):
        return GOOD_BODY + "\n```ortsom-steps\n" + block + "\n```\n"

    def test_no_block_passes(self):
        self.assertNotIn("steps-syntax", failing(evaluate()))

    def test_valid_block_passes(self):
        block = (
            '[[step]]\nactor = "alice"\ndo = "create_sell_order"\nsats = 10000\nas = "o"\n\n'
            '[[step]]\nexpect = "status"\norder = "o"\nstatus = "pending"\nfails_on_main = true\n'
        )
        self.assertNotIn("steps-syntax", failing(evaluate(pr(body=self.body_with(block)))))

    def test_invalid_toml_is_reported(self):
        reasons = failing(evaluate(pr(body=self.body_with('[[step]]\nactor = "alice'))))["steps-syntax"]
        self.assertTrue(reasons[0].startswith("`ortsom-steps`: "))

    def test_unknown_verb(self):
        reasons = failing(evaluate(pr(body=self.body_with('[[step]]\nactor = "alice"\ndo = "sleep"'))))
        self.assertIn("unknown action `sleep`", reasons["steps-syntax"][0])

    def test_step_must_be_action_or_expectation(self):
        block = '[[step]]\nactor = "alice"\ndo = "release"\nexpect = "status"'
        self.assertIn("steps-syntax", failing(evaluate(pr(body=self.body_with(block)))))

    def test_only_one_step_fails_on_main(self):
        step = '[[step]]\nexpect = "status"\nstatus = "active"\nfails_on_main = true\n'
        self.assertIn("steps-syntax", failing(evaluate(pr(body=self.body_with(step + step)))))


class CommentTest(unittest.TestCase):
    def test_comment_lists_every_reason_and_carries_the_marker(self):
        result = evaluate(fact=facts(issues=[], commits=[triage.Commit("d" * 40, False)]))
        text = triage.render_comment(result)
        self.assertTrue(text.startswith(triage.MARKER))
        self.assertIn("No accepted issue is linked", text)
        self.assertIn("are not signed", text)
        self.assertIn("CONTRIBUTING.md", text)


if __name__ == "__main__":
    unittest.main()
