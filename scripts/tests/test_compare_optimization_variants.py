"""独立判题必须拒绝接近正确但不满足合同的交付。"""
from pathlib import Path
import json
import os
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import compare_optimization_variants as variants


class VariantVerificationTests(unittest.TestCase):
    def test_continuous_variant_reuses_all_phase_judging_and_original_fixture(self):
        files = variants.task_files("continuous")
        self.assertTrue(set(variants.bench.IMMUTABLE_PATHS).issubset(files))
        with tempfile.TemporaryDirectory() as directory:
            with patch.object(variants.continuous, "verify_single_task", return_value={
                "passed": False, "error": "earlier phase failed",
            }) as judge:
                self.assertEqual(variants.verify("continuous", Path(directory)),
                                 (False, "earlier phase failed"))
                judge.assert_called_once()

    def test_stage_order_rejects_post_snapshot_changes_even_if_code_passes(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            state = root / ".observer"
            state.mkdir()
            source = root / "inventory.py"
            source.write_text("before")
            os.utime(source, ns=(20, 20))
            (state / "release").touch()
            os.utime(state / "release", ns=(30, 30))
            (state / "started.json").write_text(json.dumps({"pid": 123, "started_ns": 10}))
            snapshot = {"sha256": variants.bench.file_digest(source), "completed_ns": 40}
            (state / "snapshot.json").write_text(json.dumps(snapshot))
            with patch.object(variants.os, "kill", side_effect=ProcessLookupError):
                variants.verify_stage_order(root)
                source.write_text("after")
                with self.assertRaisesRegex(ValueError, "changed after"):
                    variants.verify_stage_order(root)
                source.write_text("before")
                os.utime(source, ns=(50, 50))
                with self.assertRaisesRegex(ValueError, "before release"):
                    variants.verify_stage_order(root)
            with patch.object(variants.os, "kill"):
                with self.assertRaisesRegex(ValueError, "still running"):
                    variants.verify_stage_order(root)

    def test_stage_observer_records_actual_source_and_is_reaped(self):
        import time
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for name, content in variants.task_files("stage_review").items():
                (root / name).write_text(content)
            observer = subprocess.Popen([sys.executable, "observer.py"], cwd=root)
            try:
                deadline = time.monotonic() + 5
                while not (root / ".observer/started.json").exists() and time.monotonic() < deadline:
                    time.sleep(0.01)
                self.assertTrue((root / ".observer/started.json").exists())
                (root / "inventory.py").write_text("value = 2\n")
                (root / ".observer/release").touch()
                self.assertEqual(observer.wait(timeout=5), 0)
                variants.verify_stage_order(root)
            finally:
                if observer.poll() is None:
                    observer.terminate()
                    observer.wait(timeout=5)

    def test_deployment_exposes_next_constraint_without_accepting_partial_config(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            config = root / "settings.json"
            config.write_text('{"enabled":true}')
            self.assertEqual(variants.verify("deployment_feedback", root),
                             (False, "settings.json: this deployment requires timeout_ms=2500"))
            config.write_text('{"enabled":true,"timeout_ms":2500,"retries":2}')
            self.assertTrue(variants.verify("deployment_feedback", root)[0])

    def test_csv_checks_actual_bytes_not_normalized_text(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            report = root / "report.csv"
            report.write_bytes(b"\xef\xbb\xbfcustomer,total\nAda,12.50\n")
            self.assertFalse(variants.verify("csv_feedback", root)[0])
            report.write_bytes(b"\xef\xbb\xbfcustomer,total\r\nAda,12.50\r\n")
            self.assertTrue(variants.verify("csv_feedback", root)[0])

    def test_seeded_code_fails_independent_behavior_checks(self):
        for task in ("parser_fix", "multi_file"):
            with self.subTest(task=task), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                for name, content in variants.TASKS[task]["files"].items():
                    (root / name).write_text(content)
                self.assertFalse(variants.verify(task, root)[0])


if __name__ == "__main__":
    unittest.main()
