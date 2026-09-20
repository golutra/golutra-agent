"""独立判题必须拒绝接近正确但不满足合同的交付。"""
from pathlib import Path
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import compare_optimization_variants as variants


class VariantVerificationTests(unittest.TestCase):
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
