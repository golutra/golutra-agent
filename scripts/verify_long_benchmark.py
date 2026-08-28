#!/usr/bin/env python3
"""Externally verify cumulative stages of the three-way long-task fixture."""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
from pathlib import Path
from typing import Callable


SENTINEL = "LEDGER-LONG-73X"


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--workspace", type=Path, required=True)
    parser.add_argument("--stage", type=int, choices=range(1, 5), required=True)
    return parser.parse_args()


def run_python(workspace: Path, code: str) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        [sys.executable, "-c", code],
        cwd=workspace,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        timeout=30,
        check=False,
    )


def require_python(workspace: Path, code: str, label: str) -> None:
    result = run_python(workspace, code)
    if result.returncode != 0:
        raise AssertionError(f"{label} failed:\n{result.stdout[-4000:]}")


def verify_visible_tests(workspace: Path) -> None:
    result = subprocess.run(
        [sys.executable, "-m", "unittest", "discover", "-s", "tests", "-v"],
        cwd=workspace,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        timeout=45,
        check=False,
    )
    if result.returncode != 0:
        raise AssertionError(f"visible tests failed:\n{result.stdout[-4000:]}")


def verify_stage_one(workspace: Path) -> None:
    verify_visible_tests(workspace)
    require_python(
        workspace,
        r'''
import json
import subprocess
import sys
import tempfile
from pathlib import Path

from jobledger.codec import decode_event, encode_event
from jobledger.ledger import JobLedger
from jobledger.model import JobEvent

def event(event_id, job_id, state, sequence, metadata=None):
    return JobEvent.from_mapping({
        "event_id": event_id,
        "job_id": job_id,
        "state": state,
        "sequence": sequence,
        "metadata": metadata or {},
    })

for invalid in (
    {"event_id": "", "job_id": "j", "state": "queued", "sequence": 0},
    {"event_id": "e", "job_id": "", "state": "queued", "sequence": 0},
    {"event_id": "e", "job_id": "j", "state": "unknown", "sequence": 0},
    {"event_id": "e", "job_id": "j", "state": "queued", "sequence": True},
    {"event_id": "e", "job_id": "j", "state": "queued", "sequence": -1},
):
    try:
        JobEvent.from_mapping(invalid)
    except (TypeError, ValueError):
        pass
    else:
        raise AssertionError(f"accepted invalid event: {invalid}")

a = event("e-a", "j-1", "queued", 3, {"z": 1, "a": [2]})
b = event("e-b", "j-1", "running", 3)
ledger = JobLedger([b, a])
assert [value.event_id for value in ledger.events()] == ["e-a", "e-b"]
assert ledger.latest("missing") is None
try:
    ledger.append(event("e-a", "j-1", "failed", 4))
except ValueError:
    pass
else:
    raise AssertionError("conflicting duplicate was accepted")

with tempfile.TemporaryDirectory() as directory:
    path = Path(directory) / "ledger.ndjson"
    payload = json.dumps(a.to_mapping(), separators=(",", ":"))
    append = subprocess.run(
        [sys.executable, "-m", "jobledger.cli", "append", str(path), payload],
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        check=False,
    )
    assert append.returncode == 0, append.stdout
    summary = subprocess.run(
        [sys.executable, "-m", "jobledger.cli", "summary", str(path)],
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        check=False,
    )
    assert summary.returncode == 0, summary.stdout
    summary_payload = json.loads(summary.stdout)
    if "state_counts" in summary_payload:
        summary_payload = summary_payload["state_counts"]
    assert summary_payload == {"queued": 1}
''',
        "stage one behavior",
    )


def verify_stage_two(workspace: Path) -> None:
    require_python(
        workspace,
        r'''
import tempfile
from pathlib import Path

from jobledger.codec import encode_event
from jobledger.ledger import JobLedger
from jobledger.model import JobEvent

def event(event_id, state="queued", sequence=0, metadata=None):
    return JobEvent.from_mapping({
        "event_id": event_id,
        "job_id": "job-1",
        "state": state,
        "sequence": sequence,
        "metadata": metadata or {},
    })

source = {"nested": {"values": [1]}}
copied = event("copy", metadata=source)
source["nested"]["values"].append(2)
assert copied.metadata == {"nested": {"values": [1]}}
projected = copied.to_mapping()
projected["metadata"]["nested"]["values"].append(3)
assert copied.metadata == {"nested": {"values": [1]}}

with tempfile.TemporaryDirectory() as directory:
    root = Path(directory)
    valid = encode_event(event("valid"))
    recoverable = root / "recoverable.ndjson"
    recoverable.write_text(valid + "\n" + '{"event_id":', encoding="utf-8")
    assert [item.event_id for item in JobLedger.load_recovering(recoverable).events()] == ["valid"]

    final_newline = root / "final-newline.ndjson"
    final_newline.write_text(valid + "\n" + '{"event_id":\n', encoding="utf-8")
    try:
        JobLedger.load_recovering(final_newline)
    except (TypeError, ValueError):
        pass
    else:
        raise AssertionError("malformed newline-terminated record was ignored")

    middle = root / "middle.ndjson"
    middle.write_text(valid + "\nnot-json\n" + valid + "\n", encoding="utf-8")
    try:
        JobLedger.load_recovering(middle)
    except (TypeError, ValueError):
        pass
    else:
        raise AssertionError("malformed middle record was ignored")

left = JobLedger([event("shared", "queued", 1), event("left", "running", 2)])
right = JobLedger([event("new", "succeeded", 3), event("shared", "failed", 4)])
before = left.events()
try:
    left.merge(right)
except ValueError:
    pass
else:
    raise AssertionError("conflicting merge was accepted")
assert left.events() == before, "merge was not transactional"

added = left.merge(JobLedger([event("new", "succeeded", 3)]))
assert added == 1
assert left.merge(JobLedger([event("new", "succeeded", 3)])) == 0
''',
        "stage two recovery and merge",
    )


def verify_stage_three(workspace: Path) -> None:
    checkpoint_path = workspace / ".long-bench" / "checkpoint.json"
    if not checkpoint_path.is_file():
        raise AssertionError("missing .long-bench/checkpoint.json")
    require_python(
        workspace,
        rf'''
import json
from dataclasses import replace
from pathlib import Path

from jobledger.checkpoint import (
    Checkpoint,
    create_checkpoint,
    decode_checkpoint,
    encode_checkpoint,
)
from jobledger.ledger import JobLedger
from jobledger.model import JobEvent

def event(event_id, state, sequence):
    return JobEvent.from_mapping({{
        "event_id": event_id,
        "job_id": "job-1",
        "state": state,
        "sequence": sequence,
        "metadata": {{}},
    }})

ledger = JobLedger([
    event("e-1", "queued", 1),
    event("e-4", "running", 4),
])
checkpoint = create_checkpoint(ledger, {SENTINEL!r})
assert checkpoint.version == 1
assert checkpoint.through_sequence == 4
assert checkpoint.state_counts == {{"queued": 1, "running": 1}}
assert checkpoint.sentinel == {SENTINEL!r}
assert len(checkpoint.checksum) == 64
encoded = encode_checkpoint(checkpoint)
assert json.dumps(json.loads(encoded), sort_keys=True, separators=(",", ":")) == encoded
assert decode_checkpoint(encoded) == checkpoint

payload = json.loads(encoded)
payload["state_counts"]["failed"] = 99
try:
    decode_checkpoint(json.dumps(payload))
except ValueError:
    pass
else:
    raise AssertionError("tampered checkpoint was accepted")

saved = decode_checkpoint(Path(".long-bench/checkpoint.json").read_text(encoding="utf-8"))
assert saved.sentinel == {SENTINEL!r}
''',
        "stage three checkpoint",
    )


def process_exists(process_id: int) -> bool:
    try:
        os.kill(process_id, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        return True
    return True


def verify_stage_four(workspace: Path) -> None:
    state = workspace / ".long-bench"
    required = ["probe.pid", "probe.started_ns", "probe.release", "probe.done_ns"]
    missing = [name for name in required if not (state / name).is_file()]
    if missing:
        raise AssertionError(f"background probe files missing: {', '.join(missing)}")
    if (state / "probe.timeout").exists():
        raise AssertionError("background probe timed out")
    process_id = int((state / "probe.pid").read_text(encoding="utf-8").strip())
    if process_exists(process_id):
        raise AssertionError(f"background probe process {process_id} is still alive")
    started_ns = int((state / "probe.started_ns").read_text(encoding="utf-8").strip())
    done_ns = int((state / "probe.done_ns").read_text(encoding="utf-8").strip())
    checkpoint_mtime = (workspace / "jobledger" / "checkpoint.py").stat().st_mtime_ns
    if not started_ns <= checkpoint_mtime <= done_ns:
        raise AssertionError("checkpoint.py was not modified while the probe was running")
    require_python(
        workspace,
        r'''
from jobledger.checkpoint import create_checkpoint, restore_counts
from jobledger.ledger import JobLedger
from jobledger.model import JobEvent

def event(event_id, state, sequence):
    return JobEvent.from_mapping({
        "event_id": event_id,
        "job_id": "job-1",
        "state": state,
        "sequence": sequence,
        "metadata": {},
    })

checkpoint = create_checkpoint(
    JobLedger([event("e-1", "queued", 1), event("e-4", "running", 4)]),
    "LEDGER-LONG-73X",
)
counts = restore_counts(
    checkpoint,
    [event("e-5", "succeeded", 5), event("e-6", "failed", 6)],
)
assert counts == {"queued": 1, "running": 1, "succeeded": 1, "failed": 1}
try:
    restore_counts(checkpoint, [event("old", "failed", 4)])
except ValueError:
    pass
else:
    raise AssertionError("restore accepted an event at or before the checkpoint")
''',
        "stage four restore",
    )
    verify_visible_tests(workspace)


STAGES: tuple[Callable[[Path], None], ...] = (
    verify_stage_one,
    verify_stage_two,
    verify_stage_three,
    verify_stage_four,
)


def main() -> int:
    args = parse_args()
    workspace = args.workspace.resolve(strict=True)
    checks: list[str] = []
    try:
        for index, verify in enumerate(STAGES[: args.stage], start=1):
            verify(workspace)
            checks.append(f"stage_{index}")
    except (AssertionError, OSError, subprocess.SubprocessError, ValueError) as error:
        print(
            json.dumps(
                {"passed": False, "checks": checks, "error": str(error)},
                ensure_ascii=True,
            )
        )
        return 1
    print(json.dumps({"passed": True, "checks": checks}, ensure_ascii=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
