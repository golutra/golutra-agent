"""Bounded background probe used to test terminal-session lifecycle semantics."""

from __future__ import annotations

import os
import time
from pathlib import Path


PROBE_TIMEOUT_SECONDS = 300.0


def main() -> int:
    state = Path(".long-bench")
    state.mkdir(parents=True, exist_ok=True)
    (state / "probe.pid").write_text(f"{os.getpid()}\n", encoding="utf-8")
    (state / "probe.started_ns").write_text(f"{time.time_ns()}\n", encoding="utf-8")
    # 外层进程期限由基准 harness 管理；这里的宽松保护只避免异常 agent 永久阻塞，且可为慢速 CI 调整。
    try:
        timeout_seconds = float(
            os.environ.get("GOLUTRA_LONG_BENCH_PROBE_TIMEOUT", PROBE_TIMEOUT_SECONDS)
        )
    except ValueError:
        timeout_seconds = PROBE_TIMEOUT_SECONDS
    deadline = time.monotonic() + max(1.0, timeout_seconds)
    release = state / "probe.release"
    while not release.exists() and time.monotonic() < deadline:
        time.sleep(0.05)
    if not release.exists():
        (state / "probe.timeout").write_text("timeout\n", encoding="utf-8")
        return 3
    (state / "probe.done_ns").write_text(f"{time.time_ns()}\n", encoding="utf-8")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
