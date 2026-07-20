from __future__ import annotations

import json
import os
import socket
import sys
import tempfile
import threading
import unittest
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path


SDK_SRC = Path(__file__).resolve().parents[1] / "src"
sys.path.insert(0, str(SDK_SRC))

from golutra_sdk import (
    TuiDriverClient,
    TuiDriverDisconnectedError,
    TuiDriverError,
)


FAKE_DRIVER = Path(__file__).with_name("fake_tui_driver.py")


class TuiDriverClientTest(unittest.TestCase):
    def spawn_client(self) -> TuiDriverClient:
        return TuiDriverClient.spawn_command(
            [sys.executable, FAKE_DRIVER],
            request_timeout=0.5,
            startup_timeout=0.5,
        )

    def test_stdio_routes_concurrent_waits_notifications_and_frozen_pages(self) -> None:
        notifications: list[dict] = []
        client = TuiDriverClient.spawn_command(
            [sys.executable, FAKE_DRIVER],
            request_timeout=0.5,
            startup_timeout=0.5,
            on_notification=notifications.append,
        )
        self.assertEqual(client.ready["instance_id"], "fake-driver")
        self.assertEqual(client.capabilities(), ["fake"])
        with ThreadPoolExecutor(max_workers=2) as executor:
            slow = executor.submit(client.wait, {"kind": "idle"}, 200)
            fast = executor.submit(client.wait, {"kind": "task_terminal"}, 200)
            self.assertEqual(fast.result()["condition"]["kind"], "task_terminal")
            self.assertEqual(slow.result()["condition"]["kind"], "idle")
        self.assertEqual(len(notifications), 2)

        frame = client.complete_snapshot({"width": 80, "height": 24})
        self.assertEqual(frame["frame_id"], "sha256:fake")
        self.assertEqual(
            [line["text"] for line in frame["lines"]],
            ["one", "two", "three", "four"],
        )
        self.assertEqual(frame["returned_range"], {"start": 1, "end": 4})
        self.assertIsNone(frame["next_range"])
        client.close()

    def test_timeout_and_disconnect_reject_pending_work(self) -> None:
        client = self.spawn_client()
        with self.assertRaisesRegex(TuiDriverError, "timed out") as timeout:
            client.request(
                {
                    "type": "wait",
                    "until": {"kind": "event", "event_type": "never"},
                    "timeout_ms": 500,
                },
                timeout=0.02,
            )
        self.assertEqual(timeout.exception.code, "request_timeout")

        result: list[BaseException] = []

        def pending_wait() -> None:
            try:
                client.request(
                    {
                        "type": "wait",
                        "until": {"kind": "event", "event_type": "never"},
                        "timeout_ms": 500,
                    },
                    timeout=0.5,
                )
            except BaseException as error:
                result.append(error)

        thread = threading.Thread(target=pending_wait)
        thread.start()
        with self.assertRaises(TuiDriverDisconnectedError):
            client.prompt("disconnect")
        thread.join(timeout=1)
        self.assertFalse(thread.is_alive())
        self.assertIsInstance(result[0], TuiDriverDisconnectedError)
        self.assertFalse(client.connected)

    @unittest.skipIf(os.name == "nt", "Unix sockets are unavailable on Windows")
    def test_unix_socket_reconnect_is_explicit_and_does_not_replay(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            socket_path = str(Path(directory) / "driver.sock")
            server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            server.bind(socket_path)
            os.chmod(socket_path, 0o600)
            server.listen()
            stop = threading.Event()
            prompt_count = [0]

            def serve() -> None:
                while not stop.is_set():
                    try:
                        connection, _ = server.accept()
                    except OSError:
                        return
                    with connection:
                        connection.sendall(_line(_ready_envelope()))
                        with connection.makefile("rb") as reader:
                            for line in reader:
                                request = json.loads(line)
                                if request["type"] == "input_prompt":
                                    prompt_count[0] += 1
                                    break
                                if request["type"] == "ping":
                                    connection.sendall(
                                        _line(
                                            {
                                                "request_id": request["request_id"],
                                                "type": "pong",
                                            }
                                        )
                                    )
                                if request["type"] == "close":
                                    connection.sendall(
                                        _line(
                                            {
                                                "request_id": request["request_id"],
                                                "type": "closed",
                                            }
                                        )
                                    )
                                    break

            server_thread = threading.Thread(target=serve, daemon=True)
            server_thread.start()
            client = TuiDriverClient.connect_socket(
                socket_path, request_timeout=0.3, startup_timeout=0.3
            )
            with self.assertRaises(TuiDriverDisconnectedError):
                client.prompt("drop")
            self.assertEqual(prompt_count[0], 1)
            client.reconnect()
            self.assertEqual(prompt_count[0], 1)
            client.ping()
            client.close()
            stop.set()
            server.close()
            server_thread.join(timeout=1)


def _ready_envelope() -> dict:
    return {
        "request_id": "ready",
        "type": "ready",
        "protocol_version": 1,
        "minimum_protocol_version": 1,
        "instance_id": "socket-driver",
        "workspace_id": "fake-workspace",
        "workspace_path": os.getcwd(),
        "thread_id": "fake-thread",
        "session_id": "fake-session",
        "controller_mode": "controller",
    }


def _line(value: dict) -> bytes:
    return json.dumps(value, separators=(",", ":")).encode() + b"\n"


if __name__ == "__main__":
    unittest.main()
