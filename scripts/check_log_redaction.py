#!/usr/bin/env python3
"""CI gate for AGENTS.md:48 ("Scrub logs that might leak invoices or Nostr
keys"). Flags any `tracing::{trace,debug,info,warn,error}!(...)` call whose
argument list interpolates an identifier that looks like a Nostr
key/identity, so a new log-scrubbing regression (issue #836's pattern) fails
CI instead of shipping quietly.

Not a Rust parser: string literals are skipped so a key-shaped *word* inside
a log message's own text doesn't trigger a false positive, but the paren
matching is a plain depth counter — a macro call containing a raw string
literal with unbalanced parens would confuse it. None of this codebase's
tracing calls do that today; if one ever needs to, exempt it inline (see
ALLOW_COMMENT below) rather than fighting the matcher.
"""

import re
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
SRC_ROOT = REPO_ROOT / "src"

MACRO_RE = re.compile(r"\b(?:tracing::)?(?:trace|debug|info|warn|error)!\s*([({\[])")

# Rust macros accept any of these three delimiter pairs; the matcher must
# track whichever one was actually opened.
DELIMITER_PAIRS = {"(": ")", "{": "}", "[": "]"}

# Identifiers that name a Nostr key/identity in this codebase. Extend this
# list, don't loosen it to a bare `key` — that also matches innocuous things
# like HashMap iteration variables.
SUSPICIOUS_RE = re.compile(
    r"\b("
    r"\w*pubkey\w*"
    r"|identity\w*"
    r"|sender\w*"
    r"|master_key"
    r"|trade_key"
    r"|nsec\w*"
    r"|priv(?:ate)?_?key\w*"
    r")\b"
)

# A `// pubkey-log-allow: <reason>` comment on the line right before a
# flagged macro call exempts it — for a documented, deliberate exception
# (e.g. an already-redacted/truncated value) rather than a silent miss.
ALLOW_COMMENT = "pubkey-log-allow:"


def find_call_span(text: str, open_delim: int) -> tuple[int, str]:
    """Return (index just past the delimiter matching
    `text[open_delim]`, the call's source with string-literal *contents*
    blanked out). Handles all three Rust macro delimiter pairs: `()`,
    `{}`, `[]`.

    Blanking string contents (not just skipping them for delimiter-matching)
    matters: a format string's own English prose can contain a key-shaped
    word ("...pubkey in order...") that isn't an interpolated argument at
    all — only the blanked version should be searched for suspicious
    identifiers, or every message that merely *mentions* a pubkey false-
    positives.
    """
    open_ch = text[open_delim]
    close_ch = DELIMITER_PAIRS[open_ch]
    depth = 0
    i = open_delim
    n = len(text)
    out = []
    while i < n:
        c = text[i]
        if c == '"':
            start = i
            i += 1
            while i < n and text[i] != '"':
                i += 2 if text[i] == "\\" else 1
            i += 1
            out.append('"' * (i - start))
            continue
        out.append(c)
        if c == open_ch:
            depth += 1
        elif c == close_ch:
            depth -= 1
            if depth == 0:
                return i + 1, "".join(out)
        i += 1
    return n, "".join(out)  # unbalanced — best effort


def line_before(text: str, index: int) -> str:
    line_start = text.rfind("\n", 0, index)
    prev_start = text.rfind("\n", 0, line_start) + 1 if line_start != -1 else 0
    return text[prev_start:line_start] if line_start != -1 else ""


def check_file(path: Path) -> list[tuple[int, str]]:
    text = path.read_text(encoding="utf-8")
    violations = []
    for m in MACRO_RE.finditer(text):
        open_delim = m.end() - 1
        _end, code_only = find_call_span(text, open_delim)
        found = SUSPICIOUS_RE.search(code_only)
        if not found:
            continue
        if ALLOW_COMMENT in line_before(text, m.start()):
            continue
        line_no = text.count("\n", 0, m.start()) + 1
        violations.append((line_no, found.group(0)))
    return violations


def main() -> int:
    total = 0
    for path in sorted(SRC_ROOT.rglob("*.rs")):
        for line_no, ident in check_file(path):
            rel = path.relative_to(REPO_ROOT)
            print(
                f"{rel}:{line_no}: tracing call interpolates `{ident}` — "
                f"looks like a Nostr key/identity (AGENTS.md:48). Drop it from "
                f"the log line, or mark a deliberate exception with a "
                f"`// {ALLOW_COMMENT} <reason>` comment on the line above."
            )
            total += 1
    if total:
        print(f"\n{total} log-redaction violation(s) found.", file=sys.stderr)
        return 1
    print("check_log_redaction: clean.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
