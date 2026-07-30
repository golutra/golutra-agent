import importlib.util
import json
import os
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

        def _render_instruction(self, instruction):
            return instruction

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
    def test_default_collector_horizon_covers_long_agent_and_test_timeouts(self):
        agent = ADAPTER.GolutraAgent()

        self.assertEqual(agent._result_collection_timeout_sec, 3600.0)
        self.assertEqual(agent._agent_command_timeout_sec(None), 600.0)

    def test_agent_timeout_follows_run_config_and_task_metadata(self):
        with TemporaryDirectory() as temporary:
            root = Path(temporary)
            dataset = root / "dataset"
            run_root = root / "runs/run"
            logging_dir = run_root / "task/task.1-of-1.run/sessions"
            (dataset / "task").mkdir(parents=True)
            logging_dir.mkdir(parents=True)
            (run_root / "run_metadata.json").write_text(
                json.dumps({"dataset_path": str(dataset)}), encoding="utf-8"
            )
            (run_root / "tb.lock").write_text(
                json.dumps(
                    {
                        "run_config": {
                            "global_agent_timeout_sec": None,
                            "global_timeout_multiplier": 1.5,
                        }
                    }
                ),
                encoding="utf-8",
            )
            (dataset / "task/task.yaml").write_text(
                "max_agent_timeout_sec: 360.0\n", encoding="utf-8"
            )
            yaml = types.ModuleType("yaml")
            yaml.safe_load = lambda _content: {"max_agent_timeout_sec": 360.0}

            with patch.dict(sys.modules, {"yaml": yaml}):
                self.assertEqual(
                    ADAPTER._terminal_bench_agent_timeout_sec(logging_dir), 540.0
                )

            (run_root / "tb.lock").write_text(
                json.dumps(
                    {
                        "run_config": {
                            "global_agent_timeout_sec": 120.0,
                            "global_timeout_multiplier": 3.0,
                        }
                    }
                ),
                encoding="utf-8",
            )
            self.assertEqual(
                ADAPTER._terminal_bench_agent_timeout_sec(logging_dir), 120.0
            )

    def test_agent_command_timeout_interrupts_the_runtime(self):
        class Result:
            def __init__(self, output: bytes = b""):
                self.exit_code = 0
                self.output = output

        class Container:
            def __init__(self):
                self.commands = []

            def exec_run(self, command):
                self.commands.append(command)
                if command == ["uname", "-m"]:
                    return Result(b"aarch64\n")
                if command == ["pwd"]:
                    return Result(b"/app\n")
                return Result()

        class Session:
            def __init__(self):
                self.container = Container()
                self.command = None
                self.copy_calls = []
                self.sent_keys = []

            def copy_to_container(self, *args, **kwargs):
                self.copy_calls.append((args, kwargs))
                if kwargs.get("container_dir") == "/root/.golutra":
                    raise OSError("archive API cannot write to a tmpfs path")

            def send_command(self, command):
                self.command = command
                raise TimeoutError("command timed out")

            def send_keys(self, *, keys, min_timeout_sec):
                self.sent_keys.append((keys, min_timeout_sec))

        with TemporaryDirectory() as temporary:
            root = Path(temporary)
            binary = root / "golutra"
            provider = root / "provider.json"
            credentials = root / "credentials.json"
            logging_dir = root / "run/task/trial/sessions"
            binary.write_text("binary", encoding="utf-8")
            provider.write_text(
                json.dumps(
                    {
                        "version": 1,
                        "active_profile": "test",
                        "profiles": [
                            {
                                "name": "test",
                                "credential_ref": {"id": "credential"},
                            }
                        ],
                    }
                ),
                encoding="utf-8",
            )
            credentials.write_text(
                json.dumps(
                    {
                        "version": 1,
                        "credentials": {"credential": {"api_key": "secret"}},
                    }
                ),
                encoding="utf-8",
            )
            logging_dir.mkdir(parents=True)
            session = Session()
            with patch.dict(ADAPTER.os.environ, {"GOLUTRA_TBENCH_PROXY": ""}):
                agent = ADAPTER.GolutraAgent(
                    arm64_binary=str(binary),
                    provider_path=str(provider),
                    credentials_path=str(credentials),
                    collector_binary=str(root / "missing-collector"),
                    agent_command_timeout_sec=10.0,
                )
            with (
                patch.object(agent, "_start_result_collector"),
                self.assertRaises(TimeoutError),
            ):
                agent.perform_task("do work", session, logging_dir)

            self.assertIsNotNone(session.command)
            self.assertIn("--yolo", session.command.command)
            self.assertGreater(session.command.max_timeout_sec, 14.0)
            self.assertLessEqual(session.command.max_timeout_sec, 15.0)
            self.assertEqual(session.sent_keys, [(["C-c"], 0.1)])
            self.assertEqual(
                [call[1]["container_dir"] for call in session.copy_calls],
                ["/installed-agent", "/installed-agent/auth", "/installed-agent/auth"],
            )
            setup_command = next(
                command[2]
                for command in session.container.commands
                if command[:2] == ["sh", "-c"]
            )
            self.assertIn(
                "cp /installed-agent/auth/provider.json /root/.golutra/provider.json",
                setup_command,
            )
            self.assertIn(
                "trap 'rm -rf /installed-agent/auth' EXIT",
                setup_command,
            )
            observation = json.loads(
                (logging_dir.parent / "golutra-adapter-observation.json").read_text()
            )
            self.assertEqual(observation["status"], "failed")
            self.assertEqual(observation["code"], "runtime_command_timeout")
            self.assertEqual(observation["facts"]["agent_timeout_sec"], 10.0)

    def test_agent_command_timeout_rejects_unbounded_values(self):
        with self.assertRaises(ValueError):
            ADAPTER.GolutraAgent(agent_command_timeout_sec=float("inf"))

    def test_installation_failure_is_retained_before_runtime_exists(self):
        with TemporaryDirectory() as temporary:
            trial_root = Path(temporary)
            logging_dir = trial_root / "sessions"
            agent = ADAPTER.GolutraAgent.__new__(ADAPTER.GolutraAgent)

            result = agent._installation_failure(
                logging_dir,
                "unsupported_architecture",
                {"architecture": "s390x"},
            )

            self.assertEqual(result.failure_mode, "agent_installation_failed")
            observation = json.loads(
                (trial_root / "golutra-adapter-observation.json").read_text()
            )
            self.assertEqual(observation["schema_version"], 1)
            self.assertEqual(observation["phase"], "setup")
            self.assertEqual(observation["status"], "failed")
            self.assertEqual(observation["code"], "unsupported_architecture")
            self.assertEqual(observation["facts"], {"architecture": "s390x"})

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
            os.utime(release, (300, 300))
            os.utime(debug, (200, 200))

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

                self.assertIsNone(
                    ADAPTER.GolutraAgent._resolve_collector_binary(
                        str(root / "missing"), repository_root=root
                    )
                )

    def test_collector_prefers_freshest_binary_and_rejects_stale_builds(self):
        with TemporaryDirectory() as temporary:
            root = Path(temporary)
            release = root / "target/release/golutra-cli"
            debug = root / "target/debug/golutra-cli"
            source = root / "crates/golutra-cli/src/main.rs"
            for candidate in (release, debug):
                candidate.parent.mkdir(parents=True, exist_ok=True)
                candidate.write_text("#!/bin/sh\n", encoding="utf-8")
                candidate.chmod(0o755)
            source.parent.mkdir(parents=True)
            source.write_text("fn main() {}\n", encoding="utf-8")
            os.utime(release, (100, 100))
            os.utime(source, (200, 200))
            os.utime(debug, (300, 300))

            with patch.dict(
                ADAPTER.os.environ,
                {"GOLUTRA_TBENCH_COLLECTOR": ""},
            ):
                self.assertEqual(
                    ADAPTER.GolutraAgent._resolve_collector_binary(
                        None, repository_root=root
                    ),
                    debug.resolve(),
                )
                os.utime(debug, (150, 150))
                self.assertIsNone(
                    ADAPTER.GolutraAgent._resolve_collector_binary(
                        None, repository_root=root
                    )
                )
                os.utime(release, (400, 400))
                self.assertEqual(
                    ADAPTER.GolutraAgent._resolve_collector_binary(
                        None, repository_root=root
                    ),
                    release.resolve(),
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

    def test_failure_detail_prefers_root_causes_deduplicates_and_redacts(self):
        with TemporaryDirectory() as temporary:
            trial_root = Path(temporary)
            panes = trial_root / "panes"
            panes.mkdir()
            (panes / "post-test.txt").write_text(
                "\n".join(
                    [
                        "routine setup output",
                        "\x1b[31mfatal: branch has no commit\x1b[0m",
                        "fatal: branch has no commit",
                        "error: authorization=Bearer top-secret-value",
                        "E       AssertionError: expected ready, got empty",
                        "FAILED tests/test_output.py::test_output",
                        "x" * 10_000,
                    ]
                ),
                encoding="utf-8",
            )

            detail = ADAPTER._failure_detail(trial_root)

            self.assertIsNotNone(detail)
            self.assertTrue(detail.startswith("fatal: branch has no commit"))
            self.assertEqual(detail.count("fatal: branch has no commit"), 1)
            self.assertIn("AssertionError: expected ready, got empty", detail)
            self.assertIn("authorization=[REDACTED]", detail)
            self.assertNotIn("top-secret-value", detail)
            self.assertNotIn("routine setup output", detail)
            self.assertLessEqual(
                len(detail.encode("utf-8")), ADAPTER._FAILURE_DETAIL_MAX_BYTES
            )

    def test_failed_assertions_and_terminal_cause_include_failure_detail(self):
        detail = "fatal: branch has no commit\nerror: refspec main does not exist"
        assertions = ADAPTER._evaluation_assertions(
            {"failed_test": "failed", "passing_test": "passed"},
            False,
            None,
            ["panes/post-test.txt"],
            detail,
        )

        failed = next(item for item in assertions if item["name"] == "failed_test")
        passed = next(item for item in assertions if item["name"] == "passing_test")
        self.assertIn(detail, failed["message"])
        self.assertEqual(passed["message"], "passed")

        _, terminal_cause = ADAPTER._evaluation_phases(
            {"task_id": "target"},
            "target",
            assertions,
            False,
            None,
            ["panes/post-test.txt"],
            detail,
        )
        self.assertIn(detail, terminal_cause["message"])
        self.assertLessEqual(
            len(terminal_cause["message"].encode("utf-8")),
            ADAPTER._FAILURE_DETAIL_MAX_BYTES,
        )

    def test_evaluation_phases_preserve_timing_and_terminal_cause(self):
        assertions = [
            {
                "assertion_id": "terminal-bench:resolved",
                "name": "resolved",
                "passed": False,
                "message": "False",
                "evidence_refs": ["results.json"],
            }
        ]
        phases, terminal_cause = ADAPTER._evaluation_phases(
            {
                "task_id": "target",
                "trial_started_at": "2026-07-24T06:21:58+00:00",
                "agent_started_at": "2026-07-24T06:22:04+00:00",
                "agent_ended_at": "2026-07-24T06:25:57+00:00",
                "test_started_at": "2026-07-24T06:26:00+00:00",
                "test_ended_at": "2026-07-24T06:27:00+00:00",
                "trial_ended_at": "2026-07-24T06:27:01+00:00",
            },
            "target",
            assertions,
            False,
            "test_timeout",
            ["results.json"],
        )

        self.assertEqual([phase["kind"] for phase in phases], ["setup", "agent", "test", "assertion"])
        self.assertEqual(phases[0]["duration_ms"], 6_000)
        self.assertEqual(phases[1]["duration_ms"], 233_000)
        self.assertEqual(phases[2]["duration_ms"], 60_000)
        self.assertEqual(phases[2]["status"], "timed_out")
        self.assertEqual(
            phases[3]["assertion_refs"], ["terminal-bench:resolved"]
        )
        self.assertEqual(terminal_cause["code"], "test_timeout")
        self.assertEqual(terminal_cause["phase_id"], "terminal-bench:test")
        self.assertTrue(terminal_cause["retryable"])

    def test_phase_timestamps_are_normalized_to_rfc3339_utc(self):
        phase = ADAPTER._phase_record(
            "setup",
            "setup",
            "passed",
            "2026-07-24T06:21:58",
            "2026-07-24T06:22:04+00:00",
            [],
        )

        self.assertEqual(phase["started_at"], "2026-07-24T06:21:58Z")
        self.assertEqual(phase["completed_at"], "2026-07-24T06:22:04Z")
        self.assertEqual(phase["duration_ms"], 6_000)

    def test_phase_duration_uses_exact_millisecond_flooring(self):
        self.assertEqual(
            ADAPTER._phase_duration_ms(
                "2026-07-24T06:21:58.999999Z",
                "2026-07-25T06:21:59.001998Z",
            ),
            86_400_001,
        )

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

    def test_collector_failure_mode_overrides_resolved_verdict(self):
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
                            "kind": "in_progress",
                            "reason": "agent still running",
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
                        "is_resolved": True,
                        "failure_mode": "test_timeout",
                        "parser_results": None,
                    }
                ),
                encoding="utf-8",
            )
            (trial_root / "commands.txt").write_text("command", encoding="utf-8")
            stale_pending = trial_root / "golutra-evaluation.pending.json"
            stale_pending.write_text(
                json.dumps({"status": "pending_inputs"}), encoding="utf-8"
            )
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
            self.assertEqual(record["base_trace_digest"], "auto")
            self.assertEqual(record["runtime_identity"], "auto")
            self.assertEqual(record["verdict"], "fail")
            self.assertEqual(
                [assertion["name"] for assertion in record["assertions"]],
                ["harness_failure_mode"],
            )
            self.assertEqual(record["assertions"][0]["message"], "test_timeout")
            self.assertEqual(record["terminal_cause"]["code"], "test_timeout")
            self.assertEqual(
                next(phase for phase in record["phases"] if phase["kind"] == "test")[
                    "status"
                ],
                "timed_out",
            )
            self.assertFalse(
                (run_dir / "terminal-bench-evaluation.pending.json").exists()
            )
            self.assertFalse(stale_pending.exists())


if __name__ == "__main__":
    unittest.main()
