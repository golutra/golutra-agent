from __future__ import annotations

import json
import sys
import tempfile
import threading
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import urlsplit
from unittest.mock import patch


SDK_SRC = Path(__file__).resolve().parents[1] / "src"
sys.path.insert(0, str(SDK_SRC))

from golutra_sdk import GolutraClient, GolutraError, GolutraHttpError, Thread


TOKEN = "python-sdk-test-token-000000000000000000000000000000000000000000"
SESSION_ID = "01900000-0000-7000-8000-000000000001"
THREAD_ID = "01900000-0000-7000-8000-000000000002"
TASK_ID = "01900000-0000-7000-8000-000000000005"
ARTIFACT_ID = "01900000-0000-7000-8000-000000000006"


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
                    "protocol_versions": {"minimum": 4, "current": 4},
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
            self._json(
                {
                    "kind": body["kind"],
                    "session_id": body["session_id"],
                    "task_id": body.get("task_id"),
                }
            )
        elif path == "/traces":
            self._json(self._trace(body))
        elif path == "/sessions/page":
            self._json(
                {
                    "sessions": [self._session_summary()],
                    "next_cursor": None,
                    "has_more": False,
                }
            )
        elif path == "/sessions/window":
            self._json(
                {
                    "anchor_thread_id": body["anchor_thread_id"],
                    "range": body["range"],
                    "sessions": [self._session_summary()],
                    "reached_boundary": True,
                }
            )
        elif path == "/artifacts/chunk":
            self._json(
                {
                    "artifact_id": body["artifact_id"],
                    "offset": body["offset"],
                    "length": body["length"],
                }
            )
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
            and self.headers.get("x-golutra-actor-id", "").startswith("python-sdk-")
            and self.headers.get("x-golutra-protocol-version") == "4"
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

    @staticmethod
    def _session_summary() -> dict:
        return {
            "thread_id": THREAD_ID,
            "session_id": SESSION_ID,
            "parent_thread_id": None,
            "forked_from_turn_id": None,
            "title": "fixture",
            "preview": "fixture preview",
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z",
            "recency_at": "2026-01-01T00:00:00Z",
        }

    @staticmethod
    def _trace(request: dict) -> dict:
        sequence_no = 1 if request.get("cursor") is None else 2
        has_more = sequence_no == 1
        return {
            "session_id": request["session_id"],
            "task_id": request["task_id"],
            "view": request["view"],
            "events": [
                {
                    "id": f"01900000-0000-7000-8000-{sequence_no:012d}",
                    "sequence_no": sequence_no,
                }
            ],
            "context_snapshots": [],
            "artifacts": [],
            "evidence": [],
            "verification_plan": None,
            "verification": None,
            "post_task_jobs": [],
            "evaluation": {"terminal": True},
            "integrity": {
                "event_count": 2,
                "first_sequence": 1,
                "last_sequence": 2,
                "event_chain_digest": "sha256:stable",
                "unresolved_refs": [],
                "missing_sections": [],
                "retention_losses": [],
                "redacted_fields": ["provider_credentials"],
                "complete": not has_more,
            },
            "next_cursor": sequence_no if has_more else None,
            "has_more": has_more,
        }


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
        self.assertEqual(
            self.client.context_projection(SESSION_ID, TASK_ID)["task_id"], TASK_ID
        )
        self.assertEqual(
            self.client.evaluation_projection(SESSION_ID, TASK_ID)["task_id"], TASK_ID
        )
        self.assertTrue(
            self.client.task_trace(
                {
                    "session_id": SESSION_ID,
                    "task_id": TASK_ID,
                    "view": "full",
                    "cursor": None,
                    "limit": 128,
                    "wait_for_evaluation": True,
                }
            )["evaluation"]["terminal"]
        )
        complete_trace = self.client.complete_task_trace(
            {
                "session_id": SESSION_ID,
                "task_id": TASK_ID,
                "view": "full",
                "cursor": None,
                "limit": 128,
                "wait_for_evaluation": True,
            }
        )
        self.assertEqual(len(complete_trace["events"]), 2)
        self.assertFalse(complete_trace["has_more"])
        self.assertTrue(complete_trace["integrity"]["complete"])
        self.assertEqual(
            self.client.read_artifact_chunk(
                {"artifact_id": ARTIFACT_ID, "offset": 0, "length": 64}
            )["artifact_id"],
            ARTIFACT_ID,
        )
        self.assertEqual(len(self.client.replay_events({"session_id": SESSION_ID})), 1)
        self.assertEqual(len(self.client.list_threads()), 1)
        self.assertEqual(
            self.client.session_page({"cursor": None, "limit": 20})["sessions"][0][
                "thread_id"
            ],
            THREAD_ID,
        )
        self.assertEqual(
            self.client.session_window(
                {
                    "anchor_thread_id": THREAD_ID,
                    "range": {"direction": "single", "count": 1},
                }
            )["anchor_thread_id"],
            THREAD_ID,
        )
        subscription = self.client.subscribe({"session_id": SESSION_ID})
        self.assertEqual(next(subscription)["event_type"], "task_completed")
        subscription.close()
        self.assertTrue(
            any(
                headers.get("x-golutra-attachment") == RuntimeHandler.attachment_id
                for headers in RuntimeHandler.observed_headers
            )
        )
        self.assertEqual(
            len(
                {
                    headers.get("x-golutra-actor-id")
                    for headers in RuntimeHandler.observed_headers
                }
            ),
            1,
        )

    def test_rejects_insecure_remote_http(self) -> None:
        with self.assertRaises(ValueError):
            GolutraClient("http://example.com", self.workspace.name, TOKEN)

    def test_persistent_gone_response_retries_once_then_fails(self) -> None:
        class GoneResponse:
            status = 410

            def __init__(self) -> None:
                self._read = False

            def __enter__(self):
                return self

            def __exit__(self, _type, _value, _traceback) -> None:
                return None

            def read(self, _size: int) -> bytes:
                if self._read:
                    return b""
                self._read = True
                return b'{"error":"stale attachment"}'

        self.client._attachment = {"attachment_id": "stale"}
        with patch.object(self.client, "_request", side_effect=["first", "second"]) as request:
            with patch.object(
                self.client,
                "_open",
                side_effect=[GoneResponse(), GoneResponse()],
            ):
                with patch.object(self.client, "_clear_attachment") as clear:
                    with self.assertRaisesRegex(GolutraError, "HTTP 410"):
                        self.client._request_json("GET", "/events")
        self.assertEqual(request.call_count, 2)
        clear.assert_called_once_with()

    def test_agent_subscription_does_not_retry_permanent_http_errors(self) -> None:
        class UnauthorizedResponse:
            status = 401

            def __init__(self) -> None:
                self._read = False

            def __enter__(self):
                return self

            def __exit__(self, _type, _value, _traceback) -> None:
                return None

            def read(self, _size: int) -> bytes:
                if self._read:
                    return b""
                self._read = True
                return b'{"error":"unauthorized"}'

        self.client._attachment = {"attachment_id": "attachment-1"}
        with patch.object(self.client, "_request", return_value="request") as request:
            with patch.object(self.client, "_open", return_value=UnauthorizedResponse()) as opened:
                stream = self.client.subscribe_agent(
                    SESSION_ID,
                    THREAD_ID,
                    "command-1",
                    10,
                    start_cursor=10,
                )
                with self.assertRaises(GolutraHttpError) as raised:
                    next(stream)
        self.assertEqual(raised.exception.status, 401)
        self.assertFalse(raised.exception.retryable)
        self.assertEqual(request.call_count, 1)
        self.assertEqual(opened.call_count, 1)

    def test_agent_subscription_reconnects_from_the_last_consumed_cursor(self) -> None:
        class SseResponse:
            status = 200

            def __init__(self, event: dict) -> None:
                payload = json.dumps(event, separators=(",", ":"))
                self.lines = iter(
                    [
                        b"event: agent_event\n",
                        f"data: {payload}\n".encode(),
                        b"\n",
                    ]
                )

            def __enter__(self):
                return self

            def __exit__(self, _type, _value, _traceback) -> None:
                return None

            def __iter__(self):
                return self.lines

        first = SseResponse({"type": "runtime.event", "event": {"sequence_no": 11}})
        second = SseResponse(
            {
                "type": "turn.completed",
                "status": "completed",
                "task_id": TASK_ID,
                "turn_id": "turn-1",
                "final_message": "done",
                "last_sequence_no": 12,
            }
        )
        self.client._attachment = {"attachment_id": "attachment-1"}
        with patch.object(self.client, "_request", side_effect=["first", "second"]) as request:
            with patch.object(self.client, "_open", side_effect=[first, second]):
                stream = self.client.subscribe_agent(
                    SESSION_ID,
                    THREAD_ID,
                    "command-1",
                    10,
                    start_cursor=10,
                )
                self.assertEqual(next(stream)["type"], "runtime.event")
                self.assertEqual(next(stream)["type"], "turn.completed")
                with self.assertRaises(StopIteration):
                    next(stream)
        self.assertEqual(request.call_count, 2)
        self.assertEqual(request.call_args_list[0].kwargs["last_event_id"], 10)
        self.assertEqual(request.call_args_list[1].kwargs["last_event_id"], 11)
        self.assertIn("start_cursor=10", request.call_args_list[1].args[1])
        self.assertIn("cursor=11", request.call_args_list[1].args[1])

    def test_high_level_thread_and_turn_handle_preserve_lifecycle_contract(self) -> None:
        calls: list[tuple[str, dict]] = []

        class FakeAgentClient:
            def rpc(self, method: str, params: dict) -> dict:
                calls.append((method, params))
                if method == "turn/start":
                    return {
                        "accepted": True,
                        "command_id": "command-1",
                        "cursor": 10,
                        "thread": {
                            "thread_id": THREAD_ID,
                            "session_id": SESSION_ID,
                            "workspace_root": self_workspace,
                        },
                    }
                return {"ack": {"accepted": True}}

            def event_page(self, request: dict) -> dict:
                calls.append(("event_page", request))
                return {
                    "direction": request["direction"],
                    "events": [],
                    "has_more": False,
                }

            def replay_events(self, request: dict) -> list[dict]:
                calls.append(("replay_events", request))
                return []

            def subscribe_agent(
                self,
                session_id: str,
                thread_id: str,
                command_id: str,
                cursor: int | None = None,
                *,
                start_cursor: int | None = None,
                stop_event=None,
            ):
                self_subscription = (
                    session_id,
                    thread_id,
                    command_id,
                    cursor,
                    start_cursor,
                    stop_event,
                )
                calls.append(("subscribe_agent", {"request": self_subscription}))
                return iter(
                    [
                        {"type": "thread.started"},
                        {"type": "turn.started", "turn_id": "turn-1"},
                        {
                            "type": "turn.completed",
                            "status": "completed",
                            "task_id": TASK_ID,
                            "turn_id": "turn-1",
                            "final_message": "done",
                            "last_sequence_no": 12,
                        },
                    ]
                )

        self_workspace = self.workspace.name
        fake = FakeAgentClient()
        thread = Thread(
            fake,
            {
                "thread_id": THREAD_ID,
                "session_id": SESSION_ID,
                "workspace_root": self_workspace,
            },
        )
        with self.assertRaises(ValueError):
            thread.run("   ")

        handle = thread.run(
            "inspect the workspace",
            output_schema={"type": "object"},
            completion_criteria=[" verified ", ""],
        )
        events = []
        stream = handle.events()
        while True:
            event = next(stream)
            events.append(event)
            if event["type"] == "turn.completed":
                break
        stream.close()
        self.assertEqual(events[-1]["type"], "turn.completed")
        self.assertEqual(handle.wait()["status"], "completed")
        self.assertEqual(handle.wait()["final_message"], "done")
        self.assertEqual(calls[0][0], "turn/start")
        self.assertEqual(calls[0][1]["completion_criteria"], [" verified "])
        self.assertEqual(calls[0][1]["output_schema"], {"type": "object"})
        self.assertEqual(calls[1][0], "subscribe_agent")
        self.assertEqual(calls[1][1]["request"][2], "command-1")
        self.assertEqual(calls[1][1]["request"][4], 10)
        self.assertEqual(
            sum(1 for name, _params in calls if name == "subscribe_agent"),
            1,
        )

        self.assertEqual(thread.steer("continue")["accepted"], True)
        self.assertEqual(thread.interrupt()["accepted"], True)
        self.assertEqual(thread.takeover()["accepted"], True)
        self.assertEqual(handle.resolve_approval("approval-1", False)["accepted"], True)
        self.assertEqual(
            thread.history(cursor=9, direction="forward", limit=25)["direction"],
            "forward",
        )
        self.assertEqual(thread.replay_events(cursor=9), [])
        event_page_call = next(params for name, params in calls if name == "event_page")
        self.assertEqual(
            event_page_call,
            {
                "session_id": SESSION_ID,
                "cursor": 9,
                "direction": "forward",
                "limit": 25,
            },
        )
        with self.assertRaises(ValueError):
            thread.event_page(limit=0)
        with self.assertRaises(ValueError):
            thread.event_page(direction="sideways")
        with self.assertRaises(ValueError):
            thread.steer("   ")

    def test_client_start_and_resume_wrap_rpc_thread_references(self) -> None:
        reference = {
            "thread_id": THREAD_ID,
            "session_id": SESSION_ID,
            "workspace_root": self.workspace.name,
        }
        with patch.object(
            self.client,
            "rpc",
            side_effect=[{"thread": reference}, {"thread": reference}],
        ) as rpc:
            started = self.client.start_thread()
            resumed = self.client.resume(THREAD_ID)
        self.assertIsInstance(started, Thread)
        self.assertEqual(started.thread_id, THREAD_ID)
        self.assertEqual(resumed.session_id, SESSION_ID)
        self.assertEqual([call.args[0] for call in rpc.call_args_list], ["thread/start", "thread/resume"])


if __name__ == "__main__":
    unittest.main()
