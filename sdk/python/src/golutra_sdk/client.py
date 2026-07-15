from __future__ import annotations

import ipaddress
import json
import os
import threading
import time
import uuid
from collections.abc import Generator, Iterable
from datetime import datetime, timezone
from typing import Any, TypeVar
from urllib.error import HTTPError, URLError
from urllib.parse import quote, urlencode, urlsplit
from urllib.request import Request, urlopen


RUNTIME_PROTOCOL_VERSION = 1
JSON_REQUEST_TIMEOUT_SECONDS = 30
MAX_JSON_RESPONSE_BYTES = 16 * 1024 * 1024
MAX_SSE_EVENT_BYTES = 1024 * 1024
T = TypeVar("T")


class GolutraError(RuntimeError):
    pass


class GolutraClient:
    def __init__(self, base_url: str, cwd: str, transport_token: str) -> None:
        self._base_url = _validate_base_url(base_url)
        normalized_cwd = cwd.strip()
        if not normalized_cwd or not os.path.isabs(normalized_cwd):
            raise ValueError(f"GolutraClient requires an absolute cwd: {cwd}")
        token = transport_token.strip()
        if not 32 <= len(token) <= 512 or any(character.isspace() for character in token):
            raise ValueError(
                "GolutraClient transport_token must contain 32..=512 non-whitespace characters"
            )
        self._cwd = normalized_cwd
        self._transport_token = token
        self._actor_id = f"python-sdk-{uuid.uuid4()}"
        self._attachment: dict[str, Any] | None = None
        self._attachment_lock = threading.Lock()

    def server_info(self) -> dict[str, Any]:
        info = self._request_json("GET", "/runtime/info", attached=False)
        versions = info.get("protocol_versions", {})
        minimum = versions.get("minimum")
        current = versions.get("current")
        if not isinstance(minimum, int) or not isinstance(current, int):
            raise GolutraError("runtime protocol range is missing")
        if not minimum <= RUNTIME_PROTOCOL_VERSION <= current:
            raise GolutraError(
                f"Golutra protocol {RUNTIME_PROTOCOL_VERSION} is incompatible with "
                f"server range {minimum}..={current}"
            )
        return info

    def runtime_info(self) -> dict[str, Any]:
        return dict(self._runtime_attachment()["runtime"])

    def send_command(self, command: dict[str, Any]) -> dict[str, Any]:
        return self._request_json("POST", "/commands", body=command)

    def query(self, query: dict[str, Any]) -> Any:
        return self._request_json("POST", "/queries", body=query)

    def prompt(self, session_id: str, prompt: str, actor_id: str | None = None) -> dict[str, Any]:
        return self.send_command(
            self.session_command(session_id, "prompt", {"prompt": prompt}, actor_id)
        )

    def takeover(self, session_id: str, actor_id: str | None = None) -> dict[str, Any]:
        return self.send_command(self.session_command(session_id, "takeover", {}, actor_id))

    def event_page(self, request: dict[str, Any]) -> dict[str, Any]:
        parameters = {
            key: value
            for key, value in request.items()
            if value is not None and key in {"session_id", "task_id", "cursor", "direction", "limit"}
        }
        return self._request_json("GET", f"/events/page?{urlencode(parameters)}")

    def replay_events(self, event_filter: dict[str, Any]) -> list[dict[str, Any]]:
        parameters = _event_parameters(event_filter)
        return self._request_json("GET", f"/events/replay?{urlencode(parameters)}")

    def subscribe(
        self,
        event_filter: dict[str, Any],
        stop_event: threading.Event | None = None,
    ) -> Generator[dict[str, Any], None, None]:
        cursor = event_filter.get("after_sequence_no")
        retry_delay = 0.1
        while stop_event is None or not stop_event.is_set():
            parameters = _event_parameters(event_filter, cursor)
            request = self._request(
                "GET",
                f"/events?{urlencode(parameters)}",
                attached=True,
                last_event_id=cursor,
            )
            try:
                with self._open(request) as response:
                    if response.status == 410:
                        self._clear_attachment()
                        continue
                    if not 200 <= response.status < 300:
                        raise self._http_error(response.status, self._read_bounded(response))
                    retry_delay = 0.1
                    event_name = ""
                    data: list[str] = []
                    frame_size = 0
                    for raw_line in response:
                        frame_size += len(raw_line)
                        if frame_size > MAX_SSE_EVENT_BYTES:
                            raise GolutraError(
                                f"SSE event exceeds {MAX_SSE_EVENT_BYTES} byte limit"
                            )
                        line = raw_line.decode("utf-8").rstrip("\r\n")
                        if line:
                            if line.startswith("event:"):
                                event_name = line[6:].strip()
                            elif line.startswith("data:"):
                                data.append(line[5:].lstrip())
                            continue
                        frame_size = 0
                        if not data:
                            event_name = ""
                            continue
                        payload = "\n".join(data)
                        data = []
                        if event_name == "error":
                            raise GolutraError(payload)
                        event_name = ""
                        event = json.loads(payload)
                        sequence = event.get("sequence_no")
                        if isinstance(sequence, int) and isinstance(cursor, int) and sequence <= cursor:
                            continue
                        yield event
                        if isinstance(sequence, int):
                            cursor = sequence
            except (GolutraError, URLError, OSError):
                if stop_event is not None and stop_event.is_set():
                    return
                time.sleep(retry_delay)
                retry_delay = min(retry_delay * 2, 2.0)

    def list_threads(self, limit: int = 20) -> list[dict[str, Any]]:
        return self._request_json("GET", f"/threads?{urlencode({'limit': limit})}")

    def thread_for_session(self, session_id: str) -> dict[str, Any] | None:
        return self._request_json("GET", f"/sessions/{quote(session_id, safe='')}/thread")

    def resume_thread(self, thread_id: str) -> dict[str, Any]:
        return self._request_json("POST", f"/threads/{quote(thread_id, safe='')}/resume")

    def fork_thread(self, thread_id: str, from_turn_id: str | None = None) -> dict[str, Any]:
        return self._request_json(
            "POST",
            f"/threads/{quote(thread_id, safe='')}/fork",
            body={"from_turn_id": from_turn_id},
        )

    def export_thread_rollout(self, thread_id: str) -> dict[str, Any]:
        return self._request_json(
            "POST", f"/threads/{quote(thread_id, safe='')}/rollout/export"
        )

    def rebind_thread(self, thread_id: str, from_workspace_root: str) -> dict[str, Any]:
        return self._request_json(
            "POST",
            f"/threads/{quote(thread_id, safe='')}/rebind",
            body={"from_workspace_root": from_workspace_root},
        )

    def storage_status(self, session_id: str) -> dict[str, Any]:
        return self.query(self.runtime_query(session_id, "storage_status"))

    def run_storage_maintenance(
        self, session_id: str, actor_id: str | None = None
    ) -> dict[str, Any]:
        return self.send_command(
            self.session_command(session_id, "run_storage_maintenance", {}, actor_id)
        )

    def list_memory(self, session_id: str) -> list[dict[str, Any]]:
        return self.query(self.runtime_query(session_id, "memory_list"))

    def evaluation_results(self, session_id: str) -> dict[str, Any]:
        return self.query(self.runtime_query(session_id, "evaluation_results"))

    def improvement_candidates(self, session_id: str) -> list[dict[str, Any]]:
        return self.query(self.runtime_query(session_id, "improvement_candidates"))

    def automation_candidates(self, session_id: str) -> dict[str, Any]:
        return self.query(self.runtime_query(session_id, "automation_candidates"))

    def evolution_state(self, session_id: str) -> dict[str, Any]:
        return self.query(self.runtime_query(session_id, "evolution_state"))

    def plan_evolution(
        self,
        session_id: str,
        objective: str,
        budget: dict[str, int] | None = None,
        actor_id: str | None = None,
    ) -> dict[str, Any]:
        normalized_budget = {
            "max_generated_tasks": 20,
            "max_selected_tasks": 3,
            "max_tool_calls_per_task": 8,
            "max_runtime_ms_per_task": 120_000,
        }
        normalized_budget.update(budget or {})
        return self.send_command(
            self.session_command(
                session_id,
                "plan_evolution",
                {"objective": objective, "budget": normalized_budget},
                actor_id,
            )
        )

    def run_evolution(
        self, session_id: str, run_id: str | None = None, actor_id: str | None = None
    ) -> dict[str, Any]:
        return self.send_command(
            self.session_command(session_id, "run_evolution", {"run_id": run_id}, actor_id)
        )

    def stage_skill(
        self, session_id: str, candidate_id: str, actor_id: str | None = None
    ) -> dict[str, Any]:
        return self.send_command(
            self.session_command(
                session_id, "stage_skill", {"candidate_id": candidate_id}, actor_id
            )
        )

    def review_skill(
        self,
        session_id: str,
        skill_id: str,
        decision: str,
        reason: str,
        regression_refs: Iterable[str] = (),
        actor_id: str | None = None,
    ) -> dict[str, Any]:
        return self.send_command(
            self.session_command(
                session_id,
                "review_skill",
                {
                    "skill_id": skill_id,
                    "decision": decision,
                    "reason": reason,
                    "regression_refs": list(regression_refs),
                },
                actor_id,
            )
        )

    def install_skill(
        self, session_id: str, skill_id: str, actor_id: str | None = None
    ) -> dict[str, Any]:
        return self.send_command(
            self.session_command(session_id, "install_skill", {"skill_id": skill_id}, actor_id)
        )

    def rollback_skill(
        self,
        session_id: str,
        skill_id: str,
        reason: str = "rolled back by SDK user",
        actor_id: str | None = None,
    ) -> dict[str, Any]:
        return self.send_command(
            self.session_command(
                session_id,
                "rollback_skill",
                {"skill_id": skill_id, "reason": reason},
                actor_id,
            )
        )

    def rollback_memory(
        self,
        session_id: str,
        memory_id: str,
        reason: str = "rolled back by SDK user",
        actor_id: str | None = None,
    ) -> dict[str, Any]:
        return self.send_command(
            self.session_command(
                session_id,
                "memory_rollback",
                {"memory_id": memory_id, "reason": reason},
                actor_id,
            )
        )

    def record_memory_feedback(
        self,
        session_id: str,
        memory_id: str,
        feedback: str,
        reason: str = "",
        actor_id: str | None = None,
    ) -> dict[str, Any]:
        return self.send_command(
            self.session_command(
                session_id,
                "memory_feedback",
                {"memory_id": memory_id, "feedback": feedback, "reason": reason},
                actor_id,
            )
        )

    def run_regression(
        self, session_id: str, candidate_id: str, actor_id: str | None = None
    ) -> dict[str, Any]:
        return self.send_command(
            self.session_command(
                session_id, "run_regression", {"candidate_id": candidate_id}, actor_id
            )
        )

    def review_candidate(
        self,
        session_id: str,
        candidate_id: str,
        decision: str,
        reason: str,
        actor_id: str | None = None,
    ) -> dict[str, Any]:
        return self.send_command(
            self.session_command(
                session_id,
                "review_candidate",
                {"candidate_id": candidate_id, "decision": decision, "reason": reason},
                actor_id,
            )
        )

    def record_benchmark(
        self,
        session_id: str,
        run: dict[str, Any],
        actor_id: str | None = None,
    ) -> dict[str, Any]:
        return self.send_command(
            self.session_command(session_id, "record_benchmark", {"run": run}, actor_id)
        )

    def compare_counterfactual(
        self, session_id: str, group_id: str, actor_id: str | None = None
    ) -> dict[str, Any]:
        return self.send_command(
            self.session_command(
                session_id, "compare_counterfactual", {"group_id": group_id}, actor_id
            )
        )

    def apply_candidate(
        self, session_id: str, candidate_id: str, actor_id: str | None = None
    ) -> dict[str, Any]:
        return self.send_command(
            self.session_command(
                session_id, "apply_candidate", {"candidate_id": candidate_id}, actor_id
            )
        )

    def rollback_candidate(
        self,
        session_id: str,
        candidate_id: str,
        reason: str = "rolled back by SDK user",
        actor_id: str | None = None,
    ) -> dict[str, Any]:
        return self.send_command(
            self.session_command(
                session_id,
                "rollback_candidate",
                {"candidate_id": candidate_id, "reason": reason},
                actor_id,
            )
        )

    def session_command(
        self,
        session_id: str,
        kind: str,
        payload: dict[str, Any],
        actor_id: str | None = None,
    ) -> dict[str, Any]:
        command_id = str(uuid.uuid4())
        return {
            "command_id": command_id,
            "session_id": session_id,
            "kind": kind,
            "idempotency_key": command_id,
            "actor": {"kind": "sdk", "id": actor_id or self._actor_id},
            "payload": payload,
            "timestamp": _timestamp(),
        }

    @staticmethod
    def runtime_query(session_id: str, kind: str) -> dict[str, Any]:
        return {
            "query_id": str(uuid.uuid4()),
            "session_id": session_id,
            "task_id": None,
            "kind": kind,
            "requester": "sdk",
            "cursor": None,
            "timestamp": _timestamp(),
        }

    def _runtime_attachment(self) -> dict[str, Any]:
        with self._attachment_lock:
            if self._attachment is None:
                self.server_info()
                attachment = self._request_json(
                    "POST",
                    "/runtime/attach",
                    body={"cwd": self._cwd, "protocol_version": RUNTIME_PROTOCOL_VERSION},
                    attached=False,
                )
                runtime_cwd = attachment.get("runtime", {}).get("cwd")
                if runtime_cwd != self._cwd:
                    raise GolutraError(
                        f"runtime attached {runtime_cwd!r} instead of requested cwd {self._cwd!r}"
                    )
                self._attachment = attachment
            return self._attachment

    def _clear_attachment(self) -> None:
        with self._attachment_lock:
            self._attachment = None

    def _request_json(
        self,
        method: str,
        path: str,
        body: dict[str, Any] | None = None,
        attached: bool = True,
    ) -> Any:
        request = self._request(method, path, body, attached)
        with self._open(request) as response:
            payload = self._read_bounded(response)
            if response.status == 410 and attached:
                self._clear_attachment()
                return self._request_json(method, path, body, attached)
            if not 200 <= response.status < 300:
                raise self._http_error(response.status, payload)
        try:
            return json.loads(payload)
        except json.JSONDecodeError as error:
            raise GolutraError(f"runtime returned invalid JSON: {error}") from error

    def _request(
        self,
        method: str,
        path: str,
        body: dict[str, Any] | None = None,
        attached: bool = True,
        last_event_id: int | None = None,
    ) -> Request:
        if not path.startswith("/") or path.startswith("//"):
            raise ValueError(f"runtime path must be absolute: {path}")
        headers = {
            "authorization": f"Bearer {self._transport_token}",
            "x-golutra-protocol-version": str(RUNTIME_PROTOCOL_VERSION),
        }
        if attached:
            headers["x-golutra-attachment"] = self._runtime_attachment()["attachment_id"]
        if last_event_id is not None:
            headers["last-event-id"] = str(last_event_id)
        data = None
        if body is not None:
            data = json.dumps(body, separators=(",", ":")).encode("utf-8")
            headers["content-type"] = "application/json"
        return Request(self._base_url + path, data=data, headers=headers, method=method)

    @staticmethod
    def _open(request: Request):
        try:
            return urlopen(request, timeout=JSON_REQUEST_TIMEOUT_SECONDS)
        except HTTPError as error:
            return error
        except URLError as error:
            raise GolutraError(f"runtime transport failed: {error}") from error

    @staticmethod
    def _read_bounded(response) -> bytes:
        body = bytearray()
        while True:
            chunk = response.read(64 * 1024)
            if not chunk:
                return bytes(body)
            if len(body) + len(chunk) > MAX_JSON_RESPONSE_BYTES:
                raise GolutraError(
                    f"runtime response exceeds {MAX_JSON_RESPONSE_BYTES} byte limit"
                )
            body.extend(chunk)

    @staticmethod
    def _http_error(status: int, body: bytes) -> GolutraError:
        try:
            value = json.loads(body)
            message = value.get("error", body.decode("utf-8", errors="replace"))
        except json.JSONDecodeError:
            message = body.decode("utf-8", errors="replace")
        return GolutraError(f"HTTP {status}: {message}")


def _event_parameters(event_filter: dict[str, Any], cursor: int | None = None) -> dict[str, Any]:
    parameters = {"session_id": event_filter["session_id"]}
    if event_filter.get("task_id") is not None:
        parameters["task_id"] = event_filter["task_id"]
    selected_cursor = cursor if cursor is not None else event_filter.get("after_sequence_no")
    if selected_cursor is not None:
        parameters["cursor"] = selected_cursor
    return parameters


def _validate_base_url(base_url: str) -> str:
    normalized = base_url.strip().rstrip("/")
    parsed = urlsplit(normalized)
    try:
        loopback = parsed.hostname == "localhost" or (
            parsed.hostname is not None and ipaddress.ip_address(parsed.hostname).is_loopback
        )
    except ValueError:
        loopback = False
    if parsed.scheme != "https" and not (parsed.scheme == "http" and loopback):
        raise ValueError("Golutra endpoint must use HTTPS or loopback HTTP")
    if parsed.path not in {"", "/"} or parsed.query or parsed.fragment or parsed.username:
        raise ValueError("Golutra endpoint must be a root URL without credentials")
    return normalized


def _timestamp() -> str:
    return datetime.now(timezone.utc).isoformat().replace("+00:00", "Z")
