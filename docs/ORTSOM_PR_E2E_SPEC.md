# Ortsom on Pull Requests — Automated E2E Gate

**Status:** Spec · Phase 0 (this document)
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
  does not compile is reported by the Rust workflow; this gate reports it
  as inconclusive and adds nothing.
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
├── map.toml          # path → scenario rules and gate settings
├── scenarios.json    # `ortsom list --json` of the pinned Ortsom, committed
├── select.py         # changed files → selection.json
├── verdict.py        # PR summary + baselines → verdict.json + comment.md
└── tests/            # unittest suites for both scripts
```

Python is used because it is on every GitHub runner and `tomllib` is in
the standard library (3.11+). No third-party packages.

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
paths = ["**/*.md", "docs/**", "LICENSE", ".github/ISSUE_TEMPLATE/**"]
ignore = true

[[rule]]
name = "trade flow"
paths = [
  "src/app/take_sell.rs", "src/app/take_buy.rs", "src/app/add_invoice.rs",
  "src/app/fiat_sent.rs", "src/app/release.rs", "src/app/order.rs",
]
tags = ["happy-path"]

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
name = "scheduler"
paths = ["src/scheduler.rs"]
tags = ["expiration"]

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
  "migrations/**", "Cargo.toml", "Cargo.lock", "rust-toolchain.toml",
]
full = true
```

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

   A remaining file matched by no rule is recorded as **unmapped**; it
   contributes nothing beyond `always_tags`.
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

`select.py` writes `selection.json`:

```json
{
  "mode": "subset",
  "scenarios": ["cancel_before_taken", "happy_buy", "happy_sell", "range_order"],
  "matched_rules": ["trade flow"],
  "unmapped_files": ["src/app/restore_session.rs"],
  "ignored_files": ["docs/ARCHITECTURE.md"]
}
```

`mode` is one of `none`, `subset` or `full`. The comment lists
`unmapped_files` explicitly: they are code paths Ortsom does not cover,
and the comment must not imply they were tested.

## 5. Baseline of `main` — `ortsom-baseline.yml`

The pull request is not compared against a second run of its own base.
`main` is measured separately and periodically, and every pull request
compares against those measurements.

- **Triggers:** `push` to `main`, `schedule` every 6 hours, and
  `workflow_dispatch`.
- **What runs:** the **full** suite (`ortsom run` with no filter) against
  `ortsom stack up --ref <github.sha>`, with the pinned Ortsom. Full,
  because pull requests select different subsets and each one needs a
  baseline result for every scenario it runs.
- **Concurrency:** group `ortsom-baseline`, `cancel-in-progress: false`.
  A push while a scheduled run is in flight queues behind it.
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
  `ready_for_review` and `labeled`, targeting `main`.
- **Skipped when:** the pull request is a draft, or carries
  `ortsom:skip`, or is authored by `dependabot[bot]`. A `labeled` event
  only proceeds when the label is `ortsom:run`, which forces a re-run.
  A skip still runs one cheap job that uploads the `ortsom-pr` artifact
  with only `meta.json`, carrying `skipped` and its reason, so the
  verdict workflow always has something to act on (removing stale
  verdict labels, removing `ortsom:run`).
- **Concurrency:** group `ortsom-pr-<number>`, `cancel-in-progress: true`.
- **Permissions:** `contents: read` only. No secrets. This workflow never
  adds or removes a label or writes a comment; every change to the pull
  request is made by `ortsom-verdict.yml` (§9.1).

Steps:

1. **Select.** Check out the pull request with full history
   (`fetch-depth: 0`, so the base SHA, the head SHA and their merge-base
   are all present), list changed files with
   `git diff --name-status <base_sha>...<head_sha>`, run `select.py`. If
   `mode` is `none`, upload the artifact and stop.
2. **Harness.** Check out `MostroP2P/ortsom` at `ortsom_ref` — or at the
   override of §8.2 — and `cargo build --release` it (Swatinem cache).
   Ortsom publishes no release binaries today; the build is cached across
   runs.
3. **Stack.** `ortsom stack up --ref <head_sha>`. Its exit code (§10,
   item C) classifies the outcome: 0 is `ok`, 3 is `build_failed`, 4 is
   `daemon_failed`, anything else is `infra_failed`.
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
   to the pinned `ortsom_ref` the PR run used. Comparing across harness
   revisions would blame the pull request for a harness change.
3. The **reference baseline** is the most recent candidate whose
   `meta.sha` is on that walk and whose `stack` is `ok`.
4. The **history** is the `stability_window` most recent candidates on
   the same walk, the reference included. Fewer than `stability_window`
   candidates is fine; the rule then uses the ones there are.
5. No reference baseline → verdict `inconclusive`, reason
   `no-baseline`. This is expected for a few hours after `ortsom_ref` is
   bumped on `main`, until the baseline workflow the bump itself
   triggers has finished.

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
| `would-close` | `stack` is `daemon_failed`, the reference baseline's stack was `ok`, and no cap applies | `ortsom:would-close` |
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
would fail the same way, and the same stack starts on `main`. So it
needs no minimum — but it is still subject to the caps.

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
- `unmapped_files`, the code this run did not exercise;
- links to the PR run, its artifacts, and the baseline run;
- how to re-run (push, or add `ortsom:run`), and the escape hatches of
  §8;
- in shadow mode, the line: *Shadow mode: this pull request would have
  been closed. It stays open; a maintainer reviews this verdict.*

## 8. Escape hatches

### 8.1 Labels

| Label | Effect |
|---|---|
| `ortsom:skip` | No run. The verdict workflow removes any verdict label. |
| `ortsom:run` | Forces a re-run. The verdict workflow removes it once that run reports, so adding it again re-runs again. |
| `ortsom:expected-break` | Regressions are reported, but the verdict is capped at `regression`. For intentional behaviour changes the harness has not caught up with yet. |
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
  `main` only, for `verdict.py`, `select.py`, `map.toml` and
  `scenarios.json`;
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
`map.toml` or `select.py` to select nothing or to forge a passing
`summary.json`. The verdict workflow limits the damage:

- It **recomputes the selection** with `main`'s `map.toml`,
  `scenarios.json` and `select.py` from the changed-file list fetched through the API. A
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
read-only token — the same exposure as the existing `cargo test` job.

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
| 3 | the daemon image could not be produced: ref not found, or mostro did not compile |
| 4 | the daemon image exists but mostrod did not start (exited, restart loop, timeout waiting for it) |
| 1 | anything else (Docker, bitcoind, LND, relay, config) |

Documented in the README and `docs/ci-regtest.md`.

## 11. Rollout

| Phase | Repository | Content |
|---|---|---|
| 0 | mostro | This spec. |
| 1 | ortsom | §10 A–C, docs, changelog; release `v0.3.0`. |
| 2 | mostro | `.github/ortsom/` (`map.toml`, `scenarios.json`, `select.py`, tests) and `ortsom-baseline.yml`. Let baselines accumulate. |
| 3 | mostro | `ortsom-pr.yml`, `verdict.py` with tests, `ortsom-verdict.yml`; labels created. **Shadow mode.** |
| 4 | — | Manual review period, at least two weeks or 20 verdicts. Every `would-close` and `regression` is checked by a maintainer; disagreements get `ortsom:false-positive`. Tune `map.toml` and thresholds. |
| 5 | mostro | Enforcement, only by explicit maintainer decision (§12). |

### Exit criteria for Phase 4

- No `ortsom:false-positive` among the last 10 `would-close` verdicts, or
  a documented, fixed root cause for each one.
- `inconclusive` below 20% of runs; otherwise the baseline or the stack
  is too unreliable to enforce anything.

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

- **Bonds.** The regtest stack runs with `[anti_abuse_bond]` off, so the
  `bond` scenarios are always skipped: a pull request touching
  `src/app/bond/**` gets no signal. A bonds-enabled stack variant is
  follow-up work in Ortsom.
- **Uncovered code.** `restore_session`, `last_trade_index`,
  `trade_pubkey`, `cashu`, `price` and `rpc` have no scenarios. They are
  reported as unmapped rather than claimed as tested.
- **Serial runs.** One identity slot means `--jobs 1`. More slots would
  shorten `full` runs considerably.
- **Fork commits.** Building `--ref <head_sha>` for a pull request from a
  fork relies on GitHub serving that commit from the base repository
  through `refs/pull/<n>/head`. To be verified in Phase 3; the fallback
  is `--ref <number>` plus a check that the built commit equals
  `head_sha`.
