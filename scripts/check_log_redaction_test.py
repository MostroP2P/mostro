#!/usr/bin/env python3
"""Regression coverage for check_log_redaction.py's macro/identifier
matching (delimiter forms and suspicious-identifier variants). Run via
`python3 scripts/check_log_redaction_test.py` — wired into the
`log-redaction` CI job alongside the checker itself.
"""

import tempfile
import unittest
from pathlib import Path

from check_log_redaction import check_file


class CheckLogRedactionTest(unittest.TestCase):
    def _violations(self, rust_src: str) -> list[tuple[int, str]]:
        with tempfile.NamedTemporaryFile(
            "w", suffix=".rs", delete=False, encoding="utf-8"
        ) as f:
            f.write(rust_src)
            path = Path(f.name)
        try:
            return check_file(path)
        finally:
            path.unlink()

    def test_paren_call_flags_pubkey(self):
        violations = self._violations('fn x() { tracing::info!("{}", pubkey); }')
        self.assertEqual(violations, [(1, "pubkey")])

    def test_brace_call_flags_pubkey(self):
        violations = self._violations("fn x() { trace! {pubkey} }")
        self.assertEqual(violations, [(1, "pubkey")])

    def test_bracket_call_flags_pubkey(self):
        violations = self._violations("fn x() { trace![pubkey] }")
        self.assertEqual(violations, [(1, "pubkey")])

    def test_identity_key_variant_is_flagged(self):
        violations = self._violations('fn x() { info!("{}", identity_key); }')
        self.assertEqual(violations, [(1, "identity_key")])

    def test_sender_key_variant_is_flagged(self):
        violations = self._violations('fn x() { info!("{}", sender_key); }')
        self.assertEqual(violations, [(1, "sender_key")])

    def test_reported_line_is_the_macro_line_not_the_first(self):
        # Every other positive case is a one-liner, so its `1` would hold
        # even if the line number were never computed. Put the call further
        # down so the assertion actually exercises that arithmetic.
        violations = self._violations(
            "fn x() {\n    let a = 1;\n    info!(\"{}\", pubkey);\n}"
        )
        self.assertEqual(violations, [(3, "pubkey")])

    def test_prose_mention_is_not_flagged(self):
        violations = self._violations('fn x() { info!("logging pubkey redaction"); }')
        self.assertEqual(violations, [])

    def test_inline_capture_flags_pubkey(self):
        # Rust 2021 captured identifiers live inside the string literal
        # itself — the checker must pull them out before blanking, not lose
        # them along with the surrounding prose.
        violations = self._violations(
            'fn x(pubkey: &str) { tracing::info!("User with pubkey {pubkey} did X"); }'
        )
        self.assertEqual(violations, [(1, "pubkey")])

    def test_inline_capture_with_format_spec_flags_pubkey(self):
        violations = self._violations(
            'fn x(pubkey: &str) { info!("pubkey={pubkey:?}"); }'
        )
        self.assertEqual(violations, [(1, "pubkey")])

    def test_positional_placeholder_is_not_a_capture(self):
        violations = self._violations('fn x() { info!("{} {:?}", pubkey, 1); }')
        self.assertEqual(violations, [(1, "pubkey")])

    def test_escaped_braces_are_not_a_capture(self):
        violations = self._violations('fn x() { info!("{{pubkey}}"); }')
        self.assertEqual(violations, [])

    def test_allow_comment_exempts_call(self):
        violations = self._violations(
            "fn x() {\n"
            "// pubkey-log-allow: already truncated\n"
            'info!("{}", pubkey);\n'
            "}"
        )
        self.assertEqual(violations, [])

    def test_allow_marker_inside_string_literal_does_not_exempt(self):
        # The marker must sit in an actual `//` comment on the line above —
        # not just appear anywhere on that line, e.g. inside another string.
        violations = self._violations(
            "fn x() {\n"
            'let note = "pubkey-log-allow: not a real exemption";\n'
            'info!("{}", pubkey);\n'
            "}"
        )
        self.assertEqual(violations, [(3, "pubkey")])


if __name__ == "__main__":
    unittest.main()
