from __future__ import annotations

import importlib.util
import json
import os
import sys
import tempfile
import time
import unittest
from pathlib import Path
from unittest import mock


SCRIPT = Path(__file__).resolve().parents[1] / "compare_pi_benchmark.py"
SPEC = importlib.util.spec_from_file_location("compare_pi_benchmark", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
benchmark = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = benchmark
SPEC.loader.exec_module(benchmark)


class CompareBenchmarkTest(unittest.TestCase):
    def test_timed_json_lines_requires_exact_line_alignment(self) -> None:
        stdout = "{}\n{}"
        with self.assertRaises(ValueError):
            list(benchmark.iter_timed_json_lines(stdout, [1.0]))
        with self.assertRaises(ValueError):
            list(benchmark.iter_timed_json_lines(stdout, [1.0, 2.0, 3.0]))

    def test_usage_normalizers_preserve_unknown_cache_and_mark_derived_total(self) -> None:
        golutra = benchmark.normalize_golutra_usage(
            {"input_tokens": 10, "output_tokens": 2, "usage_source": "provider"}
        )
        self.assertIsNone(golutra["cache_read_tokens"])
        self.assertIsNone(golutra["uncached_input_tokens"])
        self.assertIsNone(golutra["provider_total_tokens"])
        self.assertIsNone(golutra["total_tokens"])
        self.assertEqual(golutra["total_tokens_partial"], 12)
        self.assertEqual(golutra["field_sources"]["total_tokens"], "derived")
        golutra_billing = benchmark.normalize_golutra_usage(
            {"input_tokens": 10, "output_tokens": 2, "cache_write_tokens": 3}
        )
        self.assertEqual(golutra_billing["total_tokens"], 15)
        self.assertIsNone(golutra_billing["total_tokens_partial"])
        incomplete = benchmark.normalize_golutra_usage(
            {
                "input_tokens": 10,
                "output_tokens": 2,
                "usage_complete": True,
            }
        )
        self.assertFalse(incomplete["usage_complete"])

        canonical = benchmark.normalize_golutra_usage(
            {
                "input_tokens": 10,
                "non_cached_input_tokens": 3,
                "cache_read_tokens": 1,
                "output_tokens": 2,
            }
        )
        self.assertEqual(canonical["uncached_input_tokens"], 3)
        self.assertEqual(canonical["field_sources"]["uncached_input_tokens"], "reported")

        pi_missing_cache = benchmark.normalize_pi_usage({"input": 10, "output": 2})
        self.assertIsNone(pi_missing_cache["cache_read_tokens"])
        self.assertIsNone(pi_missing_cache["cache_write_tokens"])
        self.assertIsNone(pi_missing_cache["prompt_tokens"])
        self.assertIsNone(pi_missing_cache["total_tokens"])
        self.assertIsNone(pi_missing_cache["provider_total_tokens"])

        pi_derived = benchmark.normalize_pi_usage(
            {"input": 10, "cacheRead": 4, "cacheWrite": 1, "output": 2}
        )
        self.assertEqual(pi_derived["prompt_tokens"], 14)
        self.assertEqual(pi_derived["field_sources"]["prompt_tokens"], "derived")
        self.assertEqual(pi_derived["total_tokens"], 17)
        self.assertIsNone(pi_derived["provider_total_tokens"])
        self.assertEqual(pi_derived["field_sources"]["total_tokens"], "derived")
        metrics = benchmark.empty_metrics()
        benchmark.apply_usage_records(metrics, [pi_derived], 1)
        self.assertEqual(metrics["usage_coverage"]["total_tokens"]["source"], "derived")

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
            metrics = benchmark.parse_golutra(
                stdout,
                50.0,
                0,
                Path(directory),
                [2.0, 10.0, 25.0, 30.0, 35.0, 40.0, 50.0],
            )
        self.assertEqual(metrics["request_count"], 1)
        self.assertEqual(metrics["tool_call_count"], 1)
        self.assertEqual(metrics["tool_result_count"], 1)
        self.assertEqual(metrics["tool_names"], ["shell"])
        self.assertEqual(metrics["raw_input_tokens"], 20)
        self.assertEqual(metrics["prompt_tokens"], 20)
        self.assertEqual(metrics["uncached_input_tokens"], 12)
        self.assertEqual(metrics["cache_read_tokens"], 8)
        self.assertEqual(metrics["model_prep_ms"], 8.0)
        self.assertEqual(metrics["first_token_ms"], 25.0)
        self.assertEqual(metrics["turn_first_token_ms"], 23.0)
        self.assertEqual(metrics["provider_first_token_ms"], 15.0)
        self.assertEqual(metrics["terminal_ms"], 50.0)
        self.assertTrue(metrics["usage_complete"])
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

    def test_golutra_completed_event_does_not_override_nonzero_exit(self) -> None:
        stdout = json.dumps(
            {
                "type": "turn.completed",
                "status": "completed",
                "final_message": "done",
                "timestamp": 1000,
            }
        )
        with tempfile.TemporaryDirectory() as directory:
            metrics = benchmark.parse_golutra(stdout, 1.0, 7, Path(directory))
            verification = benchmark.task_verification(benchmark.TASKS[0], metrics, Path(directory))
        self.assertFalse(metrics["completed"])
        self.assertFalse(verification["passed"])
        self.assertFalse(verification["execution_completed"])

    def test_pi_parser_normalizes_cache_and_uses_host_arrival_times(self) -> None:
        stdout = "\n".join(
            [
                json.dumps({"type": "session", "timestamp": "2026-01-01T00:00:00.000Z"}),
                json.dumps({"type": "turn_start"}),
                json.dumps({
                    "type": "message_update",
                    "assistantMessageEvent": {
                        "type": "text_delta",
                        "delta": "done",
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
                            "totalTokens": 20,
                        },
                    },
                }),
                json.dumps({"type": "agent_end", "timestamp": 1015}),
            ]
        )
        metrics = benchmark.parse_pi(stdout, 220.0, 0, [5.0, 7.0, 205.0, 215.0, 220.0])
        self.assertEqual(metrics["raw_input_tokens"], 12)
        self.assertEqual(metrics["prompt_tokens"], 16)
        self.assertEqual(metrics["uncached_input_tokens"], 12)
        self.assertEqual(metrics["total_tokens"], 20)
        self.assertEqual(metrics["cache_read_tokens"], 4)
        self.assertEqual(metrics["cache_write_tokens"], 1)
        self.assertEqual(metrics["first_token_ms"], 205.0)
        self.assertEqual(metrics["turn_first_token_ms"], 198.0)
        self.assertEqual(metrics["terminal_ms"], 220.0)
        self.assertTrue(metrics["usage_complete"])

    def test_pi_prompt_is_unknown_without_cache_read(self) -> None:
        usage = benchmark.normalize_pi_usage({"input": 12, "output": 3, "cacheWrite": 1})
        self.assertIsNone(usage["prompt_tokens"])
        metrics = benchmark.empty_metrics()
        benchmark.apply_usage_records(metrics, [usage], 1)
        self.assertEqual(metrics["usage_coverage"]["prompt_tokens"]["status"], "unknown")
        self.assertEqual(metrics["usage_coverage"]["cache_write_tokens"]["status"], "complete")

    def test_aggregate_preserves_partial_coverage_without_reported_values(self) -> None:
        metrics = benchmark.empty_metrics()
        metrics["total_tokens_partial"] = 12
        metrics["usage_coverage"] = {
            field: {
                "reported_requests": 0,
                "expected_requests": 1,
                "complete": False,
                "status": "partial",
                "source": "derived",
            }
            if field == "total_tokens"
            else {
                "reported_requests": 0,
                "expected_requests": 1,
                "complete": False,
                "status": "unknown",
            }
            for field in benchmark.USAGE_FIELDS
        }
        metrics["verification"] = {"passed": False}
        summary = benchmark.aggregate_metrics([{"golutra": metrics}], "golutra")
        self.assertEqual(summary["total_tokens_partial"], 12)
        self.assertEqual(summary["usage_coverage"]["total_tokens"]["status"], "partial")
        self.assertEqual(summary["usage_coverage"]["total_tokens"]["partial_requests"], 1)

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
        self.assertEqual(
            metrics["usage_coverage"]["tool_result_tokens_estimated"]["source"],
            "estimated",
        )
        self.assertEqual(
            metrics["usage_coverage"]["tool_result_tokens_estimated"]["estimated_count"],
            1,
        )
        self.assertEqual(
            metrics["usage_coverage"]["tool_result_tokens_estimated"]["expected_requests"],
            0,
        )
        self.assertTrue(metrics["usage_coverage"]["tool_result_tokens_estimated"]["complete"])

    def test_pi_parser_uses_process_event_as_missing_turn_anchor(self) -> None:
        stdout = "\n".join(
            [
                json.dumps(
                    {
                        "type": "message_update",
                        "assistantMessageEvent": {
                            "type": "text_delta",
                            "delta": "ok",
                        },
                    }
                ),
                json.dumps({"type": "agent_end"}),
            ]
        )
        metrics = benchmark.parse_pi(stdout, 20.0, 0, [4.0, 9.0])
        self.assertEqual(metrics["process_first_event_ms"], 4.0)
        self.assertEqual(metrics["first_token_ms"], 4.0)
        self.assertEqual(metrics["turn_first_token_ms"], 0.0)
        self.assertEqual(metrics["terminal_ms"], 9.0)

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
        self.assertEqual(metrics["raw_input_tokens"], 10)
        self.assertEqual(metrics["prompt_tokens"], 10)
        self.assertEqual(metrics["total_tokens"], 12)
        self.assertEqual(metrics["tool_schema_tokens_estimated"], 5)
        self.assertTrue(metrics["completed"])

    def test_golutra_parser_deduplicates_without_request_id_using_round(self) -> None:
        def runtime(event_type: str, event_id: str, payload: dict) -> dict:
            return {
                "type": "runtime.event",
                "event": {
                    "id": event_id,
                    "event_type": event_type,
                    "causal_context": {"provider_round_id": "round-1"},
                    "payload": payload,
                },
            }

        stdout = "\n".join(
            [
                json.dumps(runtime("provider_started", "start-1", {"provider_round_id": "round-1"})),
                json.dumps(
                    runtime(
                        "token_usage_recorded",
                        "usage-1",
                        {
                            "record": {
                                "input_tokens": 10,
                                "output_tokens": 2,
                                "provider_total_tokens": 12,
                                "usage_source": "provider",
                            }
                        },
                    )
                ),
                json.dumps(
                    runtime(
                        "provider_completed",
                        "complete-1",
                        {
                            "provider_round_id": "round-1",
                            "usage": {
                                "input_tokens": 10,
                                "output_tokens": 2,
                                "total_tokens": 12,
                            },
                        },
                    )
                ),
            ]
        )
        with tempfile.TemporaryDirectory() as directory:
            metrics = benchmark.parse_golutra(stdout, 10.0, 0, Path(directory))
        self.assertEqual(metrics["request_count"], 1)
        self.assertEqual(metrics["usage_record_count"], 1)
        self.assertEqual(metrics["total_tokens"], 12)

    def test_golutra_parser_correlates_idless_provider_phases_and_projections(self) -> None:
        def runtime(event_type: str, timestamp: int, payload: dict) -> dict:
            return {
                "type": "runtime.event",
                "event": {
                    "event_type": event_type,
                    "timestamp": timestamp,
                    "payload": payload,
                },
            }

        def item(event_type: str, timestamp: int, payload: dict) -> dict:
            return {
                "type": "item.updated",
                "item": {
                    "data": {
                        "event_type": event_type,
                        "payload": {**payload, "timestamp": timestamp},
                    }
                },
            }

        lines = [
            runtime("provider_started", 100, {}),
            item("provider_started", 100, {}),
            runtime(
                "token_usage_recorded",
                110,
                {"record": {"input_tokens": 10, "output_tokens": 2, "provider_total_tokens": 12}},
            ),
            item(
                "token_usage_recorded",
                110,
                {"record": {"input_tokens": 10, "output_tokens": 2, "provider_total_tokens": 12}},
            ),
            runtime(
                "provider_completed",
                120,
                {"usage": {"input_tokens": 10, "output_tokens": 2, "total_tokens": 12}},
            ),
            item(
                "provider_completed",
                120,
                {"usage": {"input_tokens": 10, "output_tokens": 2, "total_tokens": 12}},
            ),
            json.dumps({"type": "turn.completed", "status": "completed", "timestamp": 130}),
        ]
        stdout = "\n".join(
            line if isinstance(line, str) else json.dumps(line) for line in lines
        )
        with tempfile.TemporaryDirectory() as directory:
            metrics = benchmark.parse_golutra(stdout, 30.0, 0, Path(directory))
        self.assertEqual(metrics["request_count"], 1)
        self.assertEqual(metrics["usage_record_count"], 1)
        self.assertEqual(metrics["total_tokens"], 12)

    def test_golutra_parser_keeps_distinct_idless_rounds_and_deduplicates_tools(self) -> None:
        def runtime(event_type: str, timestamp: int, payload: dict) -> dict:
            return {
                "type": "runtime.event",
                "event": {
                    "event_type": event_type,
                    "timestamp": timestamp,
                    "payload": payload,
                },
            }

        lines = [
            runtime("provider_started", 100, {}),
            runtime("token_usage_recorded", 110, {"record": {"input_tokens": 10, "output_tokens": 2}}),
            runtime("provider_completed", 120, {"usage": {"input_tokens": 10, "output_tokens": 2}}),
            runtime("provider_started", 200, {}),
            runtime("token_usage_recorded", 210, {"record": {"input_tokens": 20, "output_tokens": 3}}),
            runtime("provider_completed", 220, {"usage": {"input_tokens": 20, "output_tokens": 3}}),
            runtime("tool_started", 230, {"tool_name": "shell"}),
            runtime("tool_started", 230, {"tool_name": "shell"}),
            runtime("tool_completed", 240, {"tool_name": "shell"}),
            runtime("tool_completed", 240, {"tool_name": "shell"}),
        ]
        stdout = "\n".join(json.dumps(line) for line in lines)
        with tempfile.TemporaryDirectory() as directory:
            metrics = benchmark.parse_golutra(stdout, 30.0, 0, Path(directory))
        self.assertEqual(metrics["request_count"], 2)
        self.assertEqual(metrics["usage_record_count"], 2)
        self.assertIsNone(metrics["total_tokens"])
        self.assertEqual(metrics["total_tokens_partial"], 35)
        self.assertEqual(metrics["usage_coverage"]["total_tokens"]["status"], "partial")
        self.assertEqual(metrics["tool_call_count"], 1)
        self.assertEqual(metrics["tool_result_count"], 1)

    def test_golutra_parser_marks_partial_usage_fields(self) -> None:
        def usage(request_id: str, reasoning: int | None) -> dict:
            record = {
                "request_event_id": request_id,
                "input_tokens": 10,
                "non_cached_input_tokens": 10,
                "output_tokens": 2,
                "provider_total_tokens": 12,
                "tool_schema_tokens_estimated": 5,
                "tool_result_tokens_estimated": 0,
                "usage_complete": True,
                "usage_source": "provider",
            }
            if reasoning is not None:
                record["reasoning_tokens"] = reasoning
            return {
                "type": "runtime.event",
                "event": {
                    "event_type": "token_usage_recorded",
                    "timestamp": 1000,
                    "payload": {"record": record},
                },
            }

        stdout = "\n".join(
            [
                json.dumps(usage("request-1", 2)),
                json.dumps(usage("request-2", None)),
            ]
        )
        with tempfile.TemporaryDirectory() as directory:
            metrics = benchmark.parse_golutra(stdout, 10.0, 0, Path(directory))
        self.assertEqual(metrics["prompt_tokens"], 20)
        self.assertEqual(metrics["uncached_input_tokens"], 20)
        self.assertIsNone(metrics["reasoning_tokens"])
        self.assertEqual(metrics["reasoning_tokens_partial"], 2)
        self.assertEqual(
            metrics["usage_coverage"]["reasoning_tokens"],
            {
                "reported_requests": 1,
                "expected_requests": 2,
                "complete": False,
                "status": "partial",
                "source": "reported",
            },
        )
        self.assertEqual(
            metrics["usage_coverage"]["tool_result_tokens_estimated"]["source"],
            "estimated",
        )
        self.assertEqual(
            metrics["usage_coverage"]["tool_result_tokens_estimated"]["estimated_count"],
            2,
        )
        self.assertTrue(metrics["usage_complete"])

    def test_provider_tracker_does_not_alias_late_ids_to_active_round(self) -> None:
        tracker = benchmark.ProviderRequestTracker()
        first = tracker.resolve({"timestamp": 100}, {}, "provider_started")
        self.assertEqual(
            tracker.resolve({"timestamp": 120}, {}, "provider_completed"),
            first,
        )
        second = tracker.resolve({"timestamp": 200}, {}, "provider_started")

        late = tracker.resolve(
            {"timestamp": 110},
            {"provider_request_id": "late-request"},
            "token_usage_recorded",
        )
        self.assertEqual(late, first)
        self.assertNotEqual(late, second)

        without_time = tracker.resolve(
            {},
            {"provider_request_id": "untimed-request"},
            "token_usage_recorded",
        )
        self.assertEqual(without_time, "untimed-request")
        self.assertNotEqual(without_time, second)

    def test_provider_tracker_untimed_rules_and_oldest_completion(self) -> None:
        tracker = benchmark.ProviderRequestTracker()
        active = tracker.resolve({}, {}, "provider_started")
        self.assertEqual(
            tracker.resolve(
                {},
                {"provider_request_id": "active-start-projection"},
                "provider_started",
            ),
            active,
        )
        self.assertEqual(
            tracker.resolve(
                {},
                {"provider_request_id": "active-request"},
                "token_usage_recorded",
            ),
            active,
        )

        tracker = benchmark.ProviderRequestTracker()
        explicit = tracker.resolve(
            {"timestamp": 100},
            {"provider_request_id": "explicit-start"},
            "provider_started",
        )
        self.assertEqual(
            tracker.resolve({"timestamp": 100}, {}, "provider_started"),
            explicit,
        )
        self.assertEqual(tracker.resolve({}, {}, "provider_completed"), explicit)

        tracker = benchmark.ProviderRequestTracker()
        tracker.resolve({"timestamp": 100}, {}, "provider_started")
        self.assertNotEqual(
            tracker.resolve({"timestamp": 200}, {}, "provider_started"),
            "synthetic-provider-request:0",
        )

        tracker = benchmark.ProviderRequestTracker()
        old = tracker.resolve({"timestamp": 100}, {}, "provider_started")
        fresh = tracker.resolve(
            {"timestamp": 200},
            {"provider_request_id": "fresh-start"},
            "provider_started",
        )
        self.assertEqual(fresh, "fresh-start")
        self.assertNotEqual(fresh, old)

        tracker = benchmark.ProviderRequestTracker()
        tracker.resolve({}, {}, "provider_started")
        unknown_start = tracker.resolve(
            {"timestamp": 100},
            {"provider_request_id": "unknown-start"},
            "provider_started",
        )
        self.assertEqual(unknown_start, "unknown-start")

        tracker = benchmark.ProviderRequestTracker()
        closed = tracker.resolve(
            {"timestamp": 100},
            {"provider_request_id": "closed-start"},
            "provider_started",
        )
        tracker.resolve({"timestamp": 100}, {}, "provider_completed")
        reopened = tracker.resolve({"timestamp": 100}, {}, "provider_started")
        self.assertNotEqual(reopened, closed)

        tracker = benchmark.ProviderRequestTracker()
        first = tracker.resolve({"timestamp": 100}, {}, "provider_started")
        tracker.resolve({"timestamp": 120}, {}, "provider_completed")
        second = tracker.resolve({"timestamp": 200}, {}, "provider_started")
        self.assertEqual(
            tracker.resolve(
                {},
                {"provider_request_id": "late-without-time"},
                "token_usage_recorded",
            ),
            "late-without-time",
        )
        self.assertNotEqual("late-without-time", second)

        tracker = benchmark.ProviderRequestTracker()
        tracker.resolve({"timestamp": 100}, {}, "provider_started")
        tracker.resolve({"timestamp": 120}, {}, "provider_completed")
        fresh = tracker.resolve(
            {"timestamp": 110},
            {"provider_request_id": "fresh-start"},
            "provider_started",
        )
        self.assertEqual(fresh, "fresh-start")

        tracker = benchmark.ProviderRequestTracker()
        first = tracker.resolve({"timestamp": 100}, {}, "provider_started")
        second = tracker.resolve({"timestamp": 200}, {}, "provider_started")
        self.assertEqual(tracker.resolve({"timestamp": 250}, {}, "provider_completed"), first)
        self.assertEqual(tracker.resolve({}, {}, "provider_completed"), second)

        tracker = benchmark.ProviderRequestTracker()
        explicit = tracker.resolve(
            {"timestamp": 100},
            {"provider_request_id": "explicit-old"},
            "provider_started",
        )
        synthetic = tracker.resolve({"timestamp": 200}, {}, "provider_started")
        self.assertEqual(tracker.resolve({}, {}, "provider_completed"), explicit)
        self.assertNotEqual(explicit, synthetic)

    def test_stop_timeout_handles_concurrent_process_exit(self) -> None:
        process = mock.Mock()
        process.pid = 123
        process.wait.side_effect = ProcessLookupError
        if benchmark.os.name == "nt":
            process.terminate.side_effect = ProcessLookupError
            benchmark.stop_timed_out_process(process)
            process.terminate.assert_called_once()
        else:
            with mock.patch.object(benchmark.os, "killpg", side_effect=ProcessLookupError) as killpg:
                benchmark.stop_timed_out_process(process)
            killpg.assert_called_once_with(process.pid, benchmark.signal.SIGTERM)

    def test_windows_process_tree_cleanup_uses_taskkill(self) -> None:
        with mock.patch.object(benchmark.subprocess, "run") as run:
            benchmark.terminate_windows_process_tree(42)
        run.assert_called_once_with(
            ["taskkill", "/PID", "42", "/T", "/F"],
            check=False,
            stdout=benchmark.subprocess.DEVNULL,
            stderr=benchmark.subprocess.DEVNULL,
            timeout=benchmark.PROCESS_STOP_TIMEOUT_SECONDS,
        )

    def test_pipe_reader_join_closes_stream_after_deadline(self) -> None:
        class StuckThread:
            def __init__(self) -> None:
                self.join_timeouts: list[float | None] = []

            def join(self, timeout: float | None = None) -> None:
                self.join_timeouts.append(timeout)

            def is_alive(self) -> bool:
                return True

        class Stream:
            closed = False

            def close(self) -> None:
                self.closed = True

        thread = StuckThread()
        stream = Stream()
        benchmark.join_pipe_reader(thread, stream, time.monotonic() - 1)
        self.assertTrue(stream.closed)
        self.assertEqual(len(thread.join_timeouts), 2)
        self.assertTrue(all(timeout is not None and timeout >= 0 for timeout in thread.join_timeouts))

    @unittest.skipUnless(hasattr(os, "fork"), "requires Unix fork")
    def test_run_process_returns_when_descendant_holds_pipes(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            marker = root / "child.pid"
            code = (
                "import os, pathlib, sys, time\n"
                "if os.fork() == 0:\n"
                "    pathlib.Path(sys.argv[1]).write_text(str(os.getpid()))\n"
                "    print(os.getpid(), flush=True)\n"
                "    time.sleep(10)\n"
            )
            started = time.monotonic()
            child_pid: int | None = None
            try:
                capture = benchmark.run_process(
                    [sys.executable, "-c", code, str(marker)],
                    root,
                    os.environ.copy(),
                    1.0,
                    root / "stdout.log",
                    root / "stderr.log",
                )
                elapsed = time.monotonic() - started
                self.assertEqual(capture.return_code, 0)
                self.assertLess(
                    elapsed,
                    benchmark.PIPE_DRAIN_TIMEOUT_SECONDS
                    + benchmark.PIPE_CLOSE_JOIN_SECONDS
                    + 0.5,
                )
                deadline = time.monotonic() + 0.5
                while not marker.exists() and time.monotonic() < deadline:
                    time.sleep(0.01)
                self.assertTrue(marker.exists())
                child_pid = int(marker.read_text())
            finally:
                if child_pid is None and marker.exists():
                    try:
                        child_pid = int(marker.read_text())
                    except (OSError, ValueError):
                        child_pid = None
                if child_pid is not None:
                    try:
                        os.kill(child_pid, benchmark.signal.SIGKILL)
                    except ProcessLookupError:
                        pass
                    deadline = time.monotonic() + 1.0
                    while time.monotonic() < deadline:
                        try:
                            os.kill(child_pid, 0)
                        except ProcessLookupError:
                            break
                        time.sleep(0.01)
                    else:
                        self.fail(f"descendant process {child_pid} survived cleanup")


if __name__ == "__main__":
    unittest.main()
