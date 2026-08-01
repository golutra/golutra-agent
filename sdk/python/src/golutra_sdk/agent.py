"""High-level Thread/Turn API backed by the Golutra app server."""

from __future__ import annotations

from collections.abc import Generator, Iterable
from typing import Any, TYPE_CHECKING

if TYPE_CHECKING:
    from .client import GolutraClient
    from .generated import TaskContract, TaskReconciliationDecision


class Thread:
    """A durable runtime thread; execution remains owned by the app server."""

    def __init__(self, client: GolutraClient, reference: dict[str, Any]) -> None:
        self._client = client
        self.reference = dict(reference)
        self.thread_id = str(self.reference["thread_id"])
        self.session_id = str(self.reference["session_id"])
        self.workspace_root = self.reference.get("workspace_root")

    def run(
        self,
        prompt: str,
        *,
        output_schema: dict[str, Any] | None = None,
        task_contract: TaskContract | None = None,
        allow_network: bool = False,
        yolo: bool = False,
        max_elapsed_ms: int | None = None,
        defer_external_verification: bool = False,
        completion_criteria: Iterable[str] = (),
        external_verifiers: Iterable[dict[str, Any]] | None = None,
        discover_project_verifiers: bool = True,
    ) -> TurnHandle:
        if not prompt.strip():
            raise ValueError("turn prompt cannot be empty")
        params: dict[str, Any] = {
            "thread_id": self.thread_id,
            "prompt": prompt,
            "allow_network": allow_network,
            "yolo": yolo,
            "defer_external_verification": defer_external_verification,
            "completion_criteria": [item for item in completion_criteria if item.strip()],
        }
        if max_elapsed_ms is not None:
            params["max_elapsed_ms"] = max_elapsed_ms
        if external_verifiers is not None or not discover_project_verifiers:
            params["external_verifiers"] = [
                dict(item) for item in (external_verifiers or ())
            ]
        if task_contract is not None:
            params["task_contract"] = dict(task_contract)
        if output_schema is not None:
            params["output_schema"] = output_schema
        result = self._client.rpc("turn/start", params)
        if result.get("accepted") is not True:
            raise RuntimeError(result.get("reason") or "turn was rejected")
        return TurnHandle(self, result)

    def run_streamed(
        self,
        prompt: str,
        *,
        output_schema: dict[str, Any] | None = None,
        task_contract: TaskContract | None = None,
        allow_network: bool = False,
        yolo: bool = False,
        max_elapsed_ms: int | None = None,
        defer_external_verification: bool = False,
        completion_criteria: Iterable[str] = (),
        external_verifiers: Iterable[dict[str, Any]] | None = None,
        discover_project_verifiers: bool = True,
    ) -> TurnHandle:
        return self.run(
            prompt,
            output_schema=output_schema,
            task_contract=task_contract,
            allow_network=allow_network,
            yolo=yolo,
            max_elapsed_ms=max_elapsed_ms,
            defer_external_verification=defer_external_verification,
            completion_criteria=completion_criteria,
            external_verifiers=external_verifiers,
            discover_project_verifiers=discover_project_verifiers,
        )

    def steer(self, prompt: str) -> dict[str, Any]:
        if not prompt.strip():
            raise ValueError("steering prompt cannot be empty")
        return _command_ack(
            self._client.rpc(
                "turn/steer",
                {"thread_id": self.thread_id, "prompt": prompt},
            ),
            "turn/steer",
        )

    def interrupt(self) -> dict[str, Any]:
        return _command_ack(
            self._client.rpc("turn/interrupt", {"thread_id": self.thread_id}),
            "turn/interrupt",
        )

    def takeover(self) -> dict[str, Any]:
        """Transfer the active runtime lane to this app-server attachment."""
        return _command_ack(
            self._client.rpc("turn/takeover", {"thread_id": self.thread_id}),
            "turn/takeover",
        )

    def reconcile_task(
        self,
        decision: TaskReconciliationDecision,
        *,
        task_id: str | None = None,
        note: str | None = None,
    ) -> dict[str, Any]:
        """Resolve an uncertain task before pending turns may continue."""
        params: dict[str, Any] = {
            "thread_id": self.thread_id,
            "decision": decision,
        }
        if task_id is not None:
            params["task_id"] = task_id
        if note is not None:
            params["note"] = note
        return _command_ack(
            self._client.rpc("task/reconcile", params),
            "task/reconcile",
        )

    def event_page(
        self,
        *,
        cursor: int | None = None,
        direction: str = "backward",
        limit: int = 128,
    ) -> dict[str, Any]:
        """Read one bounded durable history page for this thread."""
        if direction not in {"forward", "backward"}:
            raise ValueError("direction must be `forward` or `backward`")
        if not 1 <= limit <= 512:
            raise ValueError("limit must be between 1 and 512")
        return self._client.event_page(
            {
                "session_id": self.session_id,
                "cursor": cursor,
                "direction": direction,
                "limit": limit,
            }
        )

    def history(
        self,
        *,
        cursor: int | None = None,
        direction: str = "backward",
        limit: int = 128,
    ) -> dict[str, Any]:
        """Alias for event_page, named for conversational clients."""
        return self.event_page(cursor=cursor, direction=direction, limit=limit)

    def replay_events(self, *, cursor: int | None = None) -> list[dict[str, Any]]:
        """Read the bounded replay compatibility endpoint for this thread."""
        return self._client.replay_events(
            {"session_id": self.session_id, "after_sequence_no": cursor}
        )

    def debug_projection(self, task_id: str) -> dict[str, Any]:
        """Read the developer projection without exposing it in ordinary user views."""
        return self._client.debug_projection(self.session_id, task_id)

    def replay(self, task_id: str, capsule_id: str | None = None) -> dict[str, Any]:
        return self._client.replay(self.session_id, task_id, capsule_id)


class TurnHandle:
    """A single accepted turn and its normalized event stream."""

    def __init__(self, thread: Thread, start: dict[str, Any]) -> None:
        self.thread = thread
        self.start = dict(start)
        self.command_id = str(start["command_id"])
        self.cursor = start.get("cursor")
        self.start_cursor = self.cursor if isinstance(self.cursor, int) else 0
        self._terminal: dict[str, Any] | None = None

    def events(self, stop_event: Any = None) -> Generator[dict[str, Any], None, None]:
        # A terminal event is durable, but the live SSE endpoint is not a
        # history query. Do not subscribe again after the turn was consumed.
        if self._terminal is not None:
            return
        for event in self.thread._client.subscribe_agent(
            self.thread.session_id,
            self.thread.thread_id,
            self.command_id,
            self.cursor,
            start_cursor=self.start_cursor,
            stop_event=stop_event,
        ):
            sequence = _agent_event_sequence(event)
            if isinstance(sequence, int):
                self.cursor = sequence
            terminal = event.get("type") in {"turn.completed", "turn.failed"}
            if terminal:
                self._terminal = event
            yield event
            if terminal:
                return

    def wait(self) -> dict[str, Any]:
        if self._terminal is not None:
            return self._result_from_terminal()
        for _event in self.events():
            pass
        if self._terminal is None:
            raise RuntimeError("agent event stream ended before turn completion")
        return self._result_from_terminal()

    def _result_from_terminal(self) -> dict[str, Any]:
        if self._terminal is None:
            raise RuntimeError("agent event stream ended before turn completion")
        return {
            "thread_id": self.thread.thread_id,
            "session_id": self.thread.session_id,
            "task_id": self._terminal.get("task_id"),
            "turn_id": self._terminal.get("turn_id"),
            "status": self._terminal.get("status", "failed"),
            "final_message": self._terminal.get("final_message"),
            "verification": self._terminal.get("verification"),
            "last_sequence_no": self._terminal.get("last_sequence_no"),
        }

    def steer(self, prompt: str) -> dict[str, Any]:
        return self.thread.steer(prompt)

    def interrupt(self) -> dict[str, Any]:
        return self.thread.interrupt()

    def resolve_approval(self, approval_id: str, approve: bool) -> dict[str, Any]:
        return _command_ack(
            self.thread._client.rpc(
                "approval/resolve",
                {
                    "thread_id": self.thread.thread_id,
                    "approval_id": approval_id,
                    "approve": approve,
                },
            ),
            "approval/resolve",
        )


def _agent_event_sequence(event: dict[str, Any]) -> int | None:
    runtime_event = event.get("event")
    if isinstance(runtime_event, dict) and isinstance(runtime_event.get("sequence_no"), int):
        return runtime_event["sequence_no"]
    item = event.get("item")
    if isinstance(item, dict) and isinstance(item.get("sequence_no"), int):
        return item["sequence_no"]
    sequence = event.get("last_sequence_no")
    return sequence if isinstance(sequence, int) else None


def _command_ack(result: dict[str, Any], method: str) -> dict[str, Any]:
    ack = result.get("ack")
    if not isinstance(ack, dict):
        raise RuntimeError(f"JSON-RPC {method} did not return a command acknowledgement")
    return ack
