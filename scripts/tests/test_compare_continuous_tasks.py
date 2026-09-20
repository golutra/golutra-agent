"""确保双端基准不把脚本修复或配置差异算作自主完成能力。"""

import json
import sys
import tempfile
import types
import unittest
from pathlib import Path
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import compare_continuous_tasks as comparison


class ContinuousComparisonTests(unittest.TestCase):
    def test_single_task_verifies_all_four_stages_even_after_failure(self):
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory)
            workspace = output / "workspace"
            with patch.object(comparison.bench, "run_verifier", side_effect=lambda _workspace, stage, _output: {
                "passed": stage != 2, "checks": [f"stage_{stage}"], "error": "failure" if stage == 2 else None,
            }) as verify:
                result = comparison.verify_single_task(workspace, output)
            self.assertEqual([call.args[1] for call in verify.call_args_list], [1, 2, 3, 4])
            self.assertEqual(len({call.args[2] for call in verify.call_args_list}), 4)
            self.assertFalse(result["passed"])
            self.assertEqual(result["error"], "failure")
            self.assertEqual(len(result["stages"]), 4)

    def test_single_task_keeps_all_requirements_and_neutral_background_contract(self):
        prompt = comparison.single_task_prompt()
        for requirement in ["load_recovering", "transactional", "restore_counts", "atomic replace",
                            "LEDGER-LONG-73X", "background_probe.py", "Do not stop after an intermediate"]:
            self.assertIn(requirement, prompt)
        self.assertNotIn("shell_session", prompt)
        self.assertNotIn("End compatibility ledger.", prompt)

    def test_codex_resume_retains_same_model_provider_and_explicit_permissions(self):
        args = types.SimpleNamespace(codex="codex", model="same-model",
                                     reasoning_effort="medium", base_url="https://example.test")
        state = types.SimpleNamespace(workspace=Path("/fixture"), thread_id="same-thread", env={})
        command = comparison.codex_command(args, state, "continue", 2)
        self.assertEqual(command[-3:], ["resume", "same-thread", "continue"])
        self.assertIn("--ignore-user-config", command)
        self.assertIn("--dangerously-bypass-approvals-and-sandbox", command)
        self.assertIn('model_providers.benchmark.wire_api="responses"', command)
        self.assertIn("same-model", command)

    def test_failed_independent_verifier_does_not_inject_a_repair_prompt(self):
        with tempfile.TemporaryDirectory() as root:
            state = comparison.bench.EngineState("codex", Path(root), Path(root), {})
            args = types.SimpleNamespace(timeout=1)
            captured = types.SimpleNamespace(stdout="", stdout_line_times_ms=(), elapsed_ms=10)
            metrics = {"runtime_terminal_success": True, "return_code": 0}
            with patch.object(comparison, "codex_command", return_value=["fixture"]), \
                 patch.object(comparison.process, "run_process", return_value=captured) as run, \
                 patch.object(comparison.bench, "parse_metrics", return_value=metrics), \
                 patch.object(comparison.bench, "run_verifier", return_value={"passed": False}), \
                 patch.object(comparison.bench, "immutable_digests", return_value={}):
                result = comparison.run_stage(args, state, "task", 1, {})
            self.assertEqual(run.call_count, 1)
            self.assertEqual(result["repair_attempts"], 0)
            self.assertFalse(result["strict_passed"])

    def test_dispatch_wait_distinguishes_missing_zero_and_duplicate_events(self):
        event = {"type": "runtime.event", "event": {"id": "one", "event_type": "provider_completed",
                 "payload": {"transport_diagnostics": {"tool_ready_to_terminal_ms": 0}}}}
        missing = {"type": "runtime.event", "event": {"id": "two", "event_type": "provider_completed", "payload": {}}}
        stdout = "\n".join(json.dumps(v) for v in [event, event, missing])
        self.assertEqual(comparison.tool_dispatch_waits(stdout), [0])

    def test_delivery_timing_uses_arrival_instead_of_producer_clock(self):
        event = {"type": "runtime.event", "event": {"event_type": "turn_completed",
                 "timestamp": "2000-01-01T00:00:00Z", "payload": {}}}
        capture = types.SimpleNamespace(stdout=json.dumps(event), stdout_line_times_ms=(20,), elapsed_ms=35)
        self.assertEqual(comparison.delivery_timings(capture, "golutra"), {
            "task_terminal_arrival_ms": 20, "process_elapsed_ms": 35, "post_terminal_ms": 15})
        self.assertIsNone(comparison.delivery_timings(capture, "codex")["post_terminal_ms"])


if __name__ == "__main__":
    unittest.main()
