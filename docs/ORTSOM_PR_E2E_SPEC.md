# Ortsom on Pull Requests — Automated E2E Gate

**Status:** Phase 2 (selection, map and `main` baseline). Phase 3 pending
**Harness:** [MostroP2P/ortsom](https://github.com/MostroP2P/ortsom)
**Initial mode:** label-only (shadow). Pull requests are never closed until
Phase 5 is explicitly enabled.

## 1. Goal

Every pull request that changes daemon behaviour is run through the
Ortsom end-to-end suite automatically, against a mostrod built from that
pull request, on the self-contained regtest stack. The scenarios run are
chosen from what the pull request touches. The result is compared with a
periodic baseline of `main`, and the pull request is labelled with a
verdict that says whether it — and not `main`, the harness or the runner —
broke scenarios.

When a pull request is responsible for **at least 50% of the eligible
scenarios failing, over at least 4 eligible scenarios**, it is the
candidate for automatic closing. During the initial phase that verdict is
only a label (`ortsom:would-close`) so maintainers can audit it by hand
before anything is closed.

### Non-goals

- Replacing `cargo test`, clippy or the existing CI. A pull request that
  does not compile is reported as `inconclusive` (reason `build-failed`),
  not as a regression: whether it compiles is the Rust CI's question.
  Note that today no Rust workflow runs on `pull_request` — `ci.yml`
  runs on `push`, which covers branches of this repository but not
  forks ([#929](https://github.com/MostroP2P/mostro/issues/929)). Until
  that is fixed, a fork pull request that does not compile is reported by
  nothing but this gate's comment; see §14.
- Running on mainnet or with real sats. Only the regtest stack is used.
- Testing Ortsom itself. Ortsom has its own CI.

## 2. What already exists in Ortsom

Nothing in this design needs a new test harness. Ortsom already provides:

| Capability | How |
|---|---|
| Build mostrod from any ref | `ortsom stack up --ref <branch\|tag\|sha\|PR>`, image `ortsom/mostro:<commit>` |
| Self-contained world, no secrets | bitcoind regtest, 3 LND nodes, in-memory relay, mostrod with a per-run identity and solver |
| Readiness gate | `ortsom doctor` (exits non-zero on an unusable stack) |
| Scenario selection | `ortsom run <names…>` or `ortsom run --tag <tag>` |
| Flakiness absorption | `retries = 1` in `ortsom.regtest.toml`; results carry `attempts` and `flaky` |
| Machine-readable report | `artifacts/<run_id>/summary.json` (`results[]` with `name`, `outcome`, `detail`, `duration_secs`, `attempts`, `flaky`; `metrics`; `totals`) |
| Cleanup and teardown | `ortsom cleanup`, `ortsom stack down` |

Scenario outcomes are `passed`, `failed`, `skipped` and `interrupted`.
A scenario whose requirements the setup cannot meet (bonds disabled,
wallet without fault injection) is **skipped**, never failed.

## 3. Architecture

Three workflows in this repository, plus one small release of Ortsom.

```text
push to main ─┐
cron (6 h) ───┴─► ortsom-baseline.yml ──► artifact ortsom-baseline (per main SHA)
                                                     │
pull_request ───► ortsom-pr.yml ──► artifact ortsom-pr (selection + summary)
                                                     │
                  workflow_run(completed) ──► ortsom-verdict.yml
                                               - trusted code from main only
                                               - recompute selection
                                               - fetch baseline for merge-base
                                               - compute verdict
                                               - sticky comment + labels
```

The split between `ortsom-pr.yml` and `ortsom-verdict.yml` is a security
boundary, see §9.

All selection and verdict logic lives in version-controlled scripts under
`.github/ortsom/`, with unit tests, so the workflows stay thin.

```text
.github/ortsom/
├── map.toml             # path → scenario rules and gate settings
├── scenarios.json       # `ortsom list --json` of the pinned Ortsom, committed
├── select_scenarios.py  # changed files → selection.json
├── verdict.py           # PR summary + baselines → verdict.json + comment.md
└── tests/               # unittest suites for both scripts
```

Python is used because it is on every GitHub runner and `tomllib` is in
the standard library (3.11+). No third-party packages. The selection
script is not called `select.py`: a module of that name next to the
script shadows the standard library's `select`, which `subprocess`
imports.

## 4. Scenario selection

### 4.1 The map

`.github/ortsom/map.toml` holds the gate settings and a list of rules.
Each rule maps path globs to tags and/or scenario names.

```toml
[settings]
# Ortsom revision the gate runs. Bumped by PR, like any dependency.
ortsom_ref = "v0.3.0"
# Tags every non-ignored selection includes.
always_tags = ["smoke"]
# Minimum eligible scenarios for the close verdict to be possible.
min_scenarios = 4
# Regression ratio (regressions / eligible) that triggers the close verdict.
close_ratio = 0.5
# How many first-parent commits back from the merge-base a baseline may be.
baseline_max_distance = 10
# How many recent baselines a scenario must have passed to be "stable".
stability_window = 3

[[rule]]
name = "docs and metadata"
paths = ["**/*.md", "docs/**", "LICENSE"]
ignore = true

[[rule]]
name = "trade flow"
paths = [
  "src/app/take_sell.rs", "src/app/take_buy.rs", "src/app/add_invoice.rs",
  "src/app/fiat_sent.rs", "src/app/release.rs", "src/app/order.rs",
  "src/app/orders.rs", "src/app/dev_fee.rs",
]
tags = ["happy-path"]

[[rule]]
name = "pricing"
paths = ["src/price/**", "src/bitcoin_price.rs"]
scenarios = ["market_price_order", "range_order"]

[[rule]]
name = "maintenance mode"
paths = ["src/app/maintenance.rs", "src/app/daemon_state.rs"]
tags = ["happy-path", "cancellation"]

[[rule]]
name = "cancellation"
paths = ["src/app/cancel.rs"]
tags = ["cancellation"]

[[rule]]
name = "disputes and admin"
paths = ["src/app/dispute.rs", "src/app/admin_*.rs"]
tags = ["dispute"]

[[rule]]
name = "rating"
paths = ["src/app/rate_user.rs"]
scenarios = ["full_trade_with_rating"]

[[rule]]
# Not only expirations: it flushes every outgoing message and runs payment
# retries, payout reconciliation, price updates, dev fee and bond payouts.
name = "scheduler"
paths = ["src/scheduler.rs"]
full = true

[[rule]]
name = "lightning"
paths = ["src/lightning/**"]
tags = ["payment-failure", "happy-path"]

[[rule]]
name = "message intake"
paths = ["src/messages.rs", "src/spam_gate.rs", "src/nip33.rs", "src/util.rs"]
tags = ["adversarial"]

[[rule]]
name = "bonds"
paths = ["src/app/bond/**"]
tags = ["bond"]

[[rule]]
name = "core plumbing"
paths = [
  "src/flow.rs", "src/db.rs", "src/app.rs", "src/main.rs", "src/config/**",
  "src/escrow.rs", "src/app/context.rs",
  "migrations/**", "Cargo.toml", "Cargo.lock", "rust-toolchain.toml",
]
full = true

[[rule]]
name = "admin RPC"
paths = ["src/rpc/**", "proto/**"]
uncovered = "ortsom has no gRPC client; admin actions it exercises go over Nostr (see the dispute rule)"

[[rule]]
name = "cashu"
paths = ["src/cashu/**", "src/app/add_cashu_escrow.rs"]
uncovered = "covered by cashu.yml, not by ortsom"

[[rule]]
name = "session restore and key bookkeeping"
paths = ["src/app/restore_session.rs", "src/app/last_trade_index.rs", "src/app/trade_pubkey.rs"]
uncovered = "no ortsom scenario yet"

[[rule]]
name = "misc daemon entry points"
paths = ["src/cli.rs", "src/lnurl.rs"]
uncovered = "no ortsom scenario yet (the stack pays bolt11 invoices, never LNURL)"
```

Every `src/**/*.rs` file and every file under `proto/` matches at least
one rule on the `main` of this spec's date. Code that Ortsom cannot
exercise is not left out of the map: it is matched by a rule carrying
`uncovered = "<reason>"`. Such a rule selects nothing beyond
`always_tags`, and the comment names the files and the reason. That
separates **known uncovered** code, a decision written down with its
reason, from **unmapped** code, which is a gap in the map.

The map above is the initial content; it is expected to be tuned during
the shadow phase.

### 4.2 Glob semantics

- Paths are repository-relative, `/`-separated.
- `*` matches any run of characters except `/`.
- `**` matches any run of characters including `/` (zero or more path
  segments). `docs/**` matches every file under `docs/`.
- `?` matches one character except `/`.
- No brace expansion, no negation. Order of rules does not matter; all
  matching rules contribute.

### 4.3 Algorithm

Input: the list of files the pull request changes (added, modified,
removed or renamed — both old and new name for renames).

1. Drop every file matched by an `ignore = true` rule.
2. If nothing is left, the selection is **empty**: no run, verdict
   `not-applicable` (no label is applied; any previous `ortsom:*` verdict
   label is removed).
3. If any remaining file matches a `full = true` rule, the selection is
   **every scenario** (`ortsom run` with no filter).
4. Otherwise the selection is the union of:
   - `always_tags`,
   - the `tags` and `scenarios` of every rule matched by a remaining file.

   A remaining file matched only by `uncovered` rules is recorded as
   **uncovered**, with the rule's reason. A remaining file matched by no
   rule at all is recorded as **unmapped**. Neither contributes anything
   beyond `always_tags`.
5. Tags are resolved to scenario names with `.github/ortsom/scenarios.json`,
   the committed output of `ortsom list --json` for the pinned
   `ortsom_ref`. A tag no scenario carries, or a scenario name that does
   not exist, is a **hard error** of the selection step (the map is stale
   and must be fixed; the verdict is `inconclusive` with that reason).

The registry is a committed snapshot rather than a live call, so that
selection needs no Ortsom build: the PR run selects before it builds the
harness, and the verdict workflow recomputes the selection without
building anything (§9.2). The snapshot is bumped in the same pull request
as `ortsom_ref`. Once the PR run has built the harness, it compares
`ortsom list --json` with the snapshot. A mismatch means the snapshot is
stale: it is recorded in `meta.json` as `registry_stale: true` and the
verdict is `inconclusive`, reason `stale-registry`. A pull request that
overrides the Ortsom ref (§8.2) skips this check and selects against the
override's own `list --json`.

`select_scenarios.py` writes `selection.json`:

```json
{
  "mode": "subset",
  "scenarios": ["cancel_before_taken", "happy_buy", "happy_sell", "range_order"],
  "matched_rules": ["trade flow"],
  "uncovered_files": {"src/rpc/service.rs": "ortsom has no gRPC client; …"},
  "unmapped_files": ["src/new_module.rs"],
  "ignored_files": ["docs/ARCHITECTURE.md"]
}
```

`mode` is one of `none`, `subset` or `full`. The comment lists
`uncovered_files` and `unmapped_files` explicitly: they are code paths
Ortsom did not exercise, and the comment must not imply they were
tested.

### 4.4 Keeping the map complete

`.github/ortsom/tests/` holds a **coverage test** for the map: it walks
`git ls-files` and asserts that every `src/**/*.rs` file and every file
under `proto/` matches at least one rule (`ignore`, `uncovered`, `full`
or a selecting rule). A new module that nobody mapped then fails a unit
test in the pull request that adds it, instead of silently running
smoke only. It only reads the tree and the map, so it runs in seconds
in its own workflow, `ortsom-map.yml`, on every pull request and every
push to `main`, together with the scripts' unit tests. It is independent
of `ortsom-pr.yml` and its skip path, so neither `ortsom:skip` nor a
draft or Dependabot pull request lets an unmapped file through; a
failure shows as a red check on that workflow.

## 5. Baseline of `main` — `ortsom-baseline.yml`

The pull request is not compared against a second run of its own base.
`main` is measured separately and periodically, and every pull request
compares against those measurements.

- **Triggers:** `push` to `main`, `schedule` every 6 hours, and
  `workflow_dispatch`.
- **Harness access:** Ortsom is a private repository, internal to the
  Mostro developers. `GITHUB_TOKEN` cannot read it, so the workflow checks
  it out with a read-only deploy key (secret `ORTSOM_DEPLOY_KEY`, deploy
  key `mostro-ci` on `MostroP2P/ortsom`), handed to that step only.
- **What runs:** the **full** suite (`ortsom run` with no filter) against
  `ortsom stack up --ref <github.sha>`, with the pinned Ortsom. Full,
  because pull requests select different subsets and each one needs a
  baseline result for every scenario it runs.
- **Concurrency:** group `ortsom-baseline`, `cancel-in-progress: false`.
  A push while a run is in flight waits behind it. GitHub keeps at most
  one pending run per group, so several pushes during one run collapse
  into a single baseline of the latest one; the commits in between get
  none, which `baseline_max_distance` (§7.1) absorbs.
- **Output:** artifact `ortsom-baseline`, retention 90 days, containing:
  - `summary.json` as written by Ortsom,
  - `meta.json` with `sha`, `ortsom_ref`, `stack`
    (`ok`, `build_failed`, `daemon_failed`, `infra_failed` or
    `doctor_failed`, classified as in §6), `run_id` and `started_at`,
  - `suite.log`, `stack.log`, `events.jsonl` for debugging.
- **Timeout:** job 330 min, suite step 300 min (same structure as
  Ortsom's nightly: always-steps must still run).
- The run's own conclusion is informational; a failing scenario on
  `main` is exactly the information the baseline records. It is also
  surfaced in the job summary so a broken `main` is visible.

Scheduled reruns on an unchanged `main` are intentional: they build the
per-scenario history that the stability rule (§7.2) uses to exclude
flaky scenarios.

## 6. Pull request run — `ortsom-pr.yml`

- **Trigger:** `pull_request` on `opened`, `synchronize`, `reopened`,
  `ready_for_review`, `labeled` and `unlabeled`, targeting `main`.
- **Label events.** Only three labels are relevant: `ortsom:run`
  (added), `ortsom:skip` (added or removed) and `ortsom:expected-break`
  (added or removed). Each of those proceeds as an ordinary run, so the
  label takes effect at once: adding `ortsom:skip` takes the skip path
  below, removing it runs the suite, and adding or removing
  `ortsom:expected-break` re-runs so the verdict reflects the cap. Any
  other label event is **irrelevant**: every job's `if` is false, no
  artifact is uploaded, and the verdict workflow, finding no artifact,
  leaves the pull request untouched.
- **Skipped when:** the pull request is a draft, or carries
  `ortsom:skip`, or is authored by `dependabot[bot]`. A skip still runs
  the map coverage job (§4.4) and one cheap job that uploads the
  `ortsom-pr` artifact with only `meta.json`, carrying `skipped` and its
  reason, so the verdict workflow always has something to act on
  (removing stale verdict labels, removing `ortsom:run`).
- **Concurrency:** on the suite job, not on the workflow: group
  `ortsom-pr-<number>`, `cancel-in-progress: true`. GitHub applies a
  workflow-level group before any job's `if` is evaluated, so there an
  irrelevant label event would cancel a suite already in flight — up to
  a `full` run of more than two hours. A job whose `if` is false never
  enters its group. The skip job joins the same group, so adding
  `ortsom:skip` cancels a suite already in flight instead of letting it
  finish and report after the skip. The map coverage job stays out of
  it.
- **Timeout:** suite job 330 min, suite step 300 min, as in §5.
- **Permissions:** `contents: read` only. No secrets. This workflow never
  adds or removes a label or writes a comment; every change to the pull
  request is made by `ortsom-verdict.yml` (§9.1).

Steps:

1. **Select.** Check out the pull request's merge commit (the
   `pull_request` default) with full history (`fetch-depth: 0`, so the
   base SHA, the head SHA and their merge-base are all present). The
   `map.toml`, `scenarios.json` and `ortsom_ref` of the run come from that
   checkout, i.e. from the current `main` plus the pull request's own
   changes. List changed files with
   `git diff --name-status <base_sha>...<head_sha>`, run `select_scenarios.py`. If
   `mode` is `none`, upload the artifact and stop.
2. **Harness.** Check out `MostroP2P/ortsom` at `ortsom_ref` — or at the
   override of §8.2 — and `cargo build --release` it (Swatinem cache).
   Ortsom publishes no release binaries today; the build is cached across
   runs.
3. **Stack.** `ortsom stack up --ref <head_sha>`. Its exit code (§10,
   item C) classifies the outcome: 0 is `ok`, 3 is `build_failed`, 4 is
   `daemon_failed`, anything else is `infra_failed`. An exit code 3 is
   retried once before it is recorded, because a download failing inside
   the image build looks the same as mostro not compiling.
4. **Readiness.** If the stack is `ok`, `ortsom doctor`. A non-zero exit
   turns the outcome into `doctor_failed`. By then `stack up` has already
   seen the daemon publish its info event, so what `doctor` still catches
   is mostly the rest of the world (relay reachability, wallet
   funding, the limits in the settings template) — not something to
   blame on the pull request. The outcome is recorded in `meta.json`.
5. **Run.** Only if the outcome is `ok`: `ortsom run <scenarios…>` (or no
   filter for `full`), `--jobs 1`. Its exit code is recorded in
   `meta.json` as `suite_exit`; a step timeout records `null`.
6. **Always:** `ortsom cleanup`, collect `stack.log`, upload artifact
   `ortsom-pr`, `ortsom stack down`.

Artifact `ortsom-pr` contains `meta.json`:

```json
{
  "pr": 980,
  "head_sha": "0123456789abcdef0123456789abcdef01234567",
  "base_sha": "89abcdef0123456789abcdef0123456789abcdef",
  "ortsom_ref": "v0.3.0",
  "ortsom_ref_overridden": false,
  "registry_stale": false,
  "skipped": null,
  "stack": "ok",
  "suite_exit": 1
}
```

plus `selection.json`, `summary.json` (if the run happened), `suite.log`,
`stack.log` and Ortsom's per-scenario artifacts.

The job's own conclusion is **success** whenever the pipeline worked,
whatever the scenarios did: a red check would duplicate the verdict and
mislead during the shadow phase. The verdict lives in the label and the
comment. Making the gate a required check is a Phase 5 decision.

## 7. Verdict

### 7.1 Finding the baseline

1. Take the merge-base from the trusted API
   (`GET /repos/{owner}/{repo}/compare/main...{head_sha}`, field
   `merge_base_commit.sha`), not from the artifact.
2. Walk `main`'s first-parent history from the merge-base back
   `baseline_max_distance` commits. Only baselines measured with the
   **same harness** as the PR run are candidates: `meta.ortsom_ref` equal
   to the `ortsom_ref` pinned in `main`'s `map.toml`, which is also the one
   the PR run used unless it was overridden (§8.2). Comparing across
   harness revisions would blame the pull request for a harness change.
   Only candidates whose `stack` is `ok` carry results; the others are
   passed over.
3. The **reference baseline** is the most recent candidate whose
   `meta.sha` is on that walk.
4. The **history** is the `stability_window` most recent candidates on
   the same walk, the reference included. Fewer than `stability_window`
   candidates is fine; the rule then uses the ones there are.
5. No reference baseline → verdict `inconclusive`, reason
   `no-baseline`. After `ortsom_ref` is bumped on `main`, this is the
   verdict of every pull request whose merge-base predates the bump:
   the PR run uses the new harness (step 1 of §6 reads the pin from the
   merge commit), while every baseline on its walk used the old one. It
   stays so until the pull request merges or rebases onto a `main` whose
   new-harness baseline has finished, and the comment says so in those
   words. A pull request branched after the bump waits only for that
   first baseline.

Artifacts are listed through the Actions API for `ortsom-baseline.yml`
runs on `main`; each run carries its `head_sha`.

### 7.2 Classifying each selected scenario

For every scenario in the trusted selection (§9.2), with PR outcome `P`:

| Condition | Class | Eligible? |
|---|---|---|
| `P` is `skipped` | `skipped` | no |
| Scenario absent from the reference baseline | `no-baseline` | no |
| Not `passed` in the reference baseline, or `flaky`, `failed` or `interrupted` in any run of the history | `unstable-on-main` | no |
| `P` is `passed` | `pass` (noted `flaky` if it needed a retry) | yes |
| `P` is `failed` | `regression` | yes |
| Scenario missing from the PR summary | `missing`, counted as `regression` | yes |

`eligible = pass + regression` and `ratio = regression / eligible`.

This table applies only to a **completed** run. Ortsom writes
`summary.json` once, after every scenario has finished, so its presence
is the completion marker. The run is **incomplete** — and the verdict
`inconclusive`, reason `incomplete-run`, with no scenario classified —
when the stack was `ok` but `summary.json` is absent (the suite step
timed out or the runner died), or when any result is `interrupted` (the
run was cancelled). An incomplete run never produces `missing` rows. In a
completed run, a missing row means the PR run did not execute a scenario
the trusted selection asked for (§9.2), which is why it counts against
the pull request.

### 7.3 Verdicts

Evaluated in order; the first that applies wins.

| Verdict | When | Label |
|---|---|---|
| `not-applicable` | selection `mode` is `none` | none; a previous `ortsom:*` verdict label is removed |
| `inconclusive` | selection error, stale registry (§4.3), gate modified (§9.2), `stack` is `build_failed`, `infra_failed` or `doctor_failed`, incomplete run (§7.2), or no reference baseline | `ortsom:inconclusive` |
| `would-close` | `stack` is `daemon_failed` and no cap applies | `ortsom:would-close` |
| `would-close` | `eligible ≥ min_scenarios` and `ratio ≥ close_ratio`, and no cap applies | `ortsom:would-close` |
| `regression` | a `would-close` condition held but a cap applies, or at least one `regression` | `ortsom:regression` |
| `pass` | otherwise | `ortsom:pass` |

**Caps.** `ortsom:expected-break` and an Ortsom override (§8.2) each cap
the verdict at `regression`. They apply to both `would-close` rows: a
pull request that adds a mandatory daemon setting fails to start against
the pinned settings template, which is precisely the case the override
exists for.

**The `daemon_failed` row is a deliberate exception to `min_scenarios`
and `close_ratio`.** Those guards protect against drawing a conclusion
from a small or noisy sample of scenarios. A daemon that builds but does
not start is not a sample: every scenario the pull request could select
would fail the same way, and the same stack starts on `main` (the
reference baseline, which exists by now, has `stack` `ok` by
definition). So it needs no minimum — but it is still subject to the
caps.

Verdict labels are mutually exclusive: applying one removes the others.
They are re-evaluated on every run, so a fixed pull request goes from
`ortsom:would-close` back to `ortsom:pass` on the next push.

`verdict.py` writes `verdict.json` (the classification of every scenario,
the counts, the verdict and its reason) and `comment.md`.

### 7.4 The comment

One sticky comment per pull request, found by the marker
`<!-- ortsom-verdict -->` and edited in place. It contains:

- the verdict, in one sentence, with the numbers, e.g. *3 of 5 eligible
  scenarios regressed (60%, threshold 50%, minimum 4)*;
- the daemon commit tested, the Ortsom revision, and the baseline commit
  compared against with its distance from the merge-base;
- a table of regressions: scenario, PR detail, baseline outcome;
- collapsed sections for passes, skipped, `unstable-on-main` and
  `no-baseline` scenarios;
- `uncovered_files` with their reasons and `unmapped_files`, the code
  this run did not exercise;
- links to the PR run, its artifacts, and the baseline run;
- how to re-run (push, or add `ortsom:run`), and the escape hatches of
  §8;
- in shadow mode, the line: *Shadow mode: this pull request would have
  been closed. It stays open; a maintainer reviews this verdict.*

## 8. Escape hatches

### 8.1 Labels

| Label | Effect |
|---|---|
| `ortsom:skip` | No suite run; the map coverage job (§4.4) still runs. The verdict workflow removes any verdict label. Takes effect when added, cancelling a suite in flight; removing it runs the suite. |
| `ortsom:run` | Forces a re-run. The verdict workflow removes it once that run reports, so adding it again re-runs again. |
| `ortsom:expected-break` | Regressions are reported, but the verdict is capped at `regression`. For intentional behaviour changes the harness has not caught up with yet. Adding or removing it re-runs. |
| `ortsom:false-positive` | Set by a maintainer who disagrees with a verdict. Changes nothing in the workflow; it is the data source for the shadow-phase review (§11). |

### 8.2 Running against an Ortsom branch

A pull request that changes the protocol, a message or a required
setting will legitimately break the pinned harness. The author opens the
matching Ortsom pull request and adds a line to the mostro pull request
body:

```text
ortsom-ref: feat/new-action
```

The PR run then uses that Ortsom ref: a branch, tag, commit or Ortsom
pull request number, resolved on `MostroP2P/ortsom` only. Because the
baseline was measured with the pinned Ortsom, the comparison mixes
harness versions: the verdict is capped at `regression` and the comment
says so.

The daemon settings template lives in Ortsom
(`ci/regtest/mostro-settings.tpl.toml`), so a mostro pull request that
adds a mandatory setting needs this path too.

## 9. Security

### 9.1 Why two workflows

A `pull_request` run from a fork gets a read-only token and no secrets,
which is what running untrusted code requires — but that token cannot
label or comment. `ortsom-verdict.yml` is triggered by `workflow_run`,
runs the workflow definition from `main` with a write token, and
therefore:

- **never checks out or executes pull request code**; it checks out
  `main` only, for `verdict.py`, `select_scenarios.py`, `map.toml` and
  `scenarios.json`;
- ignores a triggering run whose conclusion is `skipped`: it was an
  irrelevant label event (§6);
- on a triggering run whose conclusion is `cancelled`, looks for a
  **successor**: a run of `ortsom-pr.yml` with the same
  `head_repository.full_name` and `head_branch`, created after the
  cancelled one. If there is one, it does nothing, because the successor
  will report. If there is none, the run was cancelled by hand. A
  cancelled run may not have uploaded an artifact, so the pull request is
  resolved from the run's head repository and branch, and checked as
  below without the `meta` fields. The workflow then removes any verdict
  label and `ortsom:run`, and rewrites the comment to say that the last
  run was cancelled, that no verdict is current, and how to re-run;
- before applying a suite result, re-reads the pull request. If it now
  carries `ortsom:skip` or is a draft, the result is discarded and
  handled as a skip: verdict labels are removed and no scenario results
  are posted. The skip job cancels a suite in flight (§6), but a suite
  that finished moments before can still report after the skip, and
  this check makes both orders end in the same state;
- treats the `ortsom-pr` artifact as **untrusted data**: parsed as JSON
  with size limits, validated field by field (SHA format, known enum
  values, integer PR number), never interpolated into shell commands;
- resolves the pull request from `meta.pr` and, before touching it,
  **verifies** that the pull request belongs to the triggering run. For a
  pull request from a fork `workflow_run.pull_requests` is empty, so the
  association is checked field by field through the API:
  - the triggering run's `event` is `pull_request` and its workflow is
    `ortsom-pr.yml`;
  - the pull request's base is `main` of this repository;
  - its head repository (`head.repo.full_name`) equals the run's
    `head_repository.full_name`, and its head branch (`head.ref`) equals
    the run's `head_branch`;
  - its current head SHA equals both `meta.head_sha` and the run's
    `head_sha`.

  If any check fails it labels, comments and closes nothing. A head SHA
  mismatch is the ordinary case of a newer push superseding this run —
  the newer run will report; any other mismatch is logged as a rejected
  artifact;
- has permissions `pull-requests: write`, `issues: write`,
  `actions: read` and `contents: read`, and nothing else.

### 9.2 A pull request can edit its own workflow

For `pull_request` events GitHub runs the workflow files of the pull
request's merge commit, so a pull request can change `ortsom-pr.yml`,
`map.toml` or `select_scenarios.py` to select nothing or to forge a passing
`summary.json`. The verdict workflow limits the damage:

- It **recomputes the selection** with `main`'s `map.toml`,
  `scenarios.json` and `select_scenarios.py` from the changed-file list fetched through the API. A
  scenario in the trusted selection that is absent from the PR summary
  counts as `missing`, i.e. a regression.
- A pull request that touches `.github/ortsom/**` or any `ortsom-*.yml`
  workflow gets the verdict `inconclusive`, reason `gate-modified`.

A forged passing summary cannot be ruled out from inside the PR run; the
gate is a review aid, not a security control, and the label it applies
or withholds is worth nothing to an attacker.

### 9.3 Building untrusted code

The PR run compiles and executes the pull request's mostrod inside
Docker on an ephemeral GitHub-hosted runner, with no secrets and a
read-only token.

There is only a partial precedent for this in the repository.
`mutation.yml` and `cashu.yml` already build and run pull-request code on
`pull_request`, but only when a maintainer applies a label (`run-mutation`,
`cashu`). This gate is the first workflow to do it for **every** pull
request. The case for it rests on its own terms:

- the runner is ephemeral and discarded after the job;
- the job has no secrets and a `contents: read` token, so there is
  nothing to exfiltrate and nothing it can write back to the repository;
- the repository requires maintainer approval before workflows run for
  first-time contributors (fork approval policy
  `first_time_contributors`), so a drive-by pull request does not get
  compute without a human looking at it first.

What an attacker gets is runner minutes. That is the same exposure the
Rust CI will have once
[#929](https://github.com/MostroP2P/mostro/issues/929) adds
`pull_request` to it.

## 10. Changes needed in Ortsom (Phase 1)

Small, backwards-compatible changes, released together as Ortsom
`v0.3.0` so `map.toml` can pin them.

**A. `ortsom list --json`.** Writes to stdout a JSON array, one object
per scenario, sorted by name:

```json
[{ "name": "happy_sell", "tags": ["smoke", "happy-path"], "requires": [], "timeout_secs": 180 }]
```

The human-readable output stays the default.

**B. Repeatable `--tag`, union with names.** `ortsom run --tag smoke --tag
dispute happy_buy` runs the union of every named scenario and every
scenario carrying any given tag, deduplicated, in registry order. Today
names silently win over the tag and only one tag is accepted. An unknown
tag or name is still an error. `canary` keeps its single `--tag`.

**C. Distinct exit codes for `stack up` failures**, so a workflow can
tell whose fault a failed stack is without parsing text:

| Exit code | Meaning |
|---|---|
| 0 | stack up |
| 3 | the daemon image could not be produced: the ref does not exist, or `docker build` failed — usually mostro not compiling, but a download failing inside the build looks the same, hence the retry in §6 |
| 4 | the daemon image exists, but waiting for mostrod ran out: it exited, restart-looped, or never published its info event on a relay that was up |
| 1 | anything else: git or github.com unreachable, Docker (also mid-wait), bitcoind, LND, a relay that never answered, the config |

Documented in the README and `docs/ci-regtest.md`.

## 11. Rollout

| Phase | Repository | Content |
|---|---|---|
| 0 | mostro | This spec. |
| 1 | ortsom | §10 A–C, docs, changelog; release `v0.3.0`. |
| 2 | mostro | `.github/ortsom/` (`map.toml`, `scenarios.json`, `select_scenarios.py`, tests) and `ortsom-baseline.yml`. Let baselines accumulate. |
| 3 | mostro | `ortsom-pr.yml`, `verdict.py` with tests, `ortsom-verdict.yml`; labels created. **Shadow mode.** |
| 4 | — | Manual review period, at least two weeks or 20 verdicts. Every `would-close` and `regression` is checked by a maintainer; disagreements get `ortsom:false-positive`. Tune `map.toml` and thresholds. |
| 5 | mostro | Enforcement, only by explicit maintainer decision (§12). |

### Exit criteria for Phase 4

- No `ortsom:false-positive` among the last 10 `would-close` verdicts, or
  a documented, fixed root cause for each one.
- `inconclusive` below 20% of runs; otherwise the baseline or the stack
  is too unreliable to enforce anything.

The review also tracks, per run, the number of **smoke-only** selections
(files remained after ignores, but only `uncovered` or unmapped ones) and
the unmapped-file count. They say whether the map is converging, which
the verdict counts alone do not. For reference, replaying the initial
map of an earlier draft over the 29 pull requests merged in September
2026 gave 14 `full`, 7 `subset`, 5 smoke-only and 3 `none`; the five
smoke-only ones included the maintenance-mode work (#938, #940, #943,
#944), which the current map now selects `happy-path` and
`cancellation` for.

## 12. Phase 5: enforcement (not enabled by this spec)

A repository variable `ORTSOM_MODE` switches the verdict workflow:

- `label` (default, also when the variable is absent): as described
  above.
- `close`: a `would-close` verdict posts the comment, applies
  `ortsom:closed-regression` and closes the pull request.

Known cost of closing, to weigh before switching: when a collaborator or
bot closes a pull request, its author **cannot reopen it**, and a closed
pull request whose branch was force-pushed cannot be reopened by anyone.
An external contributor whose pull request is closed must open a new one
or ask a maintainer. The comment must say this and name both options. A
softer alternative worth considering at that point: convert to draft and
submit a `REQUEST_CHANGES` review instead of closing.

## 13. Cost

GitHub-hosted runners are free for public repositories; the cost is wall
time and queue contention.

| Run | Approx. duration |
|---|---|
| Cold mostrod build (per commit, no cross-run BuildKit cache) | 5–8 min |
| Stack up and doctor | 2–3 min |
| Smoke plus one family (typical PR) | 5–15 min |
| `expiration` family (serial, one identity slot) | up to ~60 min |
| Full suite (baseline, `full` PRs) | 1.5–2.5 h |

Baseline: about four scheduled runs a day plus one per merge.

## 14. Known gaps

- **Pull requests from forks cannot fetch Ortsom.** A `pull_request` run
  from a fork gets no secrets, so the deploy key of §5 is unavailable to
  `ortsom-pr.yml` as designed in §6. Phase 3 has to split it: the
  untrusted job only builds the mostrod image from the pull request and
  uploads it; a trusted `workflow_run` job fetches Ortsom with the key
  and runs the suite with `ortsom stack up --mostro-image`. That changes
  §6 and §9 and is specified before Phase 3 starts.
- **Bonds.** The regtest stack runs with `[anti_abuse_bond]` off, so the
  `bond` scenarios are always skipped: a pull request touching
  `src/app/bond/**` gets no signal. A bonds-enabled stack variant is
  follow-up work in Ortsom.
- **Uncovered code.** The admin gRPC surface (`src/rpc/**`,
  `proto/**`), Cashu, session restore, trade-key bookkeeping, the daemon
  CLI and LNURL have no Ortsom scenarios. The map lists them as
  `uncovered` with the reason, and the comment reports them rather than
  claiming they were tested. The admin RPC is the largest of these: it
  carries the maintenance drain counters, and exercising it needs a gRPC
  client in Ortsom.
- **No compile check on fork pull requests.** Until
  [#929](https://github.com/MostroP2P/mostro/issues/929) adds
  `pull_request` to the Rust CI, `build_failed` is reported only as this
  gate's `inconclusive` verdict, and the comment says plainly that the
  pull request does not compile.
- **Serial runs.** One identity slot means `--jobs 1`. More slots would
  shorten `full` runs considerably.
- **Fork commits.** Building `--ref <head_sha>` for a pull request from a
  fork relies on GitHub serving that commit from the base repository
  through `refs/pull/<n>/head`. To be verified in Phase 3; the fallback
  is `--ref <number>` plus a check that the built commit equals
  `head_sha`.
