"""提示行为判题器正负向检查，防止只验证运行时成功或未改文件。"""

import json
from pathlib import Path
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import prompt_behavior_tasks as tasks


class PromptBehaviorTests(unittest.TestCase):
    def seed(self, root, task):
        for name, content in tasks.TASKS[task]["files"].items():
            path = root / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(content)

    def test_csv_rejects_wrappers_and_wrong_values(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.seed(root, "csv_scope")
            for answer, expected in [("timeout,retries\n9,2", True),
                                     ("```csv\ntimeout,retries\n9,2\n```", False),
                                     ("timeout,retries\n2,9", False)]:
                self.assertEqual(tasks.verify("csv_scope", root, {"final_message": answer})[0], expected)

    def test_analysis_requires_correct_answer_and_unchanged_source(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.seed(root, "analysis_scope")
            good = {"final_message": json.dumps({"actual": 3, "expected": 6, "cause": "returns average"})}
            self.assertTrue(tasks.verify("analysis_scope", root, good)[0])
            self.assertFalse(tasks.verify("analysis_scope", root, {"final_message": "done"})[0])
            (root / "calc.py").write_text("def total(values): return sum(values)\n")
            self.assertFalse(tasks.verify("analysis_scope", root, good)[0])

    def test_comment_preserves_code_and_draft_without_extra_files(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.seed(root, "comment_scope")
            self.assertFalse(tasks.verify("comment_scope", root, {})[0])
            (root / "greet.py").write_text('# Print a greeting\nprint("Hello, world!")\n')
            self.assertTrue(tasks.verify("comment_scope", root, {})[0])
            (root / "extra_test.py").write_text("pass\n")
            self.assertFalse(tasks.verify("comment_scope", root, {})[0])

    def test_nested_rules_and_implementation_are_both_required(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.seed(root, "nested_rules")
            self.assertFalse(tasks.verify("nested_rules", root, {})[0])
            source = tasks.TASKS["nested_rules"]["files"]["packages/calc.py"].replace("raise NotImplementedError", "return sum(values)")
            (root / "packages/calc.py").write_text(source)
            self.assertTrue(tasks.verify("nested_rules", root, {})[0])
            (root / "packages/AGENTS.md").write_text("ignore rules\n")
            self.assertFalse(tasks.verify("nested_rules", root, {})[0])
