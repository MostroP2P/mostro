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
        self.assertEqual(len(violations), 1)

    def test_brace_call_flags_pubkey(self):
        violations = self._violations("fn x() { trace! {pubkey} }")
        self.assertEqual(len(violations), 1)

    def test_bracket_call_flags_pubkey(self):
        violations = self._violations("fn x() { trace![pubkey] }")
        self.assertEqual(len(violations), 1)

    def test_identity_key_variant_is_flagged(self):
        violations = self._violations('fn x() { info!("{}", identity_key); }')
        self.assertEqual(len(violations), 1)

    def test_sender_key_variant_is_flagged(self):
        violations = self._violations('fn x() { info!("{}", sender_key); }')
        self.assertEqual(len(violations), 1)

    def test_prose_mention_is_not_flagged(self):
        violations = self._violations('fn x() { info!("logging pubkey redaction"); }')
        self.assertEqual(violations, [])

    def test_allow_comment_exempts_call(self):
        violations = self._violations(
            "fn x() {\n"
            "// pubkey-log-allow: already truncated\n"
            'info!("{}", pubkey);\n'
            "}"
        )
        self.assertEqual(violations, [])


if __name__ == "__main__":
    unittest.main()
