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


if __name__ == "__main__":
    unittest.main()
