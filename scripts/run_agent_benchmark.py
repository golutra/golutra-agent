#!/usr/bin/env python3
"""Run a Golutra task with caller-owned verification and bounded repair turns."""

from __future__ import annotations

import argparse
import json
import subprocess
import sys
from dataclasses import asdict, dataclass
from pathlib import Path


@dataclass
class Attempt:
    number: int
    returncode: int
    thread_id: str | None
    verifier_returncode: int | None
    verifier_output: str


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Run Golutra exec and resume failed verification in the same thread."
    )
    parser.add_argument("--golutra", default="golutra")
    parser.add_argument("--workspace", type=Path, required=True)
    prompt = parser.add_mutually_exclusive_group(required=True)
    prompt.add_argument("--prompt")
    prompt.add_argument("--prompt-file", type=Path)
    parser.add_argument("--criterion", action="append", default=[])
    parser.add_argument("--max-attempts", type=int, default=2)
    parser.add_argument("--verifier-timeout", type=float, default=180.0)
    parser.add_argument("--max-feedback-bytes", type=int, default=16 * 1024)
    parser.add_argument("--summary", type=Path)
    parser.add_argument("verifier", nargs=argparse.REMAINDER)
    args = parser.parse_args()
    if args.verifier and args.verifier[0] == "--":
        args.verifier = args.verifier[1:]
    if not args.verifier:
        parser.error("a verifier argv is required after --")
    if args.max_attempts < 1:
        parser.error("--max-attempts must be positive")
    return args


def prompt_text(args: argparse.Namespace) -> str:
    if args.prompt is not None:
        return args.prompt
    return args.prompt_file.read_text(encoding="utf-8")


def parse_thread_id(output: str) -> str | None:
    for line in output.splitlines():
        try:
            event = json.loads(line)
        except json.JSONDecodeError:
            continue
        if event.get("type") == "thread.started":
            return event.get("thread_id")
    return None


def terminal_feedback(output: str) -> str:
    for line in reversed(output.splitlines()):
        try:
            event = json.loads(line)
        except json.JSONDecodeError:
            continue
        if event.get("type") not in {"turn.failed", "turn.completed"}:
            continue
        return str(event.get("final_message") or event.get("error") or "").strip()
    return ""


def bounded(value: bytes, limit: int) -> str:
    if len(value) <= limit:
        return value.decode("utf-8", errors="replace")
    suffix = f"\n[verifier output truncated at {limit} bytes]"
    return value[:limit].decode("utf-8", errors="replace") + suffix


def exec_command(
    args: argparse.Namespace, prompt: str, thread_id: str | None
) -> subprocess.CompletedProcess[str]:
    command = [
        args.golutra,
        "--cwd",
        str(args.workspace),
        "exec",
        "--json",
        "--approval-mode",
        "auto",
    ]
    for criterion in args.criterion or ["external verifier passes"]:
        command.extend(("--completion-criterion", criterion))
    command.extend(("--verify-program", args.verifier[0]))
    for argument in args.verifier[1:]:
        command.extend(("--verify-arg", argument))
    command.extend(("--verify-timeout-ms", str(int(args.verifier_timeout * 1000))))
    if thread_id is None:
        command.append(prompt)
    else:
        command.extend(("resume", thread_id, prompt))
    return subprocess.run(command, text=True, capture_output=True, check=False)


def run_verifier(args: argparse.Namespace) -> tuple[int, str]:
    try:
        result = subprocess.run(
            args.verifier,
            cwd=args.workspace,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            timeout=args.verifier_timeout,
            check=False,
        )
        return result.returncode, bounded(result.stdout, args.max_feedback_bytes)
    except subprocess.TimeoutExpired as error:
        output = error.stdout or b""
        return 124, bounded(output, args.max_feedback_bytes) + "\nverifier timed out"


def main() -> int:
    args = parse_args()
    args.workspace = args.workspace.resolve(strict=True)
    initial_prompt = prompt_text(args).strip()
    execution_constraints = (
        "Execution constraints: shell commands are argv-only. A complete quoted "
        "foreground Python heredoc such as python - <<'PY' is passed directly on "
        "stdin. For other pipes, redirection, chained commands, or command "
        "substitution, explicitly invoke bash -lc with the complete script as one "
        "quoted argument. Create a workspace script for reusable code."
    )
    thread_id = None
    attempts: list[Attempt] = []
    next_prompt = f"{initial_prompt}\n\n{execution_constraints}"
    for number in range(1, args.max_attempts + 1):
        result = exec_command(args, next_prompt, thread_id)
        thread_id = thread_id or parse_thread_id(result.stdout)
        sys.stdout.write(result.stdout)
        sys.stderr.write(result.stderr)
        if result.returncode == 0:
            attempts.append(Attempt(number, 0, thread_id, 0, ""))
            break
        verifier_code, verifier_output = run_verifier(args)
        attempts.append(
            Attempt(number, result.returncode, thread_id, verifier_code, verifier_output)
        )
        if thread_id is None or number == args.max_attempts:
            break
        next_prompt = (
            "Continue and complete the original objective below.\n\n"
            f"Original objective:\n{initial_prompt}\n\n"
            f"{execution_constraints}\n\n"
            "The caller-owned verifier failed. Fix the workspace and run an appropriate "
            "local check before finishing. Do not change the requested output contract.\n\n"
            f"Runtime summary:\n{terminal_feedback(result.stdout)}\n\n"
            f"Verifier exit code: {verifier_code}\n{verifier_output}"
        )
    summary = {
        "workspace": str(args.workspace),
        "thread_id": thread_id,
        "passed": bool(attempts and attempts[-1].returncode == 0),
        "attempts": [asdict(attempt) for attempt in attempts],
    }
    serialized = json.dumps(summary, indent=2, ensure_ascii=True) + "\n"
    if args.summary:
        args.summary.parent.mkdir(parents=True, exist_ok=True)
        args.summary.write_text(serialized, encoding="utf-8")
    sys.stderr.write(serialized)
    return 0 if summary["passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
