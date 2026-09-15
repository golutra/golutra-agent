#!/usr/bin/env python3
"""Exercise real background children, explicit context fork and same-child resume.

Uses an isolated copy of the configured Golutra provider credentials. The report
contains counters and assertions; credentials remain temporary. Raw CLI evidence
is retained beside the report in files readable only by the current user.
This is a functional smoke, not a statistical comparison against other agents.
"""
from __future__ import annotations

import argparse
import json
import os
import sqlite3
import tempfile
from collections import Counter
from datetime import datetime, timezone
from pathlib import Path

import compare_long_benchmark as long_bench
import compare_pi_benchmark as paired


PROMPT = """Perform this subagent lifecycle acceptance task using real tools.
The parent-context marker is FORK_CONTEXT_73X.
Launch these two independent subagent tasks together, run_in_background=true:
A: context=independent, agent_type=explore. Read left.txt and report its exact sentinel.
B: context=fork, agent_type=explore. Read right.txt and report its exact sentinel
and the parent-context marker inherited from this conversation. Do not copy the
marker into B's task text: this checks actual context inheritance.
Wait for both child handles with wait_mode=all and a bounded wait_ms. If a wait
expires, continue waiting on those handles, never spawn replacements.
Then resume A in the same child session: ask it to repeat the sentinel from its
own earlier findings, using its history. Observe that execution's terminal result.
Do not edit files or create more children. Finish by reporting the two sentinels
and inherited marker, based on actual returned results. Report any failure honestly.
"""


def read_events(home: Path) -> list[dict]:
    events = {}
    databases = sorted(candidate for candidate in home.rglob("*") if candidate.suffix in {".db", ".sqlite", ".sqlite3"})
    for database in databases:
        with sqlite3.connect(f"file:{database}?mode=ro", uri=True) as connection:
            tables = {row[0] for row in connection.execute("SELECT name FROM sqlite_master WHERE type='table'")}
            if "runtime_events" in tables:
                for row in connection.execute("SELECT event_json FROM runtime_events ORDER BY sequence_no"):
                    event = json.loads(row[0])
                    events[event["id"]] = event
    return sorted(events.values(), key=lambda event: (event["timestamp"], event["sequence_no"]))


def summarize(events: list[dict], metrics: dict) -> dict:
    tasks = [event for event in events if event.get("event_type") == "task_created"]
    children = [event for event in tasks if event.get("payload", {}).get("payload", {}).get("_delegated_task")]
    child_sessions = {event["session_id"] for event in children}
    starts = [event for event in events if event.get("event_type") == "tool_started"]
    calls = [event for event in starts if event.get("payload", {}).get("tool_name") == "subagent"]
    actions = Counter(event["payload"].get("arguments", {}).get("action", "spawn") for event in calls)
    notices = [event for event in events if event.get("event_type") == "subagent_updated"]
    notice_ids = [event["id"] for event in notices]
    terminal_tasks = {event.get("task_id") for event in events
                      if event.get("event_type") == "task_completed" and event.get("payload", {}).get("status") == "completed"}
    child_answers = [event for event in events if event.get("event_type") == "assistant_message"
                     and event.get("session_id") in child_sessions]
    answers = "\n".join(event.get("payload", {}).get("content", "") for event in child_answers)
    forked = [event for event in children if event["payload"]["payload"].get("_delegation_context_artifact")]
    fork_answers = "\n".join(event.get("payload", {}).get("content", "") for event in child_answers
                            if len(forked) == 1 and event["session_id"] == forked[0]["session_id"])
    execution_counts = Counter(event["session_id"] for event in children)
    notice_tasks = [event.get("payload", {}).get("facts", {}).get("child_task_id") for event in notices]
    first_by_session = {}
    for event in children:
        first_by_session.setdefault(event["session_id"], event)
    first_tasks = list(first_by_session.values())
    first_ids = {event["task_id"] for event in first_tasks}
    resumed = [event for event in children if event["task_id"] not in first_ids]
    resumed_answers = "\n".join(event.get("payload", {}).get("content", "") for event in child_answers
                                if len(resumed) == 1 and event.get("task_id") == resumed[0]["task_id"])
    first_finishes = [event["timestamp"] for event in events
                      if event.get("task_id") in first_ids and event.get("event_type") == "task_completed"]
    checks = {
        "parent_succeeded": metrics.get("completed") is True,
        "exactly_two_child_sessions": len(child_sessions) == 2,
        "child_execution_intervals_overlap": len(first_tasks) == 2 and len(first_finishes) == 2 and max(event["timestamp"] for event in first_tasks) < min(first_finishes),
        "three_child_executions": len(children) == 3,
        "all_child_executions_succeeded": len(children) == 3 and all(event["task_id"] in terminal_tasks for event in children),
        "spawn_twice_resume_once": actions["spawn"] == 2 and actions["resume"] == 1,
        "both_sentinels_in_child_findings": all(marker in answers for marker in ("LEFT_SENTINEL_42", "RIGHT_SENTINEL_84")),
        "fork_marker_in_forked_child_findings": "FORK_CONTEXT_73X" in fork_answers,
        "exactly_one_explicit_fork_without_copied_marker": len(forked) == 1 and "FORK_CONTEXT_73X" not in forked[0]["payload"]["payload"].get("prompt", ""),
        "independent_child_resumed": len(forked) == 1 and execution_counts[forked[0]["session_id"]] == 1 and sorted(execution_counts.values()) == [1, 2],
        "resumed_execution_returned_its_own_finding": "LEFT_SENTINEL_42" in resumed_answers,
        "no_recursive_delegation": not any(event["session_id"] in child_sessions for event in calls),
        "completion_notifications_unique": len(notices) == 3 and len(notice_ids) == len(set(notice_ids)),
        "notifications_match_executions": len(notice_tasks) == 3 and set(notice_tasks) == {event["task_id"] for event in children},
    }
    return {"passed": all(checks.values()), "checks": checks, "subagent_actions": dict(actions),
            "child_executions": len(children), "all_session_tool_calls": len(starts),
            "all_session_provider_rounds": sum(event.get("event_type") == "provider_started" for event in events),
            "parent_metrics": {key: metrics.get(key) for key in
                ("elapsed_ms", "request_count", "tool_call_count", "total_tokens", "cache_read_tokens", "uncached_input_tokens", "provider_first_token_ms", "return_code")}}


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--golutra", type=Path, default=Path("target/debug/golutra-cli"))
    parser.add_argument("--golutra-home-source", type=Path, default=Path.home() / ".golutra")
    parser.add_argument("--model", default="gpt-5.5")
    parser.add_argument("--reasoning-effort", default="medium")
    parser.add_argument("--base-url", default="https://api.golutra.cn")
    parser.add_argument("--timeout", type=float, default=240)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    binary = args.golutra.resolve(strict=True)
    with tempfile.TemporaryDirectory(prefix="golutra-subagent-smoke-") as temporary:
        root = Path(temporary)
        home, workspace, run = root / "home", root / "workspace", root / "run"
        long_bench.prepare_golutra_home(args, home)
        workspace.mkdir()
        (workspace / "left.txt").write_text("LEFT_SENTINEL_42\n", encoding="utf-8")
        (workspace / "right.txt").write_text("RIGHT_SENTINEL_84\n", encoding="utf-8")
        env = os.environ.copy()
        env["GOLUTRA_HOME"] = str(home)
        capture = paired.run_process([str(binary), "--cwd", str(workspace), "exec", "--json",
            "--approval-mode", "auto", "--yolo", "--no-project-verifier-discovery",
            "--max-elapsed-ms", str(int(args.timeout * 1000) - 5000), "--run-dir", str(run), PROMPT],
            workspace, env, args.timeout, root / "stdout.jsonl", root / "stderr.log")
        metrics = paired.parse_golutra(capture.stdout, capture.elapsed_ms, capture.return_code, run, capture.stdout_line_times_ms)
        events = read_events(root)
        report = summarize(events, metrics)
        usage = [paired.normalize_golutra_usage(event["payload"]["record"]) for event in events
                 if event.get("event_type") == "token_usage_recorded" and isinstance(event.get("payload", {}).get("record"), dict)]
        aggregate = paired.empty_metrics()
        paired.apply_usage_records(aggregate, usage, report["all_session_provider_rounds"])
        report["all_session_usage"] = {key: aggregate.get(key) for key in
            ("total_tokens", "provider_total_tokens", "uncached_input_tokens", "cache_read_tokens", "cache_write_tokens", "usage_complete", "usage_coverage")}
        report.update({"timestamp": datetime.now(timezone.utc).isoformat(), "model": args.model,
            "reasoning_effort": args.reasoning_effort, "scope": "functional live subagent smoke; parent usage excludes child provider usage"})
        args.output.parent.mkdir(parents=True, exist_ok=True)
        long_bench.write_private_text(args.output.with_suffix(".stdout.jsonl"), capture.stdout)
        long_bench.write_private_text(args.output.with_suffix(".stderr.log"), capture.stderr)
        long_bench.write_private_text(args.output.with_suffix(".events.jsonl"), "\n".join(json.dumps(event, ensure_ascii=False) for event in events) + "\n")
        args.output.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
        print(json.dumps(report, ensure_ascii=False))
        return 0 if report["passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
