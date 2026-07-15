from __future__ import annotations

import json
import sys
import tempfile
import threading
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import urlsplit


SDK_SRC = Path(__file__).resolve().parents[1] / "src"
sys.path.insert(0, str(SDK_SRC))

from golutra_sdk import GolutraClient


TOKEN = "python-sdk-test-token-000000000000000000000000000000000000000000"
SESSION_ID = "01900000-0000-7000-8000-000000000001"
THREAD_ID = "01900000-0000-7000-8000-000000000002"


class RuntimeHandler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    attachment_id = "attachment-1"
    observed_headers: list[dict[str, str]] = []

    def do_GET(self) -> None:
        if not self._authorized():
            return
        path = urlsplit(self.path).path
        if path == "/runtime/info":
            self._json(
                {
                    "instance_id": "server-1",
                    "pid": 1,
                    "base_url": self.server.base_url,
                    "protocol_versions": {"minimum": 1, "current": 1},
                    "started_at": "2026-01-01T00:00:00Z",
                }
            )
        elif path == "/events/replay":
            self._json([self._event()])
        elif path == "/events":
            payload = json.dumps(self._event(), separators=(",", ":"))
            body = f"id: 1\nevent: runtime_event\ndata: {payload}\n\n".encode()
            self.send_response(200)
            self.send_header("content-type", "text/event-stream")
            self.send_header("content-length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
        elif path == "/threads":
            self._json([self._thread()])
        else:
            self._json({"error": "not found"}, 404)

    def do_POST(self) -> None:
        if not self._authorized():
            return
        path = urlsplit(self.path).path
        length = int(self.headers.get("content-length", "0"))
        body = json.loads(self.rfile.read(length) or b"{}")
        if path == "/runtime/attach":
            self._json(
                {
                    "attachment_id": self.attachment_id,
                    "runtime": {
                        "instance_id": "runtime-1",
                        "pid": 1,
                        "base_url": self.server.base_url,
                        "cwd": body["cwd"],
                        "workspace_id": "01900000-0000-7000-8000-000000000003",
                        "default_session_id": SESSION_ID,
                        "default_thread_id": THREAD_ID,
                        "started_at": "2026-01-01T00:00:00Z",
                    },
                }
            )
        elif path == "/commands":
            self._json(
                {
                    "command_id": body["command_id"],
                    "accepted": True,
                    "reason": None,
                    "sequence_no": 1,
                }
            )
        elif path == "/queries":
            self._json({"kind": body["kind"], "session_id": body["session_id"]})
        else:
            self._json({"error": "not found"}, 404)

    def log_message(self, _format: str, *_args) -> None:
        return

    def _authorized(self) -> bool:
        self.observed_headers.append(
            {name.lower(): value for name, value in self.headers.items()}
        )
        valid = (
            self.headers.get("authorization") == f"Bearer {TOKEN}"
            and self.headers.get("x-golutra-protocol-version") == "1"
        )
        if not valid:
            self._json({"error": "unauthorized"}, 401)
        return valid

    def _json(self, value, status: int = 200) -> None:
        body = json.dumps(value, separators=(",", ":")).encode()
        self.send_response(status)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    @staticmethod
    def _event() -> dict:
        return {
            "event_id": "01900000-0000-7000-8000-000000000004",
            "sequence_no": 1,
            "workspace_id": "01900000-0000-7000-8000-000000000003",
            "session_id": SESSION_ID,
            "turn_id": None,
            "task_id": None,
            "parent_event_id": None,
            "event_type": "task_completed",
            "timestamp": "2026-01-01T00:00:00Z",
            "source": "runtime",
            "payload": {},
            "payload_ref": None,
            "durable": True,
        }

    @staticmethod
    def _thread() -> dict:
        return {"thread_id": THREAD_ID, "session_id": SESSION_ID, "title": "fixture"}


class ClientTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.server = ThreadingHTTPServer(("127.0.0.1", 0), RuntimeHandler)
        cls.server.base_url = f"http://127.0.0.1:{cls.server.server_port}"
        cls.thread = threading.Thread(target=cls.server.serve_forever, daemon=True)
        cls.thread.start()

    @classmethod
    def tearDownClass(cls) -> None:
        cls.server.shutdown()
        cls.server.server_close()
        cls.thread.join(timeout=2)

    def setUp(self) -> None:
        self.workspace = tempfile.TemporaryDirectory()
        self.client = GolutraClient(self.server.base_url, self.workspace.name, TOKEN)

    def tearDown(self) -> None:
        self.workspace.cleanup()

    def test_command_query_history_and_sse_share_attachment(self) -> None:
        self.assertEqual(self.client.server_info()["instance_id"], "server-1")
        self.assertEqual(self.client.runtime_info()["default_session_id"], SESSION_ID)
        ack = self.client.prompt(SESSION_ID, "hello")
        self.assertTrue(ack["accepted"])
        self.assertEqual(self.client.storage_status(SESSION_ID)["kind"], "storage_status")
        self.assertEqual(len(self.client.replay_events({"session_id": SESSION_ID})), 1)
        self.assertEqual(len(self.client.list_threads()), 1)
        subscription = self.client.subscribe({"session_id": SESSION_ID})
        self.assertEqual(next(subscription)["event_type"], "task_completed")
        subscription.close()
        self.assertTrue(
            any(
                headers.get("x-golutra-attachment") == RuntimeHandler.attachment_id
                for headers in RuntimeHandler.observed_headers
            )
        )

    def test_rejects_insecure_remote_http(self) -> None:
        with self.assertRaises(ValueError):
            GolutraClient("http://example.com", self.workspace.name, TOKEN)


if __name__ == "__main__":
    unittest.main()
