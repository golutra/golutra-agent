#!/usr/bin/env python3
"""Run a four-turn Golutra/Pi/Codex long-task comparison.

The harness uses one fixture and one Responses provider/model/reasoning setting
for all three products. Credentials are copied into owner-only temporary homes,
never serialized into the report, and removed when the run exits.
"""

from __future__ import annotations

import argparse
import copy
import hashlib
import json
import math
import os
import shutil
import signal
import statistics
import subprocess
import sys
import tempfile
import time
from dataclasses import dataclass, field
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Iterable

import compare_pi_benchmark as paired


ENGINE_NAMES = ("golutra", "pi", "codex")
SCENARIO_NAMES = (
    "first_turn_cold",
    "same_session_tool_round",
    "same_thread_next_turn",
    "long_task_resume",
)
IMMUTABLE_PATHS = ("tests/test_stage1.py", "tools/background_probe.py")
SENTINEL = "LEDGER-LONG-73X"


@dataclass
class EngineState:
    name: str
    workspace: Path
    artifact_root: Path
    env: dict[str, str]
    thread_id: str | None = None
    codex_cumulative_usage: dict[str, int] | None = None
    turns: list[dict[str, Any]] = field(default_factory=list)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument(
        "--reclassify-from",
        type=Path,
        help="rebuild status fields from an existing report and retained stdout artifacts",
    )
    parser.add_argument("--golutra", type=Path, default=Path("target/debug/golutra-cli"))
    parser.add_argument(
        "--pi-root",
        type=Path,
        default=Path("../project/pi"),
    )
    parser.add_argument("--codex", default="codex")
    parser.add_argument("--provider", default="my-api")
    parser.add_argument("--model", default="gpt-5.5")
    parser.add_argument("--reasoning-effort", default="medium")
    parser.add_argument("--base-url", default="https://api.golutra.cn")
    parser.add_argument("--timeout", type=float, default=420.0)
    parser.add_argument("--max-elapsed-ms", type=int, default=360_000)
    parser.add_argument("--work-root", type=Path)
    parser.add_argument("--keep-work-root", action="store_true")
    parser.add_argument("--golutra-home-source", type=Path, default=Path.home() / ".golutra")
    parser.add_argument("--pi-agent-source", type=Path, default=Path.home() / ".pi" / "agent")
    parser.add_argument("--codex-home-source", type=Path, default=Path.home() / ".codex")
    return parser.parse_args()


def utc_now() -> str:
    return datetime.now(timezone.utc).isoformat().replace("+00:00", "Z")


def sha256_bytes(value: bytes) -> str:
    return hashlib.sha256(value).hexdigest()


def file_digest(path: Path) -> str:
    return sha256_bytes(path.read_bytes())


def tree_digest(root: Path) -> str:
    digest = hashlib.sha256()
    for path in sorted(candidate for candidate in root.rglob("*") if candidate.is_file()):
        relative = path.relative_to(root).as_posix()
        if relative.startswith((".git/", ".long-bench/")) or "__pycache__" in path.parts:
            continue
        digest.update(relative.encode("utf-8"))
        digest.update(b"\0")
        digest.update(path.read_bytes())
        digest.update(b"\0")
    return digest.hexdigest()


def private_directory(path: Path) -> None:
    path.mkdir(parents=True, exist_ok=True)
    path.chmod(0o700)


def write_private_text(path: Path, value: str) -> None:
    flags = os.O_WRONLY | os.O_CREAT | os.O_TRUNC
    descriptor = os.open(path, flags, 0o600)
    with os.fdopen(descriptor, "w", encoding="utf-8") as stream:
        stream.write(value)


def copy_private(source: Path, destination: Path) -> None:
    if not source.is_file():
        raise FileNotFoundError(source)
    shutil.copyfile(source, destination)
    destination.chmod(0o600)


def prepare_golutra_home(args: argparse.Namespace, destination: Path) -> None:
    private_directory(destination)
    source = args.golutra_home_source.resolve(strict=True)
    payload = json.loads((source / "provider.json").read_text(encoding="utf-8"))
    active = payload.get("active_profile")
    found = False
    for profile in payload.get("profiles", []):
        if profile.get("name") != active:
            continue
        if profile.get("model_id") != args.model:
            profile["model_id"] = args.model
        profile["protocol"] = "openai-responses"
        profile["base_url"] = args.base_url.rstrip("/") + "/v1"
        profile["generation_config"] = {"reasoning_effort": args.reasoning_effort}
        found = True
    if not found:
        raise ValueError(f"active Golutra provider profile not found: {active!r}")
    write_private_text(
        destination / "provider.json",
        json.dumps(payload, indent=2, ensure_ascii=True) + "\n",
    )
    copy_private(source / "credentials.json", destination / "credentials.json")


def prepare_pi_home(args: argparse.Namespace, destination: Path) -> None:
    private_directory(destination)
    source_path = args.pi_agent_source.resolve(strict=True) / "models.json"
    source = json.loads(source_path.read_text(encoding="utf-8"))
    provider = copy.deepcopy(source.get("providers", {}).get(args.provider))
    if not isinstance(provider, dict):
        raise ValueError(f"Pi provider not found: {args.provider!r}")
    models = provider.get("models")
    if not isinstance(models, list):
        raise ValueError("Pi provider models must be a list")
    selected = next(
        (copy.deepcopy(model) for model in models if model.get("id") == args.model),
        {"id": args.model},
    )
    selected.update(
        {
            "id": args.model,
            "reasoning": True,
            "input": ["text"],
            "contextWindow": 200_000,
            "maxTokens": 32_000,
            "cost": {
                "input": 0,
                "output": 0,
                "cacheRead": 0,
                "cacheWrite": 0,
            },
        }
    )
    provider["api"] = "openai-responses"
    provider["baseUrl"] = args.base_url.rstrip("/") + "/v1"
    provider["compat"] = {
        "sendSessionIdHeader": True,
        "supportsLongCacheRetention": True,
    }
    provider["models"] = [selected]
    write_private_text(
        destination / "models.json",
        json.dumps({"providers": {args.provider: provider}}, indent=2, ensure_ascii=True) + "\n",
    )
    write_private_text(destination / "settings.json", "{}\n")


def prepare_codex_home(args: argparse.Namespace, destination: Path) -> None:
    private_directory(destination)
    source = args.codex_home_source.resolve(strict=True)
    copy_private(source / "auth.json", destination / "auth.json")
    config = "\n".join(
        (
            'model_provider = "custom"',
            f'model = {json.dumps(args.model)}',
            f'model_reasoning_effort = {json.dumps(args.reasoning_effort)}',
            "disable_response_storage = true",
            "notify = []",
            "",
            "[model_providers.custom]",
            'name = "benchmark-custom"',
            f'base_url = {json.dumps(args.base_url.rstrip("/"))}',
            'wire_api = "responses"',
            "requires_openai_auth = true",
            "",
        )
    )
    write_private_text(destination / "config.toml", config)


def long_prefix() -> str:
    return "\n".join(
        f"compat-{index:04d}: immutable ordering, idempotence, recovery, and checksums remain required."
        for index in range(640)
    )


def turn_prompts() -> tuple[str, ...]:
    stage_one = """Complete stage 1 of this long-running task in the current workspace.

Implement the existing jobledger package across model.py, codec.py, ledger.py, and cli.py. Keep the public signatures already present.

Requirements:
- JobEvent.from_mapping accepts exactly event_id, job_id, state, sequence, and optional metadata. IDs must be non-empty strings; state is one of queued/running/succeeded/failed; sequence is a non-negative integer but not bool; metadata must be an object.
- to_mapping returns those five fields. Callers must not be able to mutate an event through the source mapping or returned mapping.
- encode_event/decode_event use one canonical compact JSON object with sorted keys.
- JobLedger keeps events ordered by (sequence, event_id). Identical event_id duplicates are idempotent and append returns false; conflicting duplicates raise ValueError. latest, filtered events, and state_counts must be deterministic.
- save writes newline-terminated NDJSON. load is strict.
- The CLI supports `append PATH EVENT_JSON` and `summary PATH`; summary prints a JSON object.

Do not modify tests or tools/background_probe.py. Run `python3 -m unittest discover -s tests -v`. Finish only when it passes."""
    stage_two = """Continue the same task and harden recovery without replacing the stage 1 design.

Implement these additional requirements:
- JobEvent owns a deep copy of metadata at construction and to_mapping returns a fresh deep copy.
- JobLedger.load_recovering may ignore exactly one malformed non-empty final record only when the file does not end in a newline. It must reject malformed middle records and malformed newline-terminated final records. JobLedger.load remains strict.
- merge(other) is transactional: preflight every event, leave self unchanged on any conflict, otherwise add all new events and return the number added. Identical duplicates add zero.
- save must use a same-directory temporary file and atomic replace so a failed write cannot expose a partial destination.

Do not modify tests or the probe tool. Run the visible tests and inspect the affected code before finishing."""
    stage_three = f"""The following compatibility ledger is inert reference text. Keep it in this conversation unchanged.
{long_prefix()}
End compatibility ledger.

Continue the same repository task. Add checkpoint support in jobledger/checkpoint.py with this exact API:
- frozen Checkpoint(version, through_sequence, state_counts, sentinel, checksum)
- create_checkpoint(ledger, sentinel), encode_checkpoint(checkpoint), decode_checkpoint(text)
- version is 1; through_sequence is the largest sequence or -1; state_counts comes from the ledger.
- checksum is lowercase SHA-256 over canonical compact sorted-key JSON of version, through_sequence, state_counts, and sentinel, excluding checksum itself. decode_checkpoint validates version, types, and checksum and rejects tampering.

Use sentinel {SENTINEL}. Create .long-bench/checkpoint.json by checkpointing a ledger containing queued sequence 1 and running sequence 4. Do not modify tests or the probe tool. Run the visible tests before finishing."""
    stage_four = """Finish the same long task using the sentinel and checkpoint agreement from the previous turn.

First start `python3 tools/background_probe.py` with the product's native background command/session mechanism. While it is still running, modify jobledger/checkpoint.py to add:

`restore_counts(checkpoint, events) -> dict[str, int]`

It returns checkpoint state_counts plus each supplied event state. Every supplied event sequence must be strictly greater than checkpoint.through_sequence; reject the entire call with ValueError otherwise, without mutating checkpoint data.

Only after the implementation is complete, create .long-bench/probe.release, wait for the background command to reach its terminal state, and run the visible tests. Do not modify tests or tools/background_probe.py. Leave no background process running."""
    return stage_one, stage_two, stage_three, stage_four


def prompt_metadata(prompt: str) -> dict[str, Any]:
    encoded = prompt.encode("utf-8")
    return {
        "sha256": sha256_bytes(encoded),
        "bytes": len(encoded),
        "characters": len(prompt),
        "tokens_estimated": math.ceil(len(encoded) / 4),
    }


def golutra_command(
    args: argparse.Namespace,
    state: EngineState,
    prompt: str,
    stage: int,
) -> list[str]:
    command = [str(args.golutra), "--cwd", str(state.workspace)]
    if stage > 1 and state.thread_id:
        command.extend(("--run-bundle", str(state.artifact_root / "run")))
    command.extend(
        (
            "exec",
            "--json",
            "--approval-mode",
            "auto",
            "--yolo",
            "--no-project-verifier-discovery",
            "--max-elapsed-ms",
            str(args.max_elapsed_ms),
        )
    )
    if stage == 1 or not state.thread_id:
        command.extend(("--run-dir", str(state.artifact_root / "run"), prompt))
    else:
        command.extend(("resume", state.thread_id, prompt))
    return command


def pi_command(
    args: argparse.Namespace,
    state: EngineState,
    prompt: str,
    stage: int,
) -> list[str]:
    pi_entry = args.pi_root / "packages" / "coding-agent" / "src" / "cli.ts"
    tsx_entry = args.pi_root / "node_modules" / "tsx" / "dist" / "cli.mjs"
    command = [
        "node",
        str(tsx_entry),
        str(pi_entry),
        "--provider",
        args.provider,
        "--model",
        args.model,
        "--thinking",
        args.reasoning_effort,
        "--mode",
        "json",
        "--print",
        "--session-dir",
        str(state.artifact_root / "session"),
        "--no-skills",
        "--no-extensions",
        "--no-prompt-templates",
        "--no-themes",
        "--no-context-files",
    ]
    if stage > 1:
        command.append("--continue")
    command.append(prompt)
    return command


def codex_command(
    args: argparse.Namespace,
    state: EngineState,
    prompt: str,
    stage: int,
) -> list[str]:
    command = [
        args.codex,
        "exec",
        "--json",
        "--dangerously-bypass-approvals-and-sandbox",
        "--skip-git-repo-check",
        "--ignore-rules",
        "--model",
        args.model,
        "-c",
        f'model_provider="custom"',
        "-c",
        f'model_reasoning_effort="{args.reasoning_effort}"',
        "-c",
        "notify=[]",
        "-C",
        str(state.workspace),
    ]
    if stage == 1 or not state.thread_id:
        command.append(prompt)
    else:
        command.extend(("resume", state.thread_id, prompt))
    return command


def json_lines_with_times(
    text: str,
    line_times_ms: Iterable[float],
) -> Iterable[tuple[dict[str, Any], float]]:
    lines = text.splitlines()
    times = list(line_times_ms)
    if len(lines) != len(times):
        raise ValueError("Codex stdout timing alignment mismatch")
    for line, observed_ms in zip(lines, times, strict=True):
        try:
            value = json.loads(line)
        except json.JSONDecodeError:
            continue
        if isinstance(value, dict):
            yield value, observed_ms


def subtract_cumulative_usage(
    current: dict[str, int],
    previous: dict[str, int] | None,
) -> tuple[dict[str, int], str]:
    if previous is None:
        return current, "reported_turn_total"
    if all(current.get(key, 0) >= previous.get(key, 0) for key in current):
        return (
            {key: current.get(key, 0) - previous.get(key, 0) for key in current},
            "derived_from_cumulative_turn_totals",
        )
    return current, "reported_total_reset"


def parse_codex(
    capture: paired.ProcessCapture,
    previous_usage: dict[str, int] | None,
) -> tuple[dict[str, Any], dict[str, int] | None, str | None]:
    metrics = paired.empty_metrics()
    metrics["elapsed_ms"] = round(capture.elapsed_ms, 1)
    metrics["return_code"] = capture.return_code
    metrics["request_count"] = None
    metrics["provider_requests"] = []
    metrics["provider_first_token_ms"] = None
    metrics["model_prep_ms"] = None
    events = list(json_lines_with_times(capture.stdout, capture.stdout_line_times_ms))
    thread_id = None
    completed = False
    terminal_ms = None
    first_observable_ms = None
    final_message = ""
    cumulative: dict[str, int] | None = None
    tools: dict[str, str] = {}
    for event, observed_ms in events:
        event_type = event.get("type")
        if event_type == "thread.started":
            candidate = event.get("thread_id")
            thread_id = str(candidate) if candidate else thread_id
        if event_type in {"item.started", "item.updated", "item.completed"}:
            item = event.get("item")
            if isinstance(item, dict):
                item_type = str(item.get("type") or "")
                if first_observable_ms is None and item_type not in {"todo_list"}:
                    first_observable_ms = observed_ms
                if event_type == "item.completed" and item_type == "agent_message":
                    final_message = str(item.get("text") or final_message)
                if event_type == "item.completed" and item_type in {
                    "command_execution",
                    "file_change",
                    "mcp_tool_call",
                    "collab_tool_call",
                    "web_search",
                }:
                    tools[str(item.get("id") or f"{item_type}:{len(tools)}")] = item_type
        if event_type == "turn.completed":
            usage = event.get("usage")
            if isinstance(usage, dict):
                cumulative = {
                    "input_tokens": int(usage.get("input_tokens") or 0),
                    "cached_input_tokens": int(usage.get("cached_input_tokens") or 0),
                    "cache_write_input_tokens": int(usage.get("cache_write_input_tokens") or 0),
                    "output_tokens": int(usage.get("output_tokens") or 0),
                    "reasoning_output_tokens": int(usage.get("reasoning_output_tokens") or 0),
                }
            completed = True
            terminal_ms = observed_ms
    metrics["runtime_terminal_success"] = completed
    metrics["completed"] = completed and capture.return_code == 0
    metrics["terminal_ms"] = round(terminal_ms, 1) if terminal_ms is not None else None
    metrics["first_token_ms"] = (
        round(first_observable_ms, 1) if first_observable_ms is not None else None
    )
    metrics["turn_first_token_ms"] = metrics["first_token_ms"]
    metrics["first_observable_source"] = "codex_first_non_todo_item"
    metrics["final_message"] = final_message
    metrics["tool_call_count"] = len(tools)
    metrics["tool_result_count"] = len(tools)
    metrics["tool_names"] = sorted(tools.values())
    if cumulative is None:
        return metrics, previous_usage, thread_id
    usage, usage_source = subtract_cumulative_usage(cumulative, previous_usage)
    prompt = usage["input_tokens"]
    cache_read = usage["cached_input_tokens"]
    output = usage["output_tokens"]
    metrics.update(
        {
            "raw_input_tokens": prompt,
            "prompt_tokens": prompt,
            "uncached_input_tokens": max(0, prompt - cache_read),
            "output_tokens": output,
            "reasoning_tokens": usage["reasoning_output_tokens"],
            "cache_read_tokens": cache_read,
            "cache_write_tokens": usage["cache_write_input_tokens"],
            "provider_total_tokens": prompt + output,
            "total_tokens": prompt + output,
            "usage_source": usage_source,
            "usage_complete": True,
            "usage_record_count": 1,
            "provider_total_source": "derived_input_plus_output",
        }
    )
    coverage: dict[str, dict[str, Any]] = {}
    for field_name in paired.USAGE_FIELDS:
        applicable = field_name not in {
            "tool_schema_tokens_estimated",
            "tool_result_tokens_estimated",
        }
        coverage[field_name] = {
            "reported_requests": 1 if applicable else 0,
            "expected_requests": 1 if applicable else 0,
            "complete": applicable,
            "status": "complete" if applicable else "not_applicable",
            "source": "reported"
            if field_name not in {"uncached_input_tokens", "provider_total_tokens", "total_tokens"}
            else "derived",
        }
    metrics["usage_coverage"] = coverage
    return metrics, cumulative, thread_id


def run_verifier(workspace: Path, stage: int) -> dict[str, Any]:
    started = time.monotonic()
    result = subprocess.run(
        [
            sys.executable,
            str(Path(__file__).with_name("verify_long_benchmark.py")),
            "--workspace",
            str(workspace),
            "--stage",
            str(stage),
        ],
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        timeout=90,
        check=False,
    )
    elapsed_ms = round((time.monotonic() - started) * 1000, 1)
    try:
        payload = json.loads(result.stdout.strip().splitlines()[-1])
    except (IndexError, json.JSONDecodeError):
        payload = {"passed": False, "error": result.stdout[-4000:]}
    payload["return_code"] = result.returncode
    payload["elapsed_ms"] = elapsed_ms
    return payload


def immutable_digests(workspace: Path) -> dict[str, str | None]:
    return {
        relative: file_digest(workspace / relative)
        if (workspace / relative).is_file()
        else None
        for relative in IMMUTABLE_PATHS
    }


def classify_turn(
    metrics: dict[str, Any],
    verifier: dict[str, Any],
    immutable_ok: bool,
) -> dict[str, Any]:
    """Keep task evidence, runtime lifecycle, and wrapper status separate."""
    return_code = metrics.get("return_code")
    if not isinstance(return_code, int) or isinstance(return_code, bool):
        return_code = None
    workspace_verifier_pass = verifier.get("passed") is True
    runtime_terminal_success = metrics.get("runtime_terminal_success") is True
    strict_passed = (
        workspace_verifier_pass
        and runtime_terminal_success
        and return_code == 0
        and immutable_ok
    )
    return {
        "workspace_verifier_pass": workspace_verifier_pass,
        "runtime_terminal_success": runtime_terminal_success,
        "process_return_code": return_code,
        "strict_passed": strict_passed,
    }


def terminal_event_success(engine: str, stdout: str) -> bool:
    """Read the native terminal event without conflating process exit status."""
    events = list(paired.iter_json_lines(stdout))
    candidates: list[dict[str, Any]] = list(events)
    if engine == "golutra":
        candidates.extend(paired.nested_runtime_events(events))
    result: bool | None = None
    for event in candidates:
        event_type = str(event.get("type") or event.get("event_type") or "")
        if engine == "golutra":
            if event_type in {"turn.completed", "turn_completed"}:
                status = str(event.get("status") or "completed").lower()
                result = status == "completed"
            elif event_type in {"turn.failed", "turn_failed"}:
                result = False
        elif engine == "pi":
            if event_type == "agent_end":
                result = True
        elif event_type == "turn.completed":
            status = str(event.get("status") or "completed").lower()
            result = status == "completed"
    return result is True


def aggregate_turns(turns: list[dict[str, Any]]) -> dict[str, Any]:
    """Aggregate a list of already classified turns."""
    summary: dict[str, Any] = {
        "workspace_verifier_passed": sum(
            turn.get("workspace_verifier_pass") is True for turn in turns
        ),
        "runtime_terminal_successes": sum(
            turn.get("runtime_terminal_success") is True for turn in turns
        ),
        "strict_passed": sum(turn.get("strict_passed") is True for turn in turns),
        "stages_total": len(turns),
    }
    return_codes: dict[str, int] = {}
    for turn in turns:
        value = turn.get("process_return_code")
        key = str(value) if isinstance(value, int) and not isinstance(value, bool) else "unknown"
        return_codes[key] = return_codes.get(key, 0) + 1
    summary["process_return_codes"] = return_codes
    for field_name in (
        "prompt_tokens",
        "uncached_input_tokens",
        "cache_read_tokens",
        "cache_write_tokens",
        "output_tokens",
        "reasoning_tokens",
        "provider_total_tokens",
        "tool_schema_tokens_estimated",
        "tool_result_tokens_estimated",
        "tool_call_count",
        "request_count",
    ):
        complete, partial = sum_complete(turns, field_name)
        summary[field_name] = complete
        if partial is not None:
            summary[f"{field_name}_partial"] = partial
    prompt = summary.get("prompt_tokens")
    cache_read = summary.get("cache_read_tokens")
    summary["cache_hit_ratio"] = (
        round(cache_read / prompt, 4)
        if isinstance(prompt, int) and prompt > 0 and isinstance(cache_read, int)
        else None
    )
    elapsed = [
        float(turn["metrics"]["elapsed_ms"])
        for turn in turns
        if turn["metrics"].get("elapsed_ms") is not None
    ]
    summary["elapsed_total_ms"] = round(sum(elapsed), 1)
    summary["elapsed_p50_ms"] = round(statistics.median(elapsed), 1) if elapsed else None
    summary["elapsed_max_ms"] = max(elapsed, default=None)
    first = [
        float(turn["metrics"]["first_token_ms"])
        for turn in turns
        if turn["metrics"].get("first_token_ms") is not None
    ]
    provider_first = [
        float(turn["metrics"]["provider_first_token_ms"])
        for turn in turns
        if turn["metrics"].get("provider_first_token_ms") is not None
    ]
    summary["first_observable_p50_ms"] = quantile(first, 0.5)
    summary["provider_ttft_p50_ms"] = quantile(provider_first, 0.5)
    summary["usage_complete_stages"] = sum(
        turn["metrics"].get("usage_complete") is True for turn in turns
    )
    return summary


def command_for(
    args: argparse.Namespace,
    state: EngineState,
    prompt: str,
    stage: int,
) -> list[str]:
    if state.name == "golutra":
        return golutra_command(args, state, prompt, stage)
    if state.name == "pi":
        return pi_command(args, state, prompt, stage)
    return codex_command(args, state, prompt, stage)


def parse_metrics(
    state: EngineState,
    capture: paired.ProcessCapture,
) -> dict[str, Any]:
    if state.name == "golutra":
        return paired.parse_golutra(
            capture.stdout,
            capture.elapsed_ms,
            capture.return_code,
            state.artifact_root / "run",
            capture.stdout_line_times_ms,
        )
    if state.name == "pi":
        return paired.parse_pi(
            capture.stdout,
            capture.elapsed_ms,
            capture.return_code,
            capture.stdout_line_times_ms,
        )
    metrics, cumulative, thread_id = parse_codex(capture, state.codex_cumulative_usage)
    state.codex_cumulative_usage = cumulative
    state.thread_id = state.thread_id or thread_id
    return metrics


def sanitize_local_paths(value: Any, state: EngineState) -> Any:
    """Replace temporary benchmark paths before model text enters a report."""
    if not isinstance(value, str):
        return value
    return value.replace(str(state.workspace), "<isolated-workspace>").replace(
        str(state.artifact_root), "<isolated-artifacts>"
    )


def run_turn(
    args: argparse.Namespace,
    state: EngineState,
    prompt: str,
    stage: int,
    baseline_immutable: dict[str, str | None],
) -> dict[str, Any]:
    output_root = state.artifact_root / f"stage-{stage}"
    output_root.mkdir(parents=True, exist_ok=True)
    capture = paired.run_process(
        command_for(args, state, prompt, stage),
        state.workspace,
        state.env,
        args.timeout,
        output_root / "stdout.jsonl",
        output_root / "stderr.log",
    )
    metrics = parse_metrics(state, capture)
    metrics["final_message"] = sanitize_local_paths(metrics.get("final_message"), state)
    if state.name == "golutra" and state.thread_id is None:
        state.thread_id = paired.run_bundle_thread_id(state.artifact_root / "run")
    immutable_after = immutable_digests(state.workspace)
    immutable_ok = immutable_after == baseline_immutable
    verifier = run_verifier(state.workspace, stage)
    classification = classify_turn(metrics, verifier, immutable_ok)
    turn = {
        "stage": stage,
        **classification,
        "resumed": stage > 1 and state.thread_id is not None,
        "metrics": metrics,
        "verification": verifier,
        "immutable_inputs_preserved": immutable_ok,
        "workspace_digest": tree_digest(state.workspace),
    }
    state.turns.append(turn)
    return turn


def as_int(value: Any) -> int | None:
    return value if isinstance(value, int) and not isinstance(value, bool) else None


def sum_complete(turns: list[dict[str, Any]], field_name: str) -> tuple[int | None, int | None]:
    values: list[int] = []
    for turn in turns:
        value = as_int(turn["metrics"].get(field_name))
        if value is None:
            partial = [
                as_int(candidate["metrics"].get(field_name))
                for candidate in turns
                if as_int(candidate["metrics"].get(field_name)) is not None
            ]
            return None, sum(value for value in partial if value is not None) if partial else None
        values.append(value)
    return sum(values), None


def quantile(values: list[float], proportion: float) -> float | None:
    if not values:
        return None
    ordered = sorted(values)
    index = min(len(ordered) - 1, math.ceil(proportion * len(ordered)) - 1)
    return round(ordered[index], 1)


def aggregate(state: EngineState) -> dict[str, Any]:
    return aggregate_turns(state.turns)


def display(value: Any, *, milliseconds: bool = False) -> str:
    if value is None:
        return "unknown"
    if isinstance(value, float):
        return f"{value:,.1f}" + (" ms" if milliseconds else "")
    if isinstance(value, int):
        return f"{value:,}" + (" ms" if milliseconds else "")
    return str(value)


def markdown_report(report: dict[str, Any]) -> str:
    lines = [
        "# Golutra / Pi / Codex Long-Task Benchmark",
        "",
        f"Generated: `{report['generated_at']}`",
        "",
        "## Conditions",
        "",
        f"- Model/protocol/reasoning: `{report['conditions']['model']}` / Responses / `{report['conditions']['reasoning_effort']}`",
        f"- Fixture digest: `{report['conditions']['fixture_digest']}`",
        "- Four turns: multi-file implementation, recovery repair, long-context checkpoint, background process plus resume.",
        "- Each product used its native tool surface. Project instructions, skills, extensions, and prompt templates were disabled for the fixture.",
        "- Credentials lived only in owner-only temporary homes and are absent from this report.",
        "",
        "## Aggregate",
        "",
        "| Metric | Golutra | Pi | Codex |",
        "| --- | ---: | ---: | ---: |",
    ]
    measurement_mode = report["conditions"].get("measurement_mode")
    if measurement_mode:
        status_note = str(report["conditions"].get("status_note") or "").rstrip(".")
        detail = f"; {status_note}" if status_note else ""
        lines.insert(
            10,
            f"- Measurement mode: `{measurement_mode}`{detail}.",
        )
        current_fixture = Path(__file__).with_name("fixtures") / "long_benchmark"
        if measurement_mode == "reclassified_from_retained_artifacts" and current_fixture.is_dir():
            current_digest = tree_digest(current_fixture)
            source_digest = report["conditions"].get("fixture_digest")
            if current_digest != source_digest:
                lines.insert(
                    11,
                    f"- Current fixture digest: `{current_digest}`; the retained sample predates fixture-only changes and was not rerun.",
                )
    summaries = report["summary"]
    rows = (
        ("Workspace verifier", "workspace_verifier_passed"),
        ("Runtime terminal success", "runtime_terminal_successes"),
        ("Strict pass", "strict_passed"),
        ("Process return codes", "process_return_codes"),
        ("Provider total", "provider_total_tokens"),
        ("Prompt input", "prompt_tokens"),
        ("Uncached input", "uncached_input_tokens"),
        ("Cache read", "cache_read_tokens"),
        ("Cache write", "cache_write_tokens"),
        ("Output", "output_tokens"),
        ("Reasoning output", "reasoning_tokens"),
        ("Tool schema (estimated)", "tool_schema_tokens_estimated"),
        ("Tool result (estimated)", "tool_result_tokens_estimated"),
        ("Cache hit ratio", "cache_hit_ratio"),
        ("Provider requests", "request_count"),
        ("Tool calls", "tool_call_count"),
        ("End-to-end total", "elapsed_total_ms"),
        ("End-to-end P50", "elapsed_p50_ms"),
        ("First observable P50", "first_observable_p50_ms"),
        ("Provider TTFT P50", "provider_ttft_p50_ms"),
    )
    for label, key in rows:
        values = []
        for engine in ENGINE_NAMES:
            value = summaries[engine].get(key)
            if key in {
                "workspace_verifier_passed",
                "runtime_terminal_successes",
                "strict_passed",
            }:
                rendered = f"{value}/{summaries[engine]['stages_total']}"
            elif key == "process_return_codes":
                rendered = ", ".join(
                    f"{code}:{count}" for code, count in sorted(value.items())
                ) if value else "unknown"
            elif key == "cache_hit_ratio" and value is not None:
                rendered = f"{value * 100:.1f}%"
            elif value is None and summaries[engine].get(f"{key}_partial") is not None:
                rendered = (
                    f"unknown (partial {display(summaries[engine][f'{key}_partial'])})"
                )
            else:
                rendered = display(value, milliseconds=key.endswith("_ms"))
            values.append(rendered)
        lines.append(f"| {label} | {values[0]} | {values[1]} | {values[2]} |")
    lines.extend(
        (
            "",
            "## Per Turn",
            "",
            "| Stage | Scenario | Engine | Workspace verifier | Runtime terminal | Return code | Strict | E2E | Prompt | Uncached | Cache read | Hit | Output | Requests | Tools | Provider TTFT |",
            "| ---: | --- | --- | --- | --- | ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |",
        )
    )
    for stage in report["stages"]:
        for engine in ENGINE_NAMES:
            turn = stage[engine]
            metric = turn["metrics"]
            prompt = metric.get("prompt_tokens")
            cache_read = metric.get("cache_read_tokens")
            hit = (
                f"{cache_read / prompt * 100:.1f}%"
                if isinstance(prompt, int) and prompt > 0 and isinstance(cache_read, int)
                else "unknown"
            )
            lines.append(
                "| {stage} | {scenario} | {engine} | {workspace} | {runtime} | {return_code} | {strict} | {elapsed} | {prompt} | {uncached} | {read} | {hit} | {output} | {requests} | {tools} | {ttft} |".format(
                    stage=stage["stage"],
                    scenario=stage.get("scenario", "unknown"),
                    engine=engine,
                    workspace="yes" if turn.get("workspace_verifier_pass") else "no",
                    runtime="yes" if turn.get("runtime_terminal_success") else "no",
                    return_code=display(turn.get("process_return_code")),
                    strict="yes" if turn.get("strict_passed") else "no",
                    elapsed=display(metric.get("elapsed_ms"), milliseconds=True),
                    prompt=display(prompt),
                    uncached=display(metric.get("uncached_input_tokens")),
                    read=display(cache_read),
                    hit=hit,
                    output=display(metric.get("output_tokens")),
                    requests=display(metric.get("request_count")),
                    tools=display(metric.get("tool_call_count")),
                    ttft=display(metric.get("provider_first_token_ms"), milliseconds=True),
                )
            )
    lines.extend(
        (
            "",
            "## Measurement Notes",
            "",
            "- Golutra and Pi expose provider-round events, so provider TTFT and request counts are measured from host-observed JSONL arrival times.",
            "- Codex `exec --json` exposes a turn aggregate but not provider-round timing/counts. Its provider TTFT and request count remain `unknown`; first observable item is reported separately.",
            "- Codex resume usage is cumulative. Per-turn values are derived by subtracting the previous cumulative turn total; its provider total is derived as input plus output.",
            "- Token fields are provider reported unless a row above is explicitly described as derived. Tool schema/result values are local estimates and are not included in provider totals or cross-product rankings.",
            "- `Workspace verifier` proves the fixture behavior; `Runtime terminal` proves a native terminal event; `Return code` is the wrapper process status; `Strict pass` requires all of these plus immutable inputs.",
            "- This is one controlled sample per product, not a population-level latency claim. Network order rotates by stage to reduce, not eliminate, upstream timing bias.",
            "",
        )
    )
    lines.extend(comparison_findings(report))
    return "\n".join(lines)


def percentage_delta(value: Any, baseline: Any) -> str:
    if not isinstance(value, (int, float)) or isinstance(value, bool):
        return "unknown"
    if not isinstance(baseline, (int, float)) or isinstance(baseline, bool) or baseline == 0:
        return "unknown"
    return f"{(value / baseline - 1) * 100:+.1f}%"


def ratio_display(value: Any) -> str:
    if not isinstance(value, (int, float)) or isinstance(value, bool):
        return "unknown"
    return f"{value * 100:.1f}%"


def bounded_text(value: str, limit: int = 180) -> str:
    """Keep generated diagnostic text short enough for a durable report."""
    value = " ".join(value.split())
    return value if len(value) <= limit else f"{value[:limit]}..."


def failed_stage_details(report: dict[str, Any], engine: str) -> list[str]:
    details: list[str] = []
    for stage in report.get("stages", []):
        turn = stage.get(engine) or {}
        if turn.get("strict_passed") is True:
            continue
        reasons: list[str] = []
        if turn.get("workspace_verifier_pass") is not True:
            reasons.append("workspace verifier failed")
        if turn.get("runtime_terminal_success") is not True:
            reasons.append("runtime terminal was not successful")
        process_code = turn.get("process_return_code")
        if process_code not in (None, 0):
            reasons.append(f"process return code {process_code}")
        if turn.get("immutable_inputs_preserved") is False:
            reasons.append("immutable fixture input changed")
        if not reasons:
            reasons.append("strict conditions were not all satisfied")
        scenario = stage.get("scenario", "unknown")
        details.append(f"stage {stage.get('stage', '?')} ({scenario}): {', '.join(reasons)}")
    return details


def comparison_findings(report: dict[str, Any]) -> list[str]:
    """Add an explicit, data-backed interpretation to the machine report."""
    summaries = report["summary"]
    golutra = summaries["golutra"]
    pi = summaries["pi"]
    codex = summaries["codex"]
    first_values = [
        (summary.get("first_observable_p50_ms"), engine)
        for engine, summary in summaries.items()
        if isinstance(summary.get("first_observable_p50_ms"), (int, float))
    ]
    first_winner = min(first_values) if first_values else (None, "unknown")
    cache_values = [
        (summary.get("cache_hit_ratio"), engine)
        for engine, summary in summaries.items()
        if isinstance(summary.get("cache_hit_ratio"), (int, float))
    ]
    cache_winner = max(cache_values) if cache_values else (None, "unknown")
    provider_values = [
        (summary.get("provider_total_tokens"), engine)
        for engine, summary in summaries.items()
        if isinstance(summary.get("provider_total_tokens"), (int, float))
    ]
    elapsed_values = [
        (summary.get("elapsed_total_ms"), engine)
        for engine, summary in summaries.items()
        if isinstance(summary.get("elapsed_total_ms"), (int, float))
    ]
    tool_values = [
        (summary.get("tool_call_count"), engine)
        for engine, summary in summaries.items()
        if isinstance(summary.get("tool_call_count"), (int, float))
    ]
    provider_winner = min(provider_values) if provider_values else (None, "unknown")
    elapsed_winner = min(elapsed_values) if elapsed_values else (None, "unknown")
    tool_winner = min(tool_values) if tool_values else (None, "unknown")
    failed_details = failed_stage_details(report, "golutra")
    strict_total = golutra.get("stages_total", 0)
    strict_passed = golutra.get("strict_passed", 0)
    findings = [
        "",
        "## Capability comparison",
        "",
        "| Dimension | Golutra | Pi | Codex | Practical implication |",
        "| --- | --- | --- | --- | --- |",
        "| Tool surface | Compact seven-tool runtime plus patch/background/subagent boundaries | Compact native coding-agent tools | Broader built-in execution and collaboration surface | Golutra keeps the prompt surface small; its next gain is fewer repeated calls, not more tools. |",
        "| Long-task state | Durable runtime events, verification, token budget, parent-thread/cache scope | Session continuation and compaction centered on session history | Thread/resume model with strong continuation semantics | Golutra has the right primitives, but terminal verification must not turn successful work into a failed turn. |",
        "| Cache/usage observability | Provider-round usage, coverage and local estimates are separately labeled | Provider usage and session affinity are visible; round timing is less exposed here | Turn aggregate usage; request/TTFT detail is limited in this interface | Keep Golutra's detailed diagnostics while preserving a stable provider-facing prefix. |",
        "| Background execution | Event-driven `shell_session` lifecycle with PID cleanup checks | Native background/session behavior | Native command execution and resume | Use deterministic latches and outer deadlines; never infer lifecycle from a fixed sleep. |",
        f"| Measured result | First observable P50 {display(golutra.get('first_observable_p50_ms'), milliseconds=True)}; cache {ratio_display(golutra.get('cache_hit_ratio'))}; strict {golutra.get('strict_passed', 0)}/{golutra.get('stages_total', 0)} | Provider total {display(pi.get('provider_total_tokens'))}; E2E {display(pi.get('elapsed_total_ms'), milliseconds=True)} | Provider total {display(codex.get('provider_total_tokens'))}; E2E {display(codex.get('elapsed_total_ms'), milliseconds=True)} | Measured winners: {provider_winner[1]} by provider tokens, {elapsed_winner[1]} by E2E, {tool_winner[1]} by tool calls. |",
        "",
        "## Findings",
        "",
        "### Advantages",
        "",
        f"- Golutra first observable P50 is {display(golutra.get('first_observable_p50_ms'), milliseconds=True)}, versus Pi {display(pi.get('first_observable_p50_ms'), milliseconds=True)} and Codex {display(codex.get('first_observable_p50_ms'), milliseconds=True)}; the measured winner is {first_winner[1]} at {display(first_winner[0], milliseconds=True)}.",
        f"- Golutra cache hit ratio is {ratio_display(golutra.get('cache_hit_ratio'))}, versus Pi {ratio_display(pi.get('cache_hit_ratio'))} and Codex {ratio_display(codex.get('cache_hit_ratio'))}; the measured cache-ratio winner is {cache_winner[1]} at {ratio_display(cache_winner[0])}.",
        "- Golutra exposes provider-round timing, request counts, and detailed usage coverage that are unavailable from Codex's JSON output.",
        "",
        "### Gaps",
        "",
        f"- Golutra provider total is {display(golutra.get('provider_total_tokens'))} ({percentage_delta(golutra.get('provider_total_tokens'), pi.get('provider_total_tokens'))} vs Pi), with {display(golutra.get('output_tokens'))} output tokens; extra tool/reasoning turns drive the excess.",
        f"- End-to-end total is {display(golutra.get('elapsed_total_ms'), milliseconds=True)} ({percentage_delta(golutra.get('elapsed_total_ms'), pi.get('elapsed_total_ms'))} vs Pi; {percentage_delta(golutra.get('elapsed_total_ms'), codex.get('elapsed_total_ms'))} vs Codex). Golutra makes {display(golutra.get('tool_call_count'))} tool calls versus Pi {display(pi.get('tool_call_count'))} and Codex {display(codex.get('tool_call_count'))}.",
    ]
    if failed_details:
        findings.extend(
            [
                f"- Golutra strict status is {strict_passed}/{strict_total}; failed-stage details are recorded below. A successful workspace verifier does not erase a failed runtime terminal or process status.",
                "",
                "### Golutra failed stages",
                "",
            ]
        )
        findings.extend(f"- {detail}" for detail in failed_details)
    else:
        findings.append(f"- Golutra strict status is {strict_passed}/{strict_total}; all measured stages satisfied the strict gate.")
    findings.extend(
        [
            "",
            "### Improvement priorities",
            "",
            "1. P0: treat a recoverable tool error as recoverable only after an equivalent successful retry, while preserving hard failures and the full evidence trail.",
            "2. P0: make `shell_session` completion wait for the child process to be reaped and expose one authoritative terminal event; require the caller to use the returned process id.",
            "3. P1: compact long-context projections before the next provider round and batch independent reads, reducing uncached input and repeated tool turns without changing reasoning settings.",
            "4. P1: measure long-input P95, first-token latency, stream/redraw metrics, and live-provider error recovery in a separate controlled job.",
            "5. P2: retain provider capability/usage coverage labels (`reported`, `derived`, `estimated`, `unknown`) and do not compare cross-session cache hit rates as a normal-session metric.",
            "",
        ]
    )
    return findings


def version(command: list[str], cwd: Path) -> str:
    result = subprocess.run(
        command,
        cwd=cwd,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        timeout=10,
        check=False,
    )
    return result.stdout.strip().splitlines()[0] if result.stdout.strip() else "unknown"


def cleanup_probe(workspace: Path) -> None:
    pid_path = workspace / ".long-bench" / "probe.pid"
    try:
        process_id = int(pid_path.read_text(encoding="utf-8").strip())
    except (OSError, ValueError):
        return
    try:
        details = subprocess.run(
            ["ps", "-p", str(process_id), "-o", "command="],
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            timeout=2,
            check=False,
        ).stdout
    except (OSError, subprocess.SubprocessError):
        return
    if "background_probe.py" not in details or str(workspace) not in details:
        return
    try:
        os.kill(process_id, signal.SIGTERM)
    except ProcessLookupError:
        return


def reclassify_report(report_path: Path, work_root: Path) -> dict[str, Any]:
    """Rebuild status fields from retained artifacts without provider calls."""
    report = json.loads(report_path.read_text(encoding="utf-8"))
    report["schema_version"] = 2
    report["source_report_generated_at"] = report.get(
        "source_report_generated_at", report.get("generated_at")
    )
    report["generated_at"] = utc_now()
    for stage in report.get("stages", []):
        stage_number = int(stage["stage"])
        if 1 <= stage_number <= len(SCENARIO_NAMES):
            stage["scenario"] = SCENARIO_NAMES[stage_number - 1]
        for engine in ENGINE_NAMES:
            turn = stage[engine]
            metrics = turn.setdefault("metrics", {})
            artifact = work_root / engine / "artifacts" / f"stage-{stage_number}" / "stdout.jsonl"
            if artifact.is_file():
                runtime_success = terminal_event_success(
                    engine, artifact.read_text(encoding="utf-8", errors="replace")
                )
            else:
                runtime_success = metrics.get("runtime_terminal_success") is True
            metrics["runtime_terminal_success"] = runtime_success
            verifier = turn.get("verification")
            verifier = verifier if isinstance(verifier, dict) else {}
            immutable_ok = turn.get("immutable_inputs_preserved") is True
            turn.pop("passed", None)
            turn.update(classify_turn(metrics, verifier, immutable_ok))
    report["summary"] = {
        engine: aggregate_turns(
            [stage[engine] for stage in report.get("stages", [])]
        )
        for engine in ENGINE_NAMES
    }
    report.setdefault("conditions", {})["measurement_mode"] = (
        "reclassified_from_retained_artifacts"
    )
    current_fixture = Path(__file__).with_name("fixtures") / "long_benchmark"
    if current_fixture.is_dir():
        report["conditions"]["current_fixture_digest"] = tree_digest(current_fixture)
    report["conditions"]["status_note"] = (
        "status fields were rebuilt from the original run; no provider calls were made"
    )
    return report


def write_report(output: Path, report: dict[str, Any]) -> Path:
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(json.dumps(report, indent=2, ensure_ascii=True) + "\n", encoding="utf-8")
    markdown = output.with_suffix(".md")
    markdown.write_text(markdown_report(report), encoding="utf-8")
    return markdown


def main() -> int:
    args = parse_args()
    if args.reclassify_from is not None:
        if args.work_root is None:
            raise SystemExit("--reclassify-from requires --work-root")
        source = args.reclassify_from.resolve(strict=True)
        retained = args.work_root.resolve(strict=True)
        report = reclassify_report(source, retained)
        markdown = write_report(args.output, report)
        print(
            json.dumps(
                {
                    "output": str(args.output),
                    "markdown": str(markdown),
                    "work_root": str(retained),
                }
            ),
            file=sys.stderr,
        )
        return 0
    repository_root = Path(__file__).resolve().parents[1]
    args.golutra = (repository_root / args.golutra).resolve() if not args.golutra.is_absolute() else args.golutra.resolve()
    args.pi_root = (repository_root / args.pi_root).resolve(strict=True) if not args.pi_root.is_absolute() else args.pi_root.resolve(strict=True)
    fixture = Path(__file__).with_name("fixtures") / "long_benchmark"
    fixture = fixture.resolve(strict=True)
    baseline_immutable = immutable_digests(fixture)
    fixture_digest = tree_digest(fixture)
    prompts = turn_prompts()

    external_work_root = args.work_root.resolve() if args.work_root else None
    work_context = None if external_work_root else tempfile.TemporaryDirectory(prefix="golutra-long-threeway-")
    work_root = external_work_root or Path(work_context.name)
    private_directory(work_root)
    sensitive_context = tempfile.TemporaryDirectory(prefix="golutra-long-credentials-")
    sensitive_root = Path(sensitive_context.name)
    try:
        golutra_home = sensitive_root / "golutra"
        pi_home = sensitive_root / "pi"
        codex_home = sensitive_root / "codex"
        prepare_golutra_home(args, golutra_home)
        prepare_pi_home(args, pi_home)
        prepare_codex_home(args, codex_home)

        homes = {
            "golutra": ("GOLUTRA_HOME", golutra_home),
            "pi": ("PI_CODING_AGENT_DIR", pi_home),
            "codex": ("CODEX_HOME", codex_home),
        }
        states: dict[str, EngineState] = {}
        for engine in ENGINE_NAMES:
            workspace = work_root / engine / "workspace"
            artifact_root = work_root / engine / "artifacts"
            shutil.copytree(fixture, workspace)
            artifact_root.mkdir(parents=True)
            env = os.environ.copy()
            variable, home = homes[engine]
            env[variable] = str(home)
            if engine == "pi":
                env["PI_OFFLINE"] = "1"
            states[engine] = EngineState(engine, workspace, artifact_root, env)

        stages: list[dict[str, Any]] = []
        for stage, prompt in enumerate(prompts, start=1):
            stage_result: dict[str, Any] = {
                "stage": stage,
                "scenario": SCENARIO_NAMES[stage - 1],
                "prompt": prompt_metadata(prompt),
                "execution_order": [],
            }
            offset = (stage - 1) % len(ENGINE_NAMES)
            order = ENGINE_NAMES[offset:] + ENGINE_NAMES[:offset]
            for engine in order:
                print(f"running stage {stage}: {engine}", file=sys.stderr, flush=True)
                stage_result["execution_order"].append(engine)
                stage_result[engine] = run_turn(
                    args,
                    states[engine],
                    prompt,
                    stage,
                    baseline_immutable,
                )
            stages.append(stage_result)

        report = {
                "schema_version": 2,
            "generated_at": utc_now(),
            "conditions": {
                "provider_endpoint": args.base_url,
                "protocol": "openai-responses",
                "model": args.model,
                "reasoning_effort": args.reasoning_effort,
                "fixture_digest": fixture_digest,
                "fixture_inputs_immutable": baseline_immutable,
                "turn_count": len(prompts),
                "credentials": "owner_only_temporary_homes_not_serialized",
                "work_root_retained": bool(args.keep_work_root or external_work_root),
                "timing_source": "host_monotonic_jsonl_arrival",
                "measurement_mode": "live_provider",
                "status_note": "provider calls executed under isolated temporary homes",
                "golutra_version": version([str(args.golutra), "--version"], repository_root),
                "pi_version": version(
                    ["node", "packages/coding-agent/dist/cli.js", "--version"],
                    args.pi_root,
                ),
                "codex_version": version([args.codex, "--version"], repository_root),
            },
            "stages": stages,
            "summary": {engine: aggregate(states[engine]) for engine in ENGINE_NAMES},
        }
        markdown = write_report(args.output, report)
        print(
            json.dumps(
                {
                    "output": str(args.output),
                    "markdown": str(markdown),
                    "work_root": str(work_root) if args.keep_work_root or external_work_root else None,
                }
            ),
            file=sys.stderr,
        )
        return 0 if all(summary["strict_passed"] == len(prompts) for summary in report["summary"].values()) else 1
    finally:
        for engine_root in (work_root / engine / "workspace" for engine in ENGINE_NAMES):
            cleanup_probe(engine_root)
        sensitive_context.cleanup()
        if work_context is not None and not args.keep_work_root:
            work_context.cleanup()


if __name__ == "__main__":
    raise SystemExit(main())
