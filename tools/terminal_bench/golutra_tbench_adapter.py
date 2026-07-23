"""Terminal-Bench adapter that retains one governed Golutra run per trial."""

from __future__ import annotations

import json
import os
import shlex
import tempfile
from pathlib import Path

from terminal_bench.agents.base_agent import AgentResult, BaseAgent
from terminal_bench.agents.failure_mode import FailureMode
from terminal_bench.terminal.models import TerminalCommand
from terminal_bench.terminal.tmux_session import TmuxSession


class GolutraAgent(BaseAgent):
    """Run a locally built Golutra CLI and retain governed runtime data."""

    def __init__(
        self,
        model_name: str = "openai-compatible/gpt-5.5",
        arm64_binary: str = "/tmp/golutra-linux-bin/golutra-cli",
        amd64_binary: str = "/tmp/golutra-linux-bin-amd64/golutra-cli",
        provider_path: str | None = None,
        credentials_path: str | None = None,
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

    def perform_task(
        self,
        instruction: str,
        session: TmuxSession,
        logging_dir: Path | None = None,
    ) -> AgentResult:
        architecture_result = session.container.exec_run(["uname", "-m"])
        architecture = architecture_result.output.decode(errors="replace").strip()
        binary = self._binaries.get(architecture)
        if architecture_result.exit_code != 0 or binary is None or not binary.is_file():
            return AgentResult(failure_mode=FailureMode.AGENT_INSTALLATION_FAILED)

        try:
            with tempfile.TemporaryDirectory(prefix="golutra-tbench-auth-") as temp_dir:
                provider_file, credentials_file = self._active_auth_files(Path(temp_dir))
                session.copy_to_container(binary, container_dir="/installed-agent", container_filename="golutra")
                session.copy_to_container(provider_file, container_dir="/root/.golutra", container_filename="provider.json")
                session.copy_to_container(credentials_file, container_dir="/root/.golutra", container_filename="credentials.json")
        except (OSError, ValueError, json.JSONDecodeError):
            return AgentResult(failure_mode=FailureMode.AGENT_INSTALLATION_FAILED)

        setup_result = session.container.exec_run(
            [
                "sh",
                "-c",
                "chmod 755 /installed-agent/golutra && "
                "chmod 700 /root/.golutra && "
                "chmod 600 /root/.golutra/provider.json /root/.golutra/credentials.json && "
                "/installed-agent/golutra --help >/dev/null",
            ]
        )
        if setup_result.exit_code != 0:
            return AgentResult(failure_mode=FailureMode.AGENT_INSTALLATION_FAILED)

        rendered_instruction = self._render_instruction(instruction)
        command = (
            "HOME=/root GOLUTRA_HOME=/root/.golutra "
            "/installed-agent/golutra --cwd /app exec "
            "--run-dir /logs/golutra-runtime "
            "--approval-mode auto -- "
            f"{shlex.quote(rendered_instruction)}"
        )
        session.send_command(
            TerminalCommand(
                command=command,
                min_timeout_sec=0.0,
                max_timeout_sec=float("inf"),
                block=True,
                append_enter=True,
            )
        )
        return AgentResult()
