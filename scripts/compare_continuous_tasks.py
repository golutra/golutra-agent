#!/usr/bin/env python3
"""同模型双端四阶段验收；复用既有独立判题器，不注入基准修复回合。"""

from __future__ import annotations

import argparse
import json
import os
import re
from pathlib import Path
import shutil
import sys
import tempfile

import compare_long_benchmark as bench
import compare_pi_benchmark as process


def delivery_timings(captured, engine):
    """终态与退出使用同一宿主单调时钟，不混用事件生产时间。"""
    timed = list(bench.json_lines_with_times(captured.stdout, captured.stdout_line_times_ms))
    candidates = list(timed)
    if engine == "golutra":
        candidates.extend(process.nested_runtime_events_timed(timed))
    terminal = min((arrival for event, arrival in candidates
                    if arrival is not None and
                    (event.get("type") or event.get("event_type")) in
                    {"turn.completed", "turn.failed", "turn_completed", "turn_failed"}),
                   default=None)
    return {"task_terminal_arrival_ms": terminal,
            "process_elapsed_ms": captured.elapsed_ms,
            "post_terminal_ms": None if terminal is None else max(0, captured.elapsed_ms - terminal)}


def tool_dispatch_waits(stdout):
    """只聚合明确提供的终态时序；缺失值不伪装成零等待。"""
    events = process.nested_runtime_events(list(process.iter_json_lines(stdout)))
    samples = {}
    for event in events:
        if event.get("event_type") != "provider_completed":
            continue
        payload = event.get("payload", {})
        value = payload.get("transport_diagnostics", {}).get("tool_ready_to_terminal_ms")
        if isinstance(value, int) and not isinstance(value, bool):
            samples[event["id"]] = value
    return list(samples.values())


def codex_command(args, state, prompt, stage):
    config = {
        "model_provider": '"benchmark"',
        "model_reasoning_effort": json.dumps(args.reasoning_effort),
        "model_providers.benchmark.name": '"same-upstream-benchmark"',
        "model_providers.benchmark.base_url": json.dumps(args.base_url.rstrip("/") + "/v1"),
        "model_providers.benchmark.wire_api": '"responses"',
        "model_providers.benchmark.env_key": '"GOLUTRA_AGENT_BENCHMARK_API_KEY"',
        "features.multi_agent": "false",
        "features.multi_agent_v2": "false",
    }
    command = [args.codex, "exec", "--json", "--dangerously-bypass-approvals-and-sandbox",
               "--skip-git-repo-check", "--ignore-rules", "--ignore-user-config",
               "-C", str(state.workspace), "-m", args.model]
    for name, value in config.items():
        command.extend(["-c", f"{name}={value}"])
    if catalog := state.env.get("CODEX_BENCHMARK_MODEL_CATALOG"):
        command.extend(["-c", f"model_catalog_json={json.dumps(catalog)}"])
    if stage > 1 and state.thread_id:
        command.extend(["resume", state.thread_id])
    return command + [prompt]


def run_stage(args, state, prompt, stage, immutable):
    output = state.artifact_root / f"stage-{stage}"
    output.mkdir(parents=True)
    command = (bench.golutra_agent_command if state.name == "golutra" else codex_command)(args, state, prompt, stage)
    if state.name == "golutra" and getattr(args, "single_task", False):
        # 仅测试宿主设定终止超时；产品任务本身不注入预算。
        index = command.index("--max-elapsed-ms")
        del command[index:index + 2]
    if state.name == "golutra" and getattr(args, "full_run_export", False):
        command.insert(command.index("exec") + 1, "--full-run-export")
    captured = process.run_process(command, state.workspace, state.env, args.timeout,
                                   output / "stdout.jsonl", output / "stderr.log")
    metrics = bench.parse_metrics(state, captured)
    metrics.update(delivery_timings(captured, state.name))
    export = re.search(r"golutra terminal export: mode=(\w+), elapsed_ms=(\d+)", getattr(captured, "stderr", ""))
    metrics["terminal_export_ms"] = int(export[2]) if export else None
    metrics["terminal_export_mode"] = export[1] if export else None
    bench.write_private_text(output / "arrival-times.json", json.dumps(captured.stdout_line_times_ms))
    metrics["tool_ready_to_terminal_samples_ms"] = tool_dispatch_waits(captured.stdout)
    metrics["final_message"] = bench.sanitize_local_paths(metrics.get("final_message"), state)
    if state.name == "golutra" and state.thread_id is None:
        state.thread_id = process.run_bundle_thread_id(state.artifact_root / "run")
    verification = (verify_single_task(state.workspace, output)
                    if getattr(args, "single_task", False)
                    else bench.run_verifier(state.workspace, stage, output))
    unchanged = immutable == bench.immutable_digests(state.workspace)
    result = {"stage": stage, "prompt": bench.prompt_metadata(prompt),
              **bench.classify_turn(metrics, verification, unchanged),
              "metrics": metrics, "verification": verification,
              "immutable_inputs_preserved": unchanged, "repair_attempts": 0}
    state.turns.append(result)
    return result


def verify_single_task(workspace, output):
    """独立判题器按阶段隔离；单任务必须验收全部交付，不能仅检查最后阶段。"""
    stages = []
    for stage in range(1, 5):
        directory = output / f"verification-stage-{stage}"
        directory.mkdir(parents=True, exist_ok=True)
        stages.append(bench.run_verifier(workspace, stage, directory))
    return {"passed": all(result.get("passed") is True for result in stages),
            "checks": [check for result in stages for check in result.get("checks", [])],
            "stages": stages,
            "error": next((result.get("error") or result.get("diagnostic")
                           for result in stages if result.get("passed") is not True), None)}


def single_task_prompt():
    """将四个交付阶段明确为一次授权，保留原验收要求及后台工作的先后关系。"""
    prompts = list(bench.turn_prompts())
    prompts[2] = prompts[2].split("End compatibility ledger.\n\n", 1)[1]
    return ("Complete all four phases below in one autonomous task. Do not stop after an intermediate phase. "
            "Preserve the public API, original tests and probe tool; verify the final combined deliverable.\n\n"
            + "\n\n".join(prompts)).replace(
                "use one shell_session wait with wait_for_terminal=true and a bounded wait_ms",
                "use the product's native bounded wait mechanism"
            ) + "\nWork in this workspace only. Do not delegate to subagents."


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--single-task", action="store_true", help="Complete all four phases in one autonomous task; no injected repair turns")
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--golutra", type=Path, default=Path("target/release/golutra-agent"))
    parser.add_argument("--engines", nargs="+", choices=["golutra", "codex"], default=["golutra", "codex"])
    parser.add_argument("--full-run-export", action="store_true", help="Measure full audit export explicitly (candidate binaries only)")
    parser.add_argument("--codex", default="codex")
    parser.add_argument("--model", default="gpt-5.6-sol")
    parser.add_argument("--reasoning-effort", default="medium")
    parser.add_argument("--base-url", default="https://api.golutra.cn")
    parser.add_argument("--codex-model-catalog", type=Path,
                        help="Optional local Codex catalog: retain model metadata but disable Responses Lite for conventional Responses gateways")
    parser.add_argument("--timeout", type=float, default=600)
    parser.add_argument("--golutra-agent-home-source", type=Path, default=Path.home() / ".golutra-agent")
    args = parser.parse_args()
    args.golutra = args.golutra.resolve(strict=True)
    args.base_url = args.base_url.rstrip("/").removesuffix("/v1")
    args.max_elapsed_ms = int(args.timeout * 1000) - 5_000
    fixture = Path(__file__).parent / "fixtures" / "long_benchmark"
    immutable = bench.immutable_digests(fixture)
    source = args.golutra_agent_home_source
    config = json.loads((source / "provider.json").read_text())
    profile = next(p for p in config["profiles"] if p["name"] == config["active_profile"])
    credentials = json.loads((source / "credentials.json").read_text())
    secret = credentials["credentials"][profile["credential_ref"]["id"]]["value"]
    args.output = args.output.resolve()
    args.output.parent.mkdir(parents=True, exist_ok=True)
    # 保留任务与审计产物用于复核，临时凭据与登录目录在 finally 中销毁。
    work = Path(tempfile.mkdtemp(prefix="golutra-continuous-work-"))
    report = {"started_at": bench.utc_now(), "model": args.model,
              "engines": list(dict.fromkeys(args.engines)), "full_run_export_requested": args.full_run_export,
              "golutra_binary_sha256": bench.file_digest(args.golutra),
              "reasoning_effort": args.reasoning_effort, "protocol": "openai-responses",
              "base_url": args.base_url, "fixture_sha256": bench.tree_digest(fixture),
              "benchmark_repair_turns": 0, "work_root": str(work),
              "codex_conventional_responses_override": args.codex_model_catalog is not None,
              "versions": {"golutra": bench.version([str(args.golutra), "--version"], work),
                           "codex": bench.version([args.codex, "--version"], work)},
              "scope": ("One autonomous four-phase coding task; measured duration, not a multi-hour claim." if args.single_task else "Four sequential user stages with process resume, not a multi-hour autonomous coding claim."),
              "stages": []}
    states = {}
    try:
        with tempfile.TemporaryDirectory(prefix="golutra-continuous-credentials-") as sensitive:
            for engine in dict.fromkeys(args.engines):
                home = Path(sensitive) / engine
                env = os.environ.copy()
                if engine == "golutra":
                    bench.prepare_golutra_home(args, home)
                    env["GOLUTRA_AGENT_HOME"] = str(home)
                else:
                    bench.private_directory(home)
                    env["CODEX_HOME"] = str(home)
                    env["GOLUTRA_AGENT_BENCHMARK_API_KEY"] = secret
                    if args.codex_model_catalog is not None:
                        models = json.loads(args.codex_model_catalog.read_text())["models"]
                        model = next(m for m in models if m["slug"] == args.model)
                        model["use_responses_lite"] = False
                        catalog = home / "models.json"
                        bench.write_private_text(catalog, json.dumps({"models": [model]}))
                        env["CODEX_BENCHMARK_MODEL_CATALOG"] = str(catalog)
                workspace = work / engine / "workspace"
                shutil.copytree(fixture, workspace)
                artifacts = work / engine / "artifacts"
                artifacts.mkdir()
                states[engine] = bench.EngineState(engine, workspace, artifacts, env)
            stages = [(4, single_task_prompt())] if args.single_task else list(enumerate(bench.turn_prompts(), 1))
            for stage, prompt in stages:
                # 同一提示不指定某产品专属工具名，禁止基准意外变成子代理测评。
                prompt = prompt.replace("use one shell_session wait with wait_for_terminal=true and a bounded wait_ms",
                                        "use the product's native bounded wait mechanism")
                prompt += "\nWork in this workspace only. Do not delegate to subagents."
                # 单任务按显式 engines 顺序运行，复测时反转顺序以减少时段偏差。
                order = list(dict.fromkeys(args.engines)) if args.single_task else (["golutra", "codex"] if stage % 2 else ["codex", "golutra"])
                order = [engine for engine in order if engine in states]
                results = {"stage": stage, "order": order}
                for engine in order:
                    print(f"running stage {stage}: {engine}", file=sys.stderr, flush=True)
                    results[engine] = run_stage(args, states[engine], prompt, stage, immutable)
                    print(json.dumps({"stage": stage, "engine": engine,
                                      "passed": results[engine]["strict_passed"],
                                      "elapsed_ms": results[engine]["metrics"].get("elapsed_ms")}), flush=True)
                report["stages"].append(results)
                report["summary"] = {engine: bench.aggregate(state) for engine, state in states.items()}
                for engine, state in states.items():
                    for metric in ["task_terminal_arrival_ms", "process_elapsed_ms", "post_terminal_ms", "terminal_export_ms"]:
                        values = [turn["metrics"].get(metric) for turn in state.turns]
                        report["summary"][engine][metric] = sum(values) if all(value is not None for value in values) else None
                report["coding_comparison_eligible"] = set(states) == {"golutra", "codex"} and all(
                    (report["summary"][engine].get("tool_call_count") or 0) > 0 for engine in states)
                bench.write_private_text(args.output, json.dumps(report, indent=2) + "\n")
    finally:
        for state in states.values():
            bench.cleanup_probe(state.workspace)
        report["finished_at"] = bench.utc_now()
        bench.write_private_text(args.output, json.dumps(report, indent=2) + "\n")
    return 0 if all(s[e]["strict_passed"] for s in report["stages"] for e in states) else 1


if __name__ == "__main__":
    raise SystemExit(main())
