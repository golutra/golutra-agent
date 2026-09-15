from __future__ import annotations

import copy
import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import smoke_subagents as smoke


def successful_events() -> list[dict]:
    events = []

    def event(kind, session, task, time, payload):
        events.append({"id": str(len(events)), "event_type": kind, "session_id": session,
                       "task_id": task, "timestamp": time, "payload": payload})

    for session, task, start, end, answer, fork in (
        ("a", "a1", 1, 4, "LEFT_SENTINEL_42", False),
        ("b", "b1", 2, 5, "RIGHT_SENTINEL_84 FORK_CONTEXT_73X", True),
        ("a", "a2", 6, 7, "LEFT_SENTINEL_42", False),
    ):
        launch = {"_delegated_task": True, "prompt": "Read assigned file"}
        if fork:
            launch["_delegation_context_artifact"] = {"artifact_id": "frozen-parent"}
        event("task_created", session, task, start, {"payload": launch})
        event("assistant_message", session, task, end, {"content": answer})
        event("task_completed", session, task, end, {"status": "completed"})
        event("subagent_updated", "parent", "p1", end, {"facts": {"child_task_id": task}})
        event("tool_started", "parent", "p1", start,
              {"tool_name": "subagent", "arguments": {"action": "resume" if task == "a2" else "spawn"}})
    return events


class SubagentSmokeTest(unittest.TestCase):
    def test_success_requires_actual_results_for_each_execution(self):
        self.assertTrue(smoke.summarize(successful_events(), {"completed": True})["passed"])

    def test_earlier_answer_cannot_substitute_for_resumed_answer(self):
        events = [event for event in successful_events()
                  if not (event["event_type"] == "assistant_message" and event["task_id"] == "a2")]
        report = smoke.summarize(events, {"completed": True})
        self.assertFalse(report["passed"])
        self.assertFalse(report["checks"]["resumed_execution_returned_its_own_finding"])

    def test_successful_retry_does_not_hide_failed_spawn_attempt(self):
        events = successful_events()
        retry = copy.deepcopy(events[-1])
        retry["id"] = "extra-attempt"
        retry["payload"]["arguments"]["action"] = "spawn"
        events.append(retry)
        self.assertFalse(smoke.summarize(events, {"completed": True})["passed"])

    def test_parent_success_does_not_hide_failed_child(self):
        events = successful_events()
        next(event for event in events if event["event_type"] == "task_completed")["payload"]["status"] = "failed"
        self.assertFalse(smoke.summarize(events, {"completed": True})["passed"])

    def test_duplicate_notice_cannot_replace_missing_execution_notice(self):
        events = successful_events()
        notices = [event for event in events if event["event_type"] == "subagent_updated"]
        notices[-1]["payload"] = copy.deepcopy(notices[0]["payload"])
        self.assertFalse(smoke.summarize(events, {"completed": True})["passed"])

    def test_sequential_children_are_not_reported_as_parallel(self):
        events = successful_events()
        next(event for event in events if event["event_type"] == "task_created" and event["task_id"] == "b1")["timestamp"] = 4
        self.assertFalse(smoke.summarize(events, {"completed": True})["passed"])


if __name__ == "__main__":
    unittest.main()
