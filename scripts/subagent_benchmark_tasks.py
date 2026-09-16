"""冻结业务夹具和只读验收，供两个产品执行相同的真实编码工作。"""
from __future__ import annotations

import hashlib
import subprocess
from pathlib import Path


CODING = """Use exactly two real child agents concurrently to implement this workspace.
Both children have independent context and inherit your model and reasoning.
A owns labels.py: implement normalize_labels(values). Require a list of strings;
otherwise raise TypeError. Strip whitespace, lowercase, discard empty labels,
deduplicate preserving first-seen order. Do not mutate the input.
B owns chunks.py: implement chunks(values, size). Require values to be a list and
size a positive integer excluding bool; wrong types raise TypeError and nonpositive
sizes raise ValueError. Return new list slices of size, last may be smaller;
empty input returns []. Do not mutate input and do not alias its outer list.
Each child must read its assigned file and test its own module using the existing
test_work.py tests. Do not edit test_work.py or each other's files, and do not
delegate further. Wait for their real results and then run
python3 -m unittest -v test_work in the parent workspace. Fix actual failures if
needed, retaining truthful evidence. Finish only after the actual tests pass.
"""

CANCEL = """Exercise actual child cancellation and recovery with exactly one child.
Spawn one independent background child with your model and reasoning. Assign it:
run `python3 slow_task.py` as a foreground shell command, wait for its completion,
and report the actual outcome. Do not delegate or alter any workspace files.
In the parent, wait until ready.json exists (bounded filesystem polling is fine).
It proves the child's real process has started. Then interrupt/cancel that child,
observe its terminal state, and continue the SAME child identity with a new task:
read ready.json and report its recovery_marker. This continuation must produce an
actual new child answer. Observe that answer and report the token and the original
cancellation honestly. Do not create replacement children. Do not create or edit
ready.json, finished.txt, or slow_task.py yourself.
"""

CANCEL_HINTS = {
    "golutra": "Use subagent spawn run_in_background=true, context=independent; cancel the returned child_session_id, wait for terminal state, then resume that same child_session_id.",
    "codex": "Use spawn_agent fork_context=false; close_agent interrupts the child. Observe its state, use resume_agent for that same agent_id if closed, and send_input to assign its next task.",
    "codex-v2": "Use spawn_agent task_name=a, fork_turns=none; interrupt_agent target=a interrupts it. Use followup_task target=a to run its next task, and wait_agent for notifications.",
}

SLOW_TASK = '''import json, os, time
from pathlib import Path
Path("ready.json").write_text(json.dumps({"pid":os.getpid(), "recovery_marker":"RECOVERED_91"}))
time.sleep(90)
Path("finished.txt").write_text("finished")
'''

FANOUT = """Use exactly ten real independent child agents concurrently, named c0
through c9 where names are supported. Child N must read only part-N.txt and return
its exact contents. Inherit your model and reasoning; do not override child roles.
Do not modify files or delegate recursively. Launch the independent work together,
observe every child's terminal answer, continuing bounded waits when needed.
Never spawn replacements. Report the ten actual findings and any failures honestly.
"""


def fanout_marker(index: int) -> str:
    return f"FACT_{index}_{hashlib.sha256(f'fixture-child-{index}'.encode()).hexdigest()[:12]}"

TESTS = '''import unittest
from labels import normalize_labels
from chunks import chunks

class TestLabels(unittest.TestCase):
    def test_values(self):
        self.assertEqual(normalize_labels([" A ", "b", "a", "", "  ", "B", "世界"]), ["a", "b", "世界"])
        self.assertEqual(normalize_labels([]), [])
    def test_types(self):
        for value in (None, "abc", {}, ("a",), [1], [True], ["ok", None]):
            with self.subTest(value=value), self.assertRaises(TypeError): normalize_labels(value)
    def test_input(self):
        original = [" A ", "a"]
        result = normalize_labels(original)
        self.assertEqual(original, [" A ", "a"])
        result.append("new")
        self.assertEqual(original, [" A ", "a"])

class TestChunks(unittest.TestCase):
    def test_values(self):
        for length in range(10):
            for size in range(1, 7):
                values = list(range(length))
                self.assertEqual(chunks(values, size), [values[i:i+size] for i in range(0,length,size)])
    def test_types(self):
        for values in (None, "abc", {}, (1,2)):
            with self.subTest(values=values), self.assertRaises(TypeError): chunks(values, 1)
        for size in (True, False, 1.5, "2", None):
            with self.subTest(size=size), self.assertRaises(TypeError): chunks([1], size)
        for size in (0, -1):
            with self.subTest(size=size), self.assertRaises(ValueError): chunks([1], size)
    def test_input(self):
        values = [1,2,3]
        result = chunks(values, 10)
        result[0].append(4)
        self.assertEqual(values, [1,2,3])

if __name__ == "__main__": unittest.main()
'''


def setup(workspace: Path, scenario: str) -> dict[str, str]:
    files = {"left.txt": "LEFT_SENTINEL_42\n", "right.txt": "RIGHT_SENTINEL_84\n"}
    if scenario == "coding":
        files = {"labels.py": "def normalize_labels(values):\n    raise NotImplementedError\n",
                 "chunks.py": "def chunks(values, size):\n    raise NotImplementedError\n",
                 "test_work.py": TESTS}
    elif scenario == "cancel":
        files = {"slow_task.py": SLOW_TASK}
    elif scenario == "fanout":
        files = {f"part-{i}.txt": fanout_marker(i) + "\n" for i in range(10)}
    for name, value in files.items():
        (workspace / name).write_text(value)
    return {name: hashlib.sha256(value.encode()).hexdigest() for name, value in files.items()}


def verify(workspace: Path, scenario: str, initial: dict[str, str]) -> dict:
    if scenario == "cancel":
        import json
        ready = workspace / "ready.json"
        try:
            recorded = json.loads(ready.read_text())
        except (OSError, ValueError):
            recorded = {}
        unchanged = (workspace / "slow_task.py").read_text() == SLOW_TASK
        pid = recorded.get("pid")
        # PID 仅用于只读观察，绝不向可能已复用的 PID 发信号。
        active = subprocess.run(["ps", "-p", str(pid), "-o", "command="],
                                capture_output=True, text=True) if isinstance(pid, int) else None
        running = active is not None and "slow_task.py" in active.stdout
        return {"passed": unchanged and recorded.get("recovery_marker") == "RECOVERED_91"
                and not (workspace / "finished.txt").exists() and not running,
                "script_unchanged": unchanged, "real_process_started": isinstance(pid, int),
                "process_still_running": running, "finished_file_exists": (workspace / "finished.txt").exists()}
    if scenario != "coding":
        return {"passed": all((workspace / name).is_file() and hashlib.sha256(
            (workspace / name).read_bytes()).hexdigest() == digest for name, digest in initial.items())}
    unchanged = (workspace / "test_work.py").read_text() == TESTS
    if not unchanged:
        return {"passed": False, "verifier_unchanged": False}
    result = subprocess.run(["python3", "-I", "-m", "unittest", "discover", "-s", str(workspace),
                             "-p", "test_work.py", "-v"], cwd=workspace,
                            text=True, capture_output=True, timeout=30)
    return {"passed": result.returncode == 0, "verifier_unchanged": True,
            "return_code": result.returncode, "output": result.stdout + result.stderr}
