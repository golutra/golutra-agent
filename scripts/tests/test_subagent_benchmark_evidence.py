from __future__ import annotations

import copy
import sys
import tempfile
import unittest
from unittest.mock import patch
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import subagent_benchmark_evidence as evidence
import subagent_benchmark_tasks as tasks
import compare_subagents as runner


def fixture():
    records = []

    def row(file, kind, payload, time=1):
        records.append({"_rollout": file, "type": kind, "payload": payload,
                        "timestamp": f"2026-09-16T00:00:{time:02d}Z"})

    for owner, created, fork in (("p", 0, False), ("a", 1, False), ("b", 2, True)):
        meta = {"id": owner, "timestamp": f"2026-09-16T00:00:{created:02d}Z",
                "source": "exec" if owner == "p" else {"subagent": {}}}
        if fork:
            meta["forked_from_id"] = "p"
        row(owner, "session_meta", meta)
        if fork:
            row(owner, "session_meta", {"id": "p", "timestamp": "2026-09-16T00:00:00Z", "source": "exec"})
            row(owner, "event_msg", {"type": "task_started", "turn_id": "p1"}, 2)
        turns = [(owner + "1", created + 1, 8)]
        if owner == "a":
            turns.append(("a2", 10, 12))
        for turn, start, end in turns:
            row(owner, "event_msg", {"type": "task_started", "turn_id": turn}, start)
            row(owner, "event_msg", {"type": "item_completed", "thread_id": owner,
                                     "turn_id": turn, "item": {}}, start)
            row(owner, "event_msg", {"type": "task_complete", "turn_id": turn,
                "last_agent_message": "RIGHT_SENTINEL_84 FORK_CONTEXT_73X" if owner == "b" else "LEFT_SENTINEL_42"}, end)
            row(owner, "token_usage_record", {"thread_id": owner, "turn_id": turn,
                "response_id": turn, "usage": {"input_tokens": 100, "cached_input_tokens": 80,
                    "output_tokens": 10, "total_tokens": 110}})
    for child in ("a", "b"):
        row("p", "response_item", {"type": "function_call", "name": "spawn_agent",
            "call_id": child, "arguments": '{"message":"assigned task"}',
            "internal_chat_message_metadata_passthrough": {"turn_id": "p1"}})
    return records


class EvidenceTest(unittest.TestCase):
    def test_parent_answer_validation_uses_full_execution_text(self):
        answer = "paragraph\n" * 100 + "LAST_REQUIRED_FACT"
        events = [
            {"event_type":"task_created", "session_id":"p", "task_id":"p1", "payload":{"payload":{}}},
            {"event_type":"assistant_message", "session_id":"p", "task_id":"p1", "payload":{"content":answer}},
            {"event_type":"assistant_message", "session_id":"child", "task_id":"child1", "payload":{"content":"wrong answer"}},
        ]
        self.assertEqual(evidence.golutra_parent_answer(events), answer)
        self.assertNotIn("LAST_REQUIRED_FACT", answer[:512])

    def test_runner_keeps_remaining_samples_but_exits_nonzero_on_failure(self):
        with tempfile.TemporaryDirectory() as temporary:
            argv = ["compare_subagents", "--engines", "golutra", "codex", "--output", str(Path(temporary) / "reports")]
            with patch.object(sys, "argv", argv), patch.object(runner, "run_sample", side_effect=[
                {"passed": False, "usage": {}}, {"passed": True, "usage": {}}
            ]) as run, patch("builtins.print"):
                self.assertEqual(runner.main(), 1)
                self.assertEqual(run.call_count, 2)

    def test_execution_bound_success(self):
        report = evidence.summarize_codex(fixture(), {"completed": True})
        self.assertTrue(report["passed"])
        self.assertEqual(report["child_executions"], 3)
        self.assertEqual(report["usage"]["total_tokens"], 440)

    def test_fork_replay_does_not_double_count_parent_usage(self):
        records = fixture()
        inherited = copy.deepcopy(next(r for r in records if r["type"] == "token_usage_record"))
        inherited["_rollout"] = "b"
        records.append(inherited)
        report = evidence.summarize_codex(records, {"completed": True})
        self.assertEqual(report["usage"]["total_tokens"], 440)
        self.assertTrue(report["passed"])

    def test_conflicting_usage_is_an_error(self):
        records = fixture()
        duplicate = copy.deepcopy(next(r for r in records if r["type"] == "token_usage_record"))
        duplicate["payload"]["usage"]["total_tokens"] = 999
        records.append(duplicate)
        with self.assertRaises(ValueError): evidence.summarize_codex(records, {"completed": True})

    def test_failed_child_without_usage_is_not_dropped(self):
        records = [r for r in fixture() if not (r["_rollout"] == "b" and
                   (r["type"] == "token_usage_record" or r["payload"].get("type") == "item_completed"))]
        for r in records:
            if r["_rollout"] == "b" and r["payload"].get("type") == "task_complete":
                r["payload"].update(error={"message": "upstream 422"}, last_agent_message=None)
        report = evidence.summarize_codex(records, {"completed": True})
        self.assertFalse(report["passed"])
        self.assertEqual(report["child_executions"], 3)
        self.assertFalse(report["checks"]["all_child_executions_succeeded"])
        self.assertIsNone(report["usage"]["usage_complete"])
        self.assertIsNone(report["usage"]["cache_write_tokens"])
        self.assertIsNone(report["usage"]["total_tokens"])
        self.assertEqual(report["usage"]["total_tokens_partial"], 330)

    def test_previous_findings_do_not_substitute_for_resume(self):
        records = fixture()
        for r in records:
            if r["payload"].get("turn_id") == "a2" and r["payload"].get("type") == "task_complete":
                r["payload"]["last_agent_message"] = "forgot"
        self.assertFalse(evidence.summarize_codex(records, {"completed": True})["passed"])

    def test_copied_marker_is_not_context_inheritance(self):
        records = fixture()
        next(r for r in records if r["payload"].get("name") == "spawn_agent")["payload"]["arguments"] = "FORK_CONTEXT_73X"
        self.assertFalse(evidence.summarize_codex(records, {"completed": True})["passed"])

    def test_workspace_verifier_rejects_unimplemented_and_changed_tests(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            initial = tasks.setup(root, "coding")
            self.assertFalse(tasks.verify(root, "coding", initial)["passed"])
            (root / "test_work.py").write_text("# skipped")
            result = tasks.verify(root, "coding", initial)
            self.assertFalse(result["passed"])
            self.assertFalse(result["verifier_unchanged"])

    def test_fanout_requires_each_child_and_parent_to_return_real_findings(self):
        events = []
        markers = [tasks.fanout_marker(i) for i in range(10)]
        for i, marker in enumerate(markers):
            base = {"session_id": f"child{i}", "task_id": f"task{i}"}
            events.extend([
                base | {"timestamp": i, "event_type": "task_created", "payload": {"payload": {"_delegated_task": True}}},
                base | {"timestamp": 20+i, "event_type": "assistant_message", "payload": {"content": marker}},
                base | {"timestamp": 20+i, "event_type": "task_completed", "payload": {"status": "completed"}},
            ])
        metrics = {"completed": True, "final_message": "\n".join(markers)}
        report = evidence.fanout_checks("golutra", events, metrics, markers)
        self.assertTrue(report["passed"])
        self.assertEqual(report["peak_child_executions"], 10)
        self.assertFalse(evidence.fanout_checks("golutra", events, metrics | {"final_message": "done"}, markers)["passed"])
        events[1]["payload"]["content"] = "no finding"
        self.assertFalse(evidence.fanout_checks("golutra", events, metrics, markers)["passed"])

    def test_cancellation_recovery_cannot_reuse_the_first_execution_answer(self):
        events = [
            {"event_type": "task_created", "session_id": "a", "task_id": "a1", "payload": {"payload": {"_delegated_task": True}}},
            {"event_type": "assistant_message", "session_id": "a", "task_id": "a1", "payload": {"content": "RECOVERED_91"}},
            {"event_type": "task_aborted", "session_id": "a", "task_id": "a1", "payload": {}},
            {"event_type": "task_created", "session_id": "a", "task_id": "a2", "payload": {"payload": {"_delegated_task": True}}},
        ]
        checks = evidence.cancellation_checks("golutra", events, {"completed": True})
        self.assertTrue(checks["original_execution_stopped"])
        self.assertFalse(checks["resumed_child_returned_token"])


if __name__ == "__main__": unittest.main()
