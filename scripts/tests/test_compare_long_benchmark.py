from __future__ import annotations

import importlib.util
import json
import sys
import tempfile
import types
import unittest
from pathlib import Path


SCRIPTS = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(SCRIPTS))
SCRIPT = SCRIPTS / "compare_long_benchmark.py"
SPEC = importlib.util.spec_from_file_location("compare_long_benchmark", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
benchmark = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = benchmark
SPEC.loader.exec_module(benchmark)


class CompareLongBenchmarkTest(unittest.TestCase):
    def test_classification_separates_runtime_terminal_from_process_status(self) -> None:
        classification = benchmark.classify_turn(
            {
                "runtime_terminal_success": True,
                "completed": False,
                "return_code": 1,
            },
            {"passed": True},
            True,
        )
        self.assertEqual(
            classification,
            {
                "workspace_verifier_pass": True,
                "runtime_terminal_success": True,
                "process_return_code": 1,
                "strict_passed": False,
            },
        )

    def test_classification_requires_all_strict_conditions(self) -> None:
        classification = benchmark.classify_turn(
            {
                "runtime_terminal_success": True,
                "completed": True,
                "return_code": 0,
            },
            {"passed": False},
            True,
        )
        self.assertFalse(classification["workspace_verifier_pass"])
        self.assertTrue(classification["runtime_terminal_success"])
        self.assertFalse(classification["strict_passed"])

    def test_terminal_event_success_ignores_nonzero_wrapper_status(self) -> None:
        golutra_stdout = "".join(
            json.dumps(value) + "\n"
            for value in (
                {"type": "turn.completed", "status": "completed"},
            )
        )
        self.assertTrue(benchmark.terminal_event_success("golutra", golutra_stdout))
        self.assertTrue(
            benchmark.terminal_event_success(
                "pi", json.dumps({"type": "agent_end"}) + "\n"
            )
        )
        self.assertFalse(
            benchmark.terminal_event_success(
                "codex", json.dumps({"type": "turn.failed"}) + "\n"
            )
        )

    def test_aggregate_reports_independent_status_counts(self) -> None:
        metrics = {
            "prompt_tokens": 10,
            "uncached_input_tokens": 2,
            "cache_read_tokens": 8,
            "cache_write_tokens": 0,
            "output_tokens": 3,
            "reasoning_tokens": 1,
            "provider_total_tokens": 13,
            "tool_call_count": 1,
            "request_count": 1,
            "elapsed_ms": 100.0,
            "first_token_ms": 10.0,
            "provider_first_token_ms": 8.0,
            "usage_complete": True,
        }
        turns = [
            {
                "metrics": {**metrics},
                "workspace_verifier_pass": True,
                "runtime_terminal_success": True,
                "process_return_code": 0,
                "strict_passed": True,
            },
            {
                "metrics": {**metrics},
                "workspace_verifier_pass": True,
                "runtime_terminal_success": True,
                "process_return_code": 1,
                "strict_passed": False,
            },
        ]
        summary = benchmark.aggregate_turns(turns)
        self.assertEqual(summary["workspace_verifier_passed"], 2)
        self.assertEqual(summary["runtime_terminal_successes"], 2)
        self.assertEqual(summary["strict_passed"], 1)
        self.assertEqual(summary["process_return_codes"], {"0": 1, "1": 1})

    def test_subtract_cumulative_usage(self) -> None:
        current = {
            "input_tokens": 100,
            "cached_input_tokens": 40,
            "cache_write_input_tokens": 0,
            "output_tokens": 20,
            "reasoning_output_tokens": 5,
        }
        first, first_source = benchmark.subtract_cumulative_usage(current, None)
        self.assertEqual(first, current)
        self.assertEqual(first_source, "reported_turn_total")

        second, second_source = benchmark.subtract_cumulative_usage(
            {
                "input_tokens": 175,
                "cached_input_tokens": 90,
                "cache_write_input_tokens": 0,
                "output_tokens": 32,
                "reasoning_output_tokens": 8,
            },
            current,
        )
        self.assertEqual(
            second,
            {
                "input_tokens": 75,
                "cached_input_tokens": 50,
                "cache_write_input_tokens": 0,
                "output_tokens": 12,
                "reasoning_output_tokens": 3,
            },
        )
        self.assertEqual(second_source, "derived_from_cumulative_turn_totals")

    def test_parse_codex_preserves_unknown_provider_round_metrics(self) -> None:
        lines = [
            {"type": "thread.started", "thread_id": "thread-1"},
            {"type": "turn.started"},
            {
                "type": "item.completed",
                "item": {
                    "id": "command-1",
                    "type": "command_execution",
                    "status": "completed",
                },
            },
            {
                "type": "item.completed",
                "item": {"id": "message-1", "type": "agent_message", "text": "done"},
            },
            {
                "type": "turn.completed",
                "usage": {
                    "input_tokens": 100,
                    "cached_input_tokens": 64,
                    "cache_write_input_tokens": 2,
                    "output_tokens": 12,
                    "reasoning_output_tokens": 4,
                },
            },
        ]
        stdout = "".join(json.dumps(line) + "\n" for line in lines)
        capture = benchmark.paired.ProcessCapture(
            stdout=stdout,
            stderr="",
            return_code=0,
            elapsed_ms=80.0,
            stdout_line_times_ms=(1.0, 2.0, 30.0, 70.0, 80.0),
        )
        metrics, cumulative, thread_id = benchmark.parse_codex(capture, None)
        self.assertTrue(metrics["completed"])
        self.assertEqual(thread_id, "thread-1")
        self.assertEqual(metrics["request_count"], None)
        self.assertEqual(metrics["provider_first_token_ms"], None)
        self.assertEqual(metrics["first_token_ms"], 30.0)
        self.assertEqual(metrics["prompt_tokens"], 100)
        self.assertEqual(metrics["uncached_input_tokens"], 36)
        self.assertEqual(metrics["cache_read_tokens"], 64)
        self.assertEqual(metrics["cache_write_tokens"], 2)
        self.assertEqual(metrics["provider_total_tokens"], 112)
        self.assertEqual(metrics["tool_call_count"], 1)
        self.assertEqual(metrics["final_message"], "done")
        self.assertEqual(cumulative["input_tokens"], 100)

    def test_prompts_include_one_large_context_turn(self) -> None:
        prompts = benchmark.turn_prompts()
        self.assertEqual(len(prompts), 4)
        self.assertGreater(benchmark.prompt_metadata(prompts[2])["tokens_estimated"], 10_000)
        self.assertIn(benchmark.SENTINEL, prompts[2])
        self.assertNotIn(benchmark.SENTINEL, prompts[3])

    def test_findings_report_current_failures_without_stale_sample_claims(self) -> None:
        metrics = {
            "first_observable_p50_ms": 5000.0,
            "cache_hit_ratio": 0.88,
            "provider_total_tokens": 100,
            "output_tokens": 10,
            "elapsed_total_ms": 200.0,
            "tool_call_count": 4,
        }
        summary = {
            engine: {
                "stages_total": 1,
                "strict_passed": 1 if engine != "golutra" else 0,
                "first_observable_p50_ms": metrics["first_observable_p50_ms"] + (100 if engine == "pi" else 0),
                "cache_hit_ratio": metrics["cache_hit_ratio"],
                "provider_total_tokens": metrics["provider_total_tokens"],
                "output_tokens": metrics["output_tokens"],
                "elapsed_total_ms": metrics["elapsed_total_ms"],
                "tool_call_count": metrics["tool_call_count"],
            }
            for engine in ("golutra", "pi", "codex")
        }
        report = {
            "summary": summary,
            "stages": [
                {
                    "stage": 1,
                    "scenario": "first_turn_cold",
                    "golutra": {
                        "strict_passed": False,
                        "workspace_verifier_pass": True,
                        "runtime_terminal_success": False,
                        "process_return_code": 1,
                        "immutable_inputs_preserved": True,
                    },
                }
            ],
        }
        rendered = "\n".join(benchmark.comparison_findings(report))
        self.assertIn("strict status is 0/1", rendered)
        self.assertIn("runtime terminal was not successful", rendered)
        self.assertIn("process return code 1", rendered)
        self.assertNotIn("run predates the unittest evidence fix", rendered)

    def test_prepare_pi_home_switches_only_benchmark_copy_to_responses(self) -> None:
        with tempfile.TemporaryDirectory() as source_dir, tempfile.TemporaryDirectory() as target_dir:
            source = Path(source_dir)
            (source / "models.json").write_text(
                json.dumps(
                    {
                        "providers": {
                            "my-api": {
                                "api": "openai-completions",
                                "baseUrl": "https://example.invalid/v1",
                                "apiKey": "test-secret",
                                "models": [{"id": "gpt-5.5"}],
                            }
                        }
                    }
                ),
                encoding="utf-8",
            )
            args = types.SimpleNamespace(
                pi_agent_source=source,
                provider="my-api",
                model="gpt-5.5",
                base_url="https://api.example.invalid",
            )
            destination = Path(target_dir) / "pi"
            benchmark.prepare_pi_home(args, destination)
            payload = json.loads((destination / "models.json").read_text(encoding="utf-8"))
            provider = payload["providers"]["my-api"]
            self.assertEqual(provider["api"], "openai-responses")
            self.assertEqual(provider["apiKey"], "test-secret")
            self.assertTrue(provider["models"][0]["reasoning"])
            self.assertEqual(provider["compat"]["sendSessionIdHeader"], True)

    def test_verifier_identity_changes_when_workspace_or_stage_changes(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            workspace = Path(directory)
            (workspace / "jobledger").mkdir()
            (workspace / "tests").mkdir()
            (workspace / "tools").mkdir()
            (workspace / "jobledger" / "module.py").write_text("value=1\n", encoding="utf-8")
            (workspace / ".long-bench").mkdir()
            (workspace / ".long-bench" / "checkpoint.json").write_text("checkpoint-a\n", encoding="utf-8")
            first, identity = benchmark.verifier_cache_identity(workspace, 3)
            second, _ = benchmark.verifier_cache_identity(workspace, 3)
            self.assertEqual(first, second)
            self.assertEqual(identity["stage"], 3)
            self.assertIn("jobledger", identity["dependency_paths"])
            (workspace / "jobledger" / "module.py").write_text("value=2\n", encoding="utf-8")
            changed, _ = benchmark.verifier_cache_identity(workspace, 3)
            self.assertNotEqual(first, changed)
            other_stage, _ = benchmark.verifier_cache_identity(workspace, 4)
            self.assertNotEqual(changed, other_stage)
            (workspace / ".long-bench" / "checkpoint.json").write_text("checkpoint-b\n", encoding="utf-8")
            checkpoint_changed, _ = benchmark.verifier_cache_identity(workspace, 3)
            self.assertNotEqual(changed, checkpoint_changed)

    def test_verification_feedback_contains_only_bounded_structured_facts(self) -> None:
        feedback = benchmark.verification_feedback(
            {
                "diagnostic": {
                    "check": "stage_three_checkpoint",
                    "kind": "round_trip_contract",
                    "message": "checksum mismatch",
                    "expected_type": "Checkpoint",
                    "actual_type": "dict",
                    "field_differences": {"sentinel": {"expected": "x", "actual": "y"}},
                    "raw_prompt": "must never be sent",
                }
            }
        )
        self.assertIn("checksum mismatch", feedback)
        self.assertNotIn("must never be sent", feedback)
        self.assertIn("one explicit repair turn", feedback)

    def test_merge_repair_metrics_preserves_unknowns_and_adds_totals(self) -> None:
        primary = {
            "prompt_tokens": 100,
            "uncached_input_tokens": 20,
            "cache_read_tokens": 80,
            "cache_write_tokens": 0,
            "output_tokens": 10,
            "reasoning_tokens": 2,
            "provider_total_tokens": 110,
            "total_tokens": 110,
            "tool_call_count": 2,
            "tool_result_count": 2,
            "request_count": None,
            "elapsed_ms": 50.0,
            "first_token_ms": 8.0,
            "turn_first_token_ms": 8.0,
            "provider_first_token_ms": None,
            "terminal_ms": 50.0,
            "completed": False,
            "runtime_terminal_success": True,
            "return_code": 0,
            "final_message": "initial",
            "usage_source": "reported",
            "usage_complete": True,
        }
        repair = {
            "prompt_tokens": 40,
            "uncached_input_tokens": 10,
            "cache_read_tokens": 30,
            "cache_write_tokens": 0,
            "output_tokens": 5,
            "reasoning_tokens": 1,
            "provider_total_tokens": 45,
            "total_tokens": 45,
            "tool_call_count": 1,
            "tool_result_count": 1,
            "request_count": None,
            "elapsed_ms": 25.0,
            "first_token_ms": 6.0,
            "turn_first_token_ms": 6.0,
            "provider_first_token_ms": 4.0,
            "terminal_ms": 25.0,
            "completed": True,
            "runtime_terminal_success": True,
            "return_code": 0,
            "final_message": "repaired",
            "usage_source": "reported",
            "usage_complete": True,
        }
        merged = benchmark.merge_repair_metrics(primary, repair)
        self.assertEqual(merged["prompt_tokens"], 140)
        self.assertEqual(merged["provider_total_tokens"], 155)
        self.assertEqual(merged["tool_call_count"], 3)
        self.assertIsNone(merged["request_count"])
        self.assertEqual(merged["elapsed_ms"], 75.0)
        self.assertEqual(merged["final_message"], "repaired")
        self.assertEqual(merged["repair_attempts"], 1)

    def test_tool_accounting_separates_repeated_calls_and_background_waits(self) -> None:
        def runtime(event_type: str, event_id: str, payload: dict) -> dict:
            return {
                "type": "runtime.event",
                "event": {
                    "id": event_id,
                    "event_type": event_type,
                    "payload": payload,
                },
            }

        stdout = "\n".join(
            json.dumps(item)
            for item in (
                runtime("tool_started", "call-1", {"tool_name": "read_file", "arguments": {"path": "a"}}),
                runtime("tool_completed", "result-1", {"tool_name": "read_file"}),
                runtime("tool_started", "call-2", {"tool_name": "read_file", "arguments": {"path": "a"}}),
                runtime("tool_completed", "result-2", {"tool_name": "read_file"}),
                runtime(
                    "tool_started",
                    "call-3",
                    {
                        "tool_name": "shell_session",
                        "arguments": {"action": "wait", "process_id": "p"},
                    },
                ),
            )
        )
        metrics = {"tool_call_count": 3, "tool_result_count": 2}
        benchmark.apply_tool_call_accounting(metrics, "golutra", stdout, None)
        self.assertEqual(metrics["model_tool_call_count"], 3)
        self.assertEqual(metrics["repeated_tool_call_count"], 1)
        self.assertEqual(metrics["necessary_tool_call_count"], 2)
        self.assertEqual(metrics["background_wait_count"], 1)
        self.assertNotIn("arguments", json.dumps(metrics["tool_call_ledger"]))
        self.assertEqual(metrics["tool_call_accounting"]["result_events"], 2)

    def test_tool_accounting_joins_provider_fallback_calls_by_provider_id(self) -> None:
        def runtime(event_type: str, event_id: str, payload: dict) -> dict:
            return {
                "type": "runtime.event",
                "event": {
                    "id": event_id,
                    "event_type": event_type,
                    "payload": payload,
                },
            }

        stdout = "\n".join(
            json.dumps(item)
            for item in (
                runtime(
                    "tool_started",
                    "tool-event-1",
                    {
                        "tool_call_id": "internal-1",
                        "provider_tool_call_id": "provider-1",
                        "tool_name": "shell",
                        "arguments": {"command": "printf one"},
                    },
                ),
                runtime(
                    "provider_completed",
                    "provider-event-1",
                    {
                        "provider_tool_calls": [
                            {"provider_tool_call_id": "provider-1", "tool_name": "shell"},
                            {"provider_tool_call_id": "provider-2", "tool_name": "read_file"},
                        ]
                    },
                ),
            )
        )
        records = benchmark.collect_tool_call_records("golutra", stdout, None)
        self.assertEqual([record["tool_name"] for record in records], ["shell", "read_file"])
        self.assertEqual(len(records), 2)

    def test_tool_accounting_keeps_multiple_idless_calls_in_one_event(self) -> None:
        stdout = json.dumps(
            {
                "type": "runtime.event",
                "event": {
                    "id": "provider-event-1",
                    "event_type": "provider_completed",
                    "payload": {
                        "provider_tool_calls": [
                            {"tool_name": "shell", "arguments": {"command": "printf one"}},
                            {"tool_name": "read_file", "arguments": {"path": "one.txt"}},
                        ]
                    },
                },
            }
        )
        records = benchmark.collect_tool_call_records("golutra", stdout, None)
        self.assertEqual([record["tool_name"] for record in records], ["shell", "read_file"])

        pi_stdout = json.dumps(
            {
                "id": "message-event-1",
                "type": "message_end",
                "message": {
                    "content": [
                        {"type": "toolCall", "name": "read", "arguments": {"path": "a"}},
                        {"type": "toolCall", "name": "bash", "arguments": {"command": "pwd"}},
                    ]
                },
            }
        )
        pi_records = benchmark.collect_tool_call_records("pi", pi_stdout, None)
        self.assertEqual([record["tool_name"] for record in pi_records], ["read", "bash"])

    def test_tool_accounting_keeps_idless_same_argument_calls_at_distinct_times(self) -> None:
        def runtime(timestamp: int) -> dict:
            return {
                "type": "runtime.event",
                "event": {
                    "event_type": "tool_started",
                    "timestamp": timestamp,
                    "payload": {
                        "tool_name": "read_file",
                        "arguments": {"path": "same.txt"},
                    },
                },
            }

        stdout = "\n".join(json.dumps(runtime(timestamp)) for timestamp in (10, 20))
        records = benchmark.collect_tool_call_records("golutra", stdout, None)
        self.assertEqual(len(records), 2)

    def test_verifier_cache_rejects_an_identity_mismatch(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            workspace = Path(directory) / "workspace"
            workspace.mkdir()
            artifact = Path(directory) / "artifacts" / "stage-1"
            artifact.mkdir(parents=True)
            cache = artifact.parent / "verifier-cache"
            cache.mkdir()
            key, identity = benchmark.verifier_cache_identity(workspace, 1)
            (cache / f"{key}.json").write_text(
                json.dumps(
                    {
                        "identity": {"stage": 99},
                        "payload": {"passed": True},
                        "raw_output": "cached",
                    }
                ),
                encoding="utf-8",
            )
            original = benchmark.subprocess.run
            try:
                class Result:
                    returncode = 0
                    stdout = '{"passed": true}\n'

                benchmark.subprocess.run = lambda *args, **kwargs: Result()
                result = benchmark.run_verifier(workspace, 1, artifact)
            finally:
                benchmark.subprocess.run = original
            self.assertFalse(result["cached"])
            self.assertEqual(result["cache_key"], key)
            self.assertEqual(result["cache_identity"], identity)


if __name__ == "__main__":
    unittest.main()
