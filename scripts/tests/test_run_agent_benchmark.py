from __future__ import annotations

import importlib.util
import sys
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch


SCRIPT = Path(__file__).resolve().parents[1] / "run_agent_benchmark.py"
SPEC = importlib.util.spec_from_file_location("run_agent_benchmark", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
benchmark = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = benchmark
SPEC.loader.exec_module(benchmark)


class AgentBenchmarkTest(unittest.TestCase):
    def test_exec_uses_argv_verifier_and_resumes_the_same_thread(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            args = SimpleNamespace(
                golutra="golutra",
                workspace=Path(directory),
                criterion=["tests pass"],
                verifier=["python3", "verify.py"],
                verifier_timeout=5.0,
            )
            with patch.object(benchmark.subprocess, "run") as run:
                run.return_value = SimpleNamespace(returncode=0, stdout="", stderr="")
                benchmark.exec_command(args, "fix it", "thread-1")

            command = run.call_args.args[0]
            self.assertIn("--approval-mode", command)
            self.assertEqual(command[-3:], ["resume", "thread-1", "fix it"])
            self.assertIn("--verify-program", command)
            self.assertIn("verify.py", command)

    def test_thread_id_is_read_from_jsonl(self) -> None:
        output = (
            '{"type":"item.started"}\n'
            '{"type":"thread.started","thread_id":"thread-7"}\n'
        )
        self.assertEqual(benchmark.parse_thread_id(output), "thread-7")

    def test_terminal_feedback_reads_the_last_terminal_event(self) -> None:
        output = (
            '{"type":"turn.failed","final_message":"policy blocked inline code"}\n'
        )
        self.assertEqual(
            benchmark.terminal_feedback(output), "policy blocked inline code"
        )


if __name__ == "__main__":
    unittest.main()
