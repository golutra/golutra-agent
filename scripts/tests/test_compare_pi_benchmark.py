from __future__ import annotations

import importlib.util
import json
import sys
import tempfile
import unittest
from pathlib import Path


SCRIPT = Path(__file__).resolve().parents[1] / "compare_pi_benchmark.py"
SPEC = importlib.util.spec_from_file_location("compare_pi_benchmark", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
benchmark = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = benchmark
SPEC.loader.exec_module(benchmark)


class CompareBenchmarkTest(unittest.TestCase):
    def test_golutra_parser_reads_item_projection_events(self) -> None:
        def item(event_type: str, event_id: str, timestamp: int, payload: dict) -> dict:
            return {
                "type": "item.updated",
                "item": {
                    "data": {
                        "event_type": event_type,
                        "id": event_id,
                        "payload": {**payload, "timestamp": timestamp},
                    }
                },
            }

        stdout = "\n".join(
            [
                json.dumps({"type": "turn.started", "timestamp": 1000}),
                json.dumps(item("provider_started", "provider-start", 1001, {"provider_request_id": "request-1"})),
                json.dumps(item("provider_streamed", "stream-1", 1010, {"delta": {"kind": "text_delta", "text": "ok"}})),
                json.dumps(item("tool_started", "tool-start", 1020, {"payload": {}, "tool_name": "shell"})),
                json.dumps(item("tool_completed", "tool-end", 1030, {"tool_name": "shell"})),
                json.dumps(item("token_usage_recorded", "usage-1", 1040, {"record": {
                    "request_event_id": "request-1",
                    "input_tokens": 20,
                    "output_tokens": 4,
                    "reasoning_tokens": 2,
                    "cache_read_tokens": 8,
                    "provider_total_tokens": 24,
                    "usage_source": "provider",
                }})),
                json.dumps({"type": "turn.completed", "status": "completed", "final_message": "ok", "timestamp": 1050}),
            ]
        )
        with tempfile.TemporaryDirectory() as directory:
            metrics = benchmark.parse_golutra(stdout, 50.0, 0, Path(directory))
        self.assertEqual(metrics["request_count"], 1)
        self.assertEqual(metrics["tool_call_count"], 1)
        self.assertEqual(metrics["tool_result_count"], 1)
        self.assertEqual(metrics["tool_names"], ["shell"])
        self.assertEqual(metrics["input_tokens"], 20)
        self.assertEqual(metrics["cache_read_tokens"], 8)
        self.assertEqual(metrics["startup_ms"], 1.0)
        self.assertEqual(metrics["first_token_ms"], 9.0)
        self.assertTrue(metrics["completed"])

    def test_golutra_parser_excludes_caller_verifier_from_model_tools(self) -> None:
        stdout = json.dumps(
            {
                "type": "item.updated",
                "item": {
                    "data": {
                        "event_type": "tool_started",
                        "id": "verifier-start",
                        "payload": {"tool_name": "external_verifier", "timestamp": 1001},
                    }
                },
            }
        )
        with tempfile.TemporaryDirectory() as directory:
            metrics = benchmark.parse_golutra(stdout, 1.0, 0, Path(directory))
        self.assertEqual(metrics["tool_call_count"], 0)
        self.assertEqual(metrics["tool_names"], [])

    def test_pi_parser_uses_final_usage_and_numeric_timestamps(self) -> None:
        stdout = "\n".join(
            [
                json.dumps({"type": "session", "timestamp": "2026-01-01T00:00:00.000Z"}),
                json.dumps({
                    "type": "message_update",
                    "assistantMessageEvent": {
                        "type": "text_delta",
                        "partial": {"timestamp": 1005},
                    },
                }),
                json.dumps({
                    "type": "message_end",
                    "message": {
                        "role": "assistant",
                        "timestamp": 1010,
                        "responseId": "r1",
                        "content": [{"type": "text", "text": "done"}],
                        "usage": {
                            "input": 12,
                            "output": 3,
                            "cacheRead": 4,
                            "cacheWrite": 1,
                            "totalTokens": 15,
                        },
                    },
                }),
            ]
        )
        metrics = benchmark.parse_pi(stdout, 10.0, 0)
        self.assertEqual(metrics["input_tokens"], 12)
        self.assertEqual(metrics["total_tokens"], 15)
        self.assertEqual(metrics["cache_read_tokens"], 4)
        self.assertEqual(metrics["first_token_ms"], 0.0)
        self.assertEqual(metrics["terminal_ms"], 0.0)

    def test_pi_parser_counts_execution_result_once_and_estimates_output(self) -> None:
        stdout = "\n".join(
            [
                json.dumps({"type": "session", "timestamp": 1000}),
                json.dumps({"type": "tool_execution_start", "toolCallId": "call-1", "toolName": "bash"}),
                json.dumps({"type": "message_end", "message": {"role": "toolResult", "toolCallId": "call-1", "content": [{"type": "text", "text": "result"}]} }),
                json.dumps({"type": "tool_execution_end", "toolCallId": "call-1", "toolName": "bash", "result": {"content": [{"type": "text", "text": "result"}]}}),
                json.dumps({"type": "agent_end", "timestamp": 1010}),
            ]
        )
        metrics = benchmark.parse_pi(stdout, 10.0, 0)
        self.assertEqual(metrics["tool_call_count"], 1)
        self.assertEqual(metrics["tool_result_count"], 1)
        self.assertEqual(metrics["tool_result_tokens_estimated"], 2)

    def test_response_verification_supports_exact_and_contains(self) -> None:
        exact = benchmark.TASKS[0]
        mutation = benchmark.TASKS[2]
        self.assertTrue(benchmark.verify_response(exact, "BENCH_OK\n")["passed"])
        self.assertFalse(benchmark.verify_response(exact, "BENCH_OK: extra")["passed"])
        self.assertTrue(benchmark.verify_response(mutation, "Done.")["passed"])

    def test_golutra_parser_deduplicates_usage_by_request(self) -> None:
        runtime = {
            "type": "runtime.event",
            "event": {
                "event_type": "token_usage_recorded",
                "timestamp": "2026-01-01T00:00:01.000Z",
                "payload": {
                    "record": {
                        "request_event_id": "request-1",
                        "input_tokens": 10,
                        "output_tokens": 2,
                        "provider_total_tokens": 12,
                        "tool_schema_tokens_estimated": 5,
                        "usage_source": "provider",
                    }
                },
            },
        }
        stdout = "\n".join(
            [
                json.dumps({"type": "turn.started", "timestamp": "2026-01-01T00:00:00.000Z"}),
                json.dumps(runtime),
                json.dumps(runtime),
                json.dumps({
                    "type": "turn.completed",
                    "status": "completed",
                    "final_message": "done",
                    "timestamp": "2026-01-01T00:00:02.000Z",
                }),
            ]
        )
        with tempfile.TemporaryDirectory() as directory:
            metrics = benchmark.parse_golutra(stdout, 20.0, 0, Path(directory))
        self.assertEqual(metrics["input_tokens"], 10)
        self.assertEqual(metrics["total_tokens"], 12)
        self.assertEqual(metrics["tool_schema_tokens_estimated"], 5)
        self.assertTrue(metrics["completed"])


if __name__ == "__main__":
    unittest.main()
