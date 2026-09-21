"""本地 TUI 基准的 POSIX PTY 观察器，不使用真实凭据。"""

import codecs
import fcntl
import json
import os
import pty
import select
import statistics
import struct
import subprocess
import tempfile
import termios
import time
from pathlib import Path

import pyte

ROOT = Path(
    os.environ.get("TUI_BENCH_OUTPUT") or tempfile.mkdtemp(prefix="golutra-tui-bench-")
)
ROOT.mkdir(parents=True, exist_ok=True)
WORKSPACE = Path(__file__).resolve().parents[2]


class Terminal:
    def __init__(self, name, command, env):
        self.name = name
        self.master, slave = pty.openpty()
        os.set_blocking(self.master, False)
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 32, 120, 0, 0))
        self.screen = pyte.Screen(120, 32)
        self.stream = pyte.Stream(self.screen)
        self.decoder = codecs.getincrementaldecoder("utf-8")("replace")
        self.process = subprocess.Popen(
            command,
            stdin=slave,
            stdout=slave,
            stderr=slave,
            env=env,
            cwd=WORKSPACE,
            start_new_session=True,
        )
        os.close(slave)
        self.bytes_read = 0

    def read(self, timeout=0.01):
        if select.select([self.master], [], [], timeout)[0]:
            try:
                data = os.read(self.master, 262144)
            except OSError:
                return
            self.bytes_read += len(data)
            self.stream.feed(self.decoder.decode(data))
            if b"\x1b[6n" in data:
                os.write(
                    self.master,
                    f"\x1b[{self.screen.cursor.y + 1};{self.screen.cursor.x + 1}R".encode(),
                )

    def text(self):
        return "\n".join(self.screen.display)

    def pump(self, seconds):
        end = time.monotonic() + seconds
        while time.monotonic() < end:
            self.read(min(0.01, max(0, end - time.monotonic())))

    def send(self, text):
        data = text.encode() if isinstance(text, str) else text
        deadline = time.monotonic() + 10
        while data:
            try:
                count = os.write(self.master, data)
                data = data[count:]
            except BlockingIOError:
                if time.monotonic() > deadline:
                    raise RuntimeError("PTY send timeout")
                self.read(0.005)

    def wait(self, marker, timeout=10):
        end = time.monotonic() + timeout
        while marker not in self.text():
            if time.monotonic() > end:
                raise RuntimeError(
                    f"{self.name}: waiting for {marker!r}\n{self.text()}"
                )
            self.read()

    def measure(self, label, count=30):
        samples = []
        expected = "Q7zR8kL"
        self.send("\x1b[200~" + expected + "\x1b[201~")
        self.pump(0.15)
        start_bytes = self.bytes_read
        for i in range(count):
            letter = chr(97 + i % 26)
            expected = (expected + letter)[-8:]
            started = time.perf_counter()
            self.send(letter)
            deadline = time.monotonic() + 10
            while expected not in "".join(self.text().split()):
                if time.monotonic() > deadline:
                    raise RuntimeError(f"{self.name}: marker timeout in {label}")
                self.read()
            samples.append((time.perf_counter() - started) * 1000)
            self.pump(0.035)
        result = {
            "engine": self.name,
            "scenario": label,
            "n": count,
            "median_ms": round(statistics.median(samples), 2),
            "p95_ms": round(sorted(samples)[int(count * 0.95) - 1], 2),
            "max_ms": round(max(samples), 2),
            "output_bytes": self.bytes_read - start_bytes,
        }
        print(json.dumps(result), flush=True)
        return result

    def close(self):
        self.process.terminate()
        try:
            self.process.wait(timeout=3)
        except subprocess.TimeoutExpired:
            self.process.kill()
            self.process.wait()
        os.close(self.master)
