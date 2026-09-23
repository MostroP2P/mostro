# Contribution Quality Bar — Spec

**Status:** Spec · Phase 0 (this document)
**Related:** [Ortsom on Pull Requests](./ORTSOM_PR_E2E_SPEC.md), the e2e
gate proposed in [#975](https://github.com/MostroP2P/mostro/pull/975)
**Initial mode:** label-only (shadow). Nothing is closed automatically
until Phase 4 is explicitly enabled.

## 1. Goal

Mostro receives a growing number of pull requests that cost a reviewer
more time than they are worth: fixes for problems that do not exist,
changes that fix one flow and break another, and descriptions that do not
match the diff. Many are generated with AI tools by authors who have not
run the code.

This spec defines a **quality bar a pull request must clear before a
reviewer reads the diff**. The principle is:

> A pull request proves by itself that the problem exists and that the
> change does not break anything. If it does not, it is closed without a
> technical review, whoever or whatever wrote it.

Every check below is about evidence the pull request carries (a linked
issue, a test that fails on `main`, a manual test that can be
reproduced), never about how the pull request was produced.

### Non-goals

- **Detecting AI-generated code.** Detectors are unreliable, can be
  disputed, and would penalise good contributors who use AI tools. Using
  AI is allowed; the author answers for every line.
- **Closing pull requests with an LLM.** A model may help order the
  queue (§11), but it never closes, blocks or counts toward closing.
- **Replacing review.** A pull request that clears the bar still gets a
  full human review. The bar decides whether the review starts.
- **Replacing the Rust CI.** Whether a pull request compiles and passes
  `cargo test` is the CI's question
  ([#929](https://github.com/MostroP2P/mostro/issues/929)).

## 2. Layers

Cheapest first. Each layer removes pull requests before the next, more
expensive one runs.

| # | Layer | What it catches | Cost | Section |
|---|---|---|---|---|
| 1 | Written policy | Gives a reason to close without debate | Docs only | §3 |
| 2 | Issue before pull request | Fixes for bugs that do not exist | Docs and a label | §4 |
| 3 | Pull request template, with **Manual testing** | Authors who do not know which flow they changed or never ran it | Docs only | §5, §6 |
| 4 | Triage bot | Missing issue, template, test or signatures; oversized first pull requests | One workflow, no build | §7 |
| 5 | Red test on `main` | Fixes whose test also passes without the fix | Two test builds per fix | §8 |
| 6 | Ortsom e2e gate | "Fixes one thing, breaks another" | Separate spec | [ORTSOM_PR_E2E_SPEC.md](./ORTSOM_PR_E2E_SPEC.md) |
| 7 | Per-PR Ortsom scenario built from the Manual testing steps | Manual tests that were never run | Future; needs Ortsom work | §9 |

## 3. Policy

A new section in `CONTRIBUTING.md`, **Contribution quality bar**, states:

1. AI tools are allowed. The author is responsible for every line and
   must be able to explain any of it when a reviewer asks. "The tool
   wrote it" is not an answer.
2. A pull request that does not follow the template, does not link an
   accepted issue when one is required (§4), or fails the checks of §7
   and §8, **is closed without technical review**, with the standard
   message below. It can be reopened once it meets the bar.
3. A pull request whose description does not match its diff, or whose
   Manual testing steps do not work when a reviewer follows them, is
   closed the same way.
4. An account that repeatedly opens pull requests closed under this
   policy may be blocked from the organisation.

The standard close message lives in `.github/quality/close-message.md`,
so maintainers and the bot use the same text:

```markdown
Thanks for the contribution. This pull request is being closed without a
technical review because it does not meet the contribution quality bar
(CONTRIBUTING.md § Contribution quality bar):

<reasons>

You are welcome to reopen it, or open a new one, once it does.
```

## 4. Issue before pull request

Every pull request that is not exempt links an issue that a maintainer
has accepted with the new label `status: accepted`. The bug or the
feature is discussed in the issue, where saying "this is not a bug"
costs one comment instead of a review of the whole diff.

- **Linking** uses a closing keyword in the description (`Closes #123`,
  `Fixes #123`). The bot reads linked issues through the GraphQL field
  `closingIssuesReferences`, so a plain mention does not count.
- **Accepted** means the linked issue carries `status: accepted`. Only
  maintainers apply it, when they agree the problem is real and in
  scope. An issue its author opened minutes before the pull request is
  not accepted until a maintainer says so.
- **Exempt** pull requests are listed in §7.3: documentation only,
  bots, maintainers, and the `quality:exempt` label.

## 5. Pull request template

A new file, `.github/pull_request_template.md`. Every `##` heading is
required; the bot (§7) checks that each one is present and not empty or
left with only its placeholder comment.

```markdown
## Linked issue

Closes #

## Type

<!-- One of: fix, feat, refactor, docs, test, chore, perf, ci -->

## What changes and why

<!-- Two to five sentences. What behaviour changes, and for whom. -->

## Affected flow

<!-- Which actions (e.g. `take-sell`, `release`, `admin-settle`) and which
order or dispute statuses does this change touch? Name the files where
that behaviour lives. -->

## Blast radius

<!-- What else reads or writes the state you changed (tables, columns,
events, other handlers)? Why is it not broken? -->

## Manual testing

<!-- Required. See CONTRIBUTING.md § Manual testing: numbered steps a
reviewer can follow to see this change work, each with its expected
result. For a fix, one step must fail on `main`. -->

### Setup

### Steps

1.

### Not covered

## Automated tests

<!-- The tests you added and what each proves. For a fix: the test that
fails on `main` (CONTRIBUTING.md § Red test). -->

## Checklist

- [ ] `cargo fmt`, `cargo clippy --all-targets --all-features` and `cargo test` pass locally (`cargo test` summary line pasted under Automated tests)
- [ ] Commits are signed
- [ ] Protocol or tag change: Scope Declaration included (CONTRIBUTING.md § Protocol / Tag Changes)
- [ ] Schema change: migration added under `migrations/`
- [ ] I ran the Manual testing steps myself and can explain every line of this diff

## AI assistance

<!-- Did you use AI tools, and for which parts? This is not a reason to
close; it tells the reviewer where to look harder. -->
```

**Affected flow** and **Blast radius** are cheap for someone who
understands the change and hard to fill in convincingly for someone who
does not. A generic or wrong answer is an objective close reason under
§3.

The existing protocol **Scope Declaration** stays as it is; the template
points to it rather than duplicating it.

## 6. Manual testing

The **Manual testing** section is a step-by-step procedure that tests
**this specific pull request** end to end, against a running mostrod. It
serves three purposes:

1. **It lets a reviewer reproduce the change** without reverse-engineering
   the diff.
2. **It turns the description into checkable claims.** Each step has an
   expected result a reviewer can confirm or refute by following it, and
   vague steps are visible at a glance. The steps are evidence, not
   proof that the author ran them: a plausible procedure can be written
   without running anything (§14). The author's statement that they ran
   it is the checklist item of §5, and a procedure that does not work is
   a close reason (§3.3).
3. **It is the source of a per-PR Ortsom scenario** (§9). Once written in
   the structured form, the same steps run automatically on every push.

### 6.1 Rules

- **Setup** says which environment and which configuration: the Ortsom
  regtest stack (`ortsom stack up --ref <branch>`) or a local mostrod
  with its own LND; which `settings.toml` keys differ from
  `settings.tpl.toml`; and which client each actor uses (mostro-cli,
  Mostro Mobile, Ortsom).
- **Steps** are numbered. Each step names **one actor** (seller, buyer,
  solver, operator), **one action**, and **the expected observable
  result**: an order status on the public event (kind 38383), a dispute
  status (kind 38386), a message action received by an actor, a payment
  settled or refunded, a log line, or a database row.
- **For a fix**, one step is marked **(fails on `main`)**. It is the
  step whose result differs between `main` and this pull request, and it
  says what `main` does instead. If no step fails on `main`, the pull
  request has not demonstrated a bug.
- **Regression**: at least one step exercises a neighbouring flow named
  in **Blast radius** and shows it behaves as before.
- **Not covered** lists what the steps do not test and why (for example,
  "mainnet route selection: regtest has a single channel").
- A step that cannot be observed ("the code is now more robust") is not
  a step.

### 6.2 Example

How [#967](https://github.com/MostroP2P/mostro/pull/967) (a release
during a dispute closes it as `released`) would have described its
manual test:

```markdown
### Setup

Ortsom regtest stack: `ortsom stack up --ref <branch of #967>`.
Default settings. Seller and buyer on mostro-cli; solver on mostro-cli
with the admin key.

### Steps

1. Seller creates a sell order for 10,000 sats / 10 USD.
   Expected: kind 38383 with `s=pending`.
2. Buyer takes it and sends a payout invoice; seller pays the hold invoice.
   Expected: kind 38383 with `s=active`.
3. Buyer opens a dispute.
   Expected: kind 38386 with `s=initiated`; the seller receives
   `dispute-initiated-by-peer`.
4. Seller sends `release`. **(fails on `main`)**
   Expected: the buyer is paid, the order ends in `success`, and the
   kind 38386 event moves to `s=released`.
   On `main`: the buyer is paid, but the dispute moves to `s=settled`,
   the status a solver's `admin-settle` writes.
5. Regression: repeat steps 1–3; the solver takes the dispute and settles it.
   Expected: unchanged from `main`, the dispute ends in `s=settled`.

### Not covered

Cooperative cancel during a dispute still writes `seller-refunded`;
out of scope for this pull request.
```

### 6.3 Structured steps (optional until Phase 5)

Next to the prose, the author may add a fenced block with the language
`ortsom-steps` that expresses the same steps in the fixed vocabulary of
§9.2. It is optional until the per-PR runner exists; the bot validates
its syntax when it is present.

## 7. Triage bot — `quality-triage.yml`

A workflow on `pull_request_target` (types `opened`, `edited`,
`reopened`, `synchronize`, `ready_for_review`). It **never checks out or
runs pull request code**. It reads metadata through the API, and its
workflow and script come from `main`. That is what makes
`pull_request_target`, which has a write token, safe here (§10).

Its logic lives in `.github/quality/triage.py`, with unit tests in
`.github/quality/tests/`, and its thresholds in
`.github/quality/config.toml`.

### 7.1 Checks

| Check | Passes when | Reason on failure |
|---|---|---|
| `issue` | A closing keyword links an issue labelled `status: accepted` | "No accepted issue is linked" |
| `template` | Every heading of §5 is present, and none is empty or only its placeholder | "Section `<name>` is missing or empty" |
| `manual-testing` | **Steps** has at least 2 numbered steps, each with an `Expected:` line; for type `fix`, one step is marked `(fails on main)` | "Manual testing: `<what is missing>`" |
| `steps-syntax` | An `ortsom-steps` block, if present, parses against §9.2 | "`ortsom-steps`: `<parse error>`" |
| `fix-has-test` | For type `fix` (template field or `fix` title prefix), the diff adds at least one `#[test]` or `#[tokio::test]` function | "A fix needs a regression test" |
| `signed` | Every commit is verified (`commit.verification.verified`) | "Commits `<shas>` are not signed" |
| `size` | For a first-time contributor, at most `max_first_pr_lines` (default 400) changed lines, excluding `Cargo.lock` and `sqlx-data.json` | "First pull requests are limited to 400 changed lines; split it or discuss the scope in the issue" |
| `open-prs` | A first-time contributor has at most `max_open_prs_new` (default 1) other open pull requests | "Please wait until your open pull request is reviewed" |

A first-time contributor is one whose `author_association` is
`FIRST_TIME_CONTRIBUTOR`, `FIRST_TIMER` or `NONE`.

### 7.2 Result

- All checks pass: label `quality:ok`.
- Any check fails: label `quality:needs-info`, and one comment lists
  every failing reason. The comment is **edited in place** on each run,
  never duplicated, and deleted once every check passes.
- The check run is reported as `neutral`, never `failure`, so it does
  not block a merge. Branch protection is not part of this spec.

### 7.3 Exemptions

A pull request skips `issue`, `template`, `manual-testing` and
`fix-has-test` when any of these holds:

- the author's association is `OWNER`, `MEMBER` or `COLLABORATOR`;
- the author is a bot (`dependabot[bot]`, `github-actions[bot]`);
- every changed file is Markdown;
- a maintainer applied the label `quality:exempt`.

`signed`, `size` and `open-prs` still apply to everyone except bots.

### 7.4 Closing (Phase 4 only)

A daily scheduled job, `quality-stale.yml`, closes pull requests that
have carried `quality:needs-info` for `needs_info_days` (default 7) days
without a new push or an edit to the description, with the standard
message of §3. Before Phase 4 it only applies `quality:would-close` and
comments that the pull request **would** have been closed.

## 8. Red test on `main` — `quality-red-test.yml`

The strongest filter against fixes for problems that do not exist: the
regression test of a fix must **fail without the fix and pass with it**.
A model that invents a bug can rarely write a test that fails on `main`.

### 8.1 Commit convention

Mostro's tests live next to the code, in `#[cfg(test)] mod tests`, so a
test cannot be separated from its fix by file. It is separated by
commit instead:

- A fix pull request **starts with a commit whose subject begins with
  `test:`** and that only adds the regression test. The fix follows in
  one or more later commits.
- The test commit is a meaningful commit in the sense of
  `CONTRIBUTING.md` ("Keep the git history clean"), not a fixup: it is
  not squashed into the fix before review. It stays separate until
  merge, and the maintainer may squash when merging.
- The test commit does not need to be on the latest `main`: the job
  applies it to the current base itself (§8.2).

### 8.2 Algorithm

On `pull_request` (read-only token, no secrets), for a pull request of
type `fix`:

1. Take the pull request's first commit. If its subject does not start
   with `test:`, the result is `no-test-commit`.
2. From that commit's diff, list the test functions it adds: a `fn`
   preceded by `#[test]` or `#[tokio::test]`, possibly with other
   attributes in between. If there are none, `no-test-commit`.
3. Check that every hunk of the test commit lies inside a
   `#[cfg(test)]` module or under `tests/`. Otherwise the result is
   `test-commit-changes-code`: a test commit that also changes
   production code is not evidence.
4. Check out the **current base** (`pull_request.base.sha`) and
   cherry-pick the test commit onto it. The test commit's own parent may
   be an older `main` on which the bug still existed; running there could
   report a bug as reproduced after `main` has already fixed it. If the
   cherry-pick conflicts, the result is `test-commit-does-not-apply` (the
   author rebases).
5. On that tree, run `cargo test` filtered to those names. If it **does
   not compile**, the result is `test-does-not-compile-on-base`; if every
   test **passes**, `bug-not-reproduced`. If any fails, continue.
6. On the pull request's merge commit (the `pull_request` default
   checkout, i.e. the same current base plus the whole pull request), run
   the same tests. If they pass, the result is `bug-reproduced`;
   otherwise `test-fails-on-head`.

After the tests, a workflow step (not the tests) writes `red-test.json`
from the exit codes it recorded: `pr`, `head_sha`, `base_sha`, the
result, the test names and the tail of each `cargo test` output. The job
uploads it as an artifact. It has a 45-minute timeout and reuses the
cargo cache of `ci.yml`.

Writing the file from a workflow step keeps honest pull requests honest,
but it is not a guarantee: `cargo test` runs pull request code (tests,
`build.rs`) in the same job, which can tamper with anything that runs
after it, and on `pull_request` the pull request can also edit this
workflow (§10). A forged result is not worth much, though. The author
already controls the test, so forging `bug-reproduced` gains nothing a
contrived test would not, and the reviewer still reads the test (§8.4).
The only result that counts toward closing is `bug-not-reproduced`, and
forging that one only hurts the forger.

### 8.3 Verdict — `quality-verdict.yml`

A `workflow_run` workflow built like `ortsom-verdict.yml`
([ORTSOM_PR_E2E_SPEC.md §9.1](./ORTSOM_PR_E2E_SPEC.md#91-why-two-workflows)):
it never executes pull request code, treats the artifact as untrusted
data, and verifies that the pull request belongs to the triggering run
before touching it. The artifact must match an exact schema: missing or
unknown fields, a SHA that is not 40 hex characters, a `head_sha` that
is not the pull request's current head, or a result outside the table
below reject it, and nothing is labelled. It maps the result to a label:

| Result | Label | Meaning for the reviewer |
|---|---|---|
| `bug-reproduced` | `quality:bug-reproduced` | The test demonstrates both the bug and the fix |
| `bug-not-reproduced` | `quality:bug-not-reproduced` | The test passes without the fix. **Primary close reason** |
| `test-fails-on-head` | `quality:test-fails` | The fix does not make its own test pass |
| `no-test-commit` | `quality:needs-info`, reason added to the triage comment | The §8.1 convention is not followed |
| `test-commit-changes-code` | `quality:needs-info`, reason added to the triage comment | Same |
| `test-commit-does-not-apply` | `quality:needs-info`, reason added to the triage comment | The test commit conflicts with the current `main`; rebase |
| `test-does-not-compile-on-base` | `quality:red-test-inconclusive` | Usually the test uses an API the fix introduces; a reviewer decides |

`bug-not-reproduced` is only a label until Phase 4. Even then it never
closes a pull request by itself: it adds a failing reason to the triage
comment, and the 7-day timer of §7.4 applies, so an author whose test
was simply wrong has time to correct it.

### 8.4 What it does not prove

A test that fails on `main` for an unrelated reason (a wrong expected
value, a check of something the fix renames) passes this layer. It
filters out invented bugs; it does not prove the fix is right. The
reviewer still reads the test.

## 9. Per-PR Ortsom scenario (Phase 5)

The Ortsom gate ([ORTSOM_PR_E2E_SPEC.md](./ORTSOM_PR_E2E_SPEC.md)) runs
the **existing** scenarios selected by what a pull request touches. It
catches regressions in known flows, but not the new behaviour itself,
which by definition no scenario covers yet. The Manual testing steps
do. This phase runs them.

### 9.1 Why a fixed vocabulary, not generated code

Ortsom writes scenarios in Rust on purpose and rejects a declarative
format for them (Ortsom `spec.md`: "Scenarios are written in Rust with
an attribute macro, not in a declarative format"), because real
scenarios branch, wait conditionally and compute amounts. A manual test
procedure is different: it is **linear by construction**. A human
follows it top to bottom, one actor and one action per step, and a
linear list of calls to methods that already exist does not need a
language.

Having an LLM translate the prose into a Rust scenario on the fly is
rejected. The translation is not deterministic, so a failure could be
the translation's fault rather than the pull request's, and the author
could not run the same thing locally.

### 9.2 The `ortsom-steps` block

A TOML document in a fenced block with the language `ortsom-steps`. Each
`[[step]]` is either an **action** (`actor` and `do`) or an
**expectation** (`expect`). Every verb maps one-to-one to an existing
Ortsom actor method or assertion (Ortsom `docs/scenarios.md`, "What
`ctx` gives you"). Steps 1 to 4 of the §6.2 example:

````markdown
```ortsom-steps
[[step]]
actor = "alice"
do = "create_sell_order"
sats = 10000
fiat_code = "USD"
fiat_amount = 10
method = "bank"
as = "o"                     # names the order for later steps

[[step]]
expect = "status"
order = "o"
status = "pending"

[[step]]
actor = "bob"
do = "take_sell"
order = "o"

[[step]]
actor = "bob"
do = "provide_invoice"
order = "o"

[[step]]
actor = "alice"
do = "pay_hold_invoice"      # await_hold_invoice + pay
order = "o"

[[step]]
expect = "status"
order = "o"
status = "active"

[[step]]
actor = "bob"
do = "open_dispute"
order = "o"
as = "d"

[[step]]
expect = "dispute_status"
dispute = "d"
status = "initiated"

[[step]]
actor = "alice"
do = "release"
order = "o"

[[step]]
expect = "dispute_status"
dispute = "d"
status = "released"
fails_on_main = true         # the step marked "(fails on main)" in the prose
```
````

Rules:

- **The vocabulary is closed.** Actions: the `Actor` methods Ortsom
  documents (`create_sell_order`, `create_buy_order`, `take_sell`,
  `take_buy`, `provide_invoice`, `mark_fiat_sent`, `release`,
  `open_dispute`, `cancel`, `rate`), `pay_hold_invoice` as the one
  composite (`await_hold_invoice` followed by `pay`), and the solver's
  `take_dispute`, `settle` and `cancel`. Expectations: `status`,
  `message_received`, `dispute_status`, `payment_received`, `settled`,
  `payment_failed`, `transition_path`, `absent_from_book` and
  `status_absent`. Adding a verb is an Ortsom change.
- **No sleeps, loops or conditionals.** Every expectation waits on
  events with Ortsom's timeouts, like any scenario.
- **Teardown is automatic.** Every order the steps create is cancelled
  at the end (`ctx.defer(cancel_if_open)`), as in a hand-written
  scenario.
- **Coverage is partial by design.** Steps that cannot be expressed stay
  in the prose and are listed under **Not covered**.

### 9.3 How it runs

Inside `ortsom-pr.yml`, after the selected scenarios, on the same stack:

1. The PR job extracts the `ortsom-steps` block from the description (at
   most 64 KiB) and runs it with `ortsom run --steps <file>` against the
   pull request's mostrod.
2. For a fix, the same steps also run against the **image of `main`**
   that the baseline workflow already builds, and must fail at the step
   with `fails_on_main = true`, not earlier.

The result goes into the PR artifact as `steps.json`. The Ortsom verdict
workflow reports it on its own line of the comment, with its own labels:
`ortsom:steps-passed`, `ortsom:steps-failed`, and, for a fix whose steps
also pass on `main`, `ortsom:steps-pass-on-main`, the end-to-end
counterpart of `quality:bug-not-reproduced`. It **does not enter** the
regression ratio of the Ortsom verdict (≥50% over ≥4 scenarios): that
ratio is about known scenarios, and a procedure written by the author is
a different kind of evidence.

### 9.4 Changes needed in Ortsom

- A `--steps <file>` mode that runs the block of §9.2 as one scenario
  named `pr_steps`, with the usual artifacts and teardown.
- A step-level result in `summary.json`: which step failed, and how.
- `ortsom steps check <file>`, which parses and validates a block
  without a stack. The triage bot (`steps-syntax`) and authors use it.

These need their own spec in the Ortsom repository before Phase 5
starts.

### 9.5 Security

The description is attacker-controlled text. The block is parsed as TOML
into a closed set of verbs with typed arguments; no part of it becomes
shell, Rust or a file path. It runs in the same sandboxed PR job as the
Ortsom gate
([ORTSOM_PR_E2E_SPEC.md §9.3](./ORTSOM_PR_E2E_SPEC.md#93-building-untrusted-code)),
which already executes the pull request's own code, so the block adds no
new capability.

## 10. Security

- `quality-triage.yml` runs on `pull_request_target` with
  `pull-requests: write`, `issues: write` and `contents: read`. It checks
  out `main` only, never the pull request's head or merge ref, and never
  interpolates the title, the description or branch names into shell:
  they reach `triage.py` through environment variables and are parsed
  there, with a 64 KiB limit on the description.
- `quality-red-test.yml` builds and runs pull request code, so it runs
  on `pull_request` with a read-only token and no secrets, like
  `ortsom-pr.yml`. The repository's approval requirement for first-time
  contributors applies to it.
- `quality-verdict.yml` follows the two-workflow pattern of
  [ORTSOM_PR_E2E_SPEC.md §9.1](./ORTSOM_PR_E2E_SPEC.md#91-why-two-workflows).
- On `pull_request` a pull request can modify the workflow that tests it
  and forge its artifact. A pull request that touches `.github/quality/**`
  or any `quality-*.yml` therefore gets no `quality:bug-reproduced`
  label; the verdict adds `quality:needs-info` with the reason "changes
  the quality workflows" and leaves it to a maintainer.
  `quality-triage.yml` is not affected: `pull_request_target` always
  runs `main`'s version.

## 11. Optional: LLM triage (out of scope)

A model that reads the issue, the description and the code and answers
"does the described problem exist?" could help order the review queue.
If one is ever added, it only applies an informational label
(`quality:llm-doubt`) and **never** closes, blocks or counts toward
§7.4. False positives against legitimate contributors are expensive, and
nothing in this spec depends on it.

## 12. Labels

| Label | Applied by | Meaning |
|---|---|---|
| `status: accepted` | Maintainer | The issue describes a real problem in scope |
| `quality:ok` | Triage bot | Every §7 check passes |
| `quality:needs-info` | Triage bot, verdict | At least one check fails; the comment says which |
| `quality:would-close` | Stale job (shadow) | Would have been closed by §7.4 |
| `quality:exempt` | Maintainer | Skip the content checks (§7.3) |
| `quality:bug-reproduced` | Verdict | The §8 test fails on `main` and passes on head |
| `quality:bug-not-reproduced` | Verdict | The §8 test passes on `main` |
| `quality:test-fails` | Verdict | The §8 test fails on head |
| `quality:red-test-inconclusive` | Verdict | The §8 test does not compile without the fix |
| `ortsom:steps-passed`, `ortsom:steps-failed`, `ortsom:steps-pass-on-main` | Ortsom verdict | §9 results |

## 13. Rollout

| Phase | Repository | Content |
|---|---|---|
| 0 | mostro | This spec |
| 1 | mostro | `CONTRIBUTING.md` sections (quality bar, Manual testing, red test commit convention), `.github/pull_request_template.md`, `.github/quality/close-message.md`, labels. Maintainers apply the policy by hand |
| 2 | mostro | `quality-triage.yml`, `triage.py` with tests, `config.toml`. **Shadow**: labels and comment only, `neutral` check run |
| 3 | mostro | `quality-red-test.yml` and `quality-verdict.yml`. **Shadow** |
| 4 | mostro | Enforcement: `quality-stale.yml` closes after 7 days of `quality:needs-info`. Needs a separate, explicit decision after reviewing the shadow labels |
| 5 | ortsom, then mostro | §9: an Ortsom spec and the `--steps` runner, then the steps run in `ortsom-pr.yml`. Depends on the Ortsom gate being live |

Phase 1 is documentation only, and it should already remove most of the
pull requests this spec is about: a required accepted issue and a Manual
testing section that must name a step failing on `main` are enough to
close them with a clear, written reason.

### Exit criteria for Phase 4

- At least 4 weeks of shadow labels.
- Every `quality:would-close` of that period reviewed by a maintainer,
  with at most one a maintainer would not have closed.
- The standard close message approved by at least two maintainers.

## 14. Known gaps

- **A description can be fabricated.** A plausible Manual testing
  section can be written without running anything. Until Phase 5 makes
  the steps executable, the defence is a reviewer following them, and
  §3.3 makes a procedure that does not work a close reason.
- **Features and refactors have no red test.** §8 applies to fixes
  only. For the rest, the accepted issue and the Ortsom gate are the
  filters.
- **Flows Ortsom cannot drive** (admin RPC, Cashu and the other
  `uncovered` entries of the Ortsom map) cannot have structured steps.
  Their Manual testing stays prose.
- **The review load moves; it does not disappear.** Accepting issues
  (§4) is new work for maintainers. It is cheaper than reviewing diffs,
  but it is not free.
