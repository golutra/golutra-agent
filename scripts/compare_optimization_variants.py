#!/usr/bin/env python3
"""交替运行冻结二进制；独立验收与失败样本一起归档，不向模型追加基准修复提示。"""

from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile

import compare_long_benchmark as bench
import compare_pi_benchmark as process


TASKS = {
    "deployment_feedback": {
        "prompt": "Create settings.json enabling the service (enabled=true). The configured deployment verifier runs automatically when you finish and may report environment-specific constraints. Apply its diagnostic fixes until accepted. Do not change the verifier.",
        "files": {},
    },
    "csv_feedback": {
        "prompt": "Create report.csv containing a header customer,total and one row Ada,12.50. The configured import verifier runs automatically when you finish and may report import-format constraints. Apply its diagnostic fixes until accepted. Do not change the verifier.",
        "files": {},
    },
    "parser_fix": {
        "prompt": "Fix parse_pairs in parser.py: parse comma-separated key=value pairs, strip whitespace, preserve equals signs inside values, allow empty values and empty input, reject missing equals, empty keys and duplicate keys with ValueError. Add useful tests, run them and the existing tests, then summarize the change. Do not change test_existing.py.",
        "files": {
            "parser.py": "def parse_pairs(text):\n    return dict(part.split('=') for part in text.split(','))\n",
            "test_existing.py": "import unittest\nfrom parser import parse_pairs\nclass Existing(unittest.TestCase):\n    def test_basic(self):\n        self.assertEqual(parse_pairs('a=1,b=2'), {'a':'1','b':'2'})\n",
        },
    },
    "multi_file": {
        "prompt": "Refactor prices.py and receipt.py to use Decimal for monetary arithmetic, keeping existing public function signatures. total(['0.10','0.20']) must return Decimal('0.30'), total([]) Decimal('0.00'); render must return a two-decimal string using ROUND_HALF_UP (e.g. ['1.005'] -> '1.01'). Do not change input lists. Add and run regression tests and the existing tests. Do not change test_existing.py.",
        "files": {
            "prices.py": "def total(values):\n    return sum(float(v) for v in values)\n",
            "receipt.py": "from prices import total\ndef render(values):\n    return str(total(values))\n",
            "test_existing.py": "import unittest\nfrom receipt import render\nclass Existing(unittest.TestCase):\n    def test_whole(self):\n        self.assertEqual(render(['2.00']), '2.00')\n",
        },
    },
}


def verify(task: str, workspace: Path) -> tuple[bool, str]:
    """判题在独立进程加载交付代码；输入测试文件另由宿主摘要保护。"""
    try:
        if task == "deployment_feedback":
            data = json.loads((workspace / "settings.json").read_text())
            if data.get("enabled") is not True:
                return False, "settings.json: enabled must be true"
            if data.get("timeout_ms") != 2500:
                return False, "settings.json: this deployment requires timeout_ms=2500"
            if data.get("retries") != 2:
                return False, "settings.json: this deployment requires retries=2"
        elif task == "csv_feedback":
            raw = (workspace / "report.csv").read_bytes()
            if not raw.startswith(b"\xef\xbb\xbf"):
                return False, "report.csv: import system requires a UTF-8 BOM"
            if raw[3:] != b"customer,total\r\nAda,12.50\r\n":
                return False, "report.csv: preserve customer,total and Ada,12.50, with CRLF after every line"
        else:
            code = {
                "parser_fix": """from parser import parse_pairs
assert parse_pairs('') == {}
assert parse_pairs(' a = x=y , b= ') == {'a':'x=y','b':''}
for text in ('no_equals', '=x', 'a=1,a=2'):
    try: parse_pairs(text)
    except ValueError: pass
    else: raise AssertionError('must reject '+text)
""",
                "multi_file": """from decimal import Decimal
from prices import total
from receipt import render
assert total(['0.10','0.20']) == Decimal('0.30')
assert isinstance(total([]), Decimal)
assert total([]) == Decimal('0.00')
assert render(['1.005']) == '1.01'
assert render(['-1.005']) == '-1.01'
assert render([]) == '0.00'
values = ['0.10', '0.20']; render(values)
assert values == ['0.10', '0.20']
""",
            }[task]
            for command in ([sys.executable, "-c", code],
                            [sys.executable, "-m", "unittest", "discover", "-v"]):
                result = subprocess.run(command, cwd=workspace, capture_output=True, text=True, timeout=20)
                if result.returncode:
                    return False, (result.stdout + result.stderr)[-4000:]
        return True, "independent acceptance passed"
    except (OSError, ValueError, AttributeError, subprocess.TimeoutExpired) as error:
        return False, str(error)


def run(args):
    variants = [(name, Path(path).resolve(strict=True)) for name, path in (v.split("=", 1) for v in args.variant)]
    if len({name for name, _ in variants}) != len(variants):
        raise ValueError("variant names must be unique")
    root = Path(tempfile.mkdtemp(prefix="golutra-optimization-runs-"))
    report = {"started_at": bench.utc_now(), "model": args.model, "effort": args.reasoning_effort,
              "base_url": args.base_url, "work_root": str(root), "attempts": [],
              "binary_sha256": {name: bench.file_digest(path) for name, path in variants},
              "scope": "Two synthetic verifier-feedback stress cases plus two ordinary coding tasks; not a broad performance or multi-hour claim."}
    bench.write_private_text(args.output, json.dumps(report, indent=2))
    for repeat in range(args.repeats):
        for task_index, task in enumerate(args.task or TASKS):
            order = variants if (repeat + task_index) % 2 == 0 else list(reversed(variants))
            for name, binary in order:
                print(f"running {repeat + 1}/{args.repeats} {task} {name}", flush=True)
                attempt = root / f"{repeat}-{task}-{name}"
                workspace = attempt / "workspace"
                workspace.mkdir(parents=True)
                for path, content in TASKS[task]["files"].items():
                    (workspace / path).write_text(content)
                protected = {p: bench.file_digest(workspace / p) for p in TASKS[task]["files"] if p.startswith("test_")}
                with tempfile.TemporaryDirectory(prefix="golutra-optimization-auth-") as sensitive:
                    home = Path(sensitive)
                    bench.prepare_golutra_home(args, home)
                    env = os.environ.copy()
                    env["GOLUTRA_AGENT_HOME"] = str(home)
                    command = [str(binary), "--cwd", str(workspace), "exec", "--json", "--ephemeral",
                               "--run-dir", str(attempt / "run"), "--yolo", "--no-project-verifier-discovery",
                               "--max-elapsed-ms", str(int(args.timeout * 1000) - 5000)]
                    if task.endswith("feedback"):
                        command += ["--verify-program", sys.executable]
                        for value in [str(Path(__file__).resolve()), "--verify", task, "--workspace", str(workspace)]:
                            command += ["--verify-arg", value]
                    command += [TASKS[task]["prompt"]]
                    captured = process.run_process(command, workspace, env, args.timeout,
                                                   attempt / "stdout.jsonl", attempt / "stderr.log")
                metrics = process.parse_golutra(captured.stdout, captured.elapsed_ms, captured.return_code,
                                               attempt / "run", captured.stdout_line_times_ms)
                passed, detail = verify(task, workspace)
                unchanged = all((workspace / p).exists() and bench.file_digest(workspace / p) == digest for p, digest in protected.items())
                result = {"variant": name, "task": task, "repeat": repeat,
                          "strict_pass": bench.classify_turn(metrics, {"passed": passed}, unchanged)["strict_passed"],
                          "independent_pass": passed, "immutable_inputs_preserved": unchanged,
                          "detail": detail, "metrics": metrics}
                report["attempts"].append(result)
                bench.write_private_text(args.output, json.dumps(report, indent=2))
                print(json.dumps({k: result[k] for k in ("variant", "task", "strict_pass")}) +
                      f" elapsed_ms={captured.elapsed_ms:.0f}", flush=True)
    report["finished_at"] = bench.utc_now()
    bench.write_private_text(args.output, json.dumps(report, indent=2))


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--verify", choices=TASKS)
    parser.add_argument("--workspace", type=Path)
    parser.add_argument("--variant", action="append", default=[])
    parser.add_argument("--task", action="append", choices=TASKS)
    parser.add_argument("--output", type=Path)
    parser.add_argument("--repeats", type=int, default=2)
    parser.add_argument("--timeout", type=float, default=150)
    parser.add_argument("--model", default="gpt-5.6-sol")
    parser.add_argument("--reasoning-effort", default="medium")
    parser.add_argument("--base-url", default="https://api.golutra.cn")
    parser.add_argument("--golutra-agent-home-source", type=Path, default=Path.home() / ".golutra-agent")
    arguments = parser.parse_args()
    if arguments.verify:
        passed, detail = verify(arguments.verify, arguments.workspace)
        print(detail)
        raise SystemExit(0 if passed else 1)
    if not arguments.variant or arguments.output is None:
        parser.error("--variant NAME=PATH and --output are required")
    run(arguments)
