#!/usr/bin/env python3
"""Run a small, paired Golutra/Pi benchmark and collect provider metrics.

The harness deliberately keeps the common task set small.  It records provider
reported usage separately from local estimates so a custom OpenAI-compatible
endpoint cannot make the report look more precise than the wire response.
"""

from __future__ import annotations

import argparse
import json
import os
import shutil
import subprocess
import sys
import tempfile
import time
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Iterable


@dataclass(frozen=True)
class Task:
    task_id: str
    prompt: str
    seeds: dict[str, str]
    expected_files: dict[str, str]
    expected_response: str | None = None
    response_match: str = "non_empty"


TASKS: tuple[Task, ...] = (
    Task(
        "conversation",
        "Reply with exactly BENCH_OK and do not use tools.",
        {},
        {},
        "BENCH_OK",
        "exact",
    ),
    Task(
        "read",
        "Read bench-read.txt with the file tool and reply with its sentinel only.",
        {"bench-read.txt": "READ_SENTINEL_42\n"},
        {},
        "READ_SENTINEL_42",
        "exact",
    ),
    Task(
        "write",
        "Use the file tool to create bench-write.txt containing exactly WRITE_SENTINEL_42 followed by a newline. Then reply done.",
        {},
        {"bench-write.txt": "WRITE_SENTINEL_42\n"},
        "done",
        "contains",
    ),
    Task(
        "edit",
        "Use the file editing tool to replace the complete contents of bench-edit.txt with EDIT_SENTINEL_NEW followed by a newline. Then reply done.",
        {"bench-edit.txt": "EDIT_SENTINEL_OLD\n"},
        {"bench-edit.txt": "EDIT_SENTINEL_NEW\n"},
        "done",
        "contains",
    ),
    Task(
        "multi_edit",
        "Use file tools to update both files: bench-a.txt must contain MULTI_A_NEW and bench-b.txt must contain MULTI_B_NEW, each followed by a newline. Then reply done.",
        {"bench-a.txt": "MULTI_A_OLD\n", "bench-b.txt": "MULTI_B_OLD\n"},
        {"bench-a.txt": "MULTI_A_NEW\n", "bench-b.txt": "MULTI_B_NEW\n"},
        "done",
        "contains",
    ),
    Task(
        "shell",
        "Use the shell tool to run printf SHELL_SENTINEL_42 and reply with the exact output only.",
        {},
        {},
        "SHELL_SENTINEL_42",
        "exact",
    ),
)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--workspace", type=Path, required=True, help="fixture workspace to copy for each task")
    parser.add_argument("--pi-root", type=Path, required=True, help="Pi checkout")
    parser.add_argument("--output", type=Path, required=True, help="JSON report destination")
    parser.add_argument("--golutra", default="target/debug/golutra-cli")
    parser.add_argument("--pi-agent-dir", type=Path, default=Path.home() / ".pi" / "agent")
    parser.add_argument("--provider", default="my-api")
    parser.add_argument("--model", default="gpt-5.5")
    parser.add_argument("--task", action="append", dest="task_ids", help="task id (repeatable; default: all)")
    parser.add_argument("--timeout", type=float, default=180.0)
    parser.add_argument("--max-elapsed-ms", type=int, default=120_000)
    parser.add_argument("--work-root", type=Path, help="retain per-run work and observations here")
    return parser.parse_args()


def now_iso() -> str:
    return datetime.now(timezone.utc).isoformat().replace("+00:00", "Z")


def as_number(value: Any) -> int | None:
    if isinstance(value, bool):
        return None
    if isinstance(value, (int, float)) and value >= 0:
        return int(value)
    return None


def timestamp_ms(value: Any) -> float | None:
    if isinstance(value, (int, float)) and not isinstance(value, bool):
        return float(value)
    if not isinstance(value, str):
        return None
    try:
        return datetime.fromisoformat(value.replace("Z", "+00:00")).timestamp() * 1000
    except ValueError:
        return None


def iter_json_lines(text: str) -> Iterable[dict[str, Any]]:
    for line in text.splitlines():
        try:
            value = json.loads(line)
        except json.JSONDecodeError:
            continue
        if isinstance(value, dict):
            yield value


def nested_runtime_events(events: Iterable[dict[str, Any]]) -> Iterable[dict[str, Any]]:
    """Normalize durable runtime events and their item projections.

    Golutra's JSONL stream exposes provider/tool events twice in two different
    shapes depending on the consumer: durable facts are wrapped as
    ``runtime.event`` while the user-facing item stream stores the same wire
    event under ``item.data``.  The benchmark must understand both shapes and
    deduplicate by event id so counts remain request-based rather than frame-
    based.
    """
    seen_ids: set[str] = set()
    for outer in events:
        candidates: list[dict[str, Any]] = []
        if outer.get("type") == "runtime.event" and isinstance(outer.get("event"), dict):
            candidates.append(outer["event"])
        item = outer.get("item")
        if isinstance(item, dict) and isinstance(item.get("data"), dict):
            data = item["data"]
            if isinstance(data.get("event_type"), str):
                projected = dict(data)
                payload = projected.get("payload")
                if isinstance(payload, dict) and "timestamp" in payload:
                    projected.setdefault("timestamp", payload["timestamp"])
                candidates.append(projected)
        for event in candidates:
            event_id = str(event.get("id") or "")
            if event_id and event_id in seen_ids:
                continue
            if event_id:
                seen_ids.add(event_id)
            yield event


def empty_metrics() -> dict[str, Any]:
    return {
        "completed": False,
        "return_code": None,
        "elapsed_ms": None,
        "startup_ms": None,
        "first_token_ms": None,
        "terminal_ms": None,
        "request_count": 0,
        "tool_call_count": 0,
        "tool_result_count": 0,
        "tool_names": [],
        "input_tokens": None,
        "non_cached_input_tokens": None,
        "planned_input_tokens": None,
        "output_tokens": None,
        "reasoning_tokens": None,
        "cache_read_tokens": None,
        "cache_write_tokens": None,
        "total_tokens": None,
        "tool_schema_tokens_estimated": None,
        "tool_result_tokens_estimated": None,
        "usage_source": "unknown",
        "cost": None,
        "cost_source": "unknown",
        "final_message": "",
    }


def add_counter(metrics: dict[str, Any], field: str, value: int | None) -> None:
    if value is None:
        return
    metrics[field] = (metrics[field] or 0) + value


def event_payload(event: dict[str, Any]) -> dict[str, Any]:
    payload = event.get("payload")
    return payload if isinstance(payload, dict) else {}


def event_provider_request_id(event: dict[str, Any], payload: dict[str, Any]) -> str:
    context = event.get("causal_context")
    context_id = context.get("provider_request_id") if isinstance(context, dict) else None
    return str(
        payload.get("provider_request_id")
        or event.get("provider_request_id")
        or context_id
        or event.get("id")
        or ""
    )


def usage_record_from_provider_completed(event: dict[str, Any], payload: dict[str, Any]) -> dict[str, Any]:
    usage = payload.get("usage") if isinstance(payload.get("usage"), dict) else {}
    return {
        "request_event_id": event_provider_request_id(event, payload),
        "input_tokens": usage.get("input_tokens"),
        "output_tokens": usage.get("output_tokens"),
        "reasoning_tokens": usage.get("reasoning_tokens"),
        "cache_read_tokens": usage.get("cache_read_tokens", usage.get("cached_input_tokens")),
        "cache_write_tokens": usage.get("cache_write_tokens"),
        "provider_total_tokens": usage.get("total_tokens"),
        "usage_source": usage.get("usage_source", "provider"),
    }


def has_provider_delta(delta: dict[str, Any]) -> bool:
    return any(
        delta.get(key) not in (None, "", [], {})
        for key in ("text", "reasoning", "arguments", "tool_name", "tool_call_id")
    )


def parse_golutra(stdout: str, elapsed_ms: float, return_code: int, run_dir: Path) -> dict[str, Any]:
    metrics = empty_metrics()
    metrics["return_code"] = return_code
    metrics["elapsed_ms"] = round(elapsed_ms, 1)
    events = list(iter_json_lines(stdout))
    nested = list(nested_runtime_events(events))
    turn_starts = [
        timestamp_ms(event.get("timestamp"))
        for event in events
        if event.get("type") == "turn.started"
    ]
    turn_starts.extend(
        timestamp_ms(event.get("timestamp"))
        for event in nested
        if event.get("event_type") in {"turn_started", "turn.started"}
    )
    provider_starts = [timestamp_ms(event.get("timestamp")) for event in nested if event.get("event_type") == "provider_started"]
    turn_start = min((value for value in turn_starts if value is not None), default=None)
    provider_start = min((value for value in provider_starts if value is not None), default=None)
    first = provider_start or turn_start
    if turn_start is not None and provider_start is not None:
        metrics["startup_ms"] = round(max(0.0, provider_start - turn_start), 1)
    first_token = None
    terminal = None
    seen_requests: set[str] = set()
    seen_usage: set[str] = set()
    tool_names: set[str] = set()
    observed_tool_calls = 0
    for event in events:
        event_type = event.get("type")
        if event_type == "turn.completed":
            metrics["completed"] = event.get("status") == "completed"
            metrics["final_message"] = str(event.get("final_message") or "")[:512]
            terminal = timestamp_ms(event.get("timestamp"))
        elif event_type == "turn.failed":
            metrics["final_message"] = str(event.get("error") or event.get("final_message") or "")[:512]
            terminal = timestamp_ms(event.get("timestamp"))
    for event in nested:
        event_type = str(event.get("event_type") or "")
        payload = event_payload(event)
        event_time = timestamp_ms(event.get("timestamp"))
        if event_type == "context_built" and metrics["planned_input_tokens"] is None:
            metrics["planned_input_tokens"] = as_number(payload.get("planned_input_tokens"))
        if event_type in {"turn_completed", "turn.completed"} and terminal is None:
            metrics["completed"] = str(payload.get("status") or "completed").lower() == "completed"
            terminal = event_time
        elif event_type in {"turn_failed", "turn.failed"} and terminal is None:
            metrics["completed"] = False
            terminal = event_time
        if event_type == "provider_started":
            request_id = event_provider_request_id(event, payload)
            if request_id and request_id not in seen_requests:
                seen_requests.add(request_id)
        if event_type == "provider_streamed" and first_token is None:
            delta = payload.get("delta") if isinstance(payload.get("delta"), dict) else {}
            if has_provider_delta(delta):
                first_token = event_time
        if event_type == "token_usage_recorded":
            record = payload.get("record") if isinstance(payload.get("record"), dict) else {}
            request_id = str(
                record.get("request_event_id")
                or record.get("provider_request_id")
                or event_provider_request_id(event, payload)
            )
            if request_id in seen_usage:
                continue
            seen_usage.add(request_id)
            for source, target in (
                ("input_tokens", "input_tokens"),
                ("non_cached_input_tokens", "non_cached_input_tokens"),
                ("output_tokens", "output_tokens"),
                ("reasoning_tokens", "reasoning_tokens"),
                ("cache_read_tokens", "cache_read_tokens"),
                ("cache_write_tokens", "cache_write_tokens"),
                ("provider_total_tokens", "total_tokens"),
                ("tool_schema_tokens_estimated", "tool_schema_tokens_estimated"),
                ("tool_result_tokens_estimated", "tool_result_tokens_estimated"),
            ):
                add_counter(metrics, target, as_number(record.get(source)))
            if metrics["planned_input_tokens"] is None:
                metrics["planned_input_tokens"] = as_number(record.get("planned_input_tokens"))
            metrics["usage_source"] = str(record.get("usage_source") or "unknown")
            if record.get("estimated_cost") is not None:
                metrics["cost"] = record.get("estimated_cost")
                metrics["cost_source"] = "estimated"
        if "tool" in event_type:
            envelope = payload.get("envelope") if isinstance(payload.get("envelope"), dict) else {}
            name = payload.get("tool_name") or payload.get("name") or envelope.get("tool_name")
            is_external_verifier = name == "external_verifier"
            if not is_external_verifier and (event_type.endswith("started") or event_type.endswith("requested")):
                observed_tool_calls += 1
            if not is_external_verifier and (
                event_type.endswith("completed")
                or event_type.endswith("finished")
                or event_type.endswith("failed")
            ):
                metrics["tool_result_count"] += 1
            if isinstance(name, str) and name and not is_external_verifier:
                tool_names.add(name)
        if event_type == "provider_completed":
            request_id = event_provider_request_id(event, payload)
            if request_id and request_id not in seen_usage and isinstance(payload.get("usage"), dict):
                seen_usage.add(request_id)
                fallback_record = usage_record_from_provider_completed(event, payload)
                for source, target in (
                    ("input_tokens", "input_tokens"),
                    ("non_cached_input_tokens", "non_cached_input_tokens"),
                    ("output_tokens", "output_tokens"),
                    ("reasoning_tokens", "reasoning_tokens"),
                    ("cache_read_tokens", "cache_read_tokens"),
                    ("cache_write_tokens", "cache_write_tokens"),
                    ("provider_total_tokens", "total_tokens"),
                ):
                    add_counter(metrics, target, as_number(fallback_record.get(source)))
                metrics["usage_source"] = str(fallback_record.get("usage_source") or "provider")
            count = as_number(payload.get("tool_call_count"))
            if count is not None:
                metrics["tool_call_count"] = max(metrics["tool_call_count"], count)
            for call in payload.get("provider_tool_calls", []):
                if isinstance(call, dict):
                    name = call.get("name") or call.get("tool_name")
                    if isinstance(name, str) and name:
                        tool_names.add(name)
        if event_type == "token_usage_recorded":
            record = event_payload(event).get("record")
            request_id = str(record.get("request_event_id") or "") if isinstance(record, dict) else ""
            if request_id:
                seen_requests.add(request_id)
    metrics["tool_call_count"] = max(metrics["tool_call_count"], observed_tool_calls)
    metrics["request_count"] = len(seen_requests | seen_usage)
    metrics["tool_names"] = sorted(tool_names)
    if first_token is not None and first is not None:
        metrics["first_token_ms"] = round(max(0.0, first_token - first), 1)
    if terminal is not None and first is not None:
        metrics["terminal_ms"] = round(max(0.0, terminal - first), 1)
    if metrics["total_tokens"] is None and metrics["input_tokens"] is not None and metrics["output_tokens"] is not None:
        metrics["total_tokens"] = metrics["input_tokens"] + metrics["output_tokens"]
    manifest = run_dir / "manifest.json"
    if manifest.is_file():
        try:
            data = json.loads(manifest.read_text(encoding="utf-8"))
            metrics["run_id"] = data.get("provenance", {}).get("run_id")
            metrics["provider"] = data.get("terminal_outcome", {}).get("result", {}).get("status")
        except (OSError, json.JSONDecodeError):
            pass
    return metrics


def extract_text(content: Any) -> str:
    if isinstance(content, str):
        return content
    if not isinstance(content, list):
        return ""
    return "\n".join(str(block.get("text", "")) for block in content if isinstance(block, dict) and block.get("type") == "text")


def parse_pi(stdout: str, elapsed_ms: float, return_code: int) -> dict[str, Any]:
    metrics = empty_metrics()
    metrics["return_code"] = return_code
    metrics["elapsed_ms"] = round(elapsed_ms, 1)
    events = list(iter_json_lines(stdout))
    usage_seen: set[str] = set()
    first_event = None
    first_token = None
    terminal = None
    last_message_time = None
    tool_names: set[str] = set()
    tool_calls_seen: set[str] = set()
    tool_results_seen: set[str] = set()
    for event in events:
        event_type = event.get("type")
        if first_event is None and event_type == "session":
            first_event = timestamp_ms(event.get("timestamp"))
        if event_type == "message_update":
            update = event.get("assistantMessageEvent")
            if isinstance(update, dict) and update.get("type") in {"text_delta", "thinking_delta", "toolcall_delta"} and first_token is None:
                first_token = timestamp_ms((update.get("partial") or {}).get("timestamp"))
        if event_type == "message_end":
            message = event.get("message")
            if not isinstance(message, dict):
                continue
            if message.get("role") == "toolResult":
                result_id = str(message.get("toolCallId") or message.get("timestamp") or "")
                if result_id not in tool_results_seen:
                    tool_results_seen.add(result_id)
                    metrics["tool_result_count"] += 1
                continue
            if message.get("role") != "assistant":
                continue
            usage = message.get("usage") if isinstance(message.get("usage"), dict) else {}
            last_message_time = timestamp_ms(message.get("timestamp")) or last_message_time
            response_id = str(message.get("responseId") or message.get("timestamp") or len(usage_seen))
            if response_id in usage_seen:
                continue
            usage_seen.add(response_id)
            metrics["request_count"] += 1
            for source, target in (
                ("input", "input_tokens"),
                ("reasoning", "reasoning_tokens"),
                ("reasoningTokens", "reasoning_tokens"),
                ("output", "output_tokens"),
                ("cacheRead", "cache_read_tokens"),
                ("cacheWrite", "cache_write_tokens"),
                ("totalTokens", "total_tokens"),
            ):
                add_counter(metrics, target, as_number(usage.get(source)))
            metrics["final_message"] = extract_text(message.get("content"))[:512]
            cost = usage.get("cost")
            if isinstance(cost, dict):
                add_cost = cost.get("total")
                if isinstance(add_cost, (int, float)):
                    metrics["cost"] = (metrics["cost"] or 0) + add_cost
                    metrics["cost_source"] = "provider"
            metrics["usage_source"] = "provider"
            for block in message.get("content", []):
                if isinstance(block, dict) and block.get("type") == "toolCall":
                    call_id = str(block.get("id") or block.get("callId") or block.get("name") or "")
                    if call_id not in tool_calls_seen:
                        tool_calls_seen.add(call_id)
                        metrics["tool_call_count"] += 1
                    if isinstance(block.get("name"), str):
                        tool_names.add(block["name"])
        elif event_type in {"tool_result", "tool_execution_end"}:
            result_id = str(event.get("toolCallId") or event.get("tool_call_id") or event.get("timestamp") or "")
            if result_id not in tool_results_seen:
                tool_results_seen.add(result_id)
                metrics["tool_result_count"] += 1
            if isinstance(event.get("toolName"), str):
                tool_names.add(event["toolName"])
        elif event_type == "tool_execution_start":
            call_id = str(event.get("toolCallId") or event.get("timestamp") or "")
            if call_id not in tool_calls_seen:
                tool_calls_seen.add(call_id)
                metrics["tool_call_count"] += 1
            if isinstance(event.get("toolName"), str):
                tool_names.add(event["toolName"])
        elif event_type == "agent_end":
            terminal = timestamp_ms(event.get("timestamp"))
            metrics["completed"] = return_code == 0
    terminal = terminal or last_message_time
    metrics["tool_names"] = sorted(tool_names)
    metrics["tool_result_tokens_estimated"] = 0
    for event in events:
        if event.get("type") in {"tool_result", "tool_execution_end"}:
            result = event.get("result") or event.get("toolResult") or {}
            metrics["tool_result_tokens_estimated"] += max(0, len(extract_text(result.get("content") if isinstance(result, dict) else "")) + 3) // 4
    metrics["usage_source"] = metrics["usage_source"] or "unknown"
    if first_token is not None and first_event is not None:
        metrics["first_token_ms"] = round(max(0.0, first_token - first_event), 1)
    if terminal is not None and first_event is not None:
        metrics["terminal_ms"] = round(max(0.0, terminal - first_event), 1)
    if metrics["total_tokens"] is None and metrics["input_tokens"] is not None and metrics["output_tokens"] is not None:
        metrics["total_tokens"] = metrics["input_tokens"] + metrics["output_tokens"]
    return metrics


def run_process(command: list[str], cwd: Path, env: dict[str, str], timeout: float, stdout_path: Path, stderr_path: Path) -> tuple[str, str, int, float]:
    started = time.monotonic()
    try:
        result = subprocess.run(command, cwd=cwd, env=env, text=True, capture_output=True, timeout=timeout, check=False)
        returncode = result.returncode
        stdout, stderr = result.stdout, result.stderr
    except subprocess.TimeoutExpired as error:
        returncode = 124
        stdout = error.stdout or ""
        stderr = (error.stderr or "") + "\nprocess timed out"
    elapsed_ms = (time.monotonic() - started) * 1000
    stdout_path.write_text(stdout, encoding="utf-8", errors="replace")
    stderr_path.write_text(stderr, encoding="utf-8", errors="replace")
    return stdout, stderr, returncode, elapsed_ms


def verify_files(workspace: Path, expected: dict[str, str]) -> dict[str, Any]:
    checks: dict[str, bool] = {}
    for relative, content in expected.items():
        path = workspace / relative
        checks[relative] = path.is_file() and path.read_text(encoding="utf-8") == content
    return {"passed": all(checks.values()), "files": checks}


def verify_response(task: Task, final_message: str) -> dict[str, Any]:
    actual = final_message.strip()
    expected = task.expected_response
    if task.response_match == "exact":
        passed = bool(expected) and actual == expected
    elif task.response_match == "contains":
        passed = bool(expected) and expected.lower() in actual.lower()
    else:
        passed = bool(actual)
    return {
        "passed": passed,
        "expected": expected,
        "match": task.response_match,
        "actual": actual[:512],
    }


def task_verification(task: Task, metrics: dict[str, Any], workspace: Path) -> dict[str, Any]:
    files = verify_files(workspace, task.expected_files)
    response = verify_response(task, str(metrics.get("final_message") or ""))
    return {
        "passed": bool(metrics.get("completed")) and files["passed"] and response["passed"],
        "execution_completed": bool(metrics.get("completed")),
        "files": files,
        "response": response,
    }


def display_metric(value: Any) -> str:
    return "-" if value is None else str(value)


def prepare_workspace(source: Path, destination: Path, seeds: dict[str, str]) -> None:
    shutil.copytree(source, destination)
    for relative, content in seeds.items():
        path = destination / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(content, encoding="utf-8")


def run_task(task: Task, args: argparse.Namespace, root: Path) -> dict[str, Any]:
    task_root = root / task.task_id
    golutra_workspace = task_root / "golutra-workspace"
    pi_workspace = task_root / "pi-workspace"
    prepare_workspace(args.workspace, golutra_workspace, task.seeds)
    prepare_workspace(args.workspace, pi_workspace, task.seeds)
    golutra_run = task_root / "golutra-run"
    verifier = Path(__file__).with_name("verify_compare_task.py").resolve()
    expected_json = json.dumps(task.expected_files, ensure_ascii=True, separators=(",", ":"))
    golutra_command = [
        str(Path(args.golutra).resolve()),
        "--cwd",
        str(golutra_workspace),
        "exec",
        "--json",
        "--ephemeral",
        "--run-dir",
        str(golutra_run),
        "--approval-mode",
        "auto",
        # Pi 的基准进程不启用沙箱，因此统一执行边界，并由 harness 负责功能断言。
        "--yolo",
        "--no-project-verifier-discovery",
        "--verify-program",
        sys.executable,
        "--verify-arg",
        str(verifier),
        "--verify-arg",
        "--workspace",
        "--verify-arg",
        str(golutra_workspace),
        "--verify-arg",
        "--expected-json",
        "--verify-arg",
        expected_json,
        "--verify-timeout-ms",
        "10000",
        "--max-elapsed-ms",
        str(args.max_elapsed_ms),
        task.prompt,
    ]
    golutra_stdout, _, golutra_code, golutra_elapsed = run_process(
        golutra_command,
        args.workspace,
        os.environ.copy(),
        args.timeout,
        task_root / "golutra.stdout.jsonl",
        task_root / "golutra.stderr.log",
    )
    golutra_metrics = parse_golutra(golutra_stdout, golutra_elapsed, golutra_code, golutra_run)
    pi_session = task_root / "pi-session"
    pi_session.mkdir(parents=True)
    pi_entry = args.pi_root / "packages" / "coding-agent" / "src" / "cli.ts"
    tsx_entry = args.pi_root / "node_modules" / "tsx" / "dist" / "cli.mjs"
    pi_command = [
        "node",
        str(tsx_entry),
        str(pi_entry),
        "--provider",
        args.provider,
        "--model",
        args.model,
        "--mode",
        "json",
        "--print",
        "--session-dir",
        str(pi_session),
        "--no-skills",
        "--no-extensions",
        "--no-prompt-templates",
        "--no-themes",
        "--no-context-files",
        task.prompt,
    ]
    pi_env = os.environ.copy()
    pi_env["PI_CODING_AGENT_DIR"] = str(args.pi_agent_dir.resolve())
    pi_stdout, _, pi_code, pi_elapsed = run_process(
        pi_command,
        pi_workspace,
        pi_env,
        args.timeout,
        task_root / "pi.stdout.jsonl",
        task_root / "pi.stderr.log",
    )
    pi_metrics = parse_pi(pi_stdout, pi_elapsed, pi_code)
    return {
        "task_id": task.task_id,
        "prompt": task.prompt,
        "golutra": {**golutra_metrics, "verification": task_verification(task, golutra_metrics, golutra_workspace)},
        "pi": {**pi_metrics, "verification": task_verification(task, pi_metrics, pi_workspace)},
        "artifacts": {"root": str(task_root), "golutra_run": str(golutra_run), "pi_session": str(pi_session)},
    }


def markdown_report(report: dict[str, Any]) -> str:
    lines = [
        "# Golutra / Pi benchmark",
        "",
        f"Generated: {report['generated_at']}",
        "",
        "| Task | G total | Pi total | G input/output/reasoning | Pi input/output/reasoning | G cache R/W | Pi cache R/W | G req/tools | Pi req/tools | G startup/first/terminal ms | Pi first/terminal ms | Pass |",
        "| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- |",
    ]
    for row in report["tasks"]:
        g, p = row["golutra"], row["pi"]
        passed = g["verification"]["passed"] and p["verification"]["passed"]
        lines.append(
            f"| {row['task_id']} | {display_metric(g.get('total_tokens'))} | {display_metric(p.get('total_tokens'))} | {display_metric(g.get('input_tokens'))} / {display_metric(g.get('output_tokens'))} / {display_metric(g.get('reasoning_tokens'))} | {display_metric(p.get('input_tokens'))} / {display_metric(p.get('output_tokens'))} / {display_metric(p.get('reasoning_tokens'))} | {display_metric(g.get('cache_read_tokens'))} / {display_metric(g.get('cache_write_tokens'))} | {display_metric(p.get('cache_read_tokens'))} / {display_metric(p.get('cache_write_tokens'))} | {display_metric(g.get('request_count'))} / {display_metric(g.get('tool_call_count'))} | {display_metric(p.get('request_count'))} / {display_metric(p.get('tool_call_count'))} | {display_metric(g.get('startup_ms'))} / {display_metric(g.get('first_token_ms'))} / {display_metric(g.get('terminal_ms'))} | {display_metric(p.get('first_token_ms'))} / {display_metric(p.get('terminal_ms'))} | {'yes' if passed else 'no'} |"
        )
    lines.extend(
        [
            "",
            "Token values are provider-reported unless the field name says estimated; cache is read/write tokens; first/terminal are measured from the provider/session start.",
            "",
            "## Aggregate",
            "",
            "| Engine | Passed | Total | Input | Output | Reasoning | Cache read/write | Tool schema estimate | Tool result estimate | Avg startup ms | Avg elapsed ms |",
            "| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |",
        ]
    )
    for engine in ("golutra", "pi"):
        summary = report["summary"][engine]
        lines.append(
            f"| {engine} | {summary['passed_tasks']}/{summary['task_count']} | {display_metric(summary['total_tokens'])} | {display_metric(summary['input_tokens'])} | {display_metric(summary['output_tokens'])} | {display_metric(summary['reasoning_tokens'])} | {display_metric(summary['cache_read_tokens'])} / {display_metric(summary['cache_write_tokens'])} | {display_metric(summary['tool_schema_tokens_estimated'])} | {display_metric(summary['tool_result_tokens_estimated'])} | {display_metric(summary['avg_startup_ms'])} | {display_metric(summary['avg_elapsed_ms'])} |"
        )
    return "\n".join(lines) + "\n"


def aggregate_metrics(tasks: list[dict[str, Any]], engine: str) -> dict[str, Any]:
    metrics = [row[engine] for row in tasks]
    numeric_fields = (
        "input_tokens",
        "non_cached_input_tokens",
        "output_tokens",
        "reasoning_tokens",
        "cache_read_tokens",
        "cache_write_tokens",
        "total_tokens",
        "tool_schema_tokens_estimated",
        "tool_result_tokens_estimated",
        "request_count",
        "tool_call_count",
        "tool_result_count",
    )
    summary: dict[str, Any] = {
        "task_count": len(metrics),
        "passed_tasks": sum(1 for metric in metrics if metric.get("verification", {}).get("passed")),
    }
    for field in numeric_fields:
        values = [metric.get(field) for metric in metrics if metric.get(field) is not None]
        summary[field] = sum(values) if values else None
    elapsed = [metric["elapsed_ms"] for metric in metrics if metric.get("elapsed_ms") is not None]
    summary["avg_elapsed_ms"] = round(sum(elapsed) / len(elapsed), 1) if elapsed else None
    startup = [metric["startup_ms"] for metric in metrics if metric.get("startup_ms") is not None]
    summary["avg_startup_ms"] = round(sum(startup) / len(startup), 1) if startup else None
    return summary


def main() -> int:
    args = parse_args()
    args.workspace = args.workspace.resolve(strict=True)
    args.pi_root = args.pi_root.resolve(strict=True)
    selected = [task for task in TASKS if not args.task_ids or task.task_id in args.task_ids]
    unknown = sorted(set(args.task_ids or ()) - {task.task_id for task in TASKS})
    if unknown:
        raise SystemExit(f"unknown task id(s): {', '.join(unknown)}")
    if not selected:
        raise SystemExit("no tasks selected")
    if args.work_root:
        work_root = args.work_root.resolve()
        work_root.mkdir(parents=True, exist_ok=True)
    else:
        work_root = Path(tempfile.mkdtemp(prefix="golutra-pi-benchmark-"))
    report = {
        "schema_version": 2,
        "generated_at": now_iso(),
        "conditions": {
            "golutra": str(Path(args.golutra).resolve()),
            "pi_root": str(args.pi_root),
            "provider": args.provider,
            "model": args.model,
            "task_count": len(selected),
            "cost_source": "unknown",
            "golutra_approval_mode": "yolo",
            "project_verifier_discovery": False,
            "functional_assertions": "harness_response_and_fixture_files",
            "external_verifier": str(Path(__file__).with_name("verify_compare_task.py").resolve()),
        },
        "tasks": [],
        "work_root": str(work_root),
    }
    for task in selected:
        print(f"running {task.task_id}", file=sys.stderr, flush=True)
        report["tasks"].append(run_task(task, args, work_root))
    report["summary"] = {
        "golutra": aggregate_metrics(report["tasks"], "golutra"),
        "pi": aggregate_metrics(report["tasks"], "pi"),
    }
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, indent=2, ensure_ascii=True) + "\n", encoding="utf-8")
    markdown_path = args.output.with_suffix(".md")
    markdown_path.write_text(markdown_report(report), encoding="utf-8")
    print(json.dumps({"output": str(args.output), "markdown": str(markdown_path), "work_root": str(work_root)}), file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
