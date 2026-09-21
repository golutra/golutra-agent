"""用本地 SSE 驱动真实 TUI，通过 PTY 屏幕观察延迟，不调用真实模型。"""

import http.server
import json
import os
import re
import select
import socket
import statistics
import sys
import tempfile
import threading
import time
from pathlib import Path

from terminal import ROOT, WORKSPACE, Terminal


class ObservedTerminal(Terminal):
    def __init__(self, *args):
        super().__init__(*args)
        self.seen = {}
        self.progress = []
        self.raw = bytearray()
        self.synchronized = False
        self.control_tail = b""
        self.observer_times = []

    def read(self, timeout=0.01):
        if not select.select([self.master], [], [], timeout)[0]:
            return
        try:
            data = os.read(self.master, 262144)
        except OSError:
            return
        self.raw.extend(data)
        self.bytes_read += len(data)
        observed_at = time.monotonic()
        self.stream.feed(self.decoder.decode(data))
        self.observer_times.append((time.monotonic() - observed_at) * 1000)
        control = self.control_tail + data
        if b"\x1b[6n" in control:
            os.write(
                self.master,
                f"\x1b[{self.screen.cursor.y + 1};{self.screen.cursor.x + 1}R".encode(),
            )
        for match in re.finditer(rb"\x1b\[\?2026([hl])", control):
            self.synchronized = match[1] == b"h"
        self.control_tail = control[-7:]
        if self.synchronized:
            return
        now = time.monotonic()
        found = [int(i) for i in re.findall(r"Z(\d{4})X", "".join(self.text().split()))]
        new = [i for i in found if i not in self.seen]
        if new:
            self.progress.append((now, max(new), len(new)))
            for i in new:
                self.seen[i] = now


def chunks_for(scenario, count):
    chunks = []
    for i in range(count):
        if scenario == "code":
            text = (
                "```python\n" if i == 0 else ""
            ) + f'value_{i} = "Z{i:04d}X streamed output keeps its order"\n'
        else:
            text = f"Z{i:04d}X 这是固定节奏的流式显示测试，用来观察正文增长后是否出现延迟。 "
            if scenario != "longline" and i % 4 == 3:
                text += "\n\n"
        chunks.append(text)
    if scenario == "code":
        chunks[-1] += "```\n"
    return chunks


def stat(values):
    values = sorted(values)
    if not values:
        return None
    return {
        "p50": round(statistics.median(values), 2),
        "p95": round(values[max(0, int(len(values) * 0.95) - 1)], 2),
        "max": round(max(values), 2),
    }


def run(engine, scenario, count=400):
    if engine not in ("debug", "codex") or scenario not in (
        "paragraphs",
        "code",
        "longline",
        "bursty",
    ):
        raise ValueError("unknown engine or scenario")
    if not 1 <= count <= 1000:
        raise ValueError("chunks must be between 1 and 1000")
    chunks = chunks_for(scenario, count)
    sent = {}
    requests = []
    finished = []
    failures = []
    history_turns = int(os.environ.get("DIAG_HISTORY", "0"))
    if not 0 <= history_turns <= 100:
        raise ValueError("DIAG_HISTORY must be between 0 and 100")
    warmup_turn = 0

    class Handler(http.server.BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def log_message(self, *args):
            pass

        def do_POST(self):
            body = self.rfile.read(int(self.headers.get("Content-Length", 0)))
            request_id = len(requests)
            requests.append(self.path)
            (home / f"request-{request_id}.json").write_bytes(body)
            is_title = "Generate a concise, single-line task title" in body.decode()
            warming_up = warmup_turn
            response_chunks = ["Diagnostic display"] if is_title else chunks
            if warming_up and not is_title:
                response_chunks = [f"HISTORY_REPLY_{warming_up:03} completed."]
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Connection", "close")
            self.end_headers()
            self.connection.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
            seq = 0

            def event(kind, **payload):
                nonlocal seq
                seq += 1
                obj = dict(type=kind, sequence_number=seq, **payload)
                self.wfile.write(
                    (
                        "event: "
                        + kind
                        + "\ndata: "
                        + json.dumps(obj, ensure_ascii=False)
                        + "\n\n"
                    ).encode()
                )
                self.wfile.flush()

            try:
                item = {
                    "type": "message",
                    "id": "msg_diag",
                    "role": "assistant",
                    "status": "in_progress",
                    "content": [],
                }
                response = {
                    "id": "resp_diag",
                    "object": "response",
                    "model": "test-model",
                    "status": "in_progress",
                    "output": [],
                }
                event("response.created", response=response)
                event("response.output_item.added", output_index=0, item=item)
                event(
                    "response.content_part.added",
                    item_id="msg_diag",
                    output_index=0,
                    content_index=0,
                    part={"type": "output_text", "text": "", "annotations": []},
                )
                started = time.monotonic()
                for i, chunk in enumerate(response_chunks):
                    target = started + (
                        i // 8 * 0.2 if scenario == "bursty" else i * 0.025
                    )
                    time.sleep(max(0, target - time.monotonic()))
                    if not is_title and not warming_up:
                        sent[i] = time.monotonic()
                    event(
                        "response.output_text.delta",
                        item_id="msg_diag",
                        output_index=0,
                        content_index=0,
                        delta=chunk,
                    )
                full = "".join(response_chunks)
                part = {"type": "output_text", "text": full, "annotations": []}
                event(
                    "response.output_text.done",
                    item_id="msg_diag",
                    output_index=0,
                    content_index=0,
                    text=full,
                )
                event(
                    "response.content_part.done",
                    item_id="msg_diag",
                    output_index=0,
                    content_index=0,
                    part=part,
                )
                item.update(status="completed", content=[part])
                event("response.output_item.done", output_index=0, item=item)
                response.update(
                    status="completed",
                    output=[item],
                    usage={
                        "input_tokens": 100,
                        "output_tokens": count * 15,
                        "total_tokens": 100 + count * 15,
                    },
                )
                event("response.completed", response=response)
                if not is_title and not warming_up:
                    finished.append(time.monotonic())
            except (OSError, ValueError) as error:
                failures.append(str(error))
            self.close_connection = True

    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    endpoint = f"http://127.0.0.1:{server.server_port}/v1"
    home = Path(tempfile.mkdtemp(prefix=f"stream-{engine}-{scenario}-", dir=ROOT))
    env = {
        k: v
        for k, v in os.environ.items()
        if not k.startswith(("GOLUTRA_AGENT_", "OPENAI_", "ANTHROPIC_", "CODEX_"))
    }
    env.update(
        TERM="xterm-256color",
        NO_COLOR="",
        NO_PROXY="127.0.0.1,localhost",
        no_proxy="127.0.0.1,localhost",
    )
    if engine == "codex":
        env["CODEX_HOME"] = str(home)
        (home / "config.toml").write_text(
            'model = "test-model"\nmodel_provider = "local"\n[model_providers.local]\nname = "Local diagnostic"\nbase_url = '
            + json.dumps(endpoint)
            + '\nwire_api = "responses"\nrequires_openai_auth = false\n[projects.'
            + json.dumps(str(WORKSPACE))
            + ']\ntrust_level = "trusted"\n'
        )
        command = [os.environ.get("TUI_BENCH_CODEX", "codex"), "--no-alt-screen"]
    else:
        env.update(GOLUTRA_AGENT_HOME=str(home), DIAGNOSTIC_API_KEY="local-dummy")
        if os.environ.get("DIAG_TIMING") == "1":
            env["GOLUTRA_AGENT_TUI_TIMING"] = str(home / "ui-timing.csv")
        profile = {
            "name": "local",
            "protocol": "openai-responses",
            "model_id": "test-model",
            "base_url": endpoint,
            "enabled": True,
            "credential_ref": {
                "id": "cred_diagnostic",
                "revision": "rev_diagnostic",
                "source": {"kind": "environment", "key": "DIAGNOSTIC_API_KEY"},
                "secret_kind": "api-key",
            },
        }
        (home / "provider.json").write_text(
            json.dumps({"version": 2, "active_profile": "local", "profiles": [profile]})
        )
        binary = WORKSPACE / "target/debug/golutra-agent-tui"
        if os.environ.get("DIAG_BINARY"):
            binary = Path(os.environ["DIAG_BINARY"])
        command = [str(binary), "--cwd", str(WORKSPACE)]
    terminal = ObservedTerminal(engine, command, env)
    try:
        terminal.pump(3)
        (home / "startup.txt").write_text(terminal.text())
        for turn in range(1, history_turns + 1):
            warmup_turn = turn
            terminal.send(f"History fixture {turn} " + "abcdefghij " * 180)
            terminal.pump(0.05)
            terminal.send("\r")
            terminal.wait(f"HISTORY_REPLY_{turn:03}", timeout=10)
            terminal.pump(0.15)
        warmup_turn = 0
        history_typing = None
        if history_turns:
            history_typing = terminal.measure(f"history_{history_turns}_input")
            terminal.send("\x15")
            terminal.pump(0.1)
        terminal.observer_times.clear()
        terminal.send("Show the diagnostic response.")
        terminal.pump(0.2)
        terminal.send("\r")
        deadline = time.monotonic() + 40
        typing = None
        while time.monotonic() < deadline:
            terminal.read(0.005)
            if (
                os.environ.get("DIAG_TYPING") == "1"
                and typing is None
                and len(sent) >= 200
            ):
                typing = terminal.measure("during_stream", count=30)
            if finished and time.monotonic() - finished[-1] > 3:
                break
        delays = [
            (i, (seen - sent[i]) * 1000)
            for i, seen in terminal.seen.items()
            if i in sent
        ]
        intervals = [
            (b[0] - a[0]) * 1000
            for a, b in zip(terminal.progress, terminal.progress[1:])
        ]
        result = {
            "engine": engine,
            "scenario": scenario,
            "chunks": count,
            "sent": len(sent),
            "seen": len(delays),
            "requests": requests,
            "source_intervals_ms": stat(
                [(sent[i] - sent[i - 1]) * 1000 for i in sorted(sent) if i - 1 in sent]
            ),
            "display_delay_ms": stat([d for _, d in delays]),
            "first_quarter_delay_ms": stat([d for i, d in delays if i < count / 4]),
            "last_quarter_delay_ms": stat([d for i, d in delays if i >= count * 3 / 4]),
            "progress_gap_ms": stat(intervals),
            "max_chunks_per_observation": max(
                (p[2] for p in terminal.progress), default=0
            ),
            "last_marker_delay_ms": round(
                (terminal.seen[count - 1] - sent[count - 1]) * 1000, 2
            )
            if count - 1 in terminal.seen
            else None,
            "failures": failures,
            "home": str(home),
        }
        if typing is not None:
            result["typing"] = typing
        result["observer_feed_ms"] = stat(terminal.observer_times)
        result["history_turns"] = history_turns
        if history_typing is not None:
            result["history_typing"] = history_typing
        (home / "final.txt").write_text(terminal.text())
        (home / "terminal.ansi").write_bytes(terminal.raw)
        (home / "timeline.json").write_text(
            json.dumps(
                {
                    "epoch_minus_monotonic": time.time() - time.monotonic(),
                    "sent": sent,
                    "seen": terminal.seen,
                    "progress": terminal.progress,
                    "finished": finished,
                }
            )
        )
        (home / "result.json").write_text(json.dumps(result, indent=2))
        print(json.dumps(result), flush=True)
        return result
    finally:
        if os.environ.get("DIAG_TIMING") == "1":
            terminal.send("\x03")
            terminal.pump(0.1)
            terminal.send("\x03")
            terminal.pump(0.5)
        terminal.close()
        server.shutdown()
        server.server_close()


if __name__ == "__main__":
    engines = sys.argv[1].split(",") if len(sys.argv) > 1 else ["debug", "codex"]
    scenarios = (
        sys.argv[2].split(",")
        if len(sys.argv) > 2
        else ["paragraphs", "longline", "bursty"]
    )
    results = []
    for scenario in scenarios:
        for engine in engines:
            results.append(
                run(engine, scenario, int(sys.argv[3]) if len(sys.argv) > 3 else 400)
            )
    result_name = "stream-results.json"
    if os.environ.get("DIAG_LABEL"):
        result_name = os.environ["DIAG_LABEL"] + "-" + result_name
    (ROOT / result_name).write_text(json.dumps(results, indent=2))
    if any(
        result["seen"] != result["chunks"] or result["failures"] for result in results
    ):
        raise SystemExit(
            "Incomplete stream or provider fixture failure; inspect the saved artifacts"
        )
