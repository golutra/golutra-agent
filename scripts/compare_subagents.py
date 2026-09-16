#!/usr/bin/env python3
"""Run isolated, same-credential subagent samples; retain private raw evidence."""
from __future__ import annotations

import argparse
import hashlib
import json
import os
import shutil
import subprocess
import tempfile
from collections import Counter
from datetime import datetime, timezone
from pathlib import Path

import compare_long_benchmark as long_bench
import compare_pi_benchmark as paired
import smoke_subagents as smoke
import subagent_benchmark_evidence as evidence
import subagent_benchmark_tasks as tasks


LIFECYCLE = """Use real subagents to perform this lifecycle task.
Parent-only context marker: FORK_CONTEXT_73X.
Launch exactly two children, with concurrent execution:
A has independent context. Ask A to read left.txt and return its exact sentinel.
B inherits the parent context. Ask B to read right.txt and return its exact
sentinel plus the parent-only context marker. Do not copy the marker into B's
task text. This checks actual context inheritance.
Neither child may modify files or delegate further. Use the inherited model and
reasoning settings, with the default child role, for both children.
Observe both terminal results, continuing bounded waits when necessary without
spawning replacements. Then give A a new task in that SAME child session: repeat
its earlier sentinel from its history. Observe that new execution's final answer.
Report the two sentinels and inherited marker based on real child findings.
Report any failures honestly. Do not create other children.
"""

HINTS = {
    "golutra": """Interface: subagent spawn uses run_in_background=true and
context=independent for A, context=fork for B. Use wait with child_session_ids,
wait_mode=all, wait_ms=60000; resume A with its returned child_session_id.""",
    "codex": """Interface: spawn_agent uses fork_context=false for A and
fork_context=true for B. Use wait to observe both children; send_input starts
the next task in A's same session. Do not close A before its continuation.""",
    "codex-v2": """Interface: spawn_agent uses fork_turns=none for A and
fork_turns=all for B. Use wait_agent to observe completion notifications;
followup_task starts the next task in A's same session. send_message alone does
not start a new task. Keep task names a and b.""",
}


def prepare_home(args, home: Path, engine: str) -> dict[str, str]:
    env = os.environ.copy()
    # 同一凭据供两个隔离客户端使用，不打印密钥，也不改用户配置。
    source = args.golutra_home_source
    provider = json.loads((source / "provider.json").read_text())
    profile = next(p for p in provider["profiles"] if p["name"] == provider["active_profile"])
    credentials = json.loads((source / "credentials.json").read_text())
    secret = credentials["credentials"][profile["credential_ref"]["id"]]["value"]
    if engine == "golutra":
        long_bench.prepare_golutra_home(args, home)
        long_bench.write_private_text(home / "runtime.json", '{"subagent_max_concurrent":10}\n')
        env["GOLUTRA_HOME"] = str(home)
    else:
        long_bench.private_directory(home)
        env["CODEX_HOME"] = str(home)
        env["GOLUTRA_BENCHMARK_API_KEY"] = secret
    return env


def command(args, engine: str, workspace: Path, run: Path, prompt: str) -> list[str]:
    if engine == "golutra":
        return [str(args.golutra.resolve()), "--cwd", str(workspace), "exec", "--json",
                "--approval-mode", "auto", "--yolo", "--no-project-verifier-discovery",
                "--max-elapsed-ms", str(int(args.timeout * 1000) - 5000),
                "--run-dir", str(run), prompt]
    config = {
        "model_provider": '"benchmark"',
        "model_reasoning_effort": json.dumps(args.reasoning_effort),
        "model_providers.benchmark.name": '"same-upstream-benchmark"',
        "model_providers.benchmark.base_url": json.dumps(args.base_url.rstrip("/") + "/v1"),
        "model_providers.benchmark.wire_api": '"responses"',
        "model_providers.benchmark.env_key": '"GOLUTRA_BENCHMARK_API_KEY"',
        "agents.max_concurrent_threads_per_session": "10",
        "features.multi_agent": "true",
        "features.multi_agent_v2": str(engine == "codex-v2").lower(),
    }
    result = [args.codex, "exec", "--json", "--dangerously-bypass-approvals-and-sandbox",
              "--skip-git-repo-check", "--ignore-rules", "--ignore-user-config",
              "-C", str(workspace), "-m", args.model]
    for key, value in config.items():
        result.extend(["-c", f"{key}={value}"])
    return result + [prompt]


def read_rollouts(home: Path) -> list[dict]:
    records = []
    for path in sorted(home.glob("sessions/**/*.jsonl")):
        for line in path.read_text().splitlines():
            if line.strip():
                record = json.loads(line)
                record["_rollout"] = str(path.relative_to(home))
                records.append(record)
    return records


def scenario_prompt(scenario: str, engine: str) -> str:
    objective = tasks.CODING if scenario == "coding" else LIFECYCLE
    if scenario == "long-fork":
        objective = "Parent-only context marker: FORK_CONTEXT_73X.\n" + "\n".join(
            f"Reference-{i:04d}: preserve execution identity, strict verification, and actual observed facts."
            for i in range(1400)) + "\n" + LIFECYCLE.replace("Parent-only context marker: FORK_CONTEXT_73X.\n", "")
    hint = HINTS[engine]
    if scenario == "coding":
        hint = hint.replace("context=fork for B", "context=independent for B").replace(
            "fork_context=true for B", "fork_context=false for B").replace(
            "fork_turns=all for B", "fork_turns=none for B")
        hint = hint.split("; resume A")[0].split("; send_input starts")[0].split(";\nfollowup_task")[0]
    if scenario == "cancel":
        objective, hint = tasks.CANCEL, tasks.CANCEL_HINTS[engine]
    if scenario == "fanout":
        objective = tasks.FANOUT
        hint = {"golutra": "Use subagent run_in_background=true, context=independent and wait child_session_ids with wait_mode=all.",
                "codex": "Use spawn_agent fork_context=false and wait_agent for the returned child handles.",
                "codex-v2": "Use spawn_agent fork_turns=none and wait_agent for completion notifications."}[engine]
    return objective + "\n" + hint


def run_sample(args, engine: str, index: int) -> dict:
    stem = args.output / f"{index:02d}-{engine}-{args.scenario}"
    with tempfile.TemporaryDirectory(prefix="golutra-subagent-compare-") as temporary:
        root = Path(temporary)
        root.chmod(0o700)
        home, workspace, run = root / "home", root / "workspace", root / "run"
        env = prepare_home(args, home, engine)
        workspace.mkdir()
        initial = tasks.setup(workspace, args.scenario)
        prompt = scenario_prompt(args.scenario, engine)
        capture = paired.run_process(command(args, engine, workspace, run, prompt),
            workspace, env, args.timeout, stem.with_suffix(".stdout.jsonl"),
            stem.with_suffix(".stderr.log"))
        if engine == "golutra":
            events = smoke.read_events(root)
            metrics = paired.parse_golutra(capture.stdout, capture.elapsed_ms,
                capture.return_code, run, capture.stdout_line_times_ms)
            # 通用指标解析器只保留 512 字符展示摘要；验收必须读取原任务的完整回答。
            metrics["final_message"] = evidence.golutra_parent_answer(events)
            report = smoke.summarize(events, metrics)
            report["tool_names"] = dict(Counter(e["payload"]["tool_name"] for e in events if e.get("event_type") == "tool_started"))
            usage = [paired.normalize_golutra_usage(event["payload"]["record"])
                     for event in events if event.get("event_type") == "token_usage_recorded"]
            aggregate = paired.empty_metrics()
            paired.apply_usage_records(aggregate, usage, report["all_session_provider_rounds"])
            report["usage"] = {key: aggregate.get(key) for key in
                ("total_tokens", "total_tokens_partial", "prompt_tokens", "output_tokens", "reasoning_tokens", "uncached_input_tokens", "uncached_input_tokens_partial", "cache_read_tokens", "cache_read_tokens_partial",
                 "cache_write_tokens", "usage_complete", "usage_coverage")}
        else:
            events = read_rollouts(home)
            metrics, _, _ = long_bench.parse_codex(capture, None)
            report = evidence.summarize_codex(events, metrics)
        if args.scenario == "cancel":
            report["checks"] = evidence.cancellation_checks(engine, events, metrics)
            report["passed"] = all(report["checks"].values())
        if args.scenario == "fanout":
            report.update(evidence.fanout_checks(engine, events, metrics, [tasks.fanout_marker(i) for i in range(10)]))
        report["workspace_verification"] = tasks.verify(workspace, args.scenario, initial)
        if args.scenario == "coding":
            report["checks"] = {key: report["checks"][key] for key in
                ("parent_succeeded", "exactly_two_child_sessions", "child_execution_intervals_overlap", "no_recursive_delegation")}
            report["checks"]["two_child_executions"] = report["child_executions"] == 2
            report["checks"]["workspace_verifier_passed"] = report["workspace_verification"]["passed"]
            report["passed"] = all(report["checks"].values())
            for name in ("labels.py", "chunks.py", "test_work.py"):
                if (workspace / name).exists():
                    long_bench.write_private_text(stem.with_suffix(f".{name}"), (workspace / name).read_text())
        else:
            report["passed"] &= report["workspace_verification"]["passed"]
        report.update({"engine": engine, "scenario": args.scenario,
            "elapsed_ms": round(capture.elapsed_ms, 1), "return_code": capture.return_code,
            "timestamp": datetime.now(timezone.utc).isoformat(), "model": args.model,
            "reasoning_effort": args.reasoning_effort, "same_credential": True,
            "concurrency_limit": 10,
            "fixture_sha256": hashlib.sha256(json.dumps({"prompt": prompt, "files": initial}, sort_keys=True).encode()).hexdigest(),
            "codex_version": subprocess.check_output([args.codex, "--version"], text=True).strip() if engine != "golutra" else None,
            "executable_or_launcher_sha256": hashlib.sha256(Path(shutil.which(args.codex) if engine != "golutra"
                else args.golutra).resolve().read_bytes()).hexdigest()})
        long_bench.write_private_text(stem.with_suffix(".events.jsonl"),
            "\n".join(json.dumps(event, ensure_ascii=False) for event in events) + "\n")
        long_bench.write_private_text(stem.with_suffix(".json"), json.dumps(report, indent=2))
        return report


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--golutra", type=Path, default=Path("target/debug/golutra-cli"))
    parser.add_argument("--codex", default="codex")
    parser.add_argument("--golutra-home-source", type=Path, default=Path.home() / ".golutra")
    parser.add_argument("--model", default="gpt-5.5")
    parser.add_argument("--reasoning-effort", default="medium")
    parser.add_argument("--base-url", default="https://api.golutra.cn")
    parser.add_argument("--engines", nargs="+", choices=tuple(HINTS), default=list(HINTS))
    parser.add_argument("--scenario", choices=("lifecycle", "coding", "long-fork", "cancel", "fanout"), default="lifecycle")
    parser.add_argument("--timeout", type=float, default=240)
    parser.add_argument("--repeats", type=int, default=1)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    # 原始证据不覆盖；父目录私有，运行过程中产生的 stdout 也不公开。
    args.output.mkdir(parents=True, exist_ok=False, mode=0o700)
    any_failed = False
    for index in range(args.repeats):
        order = args.engines if index % 2 == 0 else list(reversed(args.engines))
        for engine in order:
            report = run_sample(args, engine, index)
            any_failed |= report.get("passed") is not True
            print(json.dumps({key: report.get(key) for key in
                ("engine", "scenario", "passed", "elapsed_ms", "all_session_tool_calls")}
                | {"usage": {key: report["usage"].get(key) for key in
                    ("total_tokens", "total_tokens_partial", "uncached_input_tokens", "cache_read_tokens")}},
                ensure_ascii=False), flush=True)
    return 1 if any_failed else 0


if __name__ == "__main__":
    raise SystemExit(main())
