from __future__ import annotations

import contextlib
import importlib.util
import io
import json
import sys
import tempfile
import unittest
from pathlib import Path


SCRIPT = Path(__file__).resolve().parents[1] / "verify_long_benchmark.py"
SPEC = importlib.util.spec_from_file_location("verify_long_benchmark", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
verifier = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = verifier
SPEC.loader.exec_module(verifier)


class VerifyLongBenchmarkTest(unittest.TestCase):
    def test_checkpoint_diagnostic_reports_types_keys_fields_and_checksum(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            workspace = Path(directory)
            state = workspace / ".long-bench"
            state.mkdir()
            (state / "checkpoint.json").write_text(
                json.dumps(
                    {
                        "version": 1,
                        "through_sequence": 4,
                        "state_counts": {"queued": 1, "running": 9},
                        "sentinel": "wrong",
                        "checksum": "bad",
                        "extra": True,
                    }
                ),
                encoding="utf-8",
            )
            diagnostic = verifier.checkpoint_failure_diagnostic(workspace, "round trip failed")
            self.assertEqual(diagnostic["expected_type"], "Checkpoint")
            self.assertEqual(diagnostic["actual_type"], "dict")
            self.assertIn("extra", diagnostic["unexpected_keys"])
            self.assertIn("sentinel", diagnostic["field_differences"])
            self.assertFalse(diagnostic["checksum"]["matches"])

    def test_main_executes_only_requested_stage_and_writes_failure_artifact(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            workspace = Path(directory) / "workspace"
            workspace.mkdir()
            artifact = Path(directory) / "failure.log"
            calls: list[str] = []

            def successful(path: Path) -> None:
                calls.append(str(path))

            original_stages = verifier.STAGES
            verifier.STAGES = (successful, successful, successful, successful)
            original_argv = sys.argv
            try:
                sys.argv = [
                    str(SCRIPT),
                    "--workspace",
                    str(workspace),
                    "--stage",
                    "3",
                ]
                output = io.StringIO()
                with contextlib.redirect_stdout(output):
                    self.assertEqual(verifier.main(), 0)
                payload = json.loads(output.getvalue())
                self.assertEqual(payload["checks"], ["stage_3"])
                self.assertEqual(len(calls), 1)

                def failed(path: Path) -> None:
                    raise verifier.VerificationFailure(
                        {"check": "fixture", "message": "strict failure"},
                        "raw traceback\n",
                    )

                verifier.STAGES = (failed, failed, failed, failed)
                sys.argv = [
                    str(SCRIPT),
                    "--workspace",
                    str(workspace),
                    "--stage",
                    "1",
                    "--artifact",
                    str(artifact),
                ]
                output = io.StringIO()
                with contextlib.redirect_stdout(output):
                    self.assertEqual(verifier.main(), 1)
                payload = json.loads(output.getvalue())
                self.assertFalse(payload["passed"])
                self.assertEqual(artifact.read_text(encoding="utf-8"), "raw traceback\n")
            finally:
                verifier.STAGES = original_stages
                sys.argv = original_argv

    def test_visible_test_failure_preserves_raw_output_outside_diagnostic(self) -> None:
        class Result:
            returncode = 1
            stdout = "full raw failure\n" + "x" * 8_000

        original_run = verifier.subprocess.run
        original_platform = verifier.platform.platform
        try:
            verifier.subprocess.run = lambda *args, **kwargs: Result()
            verifier.platform.platform = lambda: "test-platform"
            with tempfile.TemporaryDirectory() as directory:
                with self.assertRaises(verifier.VerificationFailure) as raised:
                    verifier.verify_visible_tests(Path(directory))
        finally:
            verifier.subprocess.run = original_run
            verifier.platform.platform = original_platform

        self.assertEqual(raised.exception.raw_output, Result.stdout)
        self.assertLessEqual(
            len(str(raised.exception.diagnostic["message"])),
            verifier.MAX_DIAGNOSTIC_TEXT + 3,
        )


if __name__ == "__main__":
    unittest.main()
