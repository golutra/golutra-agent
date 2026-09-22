#!/usr/bin/env python3
"""冻结任务、隔离运行、交替配对；复用现有采集器并区分 Codex 条目与真实工具调用。"""

import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import tempfile

import compare_continuous_tasks as continuous
import compare_long_benchmark as bench
import compare_optimization_variants as development
import compare_pi_benchmark as process
import generalization_tasks as holdout
import prompt_behavior_tasks as behavior


def task_spec(name):
    """开发集与留出集共用调度，但各自保留独立验收器。"""
    if name in holdout.TASKS:
        return holdout.TASKS[name]
    if name in behavior.TASKS:
        return behavior.TASKS[name]
    return {**development.TASKS[name], "files": development.task_files(name)}


def task_digest(spec):
    """提示、初始文件和独立验收共同锁定任务，任何一项变化都产生新实验身份。"""
    return hashlib.sha256(json.dumps(spec, sort_keys=True).encode()).hexdigest()


def result_passed(metrics, accepted, preserved):
    """运行时完成、独立交付、不可变输入必须同时成立。"""
    return bench.classify_turn(metrics, {"passed": accepted}, preserved)["strict_passed"]


def recovery_summary(stdout):
    """最终 adapter 诊断不包含前次失败；单独统计共享会话层真正开始的恢复尝试。"""
    events = process.nested_runtime_events(list(process.iter_json_lines(stdout)))
    retries = [event.get("payload", {}).get("recovery", {}) for event in events
               if event.get("event_type") == "retry_scheduled"
               and event.get("payload", {}).get("recovery", {}).get("phase") == "retrying"]
    return {"retry_attempts": len(retries),
            "network_retry_attempts": sum(bool(item.get("network")) for item in retries),
            "http_statuses": [item.get("error_metadata", {}).get("http_status") for item in retries]}


def prepare_home(args, home):
    """配置路由模式不把 Anthropic 等现有凭据强行送到 Responses 地址。"""
    if getattr(args, "configured_provider", False):
        source = args.golutra_agent_home_source.resolve(strict=True)
        for name in ["provider.json", "credentials.json"]:
            bench.copy_private(source / name, home / name)
    else:
        bench.prepare_golutra_home(args, home)


def run_attempt(args, root, label, binary, task, repeat):
    """凭据仅进入临时私有目录和子进程环境，报告只保留执行事实与验收结果。"""
    spec = task_spec(task)
    directory = root / f"{repeat}-{task}-{label}"
    workspace = directory / "workspace"
    workspace.mkdir(parents=True)
    for name, content in spec["files"].items():
        path = workspace / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(content)
    protected = {name: bench.file_digest(workspace / name) for name in spec["files"]
                 if Path(name).name.startswith("test_existing") or name in bench.IMMUTABLE_PATHS}
    artifacts = directory / "artifacts"
    artifacts.mkdir()
    with tempfile.TemporaryDirectory(prefix="golutra-suite-auth-") as sensitive:
        home = Path(sensitive)
        prepare_home(args, home)
        env = os.environ.copy()
        engine = "codex" if label == "codex" else "golutra"
        state = bench.EngineState(engine, workspace, artifacts, env)
        if engine == "codex":
            config = json.loads((home / "provider.json").read_text())
            profile = next(p for p in config["profiles"] if p["name"] == config["active_profile"])
            credentials = json.loads((home / "credentials.json").read_text())
            env["GOLUTRA_AGENT_BENCHMARK_API_KEY"] = credentials["credentials"][profile["credential_ref"]["id"]]["value"]
            env["CODEX_HOME"] = str(home)
            if args.codex_model_catalog:
                catalog = json.loads(args.codex_model_catalog.read_text())
                model = next(m for m in catalog["models"] if m["slug"] == args.model)
                model["use_responses_lite"] = False
                catalog_path = home / "models.json"
                bench.write_private_text(catalog_path, json.dumps({"models": [model]}))
                env["CODEX_BENCHMARK_MODEL_CATALOG"] = str(catalog_path)
            command = continuous.codex_command(args, state, spec["prompt"], 1)
        else:
            env["GOLUTRA_AGENT_HOME"] = str(home)
            command = [str(binary), "--cwd", str(workspace), "exec", "--json", "--ephemeral",
                       "--run-dir", str(artifacts / "run"), "--yolo", "--no-project-verifier-discovery",
                       spec["prompt"]]
        captured = process.run_process(command, workspace, env, args.timeout,
                                       artifacts / "stdout.jsonl", artifacts / "stderr.log")
        metrics = bench.parse_metrics(state, captured)
        metrics.update(continuous.delivery_timings(captured, engine))
        metrics["tool_count_source"] = "completed_cli_items" if engine == "codex" else "runtime_tool_calls"
        metrics["runtime_recovery"] = recovery_summary(captured.stdout) if engine == "golutra" else None
        bench.write_private_text(artifacts / "arrival-times.json", json.dumps(captured.stdout_line_times_ms))
    if task in behavior.TASKS:
        accepted, detail = behavior.verify(task, workspace, metrics)
    else:
        verify = holdout.verify if task in holdout.TASKS else development.verify
        accepted, detail = verify(task, workspace)
    preserved = all((workspace / name).is_file() and bench.file_digest(workspace / name) == digest
                    for name, digest in protected.items())
    return {"variant": label, "task": task, "repeat": repeat,
            "strict_pass": result_passed(metrics, accepted, preserved),
            "independent_pass": accepted, "immutable_inputs_preserved": preserved,
            "detail": detail, "metrics": metrics}


def run(args):
    """交替执行冻结版本；失败样本也落盘，不自动追加提示或替换结果。"""
    if getattr(args, "configured_provider", False):
        if args.include_codex:
            raise ValueError("configured-provider compares Golutra variants only")
        payload = json.loads((args.golutra_agent_home_source / "provider.json").read_text())
        profile = next(p for p in payload["profiles"] if p["name"] == payload["active_profile"])
        args.model = profile["model_id"]
        args.base_url = profile.get("base_url")
        args.reasoning_effort = profile.get("generation_config", {}).get("reasoning_effort")
    variants = [(name, Path(path).resolve(strict=True))
                for name, path in (value.split("=", 1) for value in args.variant)]
    if any(name == "codex" for name, _ in variants):
        raise ValueError("codex is reserved for --include-codex")
    if args.include_codex:
        variants.append(("codex", Path(shutil.which(args.codex) or args.codex).resolve(strict=True)))
    if not variants or len({name for name, _ in variants}) != len(variants):
        raise ValueError("require distinct variants; codex is reserved for --include-codex")
    tasks = args.task or list(holdout.TASKS)
    if any(task.endswith("feedback") or task in {"continuous", "stage_review"} for task in tasks):
        raise ValueError("use the existing specialized runner for verifier-feedback/background tasks")
    root = Path(tempfile.mkdtemp(prefix="golutra-generalization-"))
    report = {"started_at": bench.utc_now(), "work_root": str(root), "model": args.model,
              "reasoning_effort": args.reasoning_effort, "base_url": args.base_url,
              "task_digests": {name: task_digest(task_spec(name)) for name in tasks},
              "versions": {name: bench.version([str(path), "--version"], root) for name, path in variants},
              "binary_or_launcher_sha256": {name: bench.file_digest(path) for name, path in variants},
              "scope": "Autonomous coding tasks across domains; no multi-hour claim; no repair prompts.",
              "attempts": []}
    bench.write_private_text(args.output, json.dumps(report, indent=2))
    for repeat in range(args.repeats):
        for index, task in enumerate(tasks):
            order = variants if (repeat + index) % 2 == 0 else list(reversed(variants))
            for name, binary in order:
                print(f"running {repeat + 1}/{args.repeats} {task} {name}", flush=True)
                result = run_attempt(args, root, name, binary, task, repeat)
                report["attempts"].append(result)
                bench.write_private_text(args.output, json.dumps(report, indent=2))
                print(json.dumps({key: result[key] for key in ("variant", "task", "repeat", "strict_pass")})
                      + f" elapsed_ms={result['metrics']['elapsed_ms']:.0f}", flush=True)
    report["finished_at"] = bench.utc_now()
    bench.write_private_text(args.output, json.dumps(report, indent=2))


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--variant", action="append", default=[])
    parser.add_argument("--include-codex", action="store_true")
    parser.add_argument("--configured-provider", action="store_true",
                        help="Preserve the source provider's protocol, model, endpoint and generation settings")
    parser.add_argument("--codex", default="codex")
    parser.add_argument("--codex-model-catalog", type=Path)
    parser.add_argument("--task", action="append", choices=list(holdout.TASKS) + list(behavior.TASKS) + ["parser_fix", "multi_file"])
    parser.add_argument("--repeats", type=int, default=2)
    parser.add_argument("--timeout", type=float, default=600)
    parser.add_argument("--model", default="gpt-5.6-sol")
    parser.add_argument("--reasoning-effort", default="medium")
    parser.add_argument("--base-url", default="https://api.golutra.cn")
    parser.add_argument("--golutra-agent-home-source", type=Path, default=Path.home() / ".golutra-agent")
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    args.base_url = args.base_url.rstrip("/").removesuffix("/v1")
    run(args)
