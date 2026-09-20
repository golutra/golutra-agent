"""在释放时保存实现摘要，供宿主验收阶段顺序；不参与代理的实现。"""

from hashlib import sha256
import json
import os
from pathlib import Path
import time


def main():
    state = Path(".observer")
    state.mkdir(exist_ok=True)
    (state / "started.json").write_text(json.dumps({
        "pid": os.getpid(), "started_ns": time.time_ns(),
    }))
    deadline = time.monotonic() + 240
    while not (state / "release").exists():
        if time.monotonic() >= deadline:
            return 3
        time.sleep(0.05)
    source = Path("inventory.py")
    (state / "snapshot.json").write_text(json.dumps({
        "sha256": sha256(source.read_bytes()).hexdigest(),
        "mtime_ns": source.stat().st_mtime_ns,
        "completed_ns": time.time_ns(),
    }))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
