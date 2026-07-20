from __future__ import annotations

import copy
import json
import os
import queue
import socket
import stat
import subprocess
import threading
import uuid
from collections.abc import Callable, Generator, Mapping, Sequence
from dataclasses import dataclass
from pathlib import Path
from typing import Any, BinaryIO

from .generated import DriverNotification, DriverResponseEnvelope, DriverState, TuiFrame


TUI_DRIVER_PROTOCOL_VERSION = 1
DEFAULT_REQUEST_TIMEOUT_SECONDS = 30.0
DEFAULT_STARTUP_TIMEOUT_SECONDS = 30.0
MAX_DRIVER_LINE_BYTES = 1024 * 1024
MAX_SNAPSHOT_PAGES = 4096


class TuiDriverError(RuntimeError):
    def __init__(self, message: str, code: str = "driver_error") -> None:
        super().__init__(message)
        self.code = code


class TuiDriverDisconnectedError(TuiDriverError):
    def __init__(self, message: str = "TUI Driver connection closed") -> None:
        super().__init__(message, "driver_disconnected")


@dataclass
class _PendingRequest:
    event: threading.Event
    response: dict[str, Any] | None = None
    error: BaseException | None = None


@dataclass
class _Transport:
    reader: BinaryIO
    writer: BinaryIO
    close_callback: Callable[[], None]

    def close(self) -> None:
        try:
            self.close_callback()
        except OSError:
            pass
        for stream in (self.writer, self.reader):
            try:
                stream.close()
            except OSError:
                pass


class TuiDriverClient:
    def __init__(
        self,
        *,
        request_timeout: float = DEFAULT_REQUEST_TIMEOUT_SECONDS,
        startup_timeout: float = DEFAULT_STARTUP_TIMEOUT_SECONDS,
        on_notification: Callable[[DriverNotification], None] | None = None,
        on_diagnostic: Callable[[DriverResponseEnvelope], None] | None = None,
    ) -> None:
        self._request_timeout = _positive_timeout(request_timeout, "request_timeout")
        self._startup_timeout = _positive_timeout(startup_timeout, "startup_timeout")
        self._pending: dict[str, _PendingRequest] = {}
        self._pending_lock = threading.Lock()
        self._write_lock = threading.Lock()
        self._lifecycle_lock = threading.Lock()
        self._ready_event = threading.Event()
        self._ready: dict[str, Any] | None = None
        self._startup_error: BaseException | None = None
        self._transport: _Transport | None = None
        self._process: subprocess.Popen[bytes] | None = None
        self._socket_path: str | None = None
        self._reader_thread: threading.Thread | None = None
        self._notification_callbacks: list[Callable[[DriverNotification], None]] = []
        self._diagnostic_callbacks: list[Callable[[DriverResponseEnvelope], None]] = []
        self._notifications: queue.Queue[DriverNotification] = queue.Queue()
        self._diagnostics: queue.Queue[DriverResponseEnvelope] = queue.Queue()
        self._closing = False
        if on_notification is not None:
            self._notification_callbacks.append(on_notification)
        if on_diagnostic is not None:
            self._diagnostic_callbacks.append(on_diagnostic)

    @classmethod
    def spawn(
        cls,
        workspace_path: str | os.PathLike[str],
        *,
        binary_path: str | os.PathLike[str] = "golutra-tui",
        session: str | None = None,
        task_id: str | None = None,
        debug: bool = False,
        embedded: bool = False,
        daemon: bool = False,
        connect: str | None = None,
        width: int | None = None,
        height: int | None = None,
        idle_timeout_seconds: int | None = None,
        heartbeat_seconds: int | None = None,
        env: Mapping[str, str] | None = None,
        on_stderr: Callable[[str], None] | None = None,
        request_timeout: float = DEFAULT_REQUEST_TIMEOUT_SECONDS,
        startup_timeout: float = DEFAULT_STARTUP_TIMEOUT_SECONDS,
        on_notification: Callable[[DriverNotification], None] | None = None,
        on_diagnostic: Callable[[DriverResponseEnvelope], None] | None = None,
    ) -> TuiDriverClient:
        workspace = _absolute_path(workspace_path, "workspace_path")
        if embedded and (daemon or connect is not None):
            raise TuiDriverError(
                "embedded cannot be combined with daemon or connect", "invalid_transport"
            )
        if daemon and connect is not None:
            raise TuiDriverError("daemon cannot be combined with connect", "invalid_transport")
        command = [os.fspath(binary_path), "--cwd", workspace]
        if daemon:
            command.append("--daemon")
        if connect is not None:
            command.extend(("--connect", connect))
        if task_id is not None:
            command.extend(("--task-id", task_id))
        if debug:
            command.append("--debug")
        command.extend(("driver", "--stdio"))
        if embedded:
            command.append("--embedded")
        if session is not None:
            command.extend(("--session", session))
        if width is not None:
            command.extend(("--width", str(width)))
        if height is not None:
            command.extend(("--height", str(height)))
        if idle_timeout_seconds is not None:
            command.extend(("--idle-timeout-secs", str(idle_timeout_seconds)))
        if heartbeat_seconds is not None:
            command.extend(("--heartbeat-secs", str(heartbeat_seconds)))
        return cls.spawn_command(
            command,
            cwd=workspace,
            env=env,
            on_stderr=on_stderr,
            request_timeout=request_timeout,
            startup_timeout=startup_timeout,
            on_notification=on_notification,
            on_diagnostic=on_diagnostic,
        )

    @classmethod
    def spawn_command(
        cls,
        command: Sequence[str | os.PathLike[str]],
        *,
        cwd: str | os.PathLike[str] | None = None,
        env: Mapping[str, str] | None = None,
        on_stderr: Callable[[str], None] | None = None,
        request_timeout: float = DEFAULT_REQUEST_TIMEOUT_SECONDS,
        startup_timeout: float = DEFAULT_STARTUP_TIMEOUT_SECONDS,
        on_notification: Callable[[DriverNotification], None] | None = None,
        on_diagnostic: Callable[[DriverResponseEnvelope], None] | None = None,
    ) -> TuiDriverClient:
        if not command:
            raise TuiDriverError("Driver command must not be empty", "invalid_command")
        client = cls(
            request_timeout=request_timeout,
            startup_timeout=startup_timeout,
            on_notification=on_notification,
            on_diagnostic=on_diagnostic,
        )
        process_env = None if env is None else {**os.environ, **env}
        process = subprocess.Popen(
            [os.fspath(part) for part in command],
            cwd=None if cwd is None else os.fspath(cwd),
            env=process_env,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
        if process.stdin is None or process.stdout is None or process.stderr is None:
            process.kill()
            raise TuiDriverError("Driver process pipes are unavailable", "spawn_failed")
        client._process = process

        def close_process() -> None:
            if process.poll() is None:
                process.terminate()
                try:
                    process.wait(timeout=1)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait(timeout=1)

        client._attach_transport(_Transport(process.stdout, process.stdin, close_process))
        threading.Thread(
            target=_read_stderr,
            args=(process.stderr, on_stderr),
            name="golutra-tui-driver-stderr",
            daemon=True,
        ).start()
        try:
            client._wait_until_ready()
        except BaseException:
            client.disconnect()
            raise
        return client

    @classmethod
    def connect_socket(
        cls,
        socket_path: str | os.PathLike[str],
        *,
        request_timeout: float = DEFAULT_REQUEST_TIMEOUT_SECONDS,
        startup_timeout: float = DEFAULT_STARTUP_TIMEOUT_SECONDS,
        on_notification: Callable[[DriverNotification], None] | None = None,
        on_diagnostic: Callable[[DriverResponseEnvelope], None] | None = None,
    ) -> TuiDriverClient:
        if os.name == "nt":
            raise TuiDriverError(
                "Unix socket TUI Driver connections are unavailable on Windows",
                "unsupported_transport",
            )
        client = cls(
            request_timeout=request_timeout,
            startup_timeout=startup_timeout,
            on_notification=on_notification,
            on_diagnostic=on_diagnostic,
        )
        client._socket_path = _absolute_path(socket_path, "socket_path")
        client._open_socket()
        return client

    @property
    def ready(self) -> dict[str, Any]:
        if self._ready is None:
            raise TuiDriverDisconnectedError(
                "TUI Driver has not completed its ready handshake"
            )
        return dict(self._ready)

    @property
    def connected(self) -> bool:
        return self._transport is not None and self._ready is not None

    def __enter__(self) -> TuiDriverClient:
        return self

    def __exit__(self, _type: object, _value: object, _traceback: object) -> None:
        self.close()

    def on_notification(
        self, callback: Callable[[DriverNotification], None]
    ) -> Callable[[], None]:
        self._notification_callbacks.append(callback)

        def remove() -> None:
            try:
                self._notification_callbacks.remove(callback)
            except ValueError:
                pass

        return remove

    def on_diagnostic(
        self, callback: Callable[[DriverResponseEnvelope], None]
    ) -> Callable[[], None]:
        self._diagnostic_callbacks.append(callback)

        def remove() -> None:
            try:
                self._diagnostic_callbacks.remove(callback)
            except ValueError:
                pass

        return remove

    def next_notification(self, timeout: float | None = None) -> DriverNotification:
        return self._notifications.get(timeout=timeout)

    def next_diagnostic(self, timeout: float | None = None) -> DriverResponseEnvelope:
        return self._diagnostics.get(timeout=timeout)

    def reconnect(self) -> dict[str, Any]:
        if self._socket_path is None:
            raise TuiDriverError(
                "Only Unix socket clients support explicit reconnect",
                "unsupported_reconnect",
            )
        if self.connected:
            return self.ready
        self._open_socket()
        return self.ready

    def request(
        self, request: Mapping[str, Any], *, timeout: float | None = None
    ) -> dict[str, Any]:
        transport = self._transport
        if transport is None or self._ready is None:
            raise TuiDriverDisconnectedError()
        request_id = str(uuid.uuid4())
        request_timeout = _positive_timeout(
            self._request_timeout if timeout is None else timeout, "timeout"
        )
        pending = _PendingRequest(threading.Event())
        with self._pending_lock:
            self._pending[request_id] = pending
        envelope = {**request, "request_id": request_id}
        encoded = json.dumps(envelope, separators=(",", ":")).encode("utf-8") + b"\n"
        if len(encoded) > MAX_DRIVER_LINE_BYTES:
            self._reject_pending(
                request_id,
                TuiDriverError(
                    f"TUI Driver request exceeds {MAX_DRIVER_LINE_BYTES} bytes",
                    "request_too_large",
                ),
            )
        else:
            try:
                with self._write_lock:
                    current = self._transport
                    if current is None or current is not transport:
                        raise TuiDriverDisconnectedError()
                    current.writer.write(encoded)
                    current.writer.flush()
            except BaseException as error:
                self._reject_pending(request_id, error)
                self._transport_failed(error)
        if not pending.event.wait(request_timeout):
            with self._pending_lock:
                removed = self._pending.pop(request_id, None)
            if removed is not None:
                raise TuiDriverError(
                    f"TUI Driver request {request_id} timed out after {request_timeout}s",
                    "request_timeout",
                )
            pending.event.wait()
        if pending.error is not None:
            raise pending.error
        if pending.response is None:
            raise TuiDriverError("TUI Driver request completed without a response")
        return pending.response

    def hello(self, protocol_version: int = TUI_DRIVER_PROTOCOL_VERSION) -> dict[str, Any]:
        return _expect_response(
            self.request({"type": "hello", "protocol_version": protocol_version}), "ready"
        )

    def capabilities(self) -> list[str]:
        response = _expect_response(self.request({"type": "capabilities"}), "capabilities")
        return list(response["capabilities"])

    def state(self) -> DriverState:
        response = _expect_response(self.request({"type": "state"}), "state")
        return {
            key: value
            for key, value in response.items()
            if key not in {"request_id", "type"}
        }  # type: ignore[return-value]

    def ping(self) -> None:
        _expect_response(self.request({"type": "ping"}), "pong")

    def prompt(self, text: str, *, timeout: float | None = None) -> None:
        _expect_response(
            self.request({"type": "input_prompt", "text": text}, timeout=timeout),
            "accepted",
        )

    def slash(self, text: str, *, timeout: float | None = None) -> None:
        _expect_response(
            self.request({"type": "input_slash", "text": text}, timeout=timeout),
            "accepted",
        )

    def wait(
        self,
        until: Mapping[str, Any],
        timeout_ms: int | None = None,
        *,
        request_timeout: float | None = None,
    ) -> dict[str, Any]:
        timeout = request_timeout
        if timeout is None:
            timeout = max(self._request_timeout, (timeout_ms or 0) / 1000 + 1)
        response = self.request(
            {"type": "wait", "until": dict(until), "timeout_ms": timeout_ms},
            timeout=timeout,
        )
        if response.get("type") not in {"wait_result", "wait_timeout"}:
            raise _unexpected_response(response, "wait_result or wait_timeout")
        return response

    def snapshot(
        self, request: Mapping[str, Any], *, timeout: float | None = None
    ) -> TuiFrame:
        response = _expect_response(
            self.request({"type": "snapshot", **request}, timeout=timeout), "snapshot"
        )
        return {
            key: value
            for key, value in response.items()
            if key not in {"request_id", "type"}
        }  # type: ignore[return-value]

    def snapshot_pages(
        self,
        request: Mapping[str, Any],
        *,
        max_pages: int = MAX_SNAPSHOT_PAGES,
        timeout: float | None = None,
    ) -> Generator[TuiFrame, None, None]:
        if not isinstance(max_pages, int) or isinstance(max_pages, bool) or max_pages < 1:
            raise TuiDriverError(
                "max_pages must be a positive integer", "invalid_pagination"
            )
        base_request = {key: value for key, value in request.items() if key != "frame_id"}
        page = self.snapshot(base_request, timeout=timeout)
        yield page
        page_count = 1
        while page.get("next_range") is not None and page_count < max_pages:
            next_range = page["next_range"]
            if (
                next_range["start"] <= page["returned_range"]["end"]
                or next_range["end"] < next_range["start"]
            ):
                raise TuiDriverError(
                    "frozen snapshot pagination did not advance", "invalid_pagination"
                )
            page = self.snapshot(
                {
                    **base_request,
                    "rows": next_range,
                    "frame_id": page["frame_id"],
                },
                timeout=timeout,
            )
            yield page
            page_count += 1
        if page.get("next_range") is not None:
            raise TuiDriverError(
                f"frozen snapshot exceeds {max_pages} pages", "pagination_limit"
            )

    def complete_snapshot(
        self,
        request: Mapping[str, Any],
        *,
        max_pages: int = MAX_SNAPSHOT_PAGES,
        timeout: float | None = None,
    ) -> TuiFrame:
        combined: TuiFrame | None = None
        for page in self.snapshot_pages(
            request, max_pages=max_pages, timeout=timeout
        ):
            if combined is None:
                combined = copy.deepcopy(page)
                continue
            if page["frame_id"] != combined["frame_id"]:
                raise TuiDriverError(
                    "snapshot frame changed during pagination", "frame_mismatch"
                )
            combined["lines"].extend(page["lines"])
            if combined.get("cells") is not None and page.get("cells") is not None:
                combined["cells"].extend(page["cells"])  # type: ignore[union-attr]
            combined["returned_range"]["end"] = page["returned_range"]["end"]
            combined["next_range"] = page.get("next_range")
        if combined is None:
            raise TuiDriverError(
                "snapshot pagination returned no pages", "invalid_pagination"
            )
        return combined

    def takeover(self) -> None:
        _expect_response(self.request({"type": "takeover"}), "accepted")

    def abort(self) -> None:
        _expect_response(self.request({"type": "abort"}), "accepted")

    def close(self, abort_active_task: bool = False) -> None:
        if self._closing:
            return
        self._closing = True
        try:
            if self.connected:
                _expect_response(
                    self.request(
                        {
                            "type": "close",
                            "abort_active_task": abort_active_task,
                        },
                        timeout=self._request_timeout,
                    ),
                    "closed",
                )
        finally:
            self.disconnect()

    def disconnect(self) -> None:
        with self._lifecycle_lock:
            transport = self._transport
            self._transport = None
            self._ready = None
            self._closing = False
        if transport is not None:
            transport.close()
        self._reject_all(TuiDriverDisconnectedError())

    def _open_socket(self) -> None:
        if self._socket_path is None:
            raise TuiDriverError("socket path is missing")
        _validate_unix_socket(self._socket_path)
        self.disconnect()
        client_socket = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        try:
            client_socket.connect(self._socket_path)
            reader = client_socket.makefile("rb")
            writer = client_socket.makefile("wb")
        except BaseException:
            client_socket.close()
            raise
        self._attach_transport(_Transport(reader, writer, client_socket.close))
        try:
            self._wait_until_ready()
        except BaseException:
            self.disconnect()
            raise

    def _attach_transport(self, transport: _Transport) -> None:
        with self._lifecycle_lock:
            self._transport = transport
            self._ready = None
            self._startup_error = None
            self._ready_event.clear()
        self._reader_thread = threading.Thread(
            target=self._reader_loop,
            args=(transport,),
            name="golutra-tui-driver-reader",
            daemon=True,
        )
        self._reader_thread.start()

    def _wait_until_ready(self) -> None:
        if not self._ready_event.wait(self._startup_timeout):
            raise TuiDriverError(
                "TUI Driver ready handshake timed out", "startup_timeout"
            )
        if self._startup_error is not None:
            raise self._startup_error
        if self._ready is None:
            raise TuiDriverDisconnectedError()

    def _reader_loop(self, transport: _Transport) -> None:
        try:
            while True:
                line = transport.reader.readline(MAX_DRIVER_LINE_BYTES + 1)
                if not line:
                    raise TuiDriverDisconnectedError()
                if len(line) > MAX_DRIVER_LINE_BYTES or not line.endswith(b"\n"):
                    raise TuiDriverError(
                        f"TUI Driver response exceeds {MAX_DRIVER_LINE_BYTES} bytes",
                        "response_too_large",
                    )
                try:
                    response = json.loads(line)
                except (UnicodeDecodeError, json.JSONDecodeError) as error:
                    raise TuiDriverError(
                        f"TUI Driver emitted invalid JSON: {error}", "invalid_json"
                    ) from error
                if not isinstance(response, dict) or not isinstance(
                    response.get("request_id"), str
                ):
                    raise TuiDriverError(
                        "TUI Driver emitted an invalid envelope", "invalid_envelope"
                    )
                self._receive_response(response)
        except BaseException as error:
            self._transport_failed(error, transport)

    def _receive_response(self, response: dict[str, Any]) -> None:
        response_type = response.get("type")
        if response_type == "ready" and self._ready is None:
            minimum = response.get("minimum_protocol_version")
            current = response.get("protocol_version")
            if not isinstance(minimum, int) or not isinstance(current, int):
                self._transport_failed(
                    TuiDriverError(
                        "TUI Driver ready response has no protocol range",
                        "invalid_envelope",
                    )
                )
                return
            if minimum > TUI_DRIVER_PROTOCOL_VERSION or current < TUI_DRIVER_PROTOCOL_VERSION:
                self._transport_failed(
                    TuiDriverError(
                        f"TUI Driver protocol {TUI_DRIVER_PROTOCOL_VERSION} is incompatible "
                        f"with {minimum}..={current}",
                        "incompatible_protocol",
                    )
                )
                return
            self._ready = response
            self._ready_event.set()
            return
        if response_type == "event":
            notification = response.get("event")
            if isinstance(notification, dict):
                self._notifications.put(notification)  # type: ignore[arg-type]
                for callback in list(self._notification_callbacks):
                    try:
                        callback(notification)  # type: ignore[arg-type]
                    except Exception:
                        pass
            return
        request_id = response["request_id"]
        with self._pending_lock:
            pending = self._pending.pop(request_id, None)
        if pending is None:
            self._diagnostics.put(response)  # type: ignore[arg-type]
            for callback in list(self._diagnostic_callbacks):
                try:
                    callback(response)  # type: ignore[arg-type]
                except Exception:
                    pass
            return
        if response_type == "error":
            pending.error = TuiDriverError(
                str(response.get("message", "Driver request failed")),
                str(response.get("code", "driver_error")),
            )
        else:
            pending.response = response
        pending.event.set()

    def _transport_failed(
        self, error: BaseException, expected_transport: _Transport | None = None
    ) -> None:
        normalized = (
            error
            if isinstance(error, TuiDriverError)
            else TuiDriverDisconnectedError(str(error))
        )
        with self._lifecycle_lock:
            if expected_transport is not None and self._transport is not expected_transport:
                return
            transport = self._transport
            self._transport = None
            self._ready = None
            self._startup_error = normalized
            self._ready_event.set()
        if transport is not None:
            transport.close()
        self._reject_all(normalized)

    def _reject_pending(self, request_id: str, error: BaseException) -> None:
        with self._pending_lock:
            pending = self._pending.pop(request_id, None)
        if pending is not None:
            pending.error = error
            pending.event.set()

    def _reject_all(self, error: BaseException) -> None:
        with self._pending_lock:
            pending = list(self._pending.values())
            self._pending.clear()
        for request in pending:
            request.error = error
            request.event.set()


def _expect_response(response: dict[str, Any], response_type: str) -> dict[str, Any]:
    if response.get("type") != response_type:
        raise _unexpected_response(response, response_type)
    return response


def _unexpected_response(response: Mapping[str, Any], expected: str) -> TuiDriverError:
    return TuiDriverError(
        f"TUI Driver returned {response.get('type')}; expected {expected}",
        "unexpected_response",
    )


def _positive_timeout(value: float, name: str) -> float:
    if isinstance(value, bool) or not isinstance(value, (int, float)) or value <= 0:
        raise TuiDriverError(f"{name} must be positive", "invalid_timeout")
    return float(value)


def _absolute_path(value: str | os.PathLike[str], name: str) -> str:
    path = os.path.abspath(os.fspath(value)) if os.path.isabs(value) else ""
    if not path:
        raise TuiDriverError(f"{name} must be an absolute path", "invalid_path")
    return path


def _validate_unix_socket(path: str) -> None:
    try:
        metadata = os.lstat(path)
    except OSError as error:
        raise TuiDriverError(
            f"cannot inspect Driver socket {path}: {error}", "socket_unavailable"
        ) from error
    if not stat.S_ISSOCK(metadata.st_mode):
        raise TuiDriverError(
            f"Driver path is not a Unix socket: {path}", "invalid_socket"
        )
    if stat.S_IMODE(metadata.st_mode) & 0o077:
        raise TuiDriverError(
            f"Driver socket must not grant group or world access: {path}",
            "insecure_socket",
        )
    if hasattr(os, "geteuid") and metadata.st_uid != os.geteuid():
        raise TuiDriverError(
            f"Driver socket is owned by another user: {path}", "insecure_socket"
        )


def _read_stderr(
    stream: BinaryIO, callback: Callable[[str], None] | None
) -> None:
    try:
        while True:
            chunk = stream.readline()
            if not chunk:
                return
            if callback is not None:
                callback(chunk.decode("utf-8", errors="replace"))
    finally:
        stream.close()


__all__ = [
    "TUI_DRIVER_PROTOCOL_VERSION",
    "TuiDriverClient",
    "TuiDriverDisconnectedError",
    "TuiDriverError",
]
