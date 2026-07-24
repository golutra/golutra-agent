import importlib.util
import json
import sys
import types
import unittest
from pathlib import Path
from tempfile import TemporaryDirectory
from unittest.mock import patch


def _load_adapter():
    """Load the adapter with small import stubs instead of upstream Docker deps."""
    base_agent = types.ModuleType("terminal_bench.agents.base_agent")

    class BaseAgent:
        def __init__(self, **_kwargs):
            pass

    class AgentResult:
        def __init__(self, **kwargs):
            self.__dict__.update(kwargs)

    base_agent.BaseAgent = BaseAgent
    base_agent.AgentResult = AgentResult

    failure_mode = types.ModuleType("terminal_bench.agents.failure_mode")

    class FailureMode:
        AGENT_INSTALLATION_FAILED = "agent_installation_failed"

    failure_mode.FailureMode = FailureMode

    models = types.ModuleType("terminal_bench.terminal.models")

    class TerminalCommand:
        def __init__(self, **kwargs):
            self.__dict__.update(kwargs)

    models.TerminalCommand = TerminalCommand

    tmux = types.ModuleType("terminal_bench.terminal.tmux_session")

    class TmuxSession:
        pass

    tmux.TmuxSession = TmuxSession

    for name, module in {
        "terminal_bench": types.ModuleType("terminal_bench"),
        "terminal_bench.agents": types.ModuleType("terminal_bench.agents"),
        "terminal_bench.terminal": types.ModuleType("terminal_bench.terminal"),
        "terminal_bench.agents.base_agent": base_agent,
        "terminal_bench.agents.failure_mode": failure_mode,
        "terminal_bench.terminal.models": models,
        "terminal_bench.terminal.tmux_session": tmux,
    }.items():
        sys.modules.setdefault(name, module)

    path = Path(__file__).with_name("golutra_tbench_adapter.py")
    spec = importlib.util.spec_from_file_location("golutra_tbench_adapter_under_test", path)
    assert spec and spec.loader
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


ADAPTER = _load_adapter()


class AdapterHelpersTest(unittest.TestCase):
    def test_collector_resolution_is_explicit_and_repository_local(self):
        with TemporaryDirectory() as temporary:
            root = Path(temporary)
            explicit = root / "explicit"
            environment = root / "environment"
            release = root / "target/release/golutra-cli"
            debug = root / "target/debug/golutra-cli"
            for candidate in (explicit, environment, release, debug):
                candidate.parent.mkdir(parents=True, exist_ok=True)
                candidate.write_text("#!/bin/sh\n", encoding="utf-8")
                candidate.chmod(0o755)

            with patch.dict(
                ADAPTER.os.environ,
                {"GOLUTRA_TBENCH_COLLECTOR": str(environment)},
            ):
                self.assertEqual(
                    ADAPTER.GolutraAgent._resolve_collector_binary(
                        str(explicit), repository_root=root
                    ),
                    explicit.resolve(),
                )
                self.assertEqual(
                    ADAPTER.GolutraAgent._resolve_collector_binary(
                        None, repository_root=root
                    ),
                    environment.resolve(),
                )

            with patch.dict(
                ADAPTER.os.environ,
                {"GOLUTRA_TBENCH_COLLECTOR": ""},
            ):
                self.assertEqual(
                    ADAPTER.GolutraAgent._resolve_collector_binary(
                        None, repository_root=root
                    ),
                    release.resolve(),
                )
                release.unlink()
                self.assertEqual(
                    ADAPTER.GolutraAgent._resolve_collector_binary(
                        None, repository_root=root
                    ),
                    debug.resolve(),
                )
                debug.unlink()
                self.assertIsNone(
                    ADAPTER.GolutraAgent._resolve_collector_binary(
                        None, repository_root=root
                    )
                )

    def test_trace_identity_uses_manifest_path_relative_to_run_root(self):
        with TemporaryDirectory() as temporary:
            run_dir = Path(temporary)
            trace_path = run_dir / "observations/sessions/session/tasks/task/trace.json"
            trace_path.parent.mkdir(parents=True)
            trace_path.write_text(
                json.dumps(
                    {
                        "integrity": {"event_chain_digest": "sha256:trace"},
                        "runtime_identity": "runtime:test",
                    }
                ),
                encoding="utf-8",
            )
            (run_dir / "manifest.json").write_text(
                json.dumps(
                    {
                        "observations": {
                            "sessions": [
                                {
                                    "session_id": "session",
                                    "tasks": [
                                        {
                                            "task_id": "task",
                                            "trace_path": "observations/sessions/session/tasks/task/trace.json",
                                        }
                                    ],
                                }
                            ]
                        }
                    }
                ),
                encoding="utf-8",
            )

            self.assertEqual(
                ADAPTER._trace_identity(run_dir, "task", "session"),
                ("sha256:trace", "runtime:test"),
            )

    def test_external_digest_is_stable_for_mapping_order(self):
        left = {"evaluation_id": "one", "assertions": [{"passed": True}]}
        right = {"assertions": [{"passed": True}], "evaluation_id": "one"}
        self.assertEqual(
            ADAPTER._external_result_digest(left),
            ADAPTER._external_result_digest(right),
        )

    def test_trial_result_supports_aggregate_task_records(self):
        parser_results, resolved, failure_mode = ADAPTER._extract_trial_result(
            {
                "results": [
                    {"task_id": "other", "is_resolved": False},
                    {"task_id": "target", "parser_results": {"tests": "passed"}, "is_resolved": True},
                ]
            },
            "target",
        )
        self.assertEqual(parser_results, {"tests": "passed"})
        self.assertTrue(resolved)
        self.assertIsNone(failure_mode)

    def test_trial_result_preserves_harness_failure_mode(self):
        parser_results, resolved, failure_mode = ADAPTER._extract_trial_result(
            {
                "task_id": "target",
                "parser_results": {},
                "is_resolved": None,
                "failure_mode": "test_timeout",
            },
            "target",
        )

        self.assertEqual(parser_results, {})
        self.assertFalse(resolved)
        self.assertEqual(failure_mode, "test_timeout")

    def test_collector_uses_trial_root_as_the_evidence_base(self):
        command = ADAPTER._collector_command(
            Path("/bin/golutra"),
            Path("/trial/sessions/golutra-runtime"),
            "session-id",
            Path("/trial/sessions/golutra-runtime/evaluation.json"),
            Path("/trial"),
        )

        self.assertEqual(
            command,
            [
                "/bin/golutra",
                "--run-bundle",
                "/trial/sessions/golutra-runtime",
                "--session-id",
                "session-id",
                "eval",
                "ingest",
                "--artifact-base",
                "/trial",
                "/trial/sessions/golutra-runtime/evaluation.json",
            ],
        )

    def test_trace_token_usage_reports_provider_totals(self):
        with TemporaryDirectory() as temporary:
            trial_root = Path(temporary)
            run_dir = trial_root / "sessions/golutra-runtime"
            trace_path = run_dir / "observations/session/task/trace.json"
            trace_path.parent.mkdir(parents=True)
            trace_path.write_text(
                json.dumps(
                    {
                        "events": [
                            {
                                "event_type": "token_usage_recorded",
                                "payload": {
                                    "record": {"input_tokens": 10, "output_tokens": 2}
                                },
                            },
                            {
                                "event_type": "token_usage_recorded",
                                "payload": {
                                    "record": {"input_tokens": 20, "output_tokens": 3}
                                },
                            },
                            {"event_type": "provider_request_started", "payload": {}},
                        ]
                    }
                ),
                encoding="utf-8",
            )
            (run_dir / "manifest.json").write_text(
                json.dumps(
                    {
                        "observations": {
                            "sessions": [
                                {
                                    "tasks": [
                                        {
                                            "trace_path": "observations/session/task/trace.json"
                                        }
                                    ]
                                }
                            ]
                        }
                    }
                ),
                encoding="utf-8",
            )

            self.assertEqual(ADAPTER._trace_token_usage(trial_root), (30, 5))

    def test_collector_retains_timeout_as_a_failed_assertion(self):
        with TemporaryDirectory() as temporary:
            trial_root = Path(temporary)
            run_dir = trial_root / "sessions/golutra-runtime"
            trace_path = run_dir / "observations/session/task/trace.json"
            trace_path.parent.mkdir(parents=True)
            trace_path.write_text(
                json.dumps(
                    {
                        "integrity": {"event_chain_digest": "sha256:trace"},
                        "runtime_identity": "runtime:test",
                    }
                ),
                encoding="utf-8",
            )
            (run_dir / "manifest.json").write_text(
                json.dumps(
                    {
                        "terminal_outcome": {
                            "result": {"task_id": "task", "session_id": "session"}
                        },
                        "observations": {
                            "sessions": [
                                {
                                    "session_id": "session",
                                    "tasks": [
                                        {
                                            "task_id": "task",
                                            "trace_path": "observations/session/task/trace.json",
                                        }
                                    ],
                                }
                            ]
                        },
                    }
                ),
                encoding="utf-8",
            )
            (trial_root / "results.json").write_text(
                json.dumps(
                    {
                        "id": "trial",
                        "task_id": "csv-to-parquet",
                        "is_resolved": None,
                        "failure_mode": "test_timeout",
                        "parser_results": None,
                    }
                ),
                encoding="utf-8",
            )
            (trial_root / "commands.txt").write_text("command", encoding="utf-8")
            agent = ADAPTER.GolutraAgent.__new__(ADAPTER.GolutraAgent)
            agent._result_collection_timeout_sec = 0.1
            agent._dataset_id = "terminal-bench"
            agent._dataset_version = "0.1.1"
            agent._model_name = "test/model"
            agent._collector_binary = Path("/bin/golutra")

            completed = types.SimpleNamespace(returncode=0, stdout="ok", stderr="")
            with patch.object(ADAPTER.subprocess, "run", return_value=completed):
                agent._collect_result(trial_root)

            record = json.loads(
                (run_dir / "terminal-bench-evaluation.json").read_text()
            )
            self.assertEqual(record["verdict"], "fail")
            self.assertEqual(
                [assertion["name"] for assertion in record["assertions"]],
                ["harness_failure_mode"],
            )
            self.assertEqual(record["assertions"][0]["message"], "test_timeout")
            self.assertFalse(
                (run_dir / "terminal-bench-evaluation.pending.json").exists()
            )


if __name__ == "__main__":
    unittest.main()
