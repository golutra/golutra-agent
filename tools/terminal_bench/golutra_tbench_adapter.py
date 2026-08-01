"""Terminal-Bench adapter that retains one governed Golutra run per trial."""

from __future__ import annotations

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
_DEFAULT_GRACEFUL_DRAIN_TIMEOUT_SEC = 20.0
_COLLECTOR_RETRY_LIMIT = 8
_COLLECTOR_RETRY_DELAY_SEC = 0.5
_FAILURE_LOG_SCAN_BYTES = 1024 * 1024
_FAILURE_DETAIL_MAX_BYTES = 2048
_FAILURE_LINE_MAX_BYTES = 512
_EXTERNAL_CORRECTION_SCHEMA_VERSION = 2
_MAX_NO_PROXY_ENTRIES = 128
_MAX_NO_PROXY_BYTES = 4096

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
_COMPOSE_HOST_LABEL_RE = re.compile(r"^[A-Za-z0-9](?:[A-Za-z0-9_-]{0,61}[A-Za-z0-9])?$")


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
        graceful_drain_timeout_sec: float = _DEFAULT_GRACEFUL_DRAIN_TIMEOUT_SEC,
        max_external_correction_rounds: int = 1,
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
        self._graceful_drain_timeout_sec = _positive_timeout(graceful_drain_timeout_sec)
        if max_external_correction_rounds < 0 or max_external_correction_rounds > 2:
            raise ValueError("max_external_correction_rounds must be between 0 and 2")
        self._max_external_correction_rounds = max_external_correction_rounds
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

    def _runtime_environment(self, logging_dir: Path | None = None) -> dict[str, str]:
        environment = {"HOME": "/root", "GOLUTRA_HOME": "/root/.golutra"}
        if self._proxy_url:
            no_proxy = _merge_no_proxy(
                self._no_proxy,
                _terminal_bench_service_names(logging_dir),
            )
            environment.update(
                {name: self._proxy_url for name in self._PROXY_ENV_NAMES}
            )
            environment.update({"NO_PROXY": no_proxy, "no_proxy": no_proxy})
        return environment

    def _configure_tmux_proxy(
        self,
        session: TmuxSession,
        logging_dir: Path | None = None,
    ) -> bool:
        if not self._proxy_url:
            return True
        no_proxy = _merge_no_proxy(
            self._no_proxy,
            _terminal_bench_service_names(logging_dir),
        )
        proxy_environment = {
            name: self._proxy_url for name in self._PROXY_ENV_NAMES
        }
        proxy_environment.update({"NO_PROXY": no_proxy, "no_proxy": no_proxy})
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
        observation_path = logging_dir.parent / "golutra-adapter-observation.json"
        existing_facts: dict[str, object] = {}
        try:
            existing = json.loads(observation_path.read_text())
        except (OSError, json.JSONDecodeError):
            existing = {}
        if isinstance(existing, dict) and isinstance(existing.get("facts"), dict):
            existing_facts.update(existing["facts"])
        existing_facts.update(facts or {})
        _write_json_atomic(
            observation_path,
            {
                "schema_version": 1,
                "adapter": self.name(),
                "phase": phase,
                "status": status,
                "code": code,
                "facts": existing_facts,
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

    def _runtime_readiness(self, trial_root: Path) -> dict[str, object]:
        """Read lifecycle state only from the atomically replaced run manifest."""
        run_dir = _find_run_bundle(trial_root)
        if run_dir is None:
            return {"state": "unavailable"}
        manifest_path = run_dir / "manifest.json"
        try:
            manifest = json.loads(manifest_path.read_text())
        except (OSError, json.JSONDecodeError):
            manifest = {}
        terminal = manifest.get("terminal_outcome")
        if not isinstance(terminal, dict):
            return {"state": "unavailable"}
        kind = terminal.get("kind")
        if kind == "in_progress":
            return {"state": "running", "terminal_outcome_kind": kind}
        if kind in {"result", "error"}:
            return {"state": "terminal", "terminal_outcome_kind": kind}
        return {"state": "unavailable"}

    def _graceful_drain(self, logging_dir: Path | None) -> dict[str, object]:
        if logging_dir is None:
            return {"state": "unavailable", "elapsed_sec": 0.0}
        started = time.monotonic()
        trial_root = logging_dir.parent
        last = {"state": "unavailable"}
        deadline = started + self._graceful_drain_timeout_sec
        while time.monotonic() < deadline:
            last = self._runtime_readiness(trial_root)
            if last.get("state") in {"terminal", "external_pending"}:
                break
            time.sleep(0.25)
        last = dict(last)
        last["elapsed_sec"] = round(time.monotonic() - started, 3)
        last["deadline_sec"] = self._graceful_drain_timeout_sec
        return last

    def _external_correction_plan(
        self, run_dir: Path, record: dict, thread_id: str | None
    ) -> dict[str, object] | None:
        """Retain evaluator feedback for an isolated, unscored continuation."""
        if getattr(self, "_max_external_correction_rounds", 1) == 0 or not thread_id:
            return None
        if record.get("verdict") != "fail":
            return None
        cause = record.get("terminal_cause") or {}
        if cause.get("code") != "assertion_failed":
            return None
        marker = run_dir / "external-correction-1.json"
        if marker.is_file():
            try:
                existing = json.loads(marker.read_text())
            except (OSError, json.JSONDecodeError):
                existing = {}
            if not isinstance(existing, dict):
                existing = {}
            source = existing.get("source_evaluation", {})
            if (
                existing.get("schema_version") == _EXTERNAL_CORRECTION_SCHEMA_VERSION
                and existing.get("status") == "isolated_continuation_required"
                and existing.get("thread_id") == thread_id
                and isinstance(source, dict)
                and source.get("evaluation_id") == record.get("evaluation_id")
                and source.get("result_digest") == record.get("result_digest")
            ):
                return existing
        failure_groups: dict[str, list[str]] = {}
        for item in record.get("assertions", []):
            if item.get("passed"):
                continue
            name = str(item.get("name") or "unnamed assertion")
            message = str(item.get("message") or "failed")
            failure_groups.setdefault(message, []).append(name)
        failed = [
            f"{', '.join(names)}: {message}"
            for message, names in failure_groups.items()
        ]
        feedback = _truncate_utf8(
            "External evaluator feedback. Correct the workspace and rerun the evaluator.\n"
            + "\n".join(f"- {item}" for item in failed),
            _FAILURE_DETAIL_MAX_BYTES,
        )
        result = {
            "schema_version": _EXTERNAL_CORRECTION_SCHEMA_VERSION,
            "status": "isolated_continuation_required",
            "round": 1,
            "thread_id": thread_id,
            "feedback": feedback,
            "source_evaluation": {
                "evaluation_id": record.get("evaluation_id"),
                "result_digest": record.get("result_digest"),
                "run_bundle": ".",
            },
            "isolation": {
                "mode": "unscored_diagnostic",
                "source_trial_immutable": True,
                "requires_cloned_workspace": True,
                "requires_cloned_run_bundle": True,
                "may_replace_source_score": False,
                "promotion_requires_independent_evaluation": True,
            },
            "reason": "the scored Terminal-Bench trial is immutable; consume this feedback only after rehydrating a cloned workspace and run bundle as a separate unscored diagnostic continuation",
        }
        _write_json_atomic(marker, result)
        return result

    def _collect_result(self, trial_root: Path) -> None:
        deadline = time.monotonic() + self._result_collection_timeout_sec
        results_path: Path | None = None
        run_dir: Path | None = None
        record_path: Path | None = None
        while time.monotonic() < deadline:
            results_path = _find_trial_results(trial_root)
            run_dir = _find_run_bundle(trial_root)
            if results_path is not None and run_dir is not None:
                break
            time.sleep(min(0.25, _remaining_deadline_seconds(deadline)))
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
            manifest = self._wait_for_terminal_manifest(run_dir, deadline)
            checkpoint_only = manifest.get("terminal_outcome", {}).get("kind") == "in_progress"
            terminal_result = manifest.get("terminal_outcome", {}).get("result", {})
            task_id = terminal_result.get("task_id")
            session_id = terminal_result.get("session_id")
            thread_id = terminal_result.get("thread_id")
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
                        thread_id = tasks[0].get("thread_id")
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
            record_path = run_dir / "terminal-bench-evaluation.json"
            _write_json_atomic(record_path, record)
            _remove_if_exists(run_dir / "terminal-bench-evaluation-correction.json")
            if checkpoint_only:
                _write_json_atomic(
                    run_dir / "terminal-bench-evaluation.pending.json",
                    {
                        "status": "pending_runtime_terminal",
                        "reason": "the evaluator result is retained, but the Golutra run manifest is still in progress; ingestion must wait for a terminal bundle",
                        "record_path": str(record_path),
                    },
                )
                return
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
            collector_command = _collector_command(
                self._collector_binary,
                run_dir,
                session_id,
                record_path,
                trial_root,
            )
            completed = None
            for attempt in range(_COLLECTOR_RETRY_LIMIT):
                collector_timeout = _remaining_deadline_seconds(deadline)
                if collector_timeout <= 0:
                    _write_collector_timeout_evidence(
                        run_dir,
                        record_path,
                        "CollectionDeadlineExceeded",
                        0.0,
                        "the result collection deadline expired before the collector could finish",
                    )
                    return
                try:
                    completed = subprocess.run(
                        collector_command,
                        capture_output=True,
                        text=True,
                        timeout=collector_timeout,
                        check=False,
                    )
                except subprocess.TimeoutExpired as error:
                    _write_collector_timeout_evidence(
                        run_dir,
                        record_path,
                        type(error).__name__,
                        collector_timeout,
                        str(error),
                    )
                    return
                if completed.returncode == 0 or not _collector_failure_is_transient(
                    completed.stderr
                ):
                    break
                if attempt + 1 < _COLLECTOR_RETRY_LIMIT:
                    retry_delay = min(
                        _COLLECTOR_RETRY_DELAY_SEC,
                        _remaining_deadline_seconds(deadline),
                    )
                    if retry_delay <= 0:
                        _write_collector_timeout_evidence(
                            run_dir,
                            record_path,
                            "CollectionDeadlineExceeded",
                            0.0,
                            "the result collection deadline expired before a collector retry",
                        )
                        return
                    time.sleep(retry_delay)
            assert completed is not None
            bound_record = (
                _trace_external_evaluation(
                    run_dir,
                    task_id,
                    session_id,
                    record["evaluation_id"],
                )
                if completed.returncode == 0
                else None
            )
            _write_json_atomic(
                run_dir / "terminal-bench-evaluation.log",
                {
                    "exit_code": completed.returncode,
                    "stdout": completed.stdout,
                    "stderr": completed.stderr,
                    "record_path": str(record_path),
                    "bound_result_digest": (
                        bound_record.get("result_digest") if bound_record else None
                    ),
                },
            )
            if completed.returncode == 0 and bound_record is not None:
                correction = self._external_correction_plan(
                    run_dir, bound_record, thread_id
                )
                _remove_if_exists(run_dir / "terminal-bench-evaluation.pending.json")
                if correction is not None:
                    _write_json_atomic(
                        run_dir / "terminal-bench-evaluation-correction.json",
                        correction,
                    )
            elif completed.returncode == 0:
                _write_json_atomic(
                    run_dir / "terminal-bench-evaluation.pending.json",
                    {
                        "status": "pending_trace_binding",
                        "reason": "Golutra collector returned success, but the exported trace does not contain the bound external evaluation",
                        "record_path": str(record_path),
                        "evaluation_id": record["evaluation_id"],
                    },
                )
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
            pending = {"status": "collector_error", "reason": str(error)}
            if record_path is not None:
                pending["record_path"] = str(record_path)
            _write_json_atomic(
                target / "terminal-bench-evaluation.pending.json",
                pending,
            )

    def _wait_for_terminal_manifest(
        self, run_dir: Path, collection_deadline: float
    ) -> dict:
        """Do not ingest while the CLI still owns an in-progress run bundle."""
        remaining_collection_budget = _remaining_deadline_seconds(collection_deadline)
        if remaining_collection_budget <= 0:
            return json.loads((run_dir / "manifest.json").read_text())
        wait_budget = min(
            max(
                0.01,
                float(
                    getattr(
                        self,
                        "_graceful_drain_timeout_sec",
                        _DEFAULT_GRACEFUL_DRAIN_TIMEOUT_SEC,
                    )
                ),
            ),
            remaining_collection_budget,
        )
        deadline = time.monotonic() + wait_budget
        manifest_path = run_dir / "manifest.json"
        last_manifest: dict = {}
        while time.monotonic() < deadline:
            try:
                candidate = json.loads(manifest_path.read_text())
            except (OSError, json.JSONDecodeError):
                candidate = None
            if isinstance(candidate, dict):
                last_manifest = candidate
                if candidate.get("terminal_outcome", {}).get("kind") != "in_progress":
                    return candidate
            time.sleep(min(0.25, _remaining_deadline_seconds(deadline)))
        if last_manifest:
            return last_manifest
        return json.loads(manifest_path.read_text())

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
                {
                    "exit_code": setup_result.exit_code,
                    "architecture": architecture,
                    "loader_output": _bounded_diagnostic_output(setup_result.output),
                },
            )
        if not self._configure_tmux_proxy(session, logging_dir):
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
            for name, value in self._runtime_environment(logging_dir).items()
        )
        network_flag = "--allow-network " if self._proxy_url else ""
        setup_elapsed_sec = max(0.0, time.monotonic() - task_started_at)
        finalization_reserve_sec = (
            self._graceful_drain_timeout_sec + _AGENT_COMMAND_TIMEOUT_GRACE_SEC
        )
        runtime_elapsed_budget_ms = _runtime_elapsed_budget_ms(
            agent_timeout_sec=agent_timeout_sec,
            setup_elapsed_sec=setup_elapsed_sec,
            finalization_reserve_sec=finalization_reserve_sec,
        )
        command = (
            f"{environment} "
            f"/installed-agent/golutra --cwd {shlex.quote(workspace_path)} exec "
            "--run-dir /logs/golutra-runtime "
            f"{network_flag}--yolo --approval-mode auto "
            f"--max-elapsed-ms {runtime_elapsed_budget_ms} "
            "--defer-external-verification -- "
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
                "runtime_max_elapsed_ms": runtime_elapsed_budget_ms,
                "finalization_reserve_sec": finalization_reserve_sec,
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
            self._record_adapter_observation(
                logging_dir,
                "agent",
                "draining",
                "graceful_drain_started",
                {"timeout_class": "agent_timeout"},
            )
            drain = self._graceful_drain(logging_dir)
            interrupt_error = None
            try:
                session.send_keys(keys=["C-c"], min_timeout_sec=0.1)
            except Exception as interruption_error:  # noqa: BLE001
                interrupt_error = type(interruption_error).__name__
            self._record_adapter_observation(
                logging_dir,
                "agent",
                "failed",
                "agent_timeout",
                {
                    "error_type": type(error).__name__,
                    "timeout_class": "agent_timeout",
                    "agent_timeout_sec": agent_timeout_sec,
                    "command_timeout_sec": command_timeout_sec,
                    "graceful_drain": drain,
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


def _terminal_bench_service_names(logging_dir: Path | None) -> list[str]:
    if logging_dir is None:
        return []

    trial_root = logging_dir.parent
    task_root = trial_root.parent
    run_root = task_root.parent
    try:
        run_metadata = json.loads((run_root / "run_metadata.json").read_text())
        dataset_path = Path(run_metadata["dataset_path"])
        if not dataset_path.is_absolute():
            dataset_path = run_root / dataset_path
        compose_path = dataset_path / task_root.name / "docker-compose.yaml"
        if not compose_path.is_file():
            compose_path = compose_path.with_suffix(".yml")

        from yaml import safe_load

        compose = safe_load(compose_path.read_text())
        services = compose.get("services") if isinstance(compose, dict) else None
        if not isinstance(services, dict):
            return []
    except (ImportError, KeyError, OSError, TypeError, ValueError, json.JSONDecodeError):
        return []

    candidates: list[object] = []
    for service_name, service in services.items():
        candidates.append(service_name)
        if not isinstance(service, dict):
            continue
        candidates.extend((service.get("hostname"), service.get("container_name")))
        networks = service.get("networks")
        if not isinstance(networks, dict):
            continue
        for network in networks.values():
            if not isinstance(network, dict):
                continue
            aliases = network.get("aliases")
            if isinstance(aliases, list):
                candidates.extend(aliases)

    names: list[str] = []
    seen: set[str] = set()
    for candidate in candidates:
        if not isinstance(candidate, str):
            continue
        candidate = candidate.strip()
        key = candidate.casefold()
        if not _is_compose_hostname(candidate) or key in seen:
            continue
        seen.add(key)
        names.append(candidate)
    return names


def _is_compose_hostname(value: str) -> bool:
    if not value or len(value) > 253:
        return False
    labels = value.split(".")
    return all(_COMPOSE_HOST_LABEL_RE.fullmatch(label) for label in labels)


def _merge_no_proxy(configured: str, discovered: list[str]) -> str:
    entries: list[str] = []
    seen: set[str] = set()
    byte_count = 0
    for candidate in [*configured.split(","), *discovered]:
        entry = candidate.strip()
        key = entry.casefold()
        if not entry or key in seen or "\x00" in entry or "\n" in entry or "\r" in entry:
            continue
        separator_bytes = 1 if entries else 0
        entry_bytes = len(entry.encode("utf-8"))
        if len(entries) >= _MAX_NO_PROXY_ENTRIES:
            break
        if byte_count + separator_bytes + entry_bytes > _MAX_NO_PROXY_BYTES:
            continue
        entries.append(entry)
        seen.add(key)
        byte_count += separator_bytes + entry_bytes
    return ",".join(entries)


def _positive_timeout(value: object) -> float:
    timeout = float(value)
    if not math.isfinite(timeout) or timeout <= 0:
        raise ValueError("timeout must be a finite positive number")
    return timeout


def _remaining_deadline_seconds(deadline: float) -> float:
    return max(0.0, deadline - time.monotonic())


def _runtime_elapsed_budget_ms(
    *,
    agent_timeout_sec: float,
    setup_elapsed_sec: float,
    finalization_reserve_sec: float,
) -> int:
    available_sec = agent_timeout_sec - setup_elapsed_sec - finalization_reserve_sec
    return max(1_000, math.floor(available_sec * 1_000))


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


def _bounded_diagnostic_output(value: object) -> str:
    if isinstance(value, bytes):
        rendered = value.decode(errors="replace")
    else:
        rendered = str(value or "")
    lines: list[str] = []
    for raw_line in rendered.splitlines():
        line = _sanitize_failure_line(raw_line)
        if line and line not in lines:
            lines.append(line)
    return _join_bounded_lines(lines, _FAILURE_DETAIL_MAX_BYTES)


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
    normalized_failure = _normalized_failure_mode(failure_mode)
    failed_assertions = [item for item in assertions if not item["passed"]]
    failure_phase = None
    if normalized_failure:
        if normalized_failure in {
            "agent_installation_failed",
            "setup_failure",
            "docker_build_failure",
            "environment_timeout",
        } or "install" in normalized_failure or "setup" in normalized_failure:
            failure_phase = "terminal-bench:setup"
        elif normalized_failure in {"agent_timeout", "provider_timeout", "parse_error"} or "agent" in normalized_failure:
            failure_phase = "terminal-bench:agent"
        elif normalized_failure in {"test_timeout", "external_verifier_failure"} or "test" in normalized_failure:
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
            "retryable": normalized_failure in {
                "agent_timeout",
                "provider_timeout",
                "test_timeout",
                "environment_timeout",
            },
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


def _normalized_failure_mode(value: object) -> str | None:
    if value is None:
        return None
    normalized = str(value).strip().lower()
    if normalized in {"", "none", "unset"}:
        return None
    aliases = {
        "runtime_command_timeout": "agent_timeout",
        "agent_timeout": "agent_timeout",
        "provider_timeout": "provider_timeout",
        "test_timeout": "test_timeout",
        "setup_timeout": "environment_timeout",
        "docker_timeout": "environment_timeout",
        "docker_build": "docker_build_failure",
        "parse": "parse_error",
    }
    return aliases.get(normalized, normalized)


def _collector_failure_is_transient(stderr: str) -> bool:
    normalized = stderr.lower()
    return any(
        marker in normalized
        for marker in (
            "database disk image is malformed",
            "database is locked",
            "database table is locked",
            "database is busy",
        )
    )


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


def _trace_external_evaluation(
    run_dir: Path,
    task_id: str,
    session_id: str,
    evaluation_id: str,
) -> dict | None:
    try:
        manifest = json.loads((run_dir / "manifest.json").read_text())
    except (OSError, ValueError, json.JSONDecodeError):
        return None
    trace_paths: list[Path] = []
    for session in manifest.get("observations", {}).get("sessions", []):
        if str(session.get("session_id")) != str(session_id):
            continue
        for task in session.get("tasks", []):
            if str(task.get("task_id")) == str(task_id) and task.get("trace_path"):
                trace_paths.append(run_dir / str(task["trace_path"]))
    for path in trace_paths:
        try:
            trace = json.loads(path.read_text())
        except (OSError, ValueError, json.JSONDecodeError):
            continue
        records = trace.get("evaluation", {}).get("external_evaluations", [])
        for record in records:
            if isinstance(record, dict) and record.get("evaluation_id") == evaluation_id:
                return record
    return None


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


def _write_collector_timeout_evidence(
    run_dir: Path,
    record_path: Path,
    error_type: str,
    timeout_sec: float,
    detail: str,
) -> None:
    _write_json_atomic(
        run_dir / "terminal-bench-evaluation.pending.json",
        {
            "status": "collector_timeout",
            "reason": "Golutra collector did not finish before the result collection deadline",
            "record_path": str(record_path),
            "error_type": error_type,
            "timeout_sec": timeout_sec,
            "detail": detail,
        },
    )


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
