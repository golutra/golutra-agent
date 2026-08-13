import importlib.util
import json
import os
import sys
import threading
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
    def test_no_proxy_merge_skips_oversized_entries_without_losing_services(self):
        merged = ADAPTER._merge_no_proxy(
            f"localhost,{'x' * 4097},LOCALHOST",
            ["server", "SERVER", "book-api"],
        )

        self.assertEqual(merged, "localhost,server,book-api")

    def test_proxy_bypasses_compose_service_names_and_aliases(self):
        class Result:
            exit_code = 0
            output = b""

        class Container:
            def __init__(self):
                self.commands = []

            def exec_run(self, command):
                self.commands.append(command)
                return Result()

        class Session:
            def __init__(self):
                self.container = Container()

        with TemporaryDirectory() as temporary:
            root = Path(temporary)
            dataset = root / "dataset"
            run_root = root / "runs/run"
            logging_dir = run_root / "scraper/scraper.1-of-1.run/sessions"
            task_root = dataset / "scraper"
            logging_dir.mkdir(parents=True)
            task_root.mkdir(parents=True)
            (run_root / "run_metadata.json").write_text(
                json.dumps({"dataset_path": str(dataset)}), encoding="utf-8"
            )
            (task_root / "docker-compose.yaml").write_text(
                json.dumps(
                    {
                        "services": {
                            "client": {},
                            "server": {
                                "hostname": "books.internal",
                                "container_name": "books-server",
                                "networks": {
                                    "default": {
                                        "aliases": [
                                            "book-api",
                                            "bad alias",
                                            "${UNEXPANDED_ALIAS}",
                                        ]
                                    }
                                },
                            },
                        }
                    }
                ),
                encoding="utf-8",
            )
            yaml = types.ModuleType("yaml")
            yaml.safe_load = json.loads
            agent = ADAPTER.GolutraAgent(
                proxy_url="http://proxy.internal:7897",
                no_proxy="localhost,127.0.0.1",
            )
            session = Session()

            with patch.dict(sys.modules, {"yaml": yaml}):
                environment = agent._runtime_environment(logging_dir)
                self.assertTrue(agent._configure_tmux_proxy(session, logging_dir))

            expected = (
                "localhost,127.0.0.1,client,server,books.internal,"
                "books-server,book-api"
            )
            self.assertEqual(environment["NO_PROXY"], expected)
            self.assertEqual(environment["no_proxy"], expected)
            self.assertIn(
                ["tmux", "set-environment", "-g", "NO_PROXY", expected],
                session.container.commands,
            )
            self.assertIn(
                ["tmux", "set-environment", "-g", "no_proxy", expected],
                session.container.commands,
            )

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

    def test_agent_timeout_reads_registered_dataset_cache(self):
        with TemporaryDirectory() as temporary:
            root = Path(temporary)
            cache = root / "cache"
            run_root = root / "runs/run"
            logging_dir = run_root / "task/task.1-of-1.run/sessions"
            task_root = cache / "terminal-bench-core/0.1.1/task"
            logging_dir.mkdir(parents=True)
            task_root.mkdir(parents=True)
            (run_root / "run_metadata.json").write_text(
                json.dumps(
                    {
                        "dataset_path": None,
                        "dataset_name": "terminal-bench-core",
                        "dataset_version": "0.1.1",
                    }
                ),
                encoding="utf-8",
            )
            (run_root / "tb.lock").write_text(
                json.dumps(
                    {
                        "run_config": {
                            "global_agent_timeout_sec": None,
                            "global_timeout_multiplier": 2.0,
                        }
                    }
                ),
                encoding="utf-8",
            )
            (task_root / "task.yaml").write_text(
                "max_agent_timeout_sec: 900.0\n", encoding="utf-8"
            )

            registry_client = types.ModuleType("terminal_bench.registry.client")

            class RegistryClient:
                CACHE_DIR = cache

            registry_client.RegistryClient = RegistryClient
            yaml = types.ModuleType("yaml")
            yaml.safe_load = lambda _content: {"max_agent_timeout_sec": 900.0}

            with patch.dict(
                sys.modules,
                {
                    "terminal_bench.registry": types.ModuleType(
                        "terminal_bench.registry"
                    ),
                    "terminal_bench.registry.client": registry_client,
                    "yaml": yaml,
                },
            ):
                self.assertEqual(
                    ADAPTER._terminal_bench_agent_timeout_sec(logging_dir), 1800.0
                )

    def test_runtime_budget_reserves_setup_and_finalization_time(self):
        self.assertEqual(
            ADAPTER._runtime_elapsed_budget_ms(
                agent_timeout_sec=600.0,
                setup_elapsed_sec=40.0,
                finalization_reserve_sec=25.0,
            ),
            535_000,
        )
        self.assertEqual(
            ADAPTER._runtime_elapsed_budget_ms(
                agent_timeout_sec=10.0,
                setup_elapsed_sec=8.0,
                finalization_reserve_sec=25.0,
            ),
            1_000,
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
            def __init__(self, *, times_out: bool = True):
                self.container = Container()
                self.command = None
                self.copy_calls = []
                self.sent_keys = []
                self.times_out = times_out

            def copy_to_container(self, *args, **kwargs):
                self.copy_calls.append((args, kwargs))
                if kwargs.get("container_dir") == "/root/.golutra":
                    raise OSError("archive API cannot write to a tmpfs path")

            def send_command(self, command):
                self.command = command
                if self.times_out:
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
                    model_name="terminal-bench/model-specific-candidate",
                    arm64_binary=str(binary),
                    provider_path=str(provider),
                    credentials_path=str(credentials),
                    collector_binary=str(root / "missing-collector"),
                    agent_command_timeout_sec=10.0,
                    graceful_drain_timeout_sec=0.01,
                )
            with (
                patch.object(agent, "_start_result_collector"),
                self.assertRaises(TimeoutError),
            ):
                agent.perform_task("do work", session, logging_dir)

            self.assertIsNotNone(session.command)
            self.assertIn("--yolo", session.command.command)
            default_argv = ADAPTER.shlex.split(session.command.command)
            self.assertNotIn("--execution-mode", default_argv)
            self.assertNotIn("--tool-profile", default_argv)
            self.assertIn("--defer-external-verification", session.command.command)
            budget_match = ADAPTER.re.search(
                r"--max-elapsed-ms (\d+)", session.command.command
            )
            self.assertIsNotNone(budget_match)
            assert budget_match is not None
            self.assertGreaterEqual(int(budget_match.group(1)), 1_000)
            self.assertLessEqual(int(budget_match.group(1)), 5_000)
            self.assertGreater(session.command.max_timeout_sec, 14.0)
            self.assertLessEqual(session.command.max_timeout_sec, 15.0)
            self.assertEqual(session.sent_keys, [(["C-c"], 0.1)])
            self.assertEqual(
                [call[1]["container_dir"] for call in session.copy_calls],
                ["/installed-agent", "/installed-agent/auth", "/installed-agent/auth"],
            )
            self.assertNotIn(
                "/tests",
                [call[1]["container_dir"] for call in session.copy_calls],
                "Terminal-Bench tests must remain hidden until the harness test phase",
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
            self.assertEqual(observation["code"], "agent_timeout")
            self.assertEqual(observation["facts"]["agent_timeout_sec"], 10.0)
            self.assertEqual(
                observation["facts"]["timeout_class"], "agent_timeout"
            )
            self.assertIn("runtime_max_elapsed_ms", observation["facts"])
            self.assertEqual(observation["facts"]["finalization_reserve_sec"], 5.01)

            explicit_agent = ADAPTER.GolutraAgent(
                arm64_binary=str(binary),
                provider_path=str(provider),
                credentials_path=str(credentials),
                collector_binary=str(root / "missing-collector"),
                agent_command_timeout_sec=10.0,
                graceful_drain_timeout_sec=0.01,
                execution_mode="strict",
                tool_profile="full",
            )
            success_session = Session(times_out=False)
            with patch.object(explicit_agent, "_start_result_collector"):
                result = explicit_agent.perform_task(
                    "task_id=model-specific-task", success_session, logging_dir
                )

            self.assertEqual(result.total_input_tokens, 0)
            self.assertEqual(result.total_output_tokens, 0)
            explicit_argv = ADAPTER.shlex.split(success_session.command.command)
            self.assertEqual(
                explicit_argv[explicit_argv.index("--execution-mode") + 1], "strict"
            )
            self.assertEqual(
                explicit_argv[explicit_argv.index("--tool-profile") + 1], "full"
            )
            self.assertEqual(
                [call[1]["container_dir"] for call in success_session.copy_calls],
                ["/installed-agent", "/installed-agent/auth", "/installed-agent/auth"],
            )
            self.assertNotIn(
                "/tests",
                [call[1]["container_dir"] for call in success_session.copy_calls],
                "successful agent execution must not expose hidden evaluator files",
            )
            completed_observation = json.loads(
                (logging_dir.parent / "golutra-adapter-observation.json").read_text()
            )
            self.assertEqual(completed_observation["status"], "completed")
            self.assertEqual(
                completed_observation["facts"]["agent_timeout_sec"], 10.0
            )
            self.assertIn(
                "runtime_max_elapsed_ms", completed_observation["facts"]
            )

    def test_execution_profile_configuration_is_validated_and_normalized(self):
        agent = ADAPTER.GolutraAgent(execution_mode=" OPEN ", tool_profile="Coding")

        self.assertEqual(agent._execution_mode, "open")
        self.assertEqual(agent._tool_profile, "coding")
        with self.assertRaisesRegex(ValueError, "execution_mode"):
            ADAPTER.GolutraAgent(execution_mode="benchmark")
        with self.assertRaisesRegex(ValueError, "tool_profile"):
            ADAPTER.GolutraAgent(tool_profile="terminal-bench")

    def test_runtime_readiness_uses_manifest_without_opening_live_sqlite(self):
        with TemporaryDirectory() as temporary:
            root = Path(temporary)
            trial_root = root / "trial"
            run_dir = trial_root / "golutra-runtime"
            (run_dir / "state").mkdir(parents=True)
            (run_dir / "manifest.json").write_text(
                json.dumps({"terminal_outcome": {"kind": "in_progress"}}),
                encoding="utf-8",
            )
            (run_dir / "state" / "runtime.sqlite").write_bytes(
                b"not a readable SQLite database"
            )
            agent = ADAPTER.GolutraAgent(graceful_drain_timeout_sec=0.01)

            readiness = agent._runtime_readiness(trial_root)

            self.assertEqual(
                readiness,
                {"state": "running", "terminal_outcome_kind": "in_progress"},
            )

            (run_dir / "manifest.json").write_text(
                json.dumps({"terminal_outcome": {"kind": "result"}}),
                encoding="utf-8",
            )
            self.assertEqual(
                agent._runtime_readiness(trial_root),
                {"state": "terminal", "terminal_outcome_kind": "result"},
            )

    def test_agent_command_timeout_rejects_unbounded_values(self):
        with self.assertRaises(ValueError):
            ADAPTER.GolutraAgent(agent_command_timeout_sec=float("inf"))

    def test_failure_mode_aliases_keep_timeout_ownership_precise(self):
        self.assertEqual(
            ADAPTER._normalized_failure_mode("runtime_command_timeout"),
            "agent_timeout",
        )
        self.assertEqual(
            ADAPTER._normalized_failure_mode("docker_timeout"),
            "environment_timeout",
        )
        self.assertEqual(
            ADAPTER._normalized_failure_mode("test_timeout"), "test_timeout"
        )

    def test_collector_database_race_is_retryable(self):
        self.assertTrue(
            ADAPTER._collector_failure_is_transient(
                "database disk image is malformed"
            )
        )
        self.assertTrue(ADAPTER._collector_failure_is_transient("database is locked"))
        self.assertFalse(ADAPTER._collector_failure_is_transient("invalid record"))

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

    def test_preflight_diagnostic_is_bounded_and_redacted(self):
        raw = (
            b"/installed-agent/golutra: /lib/aarch64-linux-gnu/libc.so.6: "
            b"version `GLIBC_2.34' not found\n"
            b"authorization=Bearer top-secret-value\n"
            + b"x" * 10_000
        )

        detail = ADAPTER._bounded_diagnostic_output(raw)

        self.assertIn("GLIBC_2.34", detail)
        self.assertIn("authorization=[REDACTED]", detail)
        self.assertNotIn("top-secret-value", detail)
        self.assertLessEqual(
            len(detail.encode("utf-8")), ADAPTER._FAILURE_DETAIL_MAX_BYTES
        )

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

    def test_collector_owns_digest_canonicalization_and_correction_uses_bound_record(self):
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
                        "evaluation": {"external_evaluations": []},
                    }
                ),
                encoding="utf-8",
            )
            (run_dir / "manifest.json").write_text(
                json.dumps(
                    {
                        "terminal_outcome": {
                            "kind": "result",
                            "result": {
                                "task_id": "task",
                                "session_id": "session",
                                "thread_id": "thread",
                            },
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
                        "task_id": "task",
                        "is_resolved": False,
                        "parser_results": {"test_output": "failed"},
                        "trial_started_at": "2026-07-31T18:40:48.120000+00:00",
                        "agent_started_at": "2026-07-31T18:40:49.120000+00:00",
                        "agent_ended_at": "2026-07-31T18:40:50.120000+00:00",
                        "test_started_at": "2026-07-31T18:40:51.120000+00:00",
                        "test_ended_at": "2026-07-31T18:40:52.120000+00:00",
                        "trial_ended_at": "2026-07-31T18:40:53.120000+00:00",
                    }
                ),
                encoding="utf-8",
            )
            agent = ADAPTER.GolutraAgent.__new__(ADAPTER.GolutraAgent)
            agent._result_collection_timeout_sec = 0.1
            agent._dataset_id = "terminal-bench"
            agent._dataset_version = "local"
            agent._model_name = "test/model"
            agent._collector_binary = Path("/bin/golutra")
            agent._graceful_drain_timeout_sec = 0.01
            agent._max_external_correction_rounds = 1
            collector_timeouts = []

            def ingest(*_args, **kwargs):
                collector_timeouts.append(kwargs["timeout"])
                submitted = json.loads(
                    (run_dir / "terminal-bench-evaluation.json").read_text()
                )
                self.assertEqual(submitted["result_digest"], "auto")
                bound = dict(submitted)
                bound["result_digest"] = "sha256:rust-canonical"
                trace = json.loads(trace_path.read_text())
                trace["evaluation"]["external_evaluations"] = [bound]
                trace_path.write_text(json.dumps(trace), encoding="utf-8")
                return types.SimpleNamespace(returncode=0, stdout="ok", stderr="")

            with patch.object(ADAPTER.subprocess, "run", side_effect=ingest):
                agent._collect_result(trial_root, "task")

            self.assertEqual(len(collector_timeouts), 1)
            self.assertGreater(collector_timeouts[0], 0)
            self.assertLessEqual(
                collector_timeouts[0], agent._result_collection_timeout_sec
            )

            submitted = json.loads(
                (run_dir / "terminal-bench-evaluation.json").read_text()
            )
            self.assertEqual(submitted["result_digest"], "auto")
            correction = json.loads(
                (run_dir / "terminal-bench-evaluation-correction.json").read_text()
            )
            self.assertEqual(
                correction["source_evaluation"]["result_digest"],
                "sha256:rust-canonical",
            )

            def collector_timeout(command, **kwargs):
                raise ADAPTER.subprocess.TimeoutExpired(command, kwargs["timeout"])

            with patch.object(
                ADAPTER.subprocess, "run", side_effect=collector_timeout
            ):
                agent._collect_result(trial_root, "task")
            timeout_pending = json.loads(
                (run_dir / "terminal-bench-evaluation.pending.json").read_text()
            )
            self.assertEqual(timeout_pending["status"], "collector_timeout")
            self.assertEqual(timeout_pending["error_type"], "TimeoutExpired")
            self.assertEqual(
                timeout_pending["record_path"],
                str(run_dir / "terminal-bench-evaluation.json"),
            )
            self.assertGreater(timeout_pending["timeout_sec"], 0)
            self.assertIn("timed out", timeout_pending["detail"])
            self.assertFalse(
                (run_dir / "terminal-bench-evaluation-correction.json").exists()
            )

            trace = json.loads(trace_path.read_text())
            trace["evaluation"]["external_evaluations"] = []
            trace_path.write_text(json.dumps(trace), encoding="utf-8")
            completed = types.SimpleNamespace(returncode=0, stdout="ok", stderr="")
            with patch.object(ADAPTER.subprocess, "run", return_value=completed):
                agent._collect_result(trial_root, "task")

            pending = json.loads(
                (run_dir / "terminal-bench-evaluation.pending.json").read_text()
            )
            self.assertEqual(pending["status"], "pending_trace_binding")
            self.assertEqual(
                pending["evaluation_id"],
                "terminal-bench:trial:task",
            )
            self.assertFalse(
                (run_dir / "terminal-bench-evaluation-correction.json").exists()
            )

    def test_external_failure_writes_an_isolated_unscored_continuation_plan(self):
        with TemporaryDirectory() as temporary:
            run_dir = Path(temporary) / "golutra-runtime"
            run_dir.mkdir()
            collector = Path(temporary) / "golutra-cli"
            collector.write_text("#!/bin/sh\n", encoding="utf-8")
            collector.chmod(0o755)
            agent = ADAPTER.GolutraAgent(
                collector_binary=str(collector),
                max_external_correction_rounds=1,
            )
            record = {
                "evaluation_id": "terminal-bench:run:task",
                "result_digest": "sha256:evaluation",
                "verdict": "fail",
                "terminal_cause": {"code": "assertion_failed"},
                "assertions": [
                    {"name": "test_output", "passed": False, "message": "missing output"},
                    {"name": "test_other", "passed": True, "message": "ok"},
                ],
            }

            plan = agent._external_correction_plan(run_dir, record, "thread-1")

            self.assertIsNotNone(plan)
            assert plan is not None
            self.assertEqual(plan["schema_version"], 2)
            self.assertEqual(plan["status"], "isolated_continuation_required")
            self.assertEqual(plan["thread_id"], "thread-1")
            self.assertEqual(
                plan["feedback"],
                "External evaluator feedback. Correct the workspace and rerun the evaluator.\n- test_output: missing output",
            )
            self.assertNotIn("command", plan)
            self.assertEqual(
                plan["source_evaluation"],
                {
                    "evaluation_id": "terminal-bench:run:task",
                    "result_digest": "sha256:evaluation",
                    "run_bundle": ".",
                },
            )
            self.assertEqual(plan["isolation"]["mode"], "unscored_diagnostic")
            self.assertTrue(plan["isolation"]["source_trial_immutable"])
            self.assertTrue(plan["isolation"]["requires_cloned_workspace"])
            self.assertTrue(plan["isolation"]["requires_cloned_run_bundle"])
            self.assertFalse(plan["isolation"]["may_replace_source_score"])
            self.assertTrue(
                plan["isolation"]["promotion_requires_independent_evaluation"]
            )
            self.assertTrue((run_dir / "external-correction-1.json").is_file())

    def test_external_failure_replaces_a_legacy_in_place_resume_plan(self):
        with TemporaryDirectory() as temporary:
            run_dir = Path(temporary)
            marker = run_dir / "external-correction-1.json"
            marker.write_text(
                json.dumps(
                    {
                        "status": "manual_resume_required",
                        "thread_id": "thread-1",
                        "command": ["golutra", "exec", "resume", "thread-1"],
                    }
                ),
                encoding="utf-8",
            )
            agent = ADAPTER.GolutraAgent(max_external_correction_rounds=1)
            record = {
                "evaluation_id": "terminal-bench:run:task",
                "result_digest": "sha256:new-evaluation",
                "verdict": "fail",
                "terminal_cause": {"code": "assertion_failed"},
                "assertions": [
                    {"name": "test_output", "passed": False, "message": "missing output"}
                ],
            }

            plan = agent._external_correction_plan(run_dir, record, "thread-1")

            assert plan is not None
            self.assertEqual(plan["schema_version"], 2)
            self.assertEqual(plan["status"], "isolated_continuation_required")
            self.assertNotIn("command", plan)
            self.assertEqual(json.loads(marker.read_text()), plan)

    def test_external_failure_deduplicates_shared_evaluator_detail(self):
        with TemporaryDirectory() as temporary:
            agent = ADAPTER.GolutraAgent(max_external_correction_rounds=1)
            shared_detail = "failed\nEvaluator output:\nFAILED shared evaluator output"
            record = {
                "evaluation_id": "terminal-bench:run:task",
                "result_digest": "sha256:evaluation",
                "verdict": "fail",
                "terminal_cause": {"code": "assertion_failed"},
                "assertions": [
                    {"name": "test_one", "passed": False, "message": shared_detail},
                    {"name": "test_two", "passed": False, "message": shared_detail},
                ],
            }

            plan = agent._external_correction_plan(
                Path(temporary), record, "thread-1"
            )

            assert plan is not None
            self.assertIn("test_one, test_two", plan["feedback"])
            self.assertEqual(plan["feedback"].count("Evaluator output:"), 1)

    def test_external_correction_ignores_non_assertion_failures(self):
        agent = ADAPTER.GolutraAgent(max_external_correction_rounds=1)
        self.assertIsNone(
            agent._external_correction_plan(
                Path("/tmp/run"),
                {"verdict": "fail", "terminal_cause": {"code": "test_timeout"}},
                "thread-1",
            )
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

    def test_trial_result_rejects_mismatched_single_and_aggregate_records(self):
        with self.assertRaisesRegex(
            ADAPTER.TrialResultBindingError, "does not match bound task_id"
        ):
            ADAPTER._extract_trial_result(
                {
                    "task_id": "other",
                    "parser_results": {"wrong_test": "failed"},
                    "is_resolved": False,
                },
                "target",
            )

        with self.assertRaisesRegex(
            ADAPTER.TrialResultBindingError, "available task_ids"
        ):
            ADAPTER._extract_trial_result(
                {
                    "results": [
                        {"task_id": "other", "is_resolved": False},
                        {"task_id": "another", "is_resolved": True},
                    ]
                },
                "target",
            )

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

    def test_collector_retains_result_without_ingesting_an_in_progress_bundle(self):
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
            agent._graceful_drain_timeout_sec = 0.01

            with patch.object(ADAPTER.subprocess, "run") as collector:
                agent._collect_result(trial_root, "csv-to-parquet")
            collector.assert_not_called()

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
            pending = json.loads(
                (run_dir / "terminal-bench-evaluation.pending.json").read_text()
            )
            self.assertEqual(pending["status"], "pending_runtime_terminal")
            self.assertEqual(
                pending["record_path"],
                str(run_dir / "terminal-bench-evaluation.json"),
            )
            self.assertFalse(stale_pending.exists())

    def test_result_lookup_is_bound_to_the_exact_trial_directory(self):
        with TemporaryDirectory() as temporary:
            task_root = Path(temporary) / "task"
            trial_root = task_root / "task.1-of-1.run"
            nested = trial_root / "unrelated"
            nested.mkdir(parents=True)
            (task_root / "results.json").write_text("{}", encoding="utf-8")
            (nested / "results.json").write_text("{}", encoding="utf-8")

            self.assertIsNone(ADAPTER._find_trial_results(trial_root))

            exact = trial_root / "results.json"
            exact.write_text("{}", encoding="utf-8")
            self.assertEqual(ADAPTER._find_trial_results(trial_root), exact)

    def test_collector_thread_captures_exact_trial_and_task_identity(self):
        with TemporaryDirectory() as temporary:
            logging_dir = (
                Path(temporary)
                / "case-name"
                / "case-name.1-of-1.run"
                / "sessions"
            )
            logging_dir.mkdir(parents=True)
            captured = {}

            class Thread:
                def __init__(self, **kwargs):
                    captured.update(kwargs)

                def start(self):
                    captured["started"] = True

            agent = ADAPTER.GolutraAgent.__new__(ADAPTER.GolutraAgent)
            with patch.object(ADAPTER.threading, "Thread", Thread):
                agent._start_result_collector(logging_dir)

            self.assertEqual(
                captured["args"],
                (logging_dir.parent.resolve(), "case-name"),
            )
            self.assertEqual(
                captured["name"],
                "golutra-evaluation-case-name.1-of-1.run",
            )
            self.assertFalse(captured["daemon"])
            self.assertTrue(captured["started"])

    def test_concurrent_collectors_never_bind_sibling_trial_results(self):
        with TemporaryDirectory() as temporary:
            run_root = Path(temporary)
            cases = {
                "alpha": ("runtime-alpha", "alpha_test"),
                "beta": ("runtime-beta", "beta_test"),
            }
            trials = {}
            for case_id, (runtime_task_id, assertion_name) in cases.items():
                trial_root = run_root / case_id / f"{case_id}.1-of-1.run"
                run_dir = trial_root / "sessions" / "golutra-runtime"
                run_dir.mkdir(parents=True)
                (run_dir / "manifest.json").write_text(
                    json.dumps(
                        {
                            "terminal_outcome": {
                                "kind": "result",
                                "result": {
                                    "task_id": runtime_task_id,
                                    "session_id": f"session-{case_id}",
                                },
                            },
                            "observations": {"sessions": []},
                        }
                    ),
                    encoding="utf-8",
                )
                (trial_root / "results.json").write_text(
                    json.dumps(
                        {
                            "id": f"trial-{case_id}",
                            "task_id": case_id,
                            "is_resolved": False,
                            "parser_results": {assertion_name: "failed"},
                        }
                    ),
                    encoding="utf-8",
                )
                trials[case_id] = (trial_root, run_dir)

            (run_root / "results.json").write_text(
                json.dumps(
                    {
                        "results": [
                            {
                                "task_id": "alpha",
                                "parser_results": {"wrong_for_beta": "failed"},
                            },
                            {
                                "task_id": "beta",
                                "parser_results": {"wrong_for_alpha": "failed"},
                            },
                        ]
                    }
                ),
                encoding="utf-8",
            )

            agent = ADAPTER.GolutraAgent.__new__(ADAPTER.GolutraAgent)
            agent._result_collection_timeout_sec = 1.0
            agent._dataset_id = "terminal-bench"
            agent._dataset_version = "local"
            agent._model_name = "test/model"
            agent._collector_binary = None
            agent._graceful_drain_timeout_sec = 0.01
            barrier = threading.Barrier(len(cases))

            def collect(case_id):
                barrier.wait()
                agent._collect_result(trials[case_id][0], case_id)

            threads = [
                threading.Thread(target=collect, args=(case_id,))
                for case_id in cases
            ]
            for thread in threads:
                thread.start()
            for thread in threads:
                thread.join(timeout=2.0)
                self.assertFalse(thread.is_alive())

            for case_id, (runtime_task_id, assertion_name) in cases.items():
                record = json.loads(
                    (
                        trials[case_id][1] / "terminal-bench-evaluation.json"
                    ).read_text()
                )
                self.assertEqual(record["source_task_id"], runtime_task_id)
                self.assertEqual(record["case_id"], case_id)
                self.assertEqual(
                    [assertion["name"] for assertion in record["assertions"]],
                    [assertion_name],
                )

    def test_collector_rejects_a_result_with_another_trial_identity(self):
        with TemporaryDirectory() as temporary:
            trial_root = Path(temporary) / "target" / "target.1-of-1.run"
            run_dir = trial_root / "sessions" / "golutra-runtime"
            run_dir.mkdir(parents=True)
            (run_dir / "manifest.json").write_text(
                json.dumps(
                    {
                        "terminal_outcome": {
                            "kind": "result",
                            "result": {
                                "task_id": "runtime-task",
                                "session_id": "runtime-session",
                            },
                        }
                    }
                ),
                encoding="utf-8",
            )
            (trial_root / "results.json").write_text(
                json.dumps(
                    {
                        "id": "wrong-trial",
                        "task_id": "other",
                        "is_resolved": True,
                        "parser_results": {"wrong_test": "passed"},
                    }
                ),
                encoding="utf-8",
            )
            agent = ADAPTER.GolutraAgent.__new__(ADAPTER.GolutraAgent)
            agent._result_collection_timeout_sec = 0.1
            agent._graceful_drain_timeout_sec = 0.01

            with patch.object(ADAPTER.subprocess, "run") as collector:
                agent._collect_result(trial_root, "target")

            collector.assert_not_called()
            self.assertFalse(
                (run_dir / "terminal-bench-evaluation.json").exists()
            )
            pending = json.loads(
                (run_dir / "terminal-bench-evaluation.pending.json").read_text()
            )
            self.assertEqual(pending["status"], "result_identity_mismatch")
            self.assertEqual(pending["expected_task_id"], "target")


if __name__ == "__main__":
    unittest.main()
