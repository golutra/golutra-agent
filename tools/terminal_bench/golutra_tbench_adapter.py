"""Terminal-Bench adapter that retains one governed Golutra run per trial."""

from __future__ import annotations

import hashlib
import json
import math
import os
import re
import shlex
import shutil
import subprocess
import tempfile
import threading
import time
from datetime import datetime, timezone
from importlib import metadata
from pathlib import Path

from terminal_bench.agents.base_agent import AgentResult, BaseAgent
from terminal_bench.agents.failure_mode import FailureMode
from terminal_bench.terminal.models import TerminalCommand
from terminal_bench.terminal.tmux_session import TmuxSession

_DEFAULT_RESULT_COLLECTION_TIMEOUT_SEC = 3600.0
_DEFAULT_AGENT_COMMAND_TIMEOUT_SEC = 600.0
_AGENT_COMMAND_TIMEOUT_GRACE_SEC = 5.0
_FAILURE_LOG_SCAN_BYTES = 1024 * 1024
_FAILURE_DETAIL_MAX_BYTES = 2048
_FAILURE_LINE_MAX_BYTES = 512

_ANSI_ESCAPE_RE = re.compile(r"\x1b(?:[@-Z\\-_]|\[[0-?]*[ -/]*[@-~])")
_BEARER_CREDENTIAL_RE = re.compile(r"(?i)\bBearer\s+[^\s,;]+")
_URL_CREDENTIAL_RE = re.compile(r"(?i)(\b[a-z][a-z0-9+.-]*://)[^\s/@:]+:[^\s/@]+@")
_SECRET_ASSIGNMENT_RE = re.compile(
    r"(?i)\b(api[-_ ]?key|access[-_ ]?token|refresh[-_ ]?token|authorization|"
    r"password|passwd|secret|token)(\s*[:=]\s*)"
    r"(\"[^\"]*\"|'[^']*'|[^\s,;]+)"
)
_ROOT_CAUSE_LINE_RE = re.compile(
    r"(?i)^(?:fatal|error)\s*:|\b(?:AssertionError|[A-Za-z][A-Za-z0-9_.]*(?:Error|Exception))\b"
)
_PYTEST_DETAIL_LINE_RE = re.compile(
    r"(?i)^E(?:\s|$)|^(?:expected|actual|got)\b"
)
_FAILURE_SUMMARY_LINE_RE = re.compile(
    r"(?i)^(?:FAILED|FAILURES?\b|TESTS? FAILED\b)"
)


class GolutraAgent(BaseAgent):
    """Run a locally built Golutra CLI and retain governed runtime data."""

    _PROXY_ENV_NAMES = (
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "http_proxy",
        "https_proxy",
        "all_proxy",
    )

    def __init__(
        self,
        model_name: str = "openai-compatible/gpt-5.5",
        arm64_binary: str = "/tmp/golutra-linux-bin/golutra-cli",
        amd64_binary: str = "/tmp/golutra-linux-bin-amd64/golutra-cli",
        provider_path: str | None = None,
        credentials_path: str | None = None,
        proxy_url: str | None = None,
        no_proxy: str = "localhost,127.0.0.1,::1",
        workspace_path: str | None = None,
        collector_binary: str | None = None,
        dataset_id: str = "terminal-bench",
        dataset_version: str = "unknown",
        result_collection_timeout_sec: float = _DEFAULT_RESULT_COLLECTION_TIMEOUT_SEC,
        agent_command_timeout_sec: float | None = None,
        **kwargs,
    ):
        super().__init__(**kwargs)
        self._model_name = model_name
        self._binaries = {
            "aarch64": Path(arm64_binary),
            "arm64": Path(arm64_binary),
            "x86_64": Path(amd64_binary),
            "amd64": Path(amd64_binary),
        }
        golutra_home = Path(os.environ.get("GOLUTRA_HOME", Path.home() / ".golutra"))
        self._provider_path = Path(provider_path) if provider_path else golutra_home / "provider.json"
        self._credentials_path = (
            Path(credentials_path)
            if credentials_path
            else golutra_home / "credentials.json"
        )
        self._proxy_url = proxy_url or os.environ.get("GOLUTRA_TBENCH_PROXY")
        self._no_proxy = no_proxy
        self._workspace_path_override = workspace_path
        self._collector_binary = self._resolve_collector_binary(collector_binary)
        self._dataset_id = dataset_id
        self._dataset_version = dataset_version
        self._result_collection_timeout_sec = result_collection_timeout_sec
        self._agent_command_timeout_override = (
            _positive_timeout(agent_command_timeout_sec)
            if agent_command_timeout_sec is not None
            else None
        )

    @staticmethod
    def name() -> str:
        return "golutra"

    @property
    def version(self) -> str:
        return "local-runtime-data"

    def _active_auth_files(self, destination: Path) -> tuple[Path, Path]:
        provider = json.loads(self._provider_path.read_text())
        credentials = json.loads(self._credentials_path.read_text())
        active_name = provider.get("active_profile")
        active_profiles = [
            profile for profile in provider.get("profiles", []) if profile.get("name") == active_name
        ]
        if len(active_profiles) != 1:
            raise ValueError(f"active Golutra provider profile {active_name!r} was not found")
        active_profile = active_profiles[0]
        credential_id = active_profile.get("credential_ref", {}).get("id")
        active_credential = credentials.get("credentials", {}).get(credential_id)
        if not credential_id or active_credential is None:
            raise ValueError("active Golutra provider credential was not found")

        provider_file = destination / "provider.json"
        credentials_file = destination / "credentials.json"
        provider_file.write_text(
            json.dumps(
                {
                    "version": provider["version"],
                    "active_profile": active_name,
                    "profiles": [active_profile],
                },
                indent=2,
            )
            + "\n"
        )
        credentials_file.write_text(
            json.dumps(
                {
                    "version": credentials["version"],
                    "credentials": {credential_id: active_credential},
                },
                indent=2,
            )
            + "\n"
        )
        provider_file.chmod(0o600)
        credentials_file.chmod(0o600)
        return provider_file, credentials_file

    def _runtime_environment(self) -> dict[str, str]:
        environment = {"HOME": "/root", "GOLUTRA_HOME": "/root/.golutra"}
        if self._proxy_url:
            environment.update(
                {name: self._proxy_url for name in self._PROXY_ENV_NAMES}
            )
            environment.update({"NO_PROXY": self._no_proxy, "no_proxy": self._no_proxy})
        return environment

    def _configure_tmux_proxy(self, session: TmuxSession) -> bool:
        if not self._proxy_url:
            return True
        proxy_environment = {
            name: self._proxy_url for name in self._PROXY_ENV_NAMES
        }
        proxy_environment.update({"NO_PROXY": self._no_proxy, "no_proxy": self._no_proxy})
        for name, value in proxy_environment.items():
            result = session.container.exec_run(
                ["tmux", "set-environment", "-g", name, value]
            )
            if result.exit_code != 0:
                return False
        return True

    def _workspace_path(self, session: TmuxSession) -> str | None:
        if self._workspace_path_override:
            candidate = self._workspace_path_override.strip()
        else:
            result = session.container.exec_run(["pwd"])
            if result.exit_code != 0:
                return None
            candidate = result.output.decode(errors="replace").strip()
        if not candidate.startswith("/") or "\x00" in candidate or "\n" in candidate:
            return None
        return candidate

    @staticmethod
    def _resolve_collector_binary(
        configured: str | None,
        *,
        repository_root: Path | None = None,
    ) -> Path | None:
        repository_root = repository_root or Path(__file__).resolve().parents[2]

        def executable(value: str | Path) -> Path | None:
            path = Path(value).expanduser()
            return path.resolve() if path.is_file() and os.access(path, os.X_OK) else None

        if configured and str(configured).strip():
            return executable(configured)
        environment = os.environ.get("GOLUTRA_TBENCH_COLLECTOR")
        if environment and environment.strip():
            return executable(environment)

        local_candidates = [
            executable(repository_root / "target" / "release" / "golutra-cli"),
            executable(repository_root / "target" / "debug" / "golutra-cli"),
        ]
        local_candidates = [candidate for candidate in local_candidates if candidate]
        if not local_candidates:
            return None

        source_candidates = [repository_root / "Cargo.toml", repository_root / "Cargo.lock"]
        crates_root = repository_root / "crates"
        if crates_root.is_dir():
            source_candidates.extend(crates_root.rglob("Cargo.toml"))
            source_candidates.extend(crates_root.rglob("*.rs"))
        source_mtimes = []
        for source in source_candidates:
            try:
                source_mtimes.append(source.stat().st_mtime_ns)
            except OSError:
                continue
        if source_mtimes:
            newest_source = max(source_mtimes)
            local_candidates = [
                candidate
                for candidate in local_candidates
                if candidate.stat().st_mtime_ns >= newest_source
            ]
        if not local_candidates:
            return None

        return max(
            local_candidates,
            key=lambda candidate: (
                candidate.stat().st_mtime_ns,
                candidate.parent.name == "release",
            ),
        )

    def _start_result_collector(self, logging_dir: Path | None) -> None:
        if logging_dir is None:
            return
        thread = threading.Thread(
            target=self._collect_result,
            args=(logging_dir.parent,),
            name=f"golutra-evaluation-{logging_dir.parent.name}",
            daemon=False,
        )
        thread.start()

    def _record_adapter_observation(
        self,
        logging_dir: Path | None,
        phase: str,
        status: str,
        code: str,
        facts: dict[str, object] | None = None,
    ) -> None:
        if logging_dir is None:
            return
        _write_json_atomic(
            logging_dir.parent / "golutra-adapter-observation.json",
            {
                "schema_version": 1,
                "adapter": self.name(),
                "phase": phase,
                "status": status,
                "code": code,
                "facts": facts or {},
                "observed_at": _now_rfc3339(),
            },
        )

    def _installation_failure(
        self,
        logging_dir: Path | None,
        code: str,
        facts: dict[str, object] | None = None,
    ) -> AgentResult:
        self._record_adapter_observation(
            logging_dir,
            "setup",
            "failed",
            code,
            facts,
        )
        return AgentResult(failure_mode=FailureMode.AGENT_INSTALLATION_FAILED)

    def _agent_command_timeout_sec(self, logging_dir: Path | None) -> float:
        return (
            self._agent_command_timeout_override
            or _terminal_bench_agent_timeout_sec(logging_dir)
            or _DEFAULT_AGENT_COMMAND_TIMEOUT_SEC
        )

    def _collect_result(self, trial_root: Path) -> None:
        deadline = time.monotonic() + self._result_collection_timeout_sec
        results_path: Path | None = None
        run_dir: Path | None = None
        while time.monotonic() < deadline:
            results_path = _find_trial_results(trial_root)
            run_dir = _find_run_bundle(trial_root)
            if results_path is not None and run_dir is not None:
                break
            time.sleep(0.25)
        else:
            _write_json_atomic(
                trial_root / "golutra-evaluation.pending.json",
                {
                    "status": "pending_inputs",
                    "reason": "Terminal-Bench results.json or Golutra manifest did not appear before the collection deadline",
                    "trial_root": str(trial_root),
                    "results_path": str(results_path) if results_path else None,
                    "run_bundle": str(run_dir) if run_dir else None,
                },
            )
            return

        try:
            assert results_path is not None
            assert run_dir is not None
            _remove_if_exists(trial_root / "golutra-evaluation.pending.json")
            results = json.loads(results_path.read_text())
            manifest = json.loads((run_dir / "manifest.json").read_text())
            checkpoint_only = manifest.get("terminal_outcome", {}).get("kind") == "in_progress"
            terminal_result = manifest.get("terminal_outcome", {}).get("result", {})
            task_id = terminal_result.get("task_id")
            session_id = terminal_result.get("session_id")
            if not task_id or not session_id:
                # A harness timeout can leave the in-progress checkpoint as
                # the last manifest. Its observation index still contains the
                # canonical identity needed to attach the evaluator result.
                sessions = manifest.get("observations", {}).get("sessions", [])
                if len(sessions) == 1 and isinstance(sessions[0], dict):
                    session_id = sessions[0].get("session_id")
                    tasks = sessions[0].get("tasks", [])
                    if len(tasks) == 1 and isinstance(tasks[0], dict):
                        task_id = tasks[0].get("task_id")
            if not task_id or not session_id:
                _write_json_atomic(
                    run_dir / "terminal-bench-evaluation.pending.json",
                    {
                        "status": "pending_runtime_identity",
                        "reason": "run manifest has no terminal task/session identity",
                        "results_path": str(results_path),
                    },
                )
                return
            parser_results, resolved, failure_mode = _extract_trial_result(results, task_id)
            evidence_refs = _existing_evidence_refs(trial_root)
            failure_detail = _failure_detail(trial_root)
            assertions = _evaluation_assertions(
                parser_results,
                resolved,
                failure_mode,
                evidence_refs,
                failure_detail,
            )
            passed = sum(assertion["passed"] for assertion in assertions)
            total = len(assertions)
            phases, terminal_cause = _evaluation_phases(
                results,
                task_id,
                assertions,
                resolved,
                failure_mode,
                evidence_refs,
                failure_detail,
            )
            harness_version = _terminal_bench_version()
            # Recovery may append lifecycle events after an external timeout,
            # so the checkpoint's prefix digest is no longer the final trace
            # digest. Let `eval ingest` bind the record to the reopened trace.
            trace_digest, runtime_identity = (
                (None, None)
                if checkpoint_only
                else _trace_identity(run_dir, task_id, session_id)
            )
            record = {
                "evaluation_id": f"terminal-bench:{results.get('id', trial_root.name)}:{task_id}",
                "source_task_id": task_id,
                "evaluator_id": "terminal-bench",
                "evaluator_version": harness_version,
                "harness_id": "terminal-bench",
                "harness_version": harness_version,
                "dataset_id": self._dataset_id,
                "dataset_version": self._dataset_version,
                "case_id": str(results.get("task_id", task_id)),
                "verdict": "pass" if resolved and passed == total else "fail",
                "score": float(passed),
                "score_max": float(total) if total else 1.0,
                "assertions": assertions,
                "phases": phases,
                "terminal_cause": terminal_cause,
                "artifact_refs": evidence_refs,
                "partition": "source",
                "seed": None,
                "provider_variant": self._model_name,
                "holdout_protected": False,
                "base_trace_digest": trace_digest or "auto",
                "runtime_identity": runtime_identity or "auto",
                "result_digest": "auto",
                "trust": "owner_local",
                "attestation": None,
                "ingested_at": _now_rfc3339(),
            }
            if record["base_trace_digest"] != "auto" and record["runtime_identity"] != "auto":
                record["result_digest"] = _external_result_digest(record)
            record_path = run_dir / "terminal-bench-evaluation.json"
            _write_json_atomic(record_path, record)
            if self._collector_binary is None:
                _write_json_atomic(
                    run_dir / "terminal-bench-evaluation.pending.json",
                    {
                        "status": "pending_collector",
                        "reason": "Golutra collector binary was not found; evaluation JSON is retained for later ingestion",
                        "record_path": str(record_path),
                        "record": record,
                    },
                )
                return
            completed = subprocess.run(
                _collector_command(
                    self._collector_binary,
                    run_dir,
                    session_id,
                    record_path,
                    trial_root,
                ),
                capture_output=True,
                text=True,
                timeout=120,
                check=False,
            )
            _write_json_atomic(
                run_dir / "terminal-bench-evaluation.log",
                {
                    "exit_code": completed.returncode,
                    "stdout": completed.stdout,
                    "stderr": completed.stderr,
                    "record_path": str(record_path),
                },
            )
            if completed.returncode == 0:
                _remove_if_exists(run_dir / "terminal-bench-evaluation.pending.json")
            else:
                _write_json_atomic(
                    run_dir / "terminal-bench-evaluation.pending.json",
                    {
                        "status": "ingest_failed",
                        "reason": "Golutra collector rejected the external evaluation",
                        "record_path": str(record_path),
                        "exit_code": completed.returncode,
                    },
                )
        except (OSError, ValueError, json.JSONDecodeError, subprocess.SubprocessError) as error:
            target = run_dir or trial_root
            _write_json_atomic(
                target / "terminal-bench-evaluation.pending.json",
                {"status": "collector_error", "reason": str(error)},
            )

    def perform_task(
        self,
        instruction: str,
        session: TmuxSession,
        logging_dir: Path | None = None,
    ) -> AgentResult:
        task_started_at = time.monotonic()
        agent_timeout_sec = self._agent_command_timeout_sec(logging_dir)
        architecture_result = session.container.exec_run(["uname", "-m"])
        architecture = architecture_result.output.decode(errors="replace").strip()
        binary = self._binaries.get(architecture)
        if architecture_result.exit_code != 0:
            return self._installation_failure(
                logging_dir,
                "architecture_detection_failed",
                {"exit_code": architecture_result.exit_code},
            )
        if binary is None:
            return self._installation_failure(
                logging_dir,
                "unsupported_architecture",
                {"architecture": architecture},
            )
        if not binary.is_file():
            return self._installation_failure(
                logging_dir,
                "agent_binary_missing",
                {"architecture": architecture},
            )

        try:
            with tempfile.TemporaryDirectory(prefix="golutra-tbench-auth-") as temp_dir:
                provider_file, credentials_file = self._active_auth_files(Path(temp_dir))
                session.copy_to_container(binary, container_dir="/installed-agent", container_filename="golutra")
                session.copy_to_container(
                    provider_file,
                    container_dir="/installed-agent/auth",
                    container_filename="provider.json",
                )
                session.copy_to_container(
                    credentials_file,
                    container_dir="/installed-agent/auth",
                    container_filename="credentials.json",
                )
        except (OSError, ValueError, json.JSONDecodeError) as error:
            return self._installation_failure(
                logging_dir,
                "auth_material_unavailable",
                {"error_type": type(error).__name__},
            )

        setup_command = (
            "trap 'rm -rf /installed-agent/auth' EXIT; "
            "mkdir -p /root/.golutra && "
            "cp /installed-agent/auth/provider.json /root/.golutra/provider.json && "
            "cp /installed-agent/auth/credentials.json /root/.golutra/credentials.json && "
            "chmod 755 /installed-agent/golutra && "
            "chmod 700 /root/.golutra && "
            "chmod 600 /root/.golutra/provider.json /root/.golutra/credentials.json && "
            "/installed-agent/golutra --help >/dev/null"
        )
        setup_result = session.container.exec_run(["sh", "-c", setup_command])
        if setup_result.exit_code != 0:
            return self._installation_failure(
                logging_dir,
                "agent_binary_preflight_failed",
                {"exit_code": setup_result.exit_code, "architecture": architecture},
            )
        if not self._configure_tmux_proxy(session):
            return self._installation_failure(
                logging_dir,
                "proxy_configuration_failed",
            )
        workspace_path = self._workspace_path(session)
        if workspace_path is None:
            return self._installation_failure(
                logging_dir,
                "workspace_resolution_failed",
            )

        rendered_instruction = self._render_instruction(instruction)
        environment = " ".join(
            f"{name}={shlex.quote(value)}"
            for name, value in self._runtime_environment().items()
        )
        network_flag = "--allow-network " if self._proxy_url else ""
        command = (
            f"{environment} "
            f"/installed-agent/golutra --cwd {shlex.quote(workspace_path)} exec "
            "--run-dir /logs/golutra-runtime "
            f"{network_flag}--yolo --approval-mode auto --defer-external-verification -- "
            f"{shlex.quote(rendered_instruction)}"
        )
        command_timeout_sec = max(
            1.0,
            agent_timeout_sec
            + _AGENT_COMMAND_TIMEOUT_GRACE_SEC
            - (time.monotonic() - task_started_at),
        )
        # Start collection before waiting on the agent command.  If
        # Terminal-Bench's outer timeout abandons this call, the collector can
        # still ingest the checkpoint once the harness writes results.json.
        self._start_result_collector(logging_dir)
        self._record_adapter_observation(
            logging_dir,
            "agent",
            "running",
            "runtime_command_dispatched",
            {
                "architecture": architecture,
                "agent_timeout_sec": agent_timeout_sec,
                "command_timeout_sec": command_timeout_sec,
            },
        )
        try:
            session.send_command(
                TerminalCommand(
                    command=command,
                    min_timeout_sec=0.0,
                    max_timeout_sec=command_timeout_sec,
                    block=True,
                    append_enter=True,
                )
            )
        except TimeoutError as error:
            interrupt_error = None
            try:
                session.send_keys(keys=["C-c"], min_timeout_sec=0.1)
            except Exception as interruption_error:  # noqa: BLE001
                interrupt_error = type(interruption_error).__name__
            self._record_adapter_observation(
                logging_dir,
                "agent",
                "failed",
                "runtime_command_timeout",
                {
                    "error_type": type(error).__name__,
                    "agent_timeout_sec": agent_timeout_sec,
                    "command_timeout_sec": command_timeout_sec,
                    "interrupt_error_type": interrupt_error,
                },
            )
            raise
        except Exception as error:
            self._record_adapter_observation(
                logging_dir,
                "agent",
                "failed",
                "runtime_command_failed",
                {"error_type": type(error).__name__},
            )
            raise
        input_tokens, output_tokens = (
            _trace_token_usage(logging_dir.parent) if logging_dir is not None else (0, 0)
        )
        self._record_adapter_observation(
            logging_dir,
            "agent",
            "completed",
            "runtime_command_completed",
            {
                "architecture": architecture,
                "input_tokens": input_tokens,
                "output_tokens": output_tokens,
            },
        )
        return AgentResult(
            total_input_tokens=input_tokens,
            total_output_tokens=output_tokens,
        )


def _positive_timeout(value: object) -> float:
    timeout = float(value)
    if not math.isfinite(timeout) or timeout <= 0:
        raise ValueError("timeout must be a finite positive number")
    return timeout


def _terminal_bench_agent_timeout_sec(logging_dir: Path | None) -> float | None:
    if logging_dir is None:
        return None
    trial_root = logging_dir.parent
    task_root = trial_root.parent
    run_root = task_root.parent
    try:
        run_metadata = json.loads((run_root / "run_metadata.json").read_text())
        run_lock = json.loads((run_root / "tb.lock").read_text())
        run_config = run_lock.get("run_config", run_lock)
        configured_timeout = run_config.get("global_agent_timeout_sec")
        if configured_timeout:
            return _positive_timeout(configured_timeout)

        dataset_path = Path(run_metadata["dataset_path"])
        task_config_path = dataset_path / task_root.name / "task.yaml"
        from yaml import safe_load

        task_config = safe_load(task_config_path.read_text())
        if not isinstance(task_config, dict):
            return None
        task_timeout = _positive_timeout(task_config["max_agent_timeout_sec"])
        multiplier = _positive_timeout(run_config.get("global_timeout_multiplier", 1.0))
        return task_timeout * multiplier
    except (ImportError, KeyError, OSError, TypeError, ValueError, json.JSONDecodeError):
        return None


def _find_trial_results(trial_root: Path) -> Path | None:
    candidates = [trial_root / "results.json", trial_root.parent / "results.json"]
    for candidate in candidates:
        if candidate.is_file():
            return candidate
    # Some Terminal-Bench versions place the result one level below the task
    # root. Keep the search bounded so an aggregate results file is not picked
    # up from an unrelated sibling trial.
    try:
        matches = sorted(trial_root.glob("*/results.json"))
    except OSError:
        return None
    return matches[0] if len(matches) == 1 else None


def _find_run_bundle(trial_root: Path) -> Path | None:
    candidates = [trial_root / "golutra-runtime", trial_root / "sessions" / "golutra-runtime"]
    for candidate in candidates:
        if (candidate / "manifest.json").is_file():
            return candidate
    try:
        matches = sorted(
            path.parent
            for path in trial_root.glob("**/golutra-runtime/manifest.json")
            if path.is_file()
        )
    except OSError:
        return None
    return matches[0] if len(matches) == 1 else None


def _select_trial_result(results: dict, task_id: str) -> dict:
    if isinstance(results.get("parser_results"), dict) or any(
        key in results
        for key in ("is_resolved", "failure_mode", "trial_started_at", "test_started_at")
    ):
        return results
    aggregate = results.get("results")
    if isinstance(aggregate, list):
        candidates = [
            item
            for item in aggregate
            if isinstance(item, dict)
            and str(item.get("task_id", item.get("id", ""))) == str(task_id)
        ]
        if len(candidates) == 1:
            return candidates[0]
    if isinstance(aggregate, dict):
        candidate = aggregate.get(task_id)
        if isinstance(candidate, dict):
            return candidate
    return results


def _extract_trial_result(results: dict, task_id: str) -> tuple[dict, bool, str | None]:
    selected = _select_trial_result(results, task_id)
    if isinstance(selected.get("parser_results"), dict):
        failure_mode = selected.get("failure_mode")
        return (
            selected["parser_results"],
            bool(selected.get("is_resolved", False)),
            str(failure_mode).lower() if failure_mode is not None else None,
        )
    failure_mode = selected.get("failure_mode")
    return (
        {},
        bool(selected.get("is_resolved", selected.get("resolved", False))),
        str(failure_mode).lower() if failure_mode is not None else None,
    )


def _evaluation_assertions(
    parser_results: dict,
    resolved: bool,
    failure_mode: str | None,
    evidence_refs: list[str],
    failure_detail: str | None,
) -> list[dict]:
    assertions = []
    for name, status in sorted(parser_results.items()):
        passed = str(status).lower() in {"pass", "passed"}
        status_message = _sanitize_failure_line(str(status)) or str(status)
        assertions.append(
            {
                "assertion_id": f"terminal-bench:{name}",
                "name": name,
                "passed": passed,
                "message": (
                    status_message
                    if passed
                    else _failure_message(status_message, failure_detail)
                ),
                "evidence_refs": evidence_refs,
            }
        )
    if failure_mode not in {None, "none", "unset"}:
        assertions.append(
            {
                "assertion_id": "terminal-bench:harness_failure_mode",
                "name": "harness_failure_mode",
                "passed": False,
                "message": _failure_message(str(failure_mode), failure_detail),
                "evidence_refs": evidence_refs,
            }
        )
    if not assertions or (not resolved and all(item["passed"] for item in assertions)):
        assertions.append(
            {
                "assertion_id": "terminal-bench:resolved",
                "name": "resolved",
                "passed": resolved,
                "message": (
                    str(resolved)
                    if resolved
                    else _failure_message(str(resolved), failure_detail)
                ),
                "evidence_refs": evidence_refs,
            }
        )
    return assertions


def _failure_detail(trial_root: Path) -> str | None:
    log_path = trial_root / "panes" / "post-test.txt"
    try:
        with log_path.open("rb") as stream:
            stream.seek(0, os.SEEK_END)
            size = stream.tell()
            start = max(0, size - _FAILURE_LOG_SCAN_BYTES)
            stream.seek(start)
            raw = stream.read(_FAILURE_LOG_SCAN_BYTES)
    except OSError:
        return None

    if start > 0 and b"\n" in raw:
        raw = raw.split(b"\n", 1)[1]
    lines_by_priority: list[list[str]] = [[], [], []]
    fallback_lines: list[str] = []
    seen: set[str] = set()
    for raw_line in raw.decode("utf-8", errors="replace").splitlines():
        line = _sanitize_failure_line(raw_line)
        if not line:
            continue
        key = line.casefold()
        if key in seen:
            continue
        seen.add(key)
        fallback_lines.append(line)
        if _ROOT_CAUSE_LINE_RE.search(line):
            lines_by_priority[0].append(line)
        elif _PYTEST_DETAIL_LINE_RE.search(line):
            lines_by_priority[1].append(line)
        elif _FAILURE_SUMMARY_LINE_RE.search(line):
            lines_by_priority[2].append(line)

    preferred = [line for group in lines_by_priority for line in group]
    candidates = preferred or fallback_lines[-8:]
    detail = _join_bounded_lines(candidates, _FAILURE_DETAIL_MAX_BYTES)
    return detail or None


def _sanitize_failure_line(value: str) -> str:
    sanitized = _ANSI_ESCAPE_RE.sub("", value)
    sanitized = "".join(
        character
        if character == "\t" or (ord(character) >= 32 and ord(character) != 127)
        else " "
        for character in sanitized
    )
    sanitized = _BEARER_CREDENTIAL_RE.sub("Bearer [REDACTED]", sanitized)
    sanitized = _URL_CREDENTIAL_RE.sub(r"\1[REDACTED]@", sanitized)
    sanitized = _SECRET_ASSIGNMENT_RE.sub(r"\1\2[REDACTED]", sanitized)
    sanitized = re.sub(r"\s+", " ", sanitized).strip()
    return _truncate_utf8(sanitized, _FAILURE_LINE_MAX_BYTES)


def _failure_message(summary: str, detail: str | None) -> str:
    summary = _sanitize_failure_line(summary)
    if not detail:
        return _truncate_utf8(summary, _FAILURE_DETAIL_MAX_BYTES)
    return _truncate_utf8(
        f"{summary}\nEvaluator output:\n{detail}",
        _FAILURE_DETAIL_MAX_BYTES,
    )


def _join_bounded_lines(lines: list[str], limit: int) -> str:
    selected: list[str] = []
    used = 0
    for line in lines:
        separator_bytes = 1 if selected else 0
        remaining = limit - used - separator_bytes
        if remaining <= 0:
            break
        bounded = _truncate_utf8(line, remaining)
        if not bounded:
            continue
        selected.append(bounded)
        used += separator_bytes + len(bounded.encode("utf-8"))
        if len(bounded.encode("utf-8")) < len(line.encode("utf-8")):
            break
    return "\n".join(selected)


def _truncate_utf8(value: str, limit: int) -> str:
    encoded = value.encode("utf-8")
    if len(encoded) <= limit:
        return value
    if limit <= 3:
        return encoded[:limit].decode("utf-8", errors="ignore")
    prefix = encoded[: limit - 3].decode("utf-8", errors="ignore").rstrip()
    return f"{prefix}..."


def _parse_timestamp(value: object) -> datetime | None:
    if not isinstance(value, str) or not value.strip():
        return None
    try:
        parsed = datetime.fromisoformat(value.replace("Z", "+00:00"))
    except ValueError:
        return None
    if parsed.tzinfo is None:
        parsed = parsed.replace(tzinfo=timezone.utc)
    return parsed.astimezone(timezone.utc)


def _normalized_timestamp(value: object) -> str | None:
    parsed = _parse_timestamp(value)
    if parsed is None:
        return None
    return parsed.isoformat().replace("+00:00", "Z")


def _phase_duration_ms(started_at: object, completed_at: object) -> int | None:
    started = _parse_timestamp(started_at)
    completed = _parse_timestamp(completed_at)
    if started is None or completed is None or completed < started:
        return None
    elapsed = completed - started
    return (
        elapsed.days * 86_400_000
        + elapsed.seconds * 1_000
        + elapsed.microseconds // 1_000
    )


def _phase_record(
    phase_id: str,
    kind: str,
    status: str,
    started_at: object,
    completed_at: object,
    evidence_refs: list[str],
    assertion_refs: list[str] | None = None,
) -> dict:
    return {
        "phase_id": phase_id,
        "kind": kind,
        "status": status,
        "started_at": _normalized_timestamp(started_at),
        "completed_at": _normalized_timestamp(completed_at),
        "duration_ms": _phase_duration_ms(started_at, completed_at),
        "assertion_refs": assertion_refs or [],
        "evidence_refs": evidence_refs,
    }


def _evaluation_phases(
    results: dict,
    task_id: str,
    assertions: list[dict],
    resolved: bool,
    failure_mode: str | None,
    evidence_refs: list[str],
    failure_detail: str | None = None,
) -> tuple[list[dict], dict | None]:
    selected = _select_trial_result(results, task_id)
    normalized_failure = (
        failure_mode if failure_mode not in {None, "none", "unset"} else None
    )
    failed_assertions = [item for item in assertions if not item["passed"]]
    failure_phase = None
    if normalized_failure:
        if "install" in normalized_failure or "setup" in normalized_failure:
            failure_phase = "terminal-bench:setup"
        elif "agent" in normalized_failure:
            failure_phase = "terminal-bench:agent"
        elif "test" in normalized_failure or "timeout" in normalized_failure:
            failure_phase = "terminal-bench:test"
        else:
            failure_phase = "terminal-bench:assertions"
    elif failed_assertions or not resolved:
        failure_phase = "terminal-bench:assertions"

    def status_for(phase_id: str, completed_key: str) -> str:
        if failure_phase == phase_id:
            if normalized_failure and "timeout" in normalized_failure:
                return "timed_out"
            return "error" if normalized_failure else "failed"
        if phase_id == "terminal-bench:test":
            if failed_assertions or not resolved:
                return "failed"
            return "passed" if selected.get(completed_key) else "unknown"
        if phase_id == "terminal-bench:assertions":
            if failed_assertions or not resolved:
                return "failed"
            return "passed" if assertions else "unknown"
        return "passed" if selected.get(completed_key) else "unknown"

    phases = [
        _phase_record(
            "terminal-bench:setup",
            "setup",
            status_for("terminal-bench:setup", "agent_started_at"),
            selected.get("trial_started_at"),
            selected.get("agent_started_at"),
            evidence_refs,
        ),
        _phase_record(
            "terminal-bench:agent",
            "agent",
            status_for("terminal-bench:agent", "agent_ended_at"),
            selected.get("agent_started_at"),
            selected.get("agent_ended_at"),
            evidence_refs,
        ),
        _phase_record(
            "terminal-bench:test",
            "test",
            status_for("terminal-bench:test", "test_ended_at"),
            selected.get("test_started_at"),
            selected.get("test_ended_at"),
            evidence_refs,
        ),
        _phase_record(
            "terminal-bench:assertions",
            "assertion",
            status_for("terminal-bench:assertions", "trial_ended_at"),
            selected.get("test_ended_at"),
            selected.get("trial_ended_at"),
            evidence_refs,
            [item["assertion_id"] for item in assertions],
        ),
    ]
    if normalized_failure:
        terminal_cause = {
            "code": normalized_failure,
            "phase_id": failure_phase,
            "message": _failure_message(
                f"Terminal-Bench stopped with {normalized_failure}",
                failure_detail,
            ),
            "retryable": "timeout" in normalized_failure,
            "evidence_refs": evidence_refs,
        }
    elif failed_assertions or not resolved:
        names = ", ".join(item["name"] for item in failed_assertions) or "resolved"
        terminal_cause = {
            "code": "assertion_failed",
            "phase_id": failure_phase,
            "message": _failure_message(
                f"failed evaluator assertions: {names}",
                failure_detail,
            ),
            "retryable": False,
            "evidence_refs": evidence_refs,
        }
    else:
        terminal_cause = None
    return phases, terminal_cause


def _existing_evidence_refs(trial_root: Path) -> list[str]:
    candidates = [
        "results.json",
        "golutra-adapter-observation.json",
        "panes/post-agent.txt",
        "panes/post-test.txt",
        "commands.txt",
    ]
    return [path for path in candidates if (trial_root / path).is_file()]


def _collector_command(
    collector_binary: Path,
    run_dir: Path,
    session_id: str,
    record_path: Path,
    artifact_base: Path,
) -> list[str]:
    return [
        str(collector_binary),
        "--run-bundle",
        str(run_dir),
        "--session-id",
        session_id,
        "eval",
        "ingest",
        "--artifact-base",
        str(artifact_base),
        str(record_path),
    ]


def _trace_token_usage(trial_root: Path) -> tuple[int, int]:
    run_dir = _find_run_bundle(trial_root)
    if run_dir is None:
        return 0, 0
    try:
        manifest = json.loads((run_dir / "manifest.json").read_text())
    except (OSError, ValueError, json.JSONDecodeError):
        return 0, 0
    trace_paths = [
        run_dir / str(task["trace_path"])
        for session in manifest.get("observations", {}).get("sessions", [])
        for task in session.get("tasks", [])
        if task.get("trace_path")
    ]
    if len(trace_paths) != 1:
        return 0, 0
    try:
        trace = json.loads(trace_paths[0].read_text())
    except (OSError, ValueError, json.JSONDecodeError):
        return 0, 0
    input_tokens = 0
    output_tokens = 0
    for event in trace.get("events", []):
        if event.get("event_type") != "token_usage_recorded":
            continue
        record = event.get("payload", {}).get("record", {})
        recorded_input = record.get("input_tokens")
        recorded_output = record.get("output_tokens")
        if isinstance(recorded_input, int) and recorded_input >= 0:
            input_tokens += recorded_input
        if isinstance(recorded_output, int) and recorded_output >= 0:
            output_tokens += recorded_output
    return input_tokens, output_tokens


def _trace_identity(run_dir: Path, task_id: str, session_id: str) -> tuple[str | None, str | None]:
    manifest_path = run_dir / "manifest.json"
    try:
        manifest = json.loads(manifest_path.read_text())
    except (OSError, ValueError, json.JSONDecodeError):
        return None, None
    trace_paths: list[Path] = []
    for session in manifest.get("observations", {}).get("sessions", []):
        if str(session.get("session_id")) != str(session_id):
            continue
        for task in session.get("tasks", []):
            if str(task.get("task_id")) == str(task_id) and task.get("trace_path"):
                trace_paths.append(run_dir / str(task["trace_path"]))
    if not trace_paths:
        try:
            trace_paths = list(run_dir.glob("observations/**/trace.json"))
        except OSError:
            return None, None
    for path in trace_paths:
        try:
            trace = json.loads(path.read_text())
        except (OSError, ValueError, json.JSONDecodeError):
            continue
        integrity = trace.get("integrity") or {}
        digest = integrity.get("event_chain_digest")
        identity = trace.get("runtime_identity")
        if isinstance(digest, str) and digest.startswith("sha256:") and isinstance(identity, str):
            return digest, identity
    return None, None


def _external_result_digest(record: dict) -> str:
    facts = {
        key: record.get(key)
        for key in (
            "evaluation_id",
            "source_task_id",
            "evaluator_id",
            "evaluator_version",
            "harness_id",
            "harness_version",
            "dataset_id",
            "dataset_version",
            "case_id",
            "verdict",
            "score",
            "score_max",
            "assertions",
            "phases",
            "terminal_cause",
            "artifact_refs",
            "partition",
            "seed",
            "provider_variant",
            "holdout_protected",
            "comparison_group_id",
            "candidate_id",
            "campaign_id",
            "role",
            "base_trace_digest",
            "runtime_identity",
            "trust",
        )
    }
    encoded = json.dumps(
        _canonical_json(facts),
        ensure_ascii=False,
        separators=(",", ":"),
    ).encode("utf-8")
    return f"sha256:{hashlib.sha256(encoded).hexdigest()}"


def _canonical_json(value):
    if isinstance(value, dict):
        return {key: _canonical_json(value[key]) for key in sorted(value)}
    if isinstance(value, list):
        return [_canonical_json(item) for item in value]
    return value


def _now_rfc3339() -> str:
    return datetime.now(timezone.utc).isoformat().replace("+00:00", "Z")


def _write_json_atomic(path: Path, value: object) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    fd, temporary_name = tempfile.mkstemp(prefix=f".{path.name}-", dir=path.parent)
    temporary = Path(temporary_name)
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as stream:
            json.dump(value, stream, indent=2, ensure_ascii=False)
            stream.write("\n")
            stream.flush()
            os.fsync(stream.fileno())
        temporary.chmod(0o600)
        os.replace(temporary, path)
    finally:
        _remove_if_exists(temporary)


def _remove_if_exists(path: Path) -> None:
    try:
        path.unlink()
    except FileNotFoundError:
        return
    except IsADirectoryError:
        shutil.rmtree(path, ignore_errors=True)


def _terminal_bench_version() -> str:
    try:
        return metadata.version("terminal-bench")
    except metadata.PackageNotFoundError:
        return "unknown"
