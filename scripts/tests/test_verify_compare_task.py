from __future__ import annotations

import importlib.util
import json
import sys
import tempfile
import unittest
from pathlib import Path


SCRIPT = Path(__file__).resolve().parents[1] / "verify_compare_task.py"
SPEC = importlib.util.spec_from_file_location("verify_compare_task", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
verifier = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = verifier
SPEC.loader.exec_module(verifier)


class VerifyCompareTaskTest(unittest.TestCase):
    def test_verifier_accepts_expected_file_and_rejects_escape(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            workspace = Path(directory) / "workspace"
            workspace.mkdir()
            (workspace / "ok.txt").write_text("ok\n", encoding="utf-8")
            original_argv = sys.argv
            try:
                sys.argv = [
                    str(SCRIPT),
                    "--workspace",
                    str(workspace),
                    "--expected-json",
                    json.dumps({"ok.txt": "ok\n"}),
                ]
                self.assertEqual(verifier.main(), 0)
                sys.argv = [
                    str(SCRIPT),
                    "--workspace",
                    str(workspace),
                    "--expected-json",
                    json.dumps({"../outside.txt": "nope"}),
                ]
                self.assertEqual(verifier.main(), 1)
            finally:
                sys.argv = original_argv


if __name__ == "__main__":
    unittest.main()
