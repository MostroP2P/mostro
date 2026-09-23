# Contributing

_This guide is based on the [Bisq contributing guide](https://github.com/bisq-network/bisq/blob/master/CONTRIBUTING.md)._

Anyone is welcome to contribute to Mostro. If you're looking for somewhere to start contributing, check out the [good first issue](https://github.com/MostroP2P/mostro/labels/good%20first%20issue) list.

## Communication Channels

Most communication about technical issues on Mostro happens on the development [Telegram group](https://t.me/mostro_dev), non-technical discussions happens on this [Telegram group](https://t.me/MostroP2P). Discussion about code changes happens in GitHub issues and pull requests.

## Contributor Workflow

All Mostro contributors submit changes via pull requests. The workflow is as follows:

- Fork the repository
- Create a topic branch from the `main` branch
- Commit patches
- Squash redundant or unnecessary commits (but keep the `test:` commit of a fix separate, see [Red test](#red-test))
- Submit a pull request from your topic branch back to the `main` branch of the main repository
- Make changes to the pull request if reviewers request them and request a re-review

Pull requests should be focused on a single change. Do not mix, for example, refactorings with a bug fix or implementation of a new feature. This practice makes it easier for fellow contributors to review each pull request.

Contributions are written in **English** — commit messages, pull request titles and bodies, issue and review comments, documentation, and code comments. Discussion in other languages is welcome on Telegram, but anything that lands in the repository should be readable by every contributor.

### Protocol / Tag Changes

Changes that affect Nostr event tags (e.g. `y` tags, `z` tags) or event kinds are **protocol changes** and deserve extra care:

- **Single-kind PRs preferred** – limit each PR to one event domain (orders, info, dispute, admin, dev-fee) whenever possible.
- **Cross-kind changes** – if a PR must touch multiple event domains, include a "Scope Declaration" section in the PR body (see template below) and explain why the change cannot be split.
- **Compatibility statement** – every protocol-tag PR must state the impact on external consumers (indexers, clients, relays).

#### PR Body Template for Protocol Changes

```markdown
## Scope Declaration
- **Event domains affected:** (e.g. orders, info, dispute)
- **Cross-kind change:** yes / no
- **Reason (if cross-kind):** …
- **External compatibility impact:** (indexers, clients, relays)
```

## Contribution quality bar

A pull request has to prove two things before a reviewer reads the diff: that the problem it addresses exists, and that the change does not break anything else. The rules below are about the evidence a pull request carries, not about how it was written.

1. **AI tools are allowed.** You are responsible for every line and must be able to explain any of it when a reviewer asks. "The tool wrote it" is not an answer.
2. A pull request that does not follow the [template](#pull-request-template), does not link an [accepted issue](#issue-before-pull-request) when one is required, or does not meet the [Manual testing](#manual-testing) and [Red test](#red-test) rules below, **is closed without technical review**, with the [standard message](.github/quality/close-message.md). You can reopen it once it meets the bar.
3. A pull request whose description does not match its diff, or whose Manual testing steps do not work when a reviewer follows them, is closed the same way.
4. An account that repeatedly opens pull requests closed under this policy may be blocked from the organisation.

For now maintainers apply these rules by hand. Automated checks will first only add labels and comments; nothing is closed automatically without a separate, announced decision. The design is in [docs/CONTRIBUTION_QUALITY_SPEC.md](docs/CONTRIBUTION_QUALITY_SPEC.md).

### Issue before pull request

Every pull request links an issue that a maintainer has labelled `status: accepted`. Discuss the bug or the feature in the issue first: there, "this is not a bug" costs one comment instead of a review of a whole diff.

- Link it with a closing keyword in the description (`Closes #123`, `Fixes #123`). A plain mention does not count.
- Only maintainers apply `status: accepted`, when they agree the problem is real and in scope. An issue opened minutes before the pull request is not accepted until a maintainer says so.
- If there is no accepted issue yet, comment on the issue instead of opening the pull request.

Exempt: pull requests that only change Markdown files, pull requests from maintainers and bots, and pull requests a maintainer labels `quality:exempt`.

### Pull request template

Fill in every section of [`.github/pull_request_template.md`](.github/pull_request_template.md). A section that is missing, empty or left with only its placeholder comment counts as not filled in.

**Affected flow** names the actions (for example `take-sell`, `release`, `admin-settle`) and the order or dispute statuses the change touches, and the files where that behaviour lives. **Blast radius** names what else reads or writes the state you changed (tables, columns, events, other handlers) and says why it is not broken. A generic or wrong answer in either is a reason to close.

### Manual testing

The **Manual testing** section is a step-by-step procedure that tests this specific pull request end to end, against a running mostrod. A reviewer must be able to follow it and see the same results.

- **Setup** says which environment and configuration you used: the [Ortsom](https://github.com/MostroP2P/ortsom) regtest stack (`ortsom stack up --ref <branch>`) or a local mostrod with its own LND; which `settings.toml` keys differ from `settings.tpl.toml`; and which client each actor uses (mostro-cli, Mostro Mobile, Ortsom).
- **Steps** are numbered. Each step names one actor (seller, buyer, solver, operator), one action, and an `Expected:` line with an observable result: an order status on the public event (kind 38383), a dispute status (kind 38386), a message action received by an actor, a payment settled or refunded, a log line, or a database row.
- **For a fix**, one step is marked **(fails on `main`)** and also says what `main` does instead. If no step fails on `main`, the pull request has not shown that the bug exists.
- **Regression**: at least one step exercises a neighbouring flow named in **Blast radius** and shows it behaves as before.
- **Not covered** lists what the steps do not test and why.
- A step that cannot be observed ("the code is now more robust") is not a step.

Example, for a fix where a release during a dispute must close the dispute as `released`:

```markdown
### Setup

Ortsom regtest stack: `ortsom stack up --ref <branch>`.
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
   On `main`: the buyer is paid, but the dispute moves to `s=settled`.
5. Regression: repeat steps 1–3; the solver takes the dispute and settles it.
   Expected: unchanged from `main`, the dispute ends in `s=settled`.

### Not covered

Cooperative cancel during a dispute; out of scope for this pull request.
```

If you could not build or run mostrod, say so in the pull request instead of claiming results.

### Red test

The regression test of a fix must **fail without the fix and pass with it**. Tests live next to the code in `#[cfg(test)] mod tests`, so the test is separated from the fix by commit:

- A fix pull request **starts with a commit whose subject begins with `test:`** and that only adds the regression test (inside `#[cfg(test)]` modules or under `tests/`, no production code).
- The fix follows in one or more later commits.
- The `test:` commit is a meaningful commit, not a fixup: do not squash it into the fix. It stays separate until merge; the maintainer may squash when merging.
- Check it yourself before opening the pull request: with only the `test:` commit applied on the current `main`, the new test fails; with the whole pull request, it passes.

## Reviewing Pull Requests

Mostro follows the review workflow established by the Bitcoin Core project. The following is adapted from the [Bitcoin Core contributor documentation](https://github.com/bitcoin/bitcoin/blob/master/CONTRIBUTING.md#peer-review):

Anyone may participate in peer review which is expressed by comments in the pull request. Typically reviewers will review the code for obvious errors, as well as test out the patch set and opine on the technical merits of the patch. Project maintainers take into account the peer review when determining if there is consensus to merge a pull request (remember that discussions may have been spread out over GitHub and Telegram). The following language is used within pull-request comments:

- `ACK` means "I have tested the code and I agree it should be merged";
- `NACK` means "I disagree this should be merged", and must be accompanied by sound technical justification. NACKs without accompanying reasoning may be disregarded;
- `utACK` means "I have not tested the code, but I have reviewed it and it looks OK, I agree it can be merged";
- `Concept ACK` means "I agree in the general principle of this pull request";
- `Nit` refers to trivial, often non-blocking issues.

Reviewers should also verify **external contract impact** for any PR that modifies event kinds, tags, or message formats — confirm that indexers, clients, and relays are not silently broken by the change.

Please note that Pull Requests marked `NACK` and/or GitHub's `Change requested` are closed after 30 days if not addressed.

## Code formatting

Run `cargo fmt` and `cargo clippy` before committing to ensure that code is consistently formatted.

### Configure Git user name and email metadata

See <https://help.github.com/articles/setting-your-username-in-git/> for instructions.

### Write well-formed commit messages

From <https://chris.beams.io/posts/git-commit/#seven-rules>:

1. Separate subject from body with a blank line
2. Limit the subject line to 50 characters (\*)
3. Capitalize the subject line (a type prefix such as `test:` or `fix:` stays lowercase)
4. Do not end the subject line with a period
5. Use the imperative mood in the subject line
6. Wrap the body at 72 characters (\*)
7. Use the body to explain what and why vs. how

### Sign your commits with GPG

See <https://github.com/blog/2144-gpg-signature-verification> for background and
<https://help.github.com/articles/signing-commits-with-gpg/> for instructions.

### Keep the git history clean

It's very important to keep the git history clear, light and easily browsable. This means contributors must make sure their pull requests include only meaningful commits (if they are redundant or were added after a review, they should be removed) and _no merge commits_.
