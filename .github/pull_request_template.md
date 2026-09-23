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
