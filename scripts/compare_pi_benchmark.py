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
import signal
import shutil
import subprocess
import sys
import tempfile
import threading
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


@dataclass(frozen=True)
class ProcessCapture:
    stdout: str
    stderr: str
    return_code: int
    elapsed_ms: float
    stdout_line_times_ms: tuple[float, ...]


PROCESS_STOP_TIMEOUT_SECONDS = 3.0
PIPE_DRAIN_TIMEOUT_SECONDS = 1.0
PIPE_CLOSE_JOIN_SECONDS = 0.2
# durable 事件与 projection 的时间可能有极小调度抖动，超过该范围不再猜测同轮。
PROVIDER_START_MATCH_TOLERANCE_MS = 1.0


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
    for value, _ in iter_timed_json_lines(text):
        yield value


def iter_timed_json_lines(
    text: str,
    line_times_ms: Iterable[float] | None = None,
) -> Iterable[tuple[dict[str, Any], float | None]]:
    lines = text.splitlines()
    arrivals = list(line_times_ms) if line_times_ms is not None else None
    if arrivals is not None:
        if len(arrivals) != len(lines) or any(value is None for value in arrivals):
            raise ValueError("line_times_ms must contain one timestamp for every stdout line")
    for index, line in enumerate(lines):
        arrival_ms = arrivals[index] if arrivals is not None else None
        try:
            value = json.loads(line)
        except json.JSONDecodeError:
            continue
        if isinstance(value, dict):
            yield value, arrival_ms


def nested_runtime_events(events: Iterable[dict[str, Any]]) -> Iterable[dict[str, Any]]:
    timed = ((event, None) for event in events)
    for event, _ in nested_runtime_events_timed(timed):
        yield event


_RUNTIME_EVENT_PRESENTATION_KEYS = frozenset(
    {"id", "runtime_event_id", "sequence_no", "parent_event_id", "causal_links", "payload_ref"}
)


def runtime_event_fingerprint(event: dict[str, Any]) -> str:
    """为缺少 durable id 的事件生成与投影形态无关的身份。"""
    canonical = {
        key: value
        for key, value in event.items()
        if key not in _RUNTIME_EVENT_PRESENTATION_KEYS
    }
    payload = canonical.get("payload")
    if (
        isinstance(payload, dict)
        and payload.get("timestamp") == canonical.get("timestamp")
    ):
        payload = dict(payload)
        payload.pop("timestamp", None)
        canonical["payload"] = payload
    return json.dumps(canonical, sort_keys=True, separators=(",", ":"), ensure_ascii=True)


def nested_runtime_events_timed(
    events: Iterable[tuple[dict[str, Any], float | None]],
) -> Iterable[tuple[dict[str, Any], float | None]]:
    """Normalize durable runtime events and their item projections.

    Golutra's JSONL stream exposes provider/tool events twice in two different
    shapes depending on the consumer: durable facts are wrapped as
    ``runtime.event`` while the user-facing item stream stores the same wire
    event under ``item.data``.  The benchmark must understand both shapes and
    deduplicate by event id so counts remain request-based rather than frame-
    based.
    """
    seen_ids: set[str] = set()
    seen_fingerprints: set[str] = set()
    for outer, arrival_ms in events:
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
            fingerprint = runtime_event_fingerprint(event)
            if fingerprint in seen_fingerprints:
                continue
            seen_fingerprints.add(fingerprint)
            if event_id:
                seen_ids.add(event_id)
            yield event, arrival_ms


def empty_metrics() -> dict[str, Any]:
    return {
        "completed": False,
        "return_code": None,
        "elapsed_ms": None,
        "process_first_event_ms": None,
        "model_prep_ms": None,
        "first_token_ms": None,
        "turn_first_token_ms": None,
        "provider_first_token_ms": None,
        "terminal_ms": None,
        "request_count": 0,
        "tool_call_count": 0,
        "tool_result_count": 0,
        "tool_names": [],
        "raw_input_tokens": None,
        "prompt_tokens": None,
        "uncached_input_tokens": None,
        "planned_input_tokens": None,
        "output_tokens": None,
        "reasoning_tokens": None,
        "cache_read_tokens": None,
        "cache_write_tokens": None,
        "provider_total_tokens": None,
        "total_tokens": None,
        "tool_schema_tokens_estimated": None,
        "tool_result_tokens_estimated": None,
        "usage_source": "unknown",
        "usage_complete": False,
        "usage_record_count": 0,
        "usage_coverage": {},
        "prompt_semantics": "context",
        "total_semantics": "provider_billing",
        "cost": None,
        "cost_source": "unknown",
        "final_message": "",
    }


USAGE_FIELDS = (
    "raw_input_tokens",
    "prompt_tokens",
    "uncached_input_tokens",
    "output_tokens",
    "reasoning_tokens",
    "cache_read_tokens",
    "cache_write_tokens",
    "provider_total_tokens",
    "total_tokens",
    "tool_schema_tokens_estimated",
    "tool_result_tokens_estimated",
)

ESTIMATED_USAGE_FIELDS = frozenset(
    {
        "tool_schema_tokens_estimated",
        "tool_result_tokens_estimated",
    }
)


def normalize_golutra_usage(record: dict[str, Any]) -> dict[str, Any]:
    raw_input = as_number(record.get("input_tokens"))
    canonical_uncached = as_number(record.get("non_cached_input_tokens"))
    # 持久化记录只接受 canonical cache_read_tokens；provider wire 的别名
    # 必须在 Rust 适配层归一化，基准解析器不能替旧会话字段兜底。
    cache_read = as_number(record.get("cache_read_tokens"))
    output = as_number(record.get("output_tokens"))
    provider_total = as_number(record.get("provider_total_tokens"))
    total = provider_total
    total_partial = None
    field_sources: dict[str, str] = {}
    if raw_input is not None:
        field_sources["raw_input_tokens"] = "reported"
        field_sources["prompt_tokens"] = "reported"
    if canonical_uncached is not None:
        uncached = canonical_uncached
        field_sources["uncached_input_tokens"] = "reported"
    elif raw_input is not None and cache_read is not None:
        uncached = max(0, raw_input - cache_read)
        field_sources["uncached_input_tokens"] = "derived"
    else:
        uncached = None
    if output is not None:
        field_sources["output_tokens"] = "reported"
    if cache_read is not None:
        field_sources["cache_read_tokens"] = "reported"
    cache_write = as_number(record.get("cache_write_tokens"))
    if cache_write is not None:
        field_sources["cache_write_tokens"] = "reported"
    reasoning = as_number(record.get("reasoning_tokens"))
    if reasoning is not None:
        field_sources["reasoning_tokens"] = "reported"
    if provider_total is not None:
        field_sources["provider_total_tokens"] = "reported"
    if total is None and raw_input is not None and output is not None:
        if cache_write is not None:
            total = raw_input + output + cache_write
            field_sources["total_tokens"] = "derived"
        else:
            total_partial = raw_input + output
            field_sources["total_tokens"] = "derived"
    elif total is not None:
        field_sources["total_tokens"] = "reported"
    tool_schema = as_number(record.get("tool_schema_tokens_estimated"))
    tool_result = as_number(record.get("tool_result_tokens_estimated"))
    if tool_schema is not None:
        field_sources["tool_schema_tokens_estimated"] = "estimated"
    if tool_result is not None:
        field_sources["tool_result_tokens_estimated"] = "estimated"
    complete = record.get("usage_complete")
    required_fields = raw_input is not None and output is not None and total is not None
    if isinstance(complete, bool):
        complete = complete and required_fields
    else:
        complete = required_fields
    return {
        "raw_input_tokens": raw_input,
        "prompt_tokens": raw_input,
        "uncached_input_tokens": uncached,
        "output_tokens": output,
        "reasoning_tokens": reasoning,
        "cache_read_tokens": cache_read,
        "cache_write_tokens": cache_write,
        "provider_total_tokens": provider_total,
        "total_tokens": total,
        "total_tokens_partial": total_partial,
        "tool_schema_tokens_estimated": tool_schema,
        "tool_result_tokens_estimated": tool_result,
        "usage_source": str(record.get("usage_source") or "unknown"),
        "usage_complete": complete,
        "field_sources": field_sources,
    }


def normalize_pi_usage(usage: dict[str, Any]) -> dict[str, Any]:
    raw_input = as_number(usage.get("input"))
    cache_read = as_number(usage.get("cacheRead"))
    cache_write = as_number(usage.get("cacheWrite"))
    field_sources: dict[str, str] = {}
    if raw_input is not None:
        field_sources["raw_input_tokens"] = "reported"
        field_sources["uncached_input_tokens"] = "reported"
    if cache_read is not None:
        field_sources["cache_read_tokens"] = "reported"
    if cache_write is not None:
        field_sources["cache_write_tokens"] = "reported"
    prompt = None
    if raw_input is not None and cache_read is not None:
        # ``input`` 是未命中缓存的上下文；cache read 属于上下文 prompt，
        # cache write 则只保留为计费分项。
        prompt = raw_input + cache_read
        field_sources["prompt_tokens"] = "derived"
    output = as_number(usage.get("output"))
    if output is not None:
        field_sources["output_tokens"] = "reported"
    provider_total = as_number(usage.get("totalTokens"))
    total = provider_total
    if (
        total is None
        and raw_input is not None
        and output is not None
        and cache_read is not None
        and cache_write is not None
    ):
        # Pi 的 provider total 同时包含 cache read 和 cache write 计费量。
        total = raw_input + output + cache_read + cache_write
        field_sources["total_tokens"] = "derived"
    elif total is not None:
        field_sources["provider_total_tokens"] = "reported"
        field_sources["total_tokens"] = "reported"
    reasoning = as_number(usage.get("reasoning"))
    if reasoning is None:
        reasoning = as_number(usage.get("reasoningTokens"))
    if reasoning is not None:
        field_sources["reasoning_tokens"] = "reported"
    return {
        "raw_input_tokens": raw_input,
        "prompt_tokens": prompt,
        "uncached_input_tokens": raw_input,
        "output_tokens": output,
        "reasoning_tokens": reasoning,
        "cache_read_tokens": cache_read,
        "cache_write_tokens": cache_write,
        "provider_total_tokens": provider_total,
        "total_tokens": total,
        "tool_schema_tokens_estimated": None,
        "tool_result_tokens_estimated": None,
        "usage_source": "provider",
        "usage_complete": raw_input is not None and output is not None and total is not None,
        "field_sources": field_sources,
    }


def apply_usage_records(
    metrics: dict[str, Any],
    records: Iterable[dict[str, Any]],
    expected_requests: int,
) -> None:
    usage_records = list(records)
    metrics["usage_record_count"] = len(usage_records)
    metrics["usage_complete"] = (
        expected_requests > 0
        and len(usage_records) == expected_requests
        and all(record.get("usage_complete") is True for record in usage_records)
    )
    metrics["usage_source"] = ",".join(
        sorted({str(record.get("usage_source") or "unknown") for record in usage_records})
    ) or "unknown"
    coverage: dict[str, dict[str, Any]] = {}
    for field in USAGE_FIELDS:
        values = [record[field] for record in usage_records if record.get(field) is not None]
        partial_field = f"{field}_partial"
        known_values = [
            record[field] if record.get(field) is not None else record.get(partial_field)
            for record in usage_records
            if record.get(field) is not None or record.get(partial_field) is not None
        ]
        complete = expected_requests > 0 and len(values) == expected_requests
        field_sources = {
            str(record.get("field_sources", {}).get(field))
            for record in usage_records
            if (record.get(field) is not None or record.get(partial_field) is not None)
            and isinstance(record.get("field_sources"), dict)
            and record.get("field_sources", {}).get(field)
        }
        status = "complete" if complete else ("partial" if known_values else "unknown")
        coverage_entry = {
            "reported_requests": len(values),
            "expected_requests": expected_requests,
            "complete": complete,
            "status": status,
        }
        if known_values and field in ESTIMATED_USAGE_FIELDS:
            coverage_entry["source"] = "estimated"
            coverage_entry["estimated_count"] = len(known_values)
        elif field_sources:
            source = field_sources.pop() if len(field_sources) == 1 else "mixed"
            coverage_entry["source"] = source
        coverage[field] = coverage_entry
        metrics[field] = sum(values) if complete else None
        if known_values and not complete:
            metrics[partial_field] = sum(known_values)
        else:
            metrics.pop(partial_field, None)
    metrics["usage_coverage"] = coverage


def event_payload(event: dict[str, Any]) -> dict[str, Any]:
    payload = event.get("payload")
    return payload if isinstance(payload, dict) else {}


def provider_request_key(
    event: dict[str, Any],
    payload: dict[str, Any],
    *,
    fallback_event_id: bool = False,
) -> str:
    """为同一 provider round 的所有投影返回稳定一致的身份标识。

    JSONL 流可能同时以 ``runtime.event`` 和 ``item.data`` 暴露同一事实。
    事件 id 标识的是投影，不一定是 provider 请求，因此请求统计应优先使用
    传播的 request/round/response id，只有最后才退回事件 id。
    """
    record = payload.get("record")
    record = record if isinstance(record, dict) else {}
    context = event.get("causal_context")
    context = context if isinstance(context, dict) else {}
    candidates = (
        record.get("request_event_id"),
        record.get("provider_request_id"),
        payload.get("provider_request_id"),
        event.get("provider_request_id"),
        context.get("provider_request_id"),
        record.get("provider_round_id"),
        payload.get("provider_round_id"),
        event.get("provider_round_id"),
        context.get("provider_round_id"),
        record.get("response_event_id"),
        record.get("provider_response_id"),
        payload.get("provider_response_id"),
        event.get("provider_response_id"),
        context.get("provider_response_id"),
        record.get("step_id"),
        payload.get("step_id"),
        event.get("step_id"),
        context.get("step_id"),
    )
    for candidate in candidates:
        if candidate is not None and str(candidate):
            return str(candidate)
    if fallback_event_id:
        return str(event.get("id") or "")
    return ""


@dataclass
class _SyntheticProviderRequest:
    key: str
    started_ms: float | None
    completed_ms: float | None = None
    closed: bool = False


def provider_event_timestamp(event: dict[str, Any], payload: dict[str, Any]) -> float | None:
    """提取用于请求关联的持久事件时间。

    投影记录可能把时间放在事件、payload 或 usage record 上。这里不使用
    stdout 到达时间，因为迟到行的到达时间不能代表其所属 provider round。
    """
    record = payload.get("record")
    record = record if isinstance(record, dict) else {}
    for candidate in (
        event.get("timestamp"),
        payload.get("timestamp"),
        record.get("timestamp"),
    ):
        timestamp = timestamp_ms(candidate)
        if timestamp is not None:
            return timestamp
    return None


class ProviderRequestTracker:
    """在导出器省略 request 标识时，按时间区间关联 provider 事件。

    显式 id 可能在无 id 的 round 完成后才到达。只有当持久时间戳落在该
    round 的时间区间内才建立 alias；没有时间戳时，除非只有一个尚未完成
    的 synthetic round 且没有已完成候选，否则保留独立 id，避免误挂当前轮。
    """

    def __init__(self) -> None:
        self._next_fallback = 0
        self._active: list[str] = []
        self._synthetic: dict[str, _SyntheticProviderRequest] = {}
        self._aliases: dict[str, str] = {}
        self._known: set[str] = set()
        self._explicit_started: dict[str, float | None] = {}
        self._last: str | None = None

    def _new_fallback(self, timestamp: float | None) -> str:
        key = f"synthetic-provider-request:{self._next_fallback}"
        self._next_fallback += 1
        self._active.append(key)
        self._synthetic[key] = _SyntheticProviderRequest(key, timestamp)
        self._known.add(key)
        self._last = key
        return key

    def _synthetic_for_timestamp(self, timestamp: float | None) -> str | None:
        if timestamp is None:
            return None
        completed = [
            request
            for request in self._synthetic.values()
            if request.closed
            and request.completed_ms is not None
            and request.started_ms is not None
            and request.started_ms <= timestamp
            and timestamp <= request.completed_ms
        ]
        if completed:
            # 迟到数据与较新的 active round 重叠时，优先已完成区间，再取
            # 起始时间最近的候选。
            return max(completed, key=lambda request: request.started_ms or float("-inf")).key
        active = [
            request
            for request in self._synthetic.values()
            if not request.closed
            and request.started_ms is not None
            and request.started_ms <= timestamp
        ]
        if active:
            return max(active, key=lambda request: request.started_ms or float("-inf")).key
        return None

    def _resolve_explicit(self, explicit: str, timestamp: float | None) -> str:
        alias = self._aliases.get(explicit)
        if alias is not None:
            return alias
        if explicit in self._known:
            return explicit
        candidate = self._synthetic_for_timestamp(timestamp)
        if candidate is not None:
            self._aliases[explicit] = candidate
            return candidate
        if timestamp is None:
            active = [
                request
                for request in self._synthetic.values()
                if not request.closed
            ]
            completed = [request for request in self._synthetic.values() if request.closed]
            if len(active) == 1 and not completed:
                self._aliases[explicit] = active[0].key
                return active[0].key
        # 没有足够时间信息时保留显式 id，不能猜测它属于哪个 synthetic。
        self._known.add(explicit)
        return explicit

    def _resolve_explicit_start(self, explicit: str, timestamp: float | None) -> str:
        alias = self._aliases.get(explicit)
        if alias is not None:
            synthetic = self._synthetic.get(alias)
            if synthetic is None or synthetic.closed:
                self._aliases.pop(explicit, None)
            elif (
                timestamp is None
                or (
                    synthetic.started_ms is not None
                    and abs(timestamp - synthetic.started_ms)
                    <= PROVIDER_START_MATCH_TOLERANCE_MS
                )
            ):
                return alias
            else:
                self._aliases.pop(explicit, None)
        if explicit in self._known:
            return explicit

        def matches_start(request: _SyntheticProviderRequest) -> bool:
            if request.closed:
                return False
            if timestamp is None:
                return request.started_ms is None
            return (
                request.started_ms is not None
                and abs(timestamp - request.started_ms) <= PROVIDER_START_MATCH_TOLERANCE_MS
            )

        candidates = [request for request in self._synthetic.values() if matches_start(request)]
        if len(candidates) == 1:
            self._aliases[explicit] = candidates[0].key
            return candidates[0].key
        if timestamp is None:
            active = [request for request in self._synthetic.values() if not request.closed]
            completed = [request for request in self._synthetic.values() if request.closed]
            if len(active) == 1 and not completed:
                self._aliases[explicit] = active[0].key
                return active[0].key
        self._known.add(explicit)
        return explicit

    def _resolve_without_explicit(
        self,
        timestamp: float | None,
        *,
        prefer_oldest: bool = False,
    ) -> str:
        active_keys = [
            key
            for key in self._active
            if key not in self._synthetic or not self._synthetic[key].closed
        ]
        if prefer_oldest:
            if len(active_keys) > 1:
                oldest = self._oldest_active_key(active_keys)
                if oldest is not None:
                    return oldest
        candidate = self._synthetic_for_timestamp(timestamp)
        if candidate is not None:
            return candidate
        if prefer_oldest:
            if active_keys:
                oldest = self._oldest_active_key(active_keys)
                if oldest is not None:
                    return oldest
            if self._active:
                return self._active[0]
        if self._active:
            return self._active[-1]
        if self._last is not None:
            return self._last
        return self._new_fallback(timestamp)

    def _oldest_active_key(self, active_keys: list[str]) -> str | None:
        timed = []
        for index, key in enumerate(active_keys):
            synthetic = self._synthetic.get(key)
            started = (
                synthetic.started_ms
                if synthetic is not None
                else self._explicit_started.get(key)
            )
            if started is not None:
                timed.append((started, index, key))
        if timed:
            return min(timed, key=lambda entry: entry[0])[2]
        return active_keys[0] if active_keys else None

    def _active_round_for_start(self, timestamp: float | None) -> str | None:
        if len(self._active) != 1:
            return None
        key = self._active[0]
        synthetic = self._synthetic.get(key)
        if synthetic is not None and synthetic.closed:
            return None
        started = (
            synthetic.started_ms
            if synthetic is not None
            else self._explicit_started.get(key)
        )
        if timestamp is None:
            return key
        if started is not None and abs(timestamp - started) <= PROVIDER_START_MATCH_TOLERANCE_MS:
            return key
        return None

    def resolve(
        self,
        event: dict[str, Any],
        payload: dict[str, Any],
        event_type: str,
    ) -> str:
        timestamp = provider_event_timestamp(event, payload)
        explicit = provider_request_key(event, payload)
        if explicit:
            if event_type == "provider_started":
                key = self._resolve_explicit_start(explicit, timestamp)
                synthetic = self._synthetic.get(key)
                if synthetic is not None and synthetic.closed and key != explicit:
                    self._aliases.pop(explicit, None)
                    key = explicit
                    self._known.add(key)
                self._known.add(key)
            else:
                key = self._resolve_explicit(explicit, timestamp)
            synthetic = self._synthetic.get(key)
            if (
                event_type == "provider_started"
                and key not in self._active
                and (synthetic is None or not synthetic.closed)
            ):
                self._active.append(key)
            if event_type == "provider_started" and synthetic is None:
                self._explicit_started.setdefault(key, timestamp)
            self._known.add(key)
            self._last = key
        elif event_type == "provider_started":
            active_round = self._active_round_for_start(timestamp)
            if active_round is not None:
                self._last = active_round
                return active_round
            return self._new_fallback(timestamp)
        else:
            key = self._resolve_without_explicit(
                timestamp,
                prefer_oldest=event_type == "provider_completed",
            )

        if event_type == "provider_completed":
            self._active = [candidate for candidate in self._active if candidate != key]
            synthetic = self._synthetic.get(key)
            if synthetic is not None and not synthetic.closed:
                synthetic.closed = True
                synthetic.completed_ms = timestamp
            if synthetic is None:
                self._explicit_started.pop(key, None)
            self._last = key
        return key


def usage_record_from_provider_completed(event: dict[str, Any], payload: dict[str, Any]) -> dict[str, Any]:
    usage = payload.get("usage") if isinstance(payload.get("usage"), dict) else {}
    return {
        "request_event_id": provider_request_key(event, payload, fallback_event_id=True),
        "input_tokens": usage.get("input_tokens"),
        "non_cached_input_tokens": usage.get("non_cached_input_tokens"),
        "output_tokens": usage.get("output_tokens"),
        "reasoning_tokens": usage.get("reasoning_tokens"),
        "cache_read_tokens": usage.get("cache_read_tokens"),
        "cache_write_tokens": usage.get("cache_write_tokens"),
        "provider_total_tokens": usage.get("total_tokens"),
        "usage_source": usage.get("usage_source", "provider"),
    }


def has_provider_delta(delta: dict[str, Any]) -> bool:
    return any(
        delta.get(key) not in (None, "", [], {})
        for key in ("text", "reasoning", "arguments", "tool_name", "tool_call_id")
    )


def has_pi_delta(update: dict[str, Any]) -> bool:
    if update.get("type") not in {"text_delta", "thinking_delta", "toolcall_delta"}:
        return False
    return update.get("delta") not in (None, "", [], {})


def observed_time(event: dict[str, Any], arrival_ms: float | None) -> float | None:
    if arrival_ms is not None:
        return arrival_ms
    direct = timestamp_ms(event.get("timestamp"))
    if direct is not None:
        return direct
    for key in ("message", "partial"):
        candidate = event.get(key)
        if isinstance(candidate, dict):
            nested = timestamp_ms(candidate.get("timestamp"))
            if nested is not None:
                return nested
    update = event.get("assistantMessageEvent")
    if isinstance(update, dict):
        partial = update.get("partial")
        if isinstance(partial, dict):
            nested = timestamp_ms(partial.get("timestamp"))
            if nested is not None:
                return nested
    messages = event.get("messages")
    if isinstance(messages, list):
        timestamps = [
            timestamp_ms(message.get("timestamp"))
            for message in messages
            if isinstance(message, dict)
        ]
        present = [value for value in timestamps if value is not None]
        if present:
            return max(present)
    return None


def elapsed_between(end: float | None, start: float | None) -> float | None:
    if end is None or start is None:
        return None
    return round(max(0.0, end - start), 1)


def parse_golutra(
    stdout: str,
    elapsed_ms: float,
    return_code: int,
    run_dir: Path,
    line_times_ms: Iterable[float] | None = None,
) -> dict[str, Any]:
    metrics = empty_metrics()
    metrics["return_code"] = return_code
    metrics["elapsed_ms"] = round(elapsed_ms, 1)
    metrics["raw_input_semantics"] = "includes_cache_read"
    timed_events = list(iter_timed_json_lines(stdout, line_times_ms))
    nested_timed = list(nested_runtime_events_timed(timed_events))
    uses_arrival_clock = line_times_ms is not None
    first_event = min(
        (
            observed_time(event, arrival)
            for event, arrival in timed_events
            if observed_time(event, arrival) is not None
        ),
        default=None,
    )
    process_origin = 0.0 if uses_arrival_clock else first_event
    metrics["process_first_event_ms"] = elapsed_between(first_event, process_origin)
    turn_starts = [
        observed_time(event, arrival)
        for event, arrival in timed_events
        if event.get("type") == "turn.started"
    ]
    turn_starts.extend(
        observed_time(event, arrival)
        for event, arrival in nested_timed
        if event.get("event_type") in {"turn_started", "turn.started"}
    )
    provider_starts = [
        observed_time(event, arrival)
        for event, arrival in nested_timed
        if event.get("event_type") == "provider_started"
    ]
    turn_start = min((value for value in turn_starts if value is not None), default=None)
    provider_start = min((value for value in provider_starts if value is not None), default=None)
    metrics["model_prep_ms"] = elapsed_between(provider_start, turn_start)
    first_token = None
    terminal = None
    seen_requests: set[str] = set()
    request_tracker = ProviderRequestTracker()
    usage_records: dict[str, dict[str, Any]] = {}
    fallback_usage_records: dict[str, dict[str, Any]] = {}
    tool_names: set[str] = set()
    observed_tool_calls = 0
    for event, arrival in timed_events:
        event_type = event.get("type")
        if event_type == "turn.completed":
            metrics["completed"] = event.get("status") == "completed"
            metrics["final_message"] = str(event.get("final_message") or "")[:512]
            terminal = observed_time(event, arrival)
        elif event_type == "turn.failed":
            metrics["final_message"] = str(event.get("error") or event.get("final_message") or "")[:512]
            terminal = observed_time(event, arrival)
    for event, arrival in nested_timed:
        event_type = str(event.get("event_type") or "")
        payload = event_payload(event)
        event_time = observed_time(event, arrival)
        if event_type == "context_built" and metrics["planned_input_tokens"] is None:
            metrics["planned_input_tokens"] = as_number(payload.get("planned_input_tokens"))
        if event_type in {"turn_completed", "turn.completed"} and terminal is None:
            metrics["completed"] = str(payload.get("status") or "completed").lower() == "completed"
            terminal = event_time
        elif event_type in {"turn_failed", "turn.failed"} and terminal is None:
            metrics["completed"] = False
            terminal = event_time
        if event_type == "provider_started":
            request_id = request_tracker.resolve(event, payload, event_type)
            seen_requests.add(request_id)
        if event_type == "provider_streamed" and first_token is None:
            delta = payload.get("delta") if isinstance(payload.get("delta"), dict) else {}
            if has_provider_delta(delta):
                first_token = event_time
        if event_type == "token_usage_recorded":
            record = payload.get("record") if isinstance(payload.get("record"), dict) else {}
            request_id = request_tracker.resolve(event, payload, event_type)
            if request_id:
                usage_records[request_id] = normalize_golutra_usage(record)
                seen_requests.add(request_id)
            if metrics["planned_input_tokens"] is None:
                metrics["planned_input_tokens"] = as_number(record.get("planned_input_tokens"))
            if record.get("estimated_cost") is not None:
                metrics["cost"] = (metrics["cost"] or 0) + record["estimated_cost"]
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
            request_id = request_tracker.resolve(event, payload, event_type)
            seen_requests.add(request_id)
            if request_id and isinstance(payload.get("usage"), dict):
                fallback_record = normalize_golutra_usage(
                    usage_record_from_provider_completed(event, payload)
                )
                fallback_record["request_event_id"] = request_id
                fallback_usage_records[request_id] = fallback_record
            count = as_number(payload.get("tool_call_count"))
            if count is not None:
                metrics["tool_call_count"] = max(metrics["tool_call_count"], count)
            for call in payload.get("provider_tool_calls", []):
                if isinstance(call, dict):
                    name = call.get("name") or call.get("tool_name")
                    if isinstance(name, str) and name:
                        tool_names.add(name)
    metrics["tool_call_count"] = max(metrics["tool_call_count"], observed_tool_calls)
    normalized_usage = dict(fallback_usage_records)
    normalized_usage.update(usage_records)
    metrics["request_count"] = len(seen_requests | set(normalized_usage))
    apply_usage_records(metrics, normalized_usage.values(), metrics["request_count"])
    metrics["tool_names"] = sorted(tool_names)
    metrics["first_token_ms"] = elapsed_between(first_token, process_origin)
    metrics["turn_first_token_ms"] = elapsed_between(first_token, turn_start)
    metrics["provider_first_token_ms"] = elapsed_between(first_token, provider_start)
    metrics["terminal_ms"] = elapsed_between(terminal, process_origin)
    manifest = run_dir / "manifest.json"
    if manifest.is_file():
        try:
            data = json.loads(manifest.read_text(encoding="utf-8"))
            metrics["run_id"] = data.get("provenance", {}).get("run_id")
            metrics["provider"] = data.get("terminal_outcome", {}).get("result", {}).get("status")
        except (OSError, json.JSONDecodeError):
            pass
    metrics["completed"] = bool(metrics["completed"]) and return_code == 0
    return metrics


def extract_text(content: Any) -> str:
    if isinstance(content, str):
        return content
    if not isinstance(content, list):
        return ""
    return "\n".join(str(block.get("text", "")) for block in content if isinstance(block, dict) and block.get("type") == "text")


def parse_pi(
    stdout: str,
    elapsed_ms: float,
    return_code: int,
    line_times_ms: Iterable[float] | None = None,
) -> dict[str, Any]:
    metrics = empty_metrics()
    metrics["return_code"] = return_code
    metrics["elapsed_ms"] = round(elapsed_ms, 1)
    metrics["raw_input_semantics"] = "excludes_cache_read_and_write"
    timed_events = list(iter_timed_json_lines(stdout, line_times_ms))
    uses_arrival_clock = line_times_ms is not None
    process_first_event = min(
        (
            observed_time(event, arrival)
            for event, arrival in timed_events
            if observed_time(event, arrival) is not None
        ),
        default=None,
    )
    process_origin = 0.0 if uses_arrival_clock else process_first_event
    metrics["process_first_event_ms"] = elapsed_between(process_first_event, process_origin)
    usage_records: dict[str, dict[str, Any]] = {}
    session_start = None
    turn_start = None
    first_token = None
    terminal = None
    last_message_time = None
    tool_names: set[str] = set()
    tool_calls_seen: set[str] = set()
    tool_results_seen: set[str] = set()
    estimated_results: dict[str, int] = {}
    for event, arrival in timed_events:
        event_type = event.get("type")
        if session_start is None and event_type == "session":
            session_start = observed_time(event, arrival)
        if turn_start is None and event_type == "turn_start":
            turn_start = observed_time(event, arrival)
        if event_type == "message_update":
            update = event.get("assistantMessageEvent")
            if isinstance(update, dict) and has_pi_delta(update) and first_token is None:
                first_token = observed_time(event, arrival)
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
            message_time = observed_time(event, arrival)
            if message_time is not None:
                last_message_time = message_time
            response_id = str(message.get("responseId") or message.get("timestamp") or len(usage_records))
            if response_id in usage_records:
                continue
            usage_records[response_id] = normalize_pi_usage(usage)
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
            result = event.get("result") or event.get("toolResult") or {}
            content = result.get("content") if isinstance(result, dict) else ""
            estimated_results[result_id] = max(0, len(extract_text(content)) + 3) // 4
        elif event_type == "tool_execution_start":
            call_id = str(event.get("toolCallId") or event.get("timestamp") or "")
            if call_id not in tool_calls_seen:
                tool_calls_seen.add(call_id)
                metrics["tool_call_count"] += 1
            if isinstance(event.get("toolName"), str):
                tool_names.add(event["toolName"])
        elif event_type == "agent_end":
            terminal = observed_time(event, arrival)
            metrics["completed"] = return_code == 0
    if terminal is None:
        terminal = last_message_time
    metrics["request_count"] = len(usage_records)
    apply_usage_records(metrics, usage_records.values(), metrics["request_count"])
    metrics["tool_names"] = sorted(tool_names)
    # No tool-result event is different from a measured zero-token result.  A
    # missing estimate must remain unknown so aggregate reports do not turn it
    # into a misleading numeric zero.
    metrics["tool_result_tokens_estimated"] = (
        sum(estimated_results.values()) if estimated_results else None
    )
    estimate_coverage = {
        "reported_requests": 0,
        "expected_requests": 0,
        "estimated_count": len(estimated_results),
        "complete": bool(estimated_results),
        "status": "complete" if estimated_results else "unknown",
    }
    if estimated_results:
        estimate_coverage["source"] = "estimated"
    metrics["usage_coverage"]["tool_result_tokens_estimated"] = estimate_coverage
    metrics["first_token_ms"] = elapsed_between(first_token, process_origin)
    turn_anchor = turn_start if turn_start is not None else session_start
    if turn_anchor is None:
        turn_anchor = process_first_event
    metrics["turn_first_token_ms"] = elapsed_between(first_token, turn_anchor)
    metrics["terminal_ms"] = elapsed_between(terminal, process_origin)
    return metrics


def drain_process_stream(
    stream: Any,
    lines: list[str],
    started: float,
    line_times_ms: list[float] | None = None,
) -> None:
    try:
        for line in stream:
            lines.append(line)
            if line_times_ms is not None:
                line_times_ms.append((time.monotonic() - started) * 1000)
    except (OSError, ValueError):
        pass
    finally:
        try:
            stream.close()
        except (OSError, ValueError):
            pass


def close_process_stream(stream: Any) -> None:
    try:
        file_descriptor = stream.fileno()
    except (AttributeError, OSError, ValueError):
        try:
            stream.close()
        except (AttributeError, OSError, ValueError):
            pass
        return
    try:
        os.close(file_descriptor)
    except (OSError, ValueError):
        pass


def join_pipe_reader(thread: threading.Thread, stream: Any, deadline: float) -> None:
    remaining = max(0.0, deadline - time.monotonic())
    thread.join(timeout=remaining)
    if thread.is_alive():
        close_process_stream(stream)
        thread.join(timeout=PIPE_CLOSE_JOIN_SECONDS)


def terminate_windows_process_tree(process_id: int) -> None:
    try:
        subprocess.run(
            ["taskkill", "/PID", str(process_id), "/T", "/F"],
            check=False,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            timeout=PROCESS_STOP_TIMEOUT_SECONDS,
        )
    except (OSError, ProcessLookupError, subprocess.TimeoutExpired):
        pass


def stop_timed_out_process(process: subprocess.Popen[str]) -> None:
    if os.name == "nt":
        try:
            process.terminate()
        except ProcessLookupError:
            pass
        terminate_windows_process_tree(process.pid)
    else:
        try:
            os.killpg(process.pid, signal.SIGTERM)
        except ProcessLookupError:
            return
    try:
        process.wait(timeout=PROCESS_STOP_TIMEOUT_SECONDS)
        return
    except ProcessLookupError:
        return
    except subprocess.TimeoutExpired:
        pass
    try:
        if os.name == "nt":
            terminate_windows_process_tree(process.pid)
        else:
            os.killpg(process.pid, signal.SIGKILL)
    except ProcessLookupError:
        return
    try:
        process.wait(timeout=PROCESS_STOP_TIMEOUT_SECONDS)
    except (ProcessLookupError, subprocess.TimeoutExpired):
        pass


def run_process(
    command: list[str],
    cwd: Path,
    env: dict[str, str],
    timeout: float,
    stdout_path: Path,
    stderr_path: Path,
) -> ProcessCapture:
    started = time.monotonic()
    popen_options: dict[str, Any] = {
        "cwd": cwd,
        "env": env,
        "stdout": subprocess.PIPE,
        "stderr": subprocess.PIPE,
        "text": True,
        "encoding": "utf-8",
        "errors": "replace",
        "bufsize": 1,
    }
    if os.name == "nt":
        popen_options["creationflags"] = getattr(subprocess, "CREATE_NEW_PROCESS_GROUP", 0)
    else:
        popen_options["start_new_session"] = True
    process = subprocess.Popen(command, **popen_options)
    assert process.stdout is not None
    assert process.stderr is not None
    stdout_lines: list[str] = []
    stderr_lines: list[str] = []
    stdout_line_times_ms: list[float] = []
    stdout_worker = threading.Thread(
        target=drain_process_stream,
        args=(process.stdout, stdout_lines, started, stdout_line_times_ms),
        daemon=True,
    )
    stderr_worker = threading.Thread(
        target=drain_process_stream,
        args=(process.stderr, stderr_lines, started),
        daemon=True,
    )
    stdout_worker.start()
    stderr_worker.start()
    try:
        returncode = process.wait(timeout=timeout)
    except subprocess.TimeoutExpired:
        returncode = 124
        stop_timed_out_process(process)
        stderr_lines.append("\nprocess timed out\n")
    pipe_deadline = time.monotonic() + PIPE_DRAIN_TIMEOUT_SECONDS
    join_pipe_reader(stdout_worker, process.stdout, pipe_deadline)
    join_pipe_reader(stderr_worker, process.stderr, pipe_deadline)
    elapsed_ms = (time.monotonic() - started) * 1000
    stdout = "".join(stdout_lines)
    stderr = "".join(stderr_lines)
    stdout_path.parent.mkdir(parents=True, exist_ok=True)
    stdout_path.write_text(stdout, encoding="utf-8", errors="replace")
    stderr_path.write_text(stderr, encoding="utf-8", errors="replace")
    return ProcessCapture(
        stdout=stdout,
        stderr=stderr,
        return_code=returncode,
        elapsed_ms=elapsed_ms,
        stdout_line_times_ms=tuple(stdout_line_times_ms),
    )


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
    execution_completed = bool(metrics.get("completed")) and metrics.get("return_code") == 0
    return {
        "passed": execution_completed and files["passed"] and response["passed"],
        "execution_completed": execution_completed,
        "files": files,
        "response": response,
    }


def display_metric(value: Any) -> str:
    return "-" if value is None else str(value)


def usage_coverage_entry(metrics: dict[str, Any], field: str) -> dict[str, Any]:
    coverage = metrics.get("usage_coverage")
    if not isinstance(coverage, dict):
        return {}
    entry = coverage.get(field)
    return entry if isinstance(entry, dict) else {}


def usage_coverage_state(metrics: dict[str, Any], field: str) -> dict[str, Any]:
    """Return normalized coverage counts for one task/usage field.

    A parser may intentionally emit ``unknown`` with zero expected requests
    when a field is not applicable (for example, no tool result was produced).
    Missing coverage on a task that did issue requests is different: it is an
    unknown measurement and must count against aggregate completeness.
    """
    entry = usage_coverage_entry(metrics, field)
    request_count = as_number(metrics.get("request_count")) or 0
    expected_value = as_number(entry.get("expected_requests"))
    expected = expected_value if expected_value is not None else request_count
    reported = as_number(entry.get("reported_requests")) or 0
    estimated = as_number(entry.get("estimated_count")) or 0
    status = entry.get("status")
    value = metrics.get(field)
    tool_result_count = as_number(metrics.get("tool_result_count")) or 0
    explicitly_not_applicable = (
        field in ESTIMATED_USAGE_FIELDS
        and expected_value == 0
        and reported == 0
        and estimated == 0
        and status == "unknown"
        and (field != "tool_result_tokens_estimated" or tool_result_count == 0)
    )
    has_value_without_count = (
        value is not None
        and status != "complete"
        and expected == 0
        and reported == 0
        and estimated == 0
    )
    missing_for_active_task = not entry and request_count > 0
    applicable = (
        not explicitly_not_applicable
        and (
            expected > 0
            or reported > 0
            or estimated > 0
            or status == "partial"
            or (
                field == "tool_result_tokens_estimated"
                and tool_result_count > 0
            )
        )
    )
    if has_value_without_count or missing_for_active_task:
        applicable = True
        status = "unknown"
        if expected == 0:
            expected = max(1, request_count)
    return {
        "reported": reported,
        "expected": expected,
        "estimated": estimated,
        "status": status,
        "applicable": applicable,
        "source": entry.get("source"),
    }


def display_field(metrics: dict[str, Any], field: str) -> str:
    # A numeric value is only displayable when its coverage says that it was
    # observed.  This keeps an accidental zero from masquerading as a provider
    # measurement when the corresponding usage field is unknown.
    coverage = usage_coverage_entry(metrics, field)
    if field in USAGE_FIELDS and coverage.get("status") not in {
        "complete",
        "partial",
    }:
        return "-"
    value = metrics.get(field)
    if value is not None:
        return str(value)
    partial = metrics.get(f"{field}_partial")
    return f"{partial}*" if partial is not None else "-"


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
    golutra_capture = run_process(
        golutra_command,
        args.workspace,
        os.environ.copy(),
        args.timeout,
        task_root / "golutra.stdout.jsonl",
        task_root / "golutra.stderr.log",
    )
    golutra_metrics = parse_golutra(
        golutra_capture.stdout,
        golutra_capture.elapsed_ms,
        golutra_capture.return_code,
        golutra_run,
        golutra_capture.stdout_line_times_ms,
    )
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
    pi_capture = run_process(
        pi_command,
        pi_workspace,
        pi_env,
        args.timeout,
        task_root / "pi.stdout.jsonl",
        task_root / "pi.stderr.log",
    )
    pi_metrics = parse_pi(
        pi_capture.stdout,
        pi_capture.elapsed_ms,
        pi_capture.return_code,
        pi_capture.stdout_line_times_ms,
    )
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
        "| Task | G total | Pi total | G context prompt/uncached/output/reasoning | Pi context prompt/uncached/output/reasoning | G cache R/W | Pi cache R/W | G req/tools | Pi req/tools | G prep/provider-first/terminal ms | Pi turn-first/terminal ms | Pass |",
        "| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- |",
    ]
    for row in report["tasks"]:
        g, p = row["golutra"], row["pi"]
        passed = g["verification"]["passed"] and p["verification"]["passed"]
        lines.append(
            f"| {row['task_id']} | {display_field(g, 'total_tokens')} | {display_field(p, 'total_tokens')} | {display_field(g, 'prompt_tokens')} / {display_field(g, 'uncached_input_tokens')} / {display_field(g, 'output_tokens')} / {display_field(g, 'reasoning_tokens')} | {display_field(p, 'prompt_tokens')} / {display_field(p, 'uncached_input_tokens')} / {display_field(p, 'output_tokens')} / {display_field(p, 'reasoning_tokens')} | {display_field(g, 'cache_read_tokens')} / {display_field(g, 'cache_write_tokens')} | {display_field(p, 'cache_read_tokens')} / {display_field(p, 'cache_write_tokens')} | {display_metric(g.get('request_count'))} / {display_metric(g.get('tool_call_count'))} | {display_metric(p.get('request_count'))} / {display_metric(p.get('tool_call_count'))} | {display_metric(g.get('model_prep_ms'))} / {display_metric(g.get('provider_first_token_ms'))} / {display_metric(g.get('terminal_ms'))} | {display_metric(p.get('turn_first_token_ms'))} / {display_metric(p.get('terminal_ms'))} | {'yes' if passed else 'no'} |"
        )
    lines.extend(
        [
            "",
            "Context prompt is Golutra raw input (which includes cache reads) and Pi raw input + cache read; cache write is a separate billing field and is excluded from prompt. Total/provider total retain provider billing semantics. Timings use host monotonic stdout arrival; terminal is process-relative. An asterisk marks a partial provider field; a dash is unknown. JSON usage coverage reports status=complete|partial|unknown and local estimates with source=estimated.",
            "",
            "## Aggregate",
            "",
            "| Engine | Passed | Total | Context prompt | Uncached | Output | Reasoning | Cache read/write | Tool schema estimate | Tool result estimate | Avg prep/provider-first/first ms | Avg elapsed ms |",
            "| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |",
        ]
    )
    for engine in ("golutra", "pi"):
        summary = report["summary"][engine]
        lines.append(
            f"| {engine} | {summary['passed_tasks']}/{summary['task_count']} | {display_field(summary, 'total_tokens')} | {display_field(summary, 'prompt_tokens')} | {display_field(summary, 'uncached_input_tokens')} | {display_field(summary, 'output_tokens')} | {display_field(summary, 'reasoning_tokens')} | {display_field(summary, 'cache_read_tokens')} / {display_field(summary, 'cache_write_tokens')} | {display_field(summary, 'tool_schema_tokens_estimated')} | {display_field(summary, 'tool_result_tokens_estimated')} | {display_metric(summary.get('avg_model_prep_ms'))} / {display_metric(summary.get('avg_provider_first_token_ms'))} / {display_metric(summary.get('avg_first_token_ms'))} | {display_metric(summary.get('avg_elapsed_ms'))} |"
        )
    return "\n".join(lines) + "\n"


def aggregate_metrics(tasks: list[dict[str, Any]], engine: str) -> dict[str, Any]:
    metrics = [row[engine] for row in tasks]
    numeric_fields = (
        "raw_input_tokens",
        "prompt_tokens",
        "uncached_input_tokens",
        "output_tokens",
        "reasoning_tokens",
        "cache_read_tokens",
        "cache_write_tokens",
        "provider_total_tokens",
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
        "usage_complete": bool(metrics)
        and all(metric.get("usage_complete") is True for metric in metrics),
    }
    for field in numeric_fields:
        if field in USAGE_FIELDS:
            # Usage fields are valid only within their per-task coverage
            # contract.  In particular, do not sum a stale/zero value when a
            # provider omitted that field and the parser marked it unknown.
            known_values: list[int] = []
            applicable_count = 0
            all_complete = True
            for metric in metrics:
                coverage = usage_coverage_state(metric, field)
                if not coverage["applicable"]:
                    continue
                applicable_count += 1
                if coverage["status"] not in {
                    "complete",
                    "partial",
                }:
                    all_complete = False
                    continue
                if coverage["status"] != "complete":
                    all_complete = False
                value = metric.get(field)
                if value is None:
                    value = metric.get(f"{field}_partial")
                normalized = as_number(value)
                if normalized is None:
                    all_complete = False
                    continue
                known_values.append(normalized)
            all_complete = all_complete and applicable_count > 0
            summary[field] = sum(known_values) if all_complete else None
            if known_values and not all_complete:
                summary[f"{field}_partial"] = sum(known_values)
            continue
        complete_values = [metric.get(field) for metric in metrics]
        if metrics and all(value is not None for value in complete_values):
            summary[field] = sum(complete_values)
            continue
        partial_values = [
            metric.get(field)
            if metric.get(field) is not None
            else metric.get(f"{field}_partial")
            for metric in metrics
        ]
        reported = [value for value in partial_values if value is not None]
        summary[field] = None
        if reported:
            summary[f"{field}_partial"] = sum(reported)
    usage_coverage: dict[str, dict[str, Any]] = {}
    for field in USAGE_FIELDS:
        field_coverage = [usage_coverage_state(metric, field) for metric in metrics]
        applicable_coverage = [entry for entry in field_coverage if entry["applicable"]]
        reported = sum(entry["reported"] for entry in applicable_coverage)
        expected = sum(entry["expected"] for entry in applicable_coverage)
        estimated = sum(entry["estimated"] for entry in applicable_coverage)
        statuses = [entry["status"] for entry in applicable_coverage]
        partial = any(status == "partial" for status in statuses)
        # Missing or malformed coverage is unknown too.  Treating it as an
        # empty dictionary must not let another task make the aggregate look
        # complete.
        unknown = any(status not in {"complete", "partial"} for status in statuses)
        partial_requests = sum(
            max(
                0,
                entry["expected"] - entry["reported"],
            )
            for entry in applicable_coverage
            if entry["status"] == "partial"
        )
        unknown_requests = sum(
            max(
                0,
                entry["expected"] - entry["reported"],
            )
            for entry in applicable_coverage
            if entry["status"] not in {"complete", "partial"}
        )
        sources = {
            str(entry["source"])
            for entry in applicable_coverage
            if entry["source"]
        }
        # Any unknown task keeps the aggregate from claiming complete
        # coverage, even when the known tasks happen to line up exactly.
        complete = expected > 0 and reported == expected and not unknown and not partial
        estimate_only = estimated > 0 and reported == 0 and not unknown and not partial
        has_known_values = reported > 0 or estimated > 0
        coverage_entry = {
            "reported_requests": reported,
            "expected_requests": expected,
            "complete": complete or estimate_only,
            "status": (
                "complete"
                if complete or estimate_only
                else ("partial" if partial or has_known_values else "unknown")
            ),
        }
        if partial_requests:
            coverage_entry["partial_requests"] = partial_requests
        if unknown_requests:
            coverage_entry["unknown_requests"] = unknown_requests
        if estimated:
            coverage_entry["estimated_count"] = estimated
        usage_coverage[field] = coverage_entry
        if sources:
            usage_coverage[field]["source"] = sources.pop() if len(sources) == 1 else "mixed"
    summary["usage_coverage"] = usage_coverage
    elapsed = [metric["elapsed_ms"] for metric in metrics if metric.get("elapsed_ms") is not None]
    summary["avg_elapsed_ms"] = round(sum(elapsed) / len(elapsed), 1) if elapsed else None
    for field in (
        "model_prep_ms",
        "provider_first_token_ms",
        "first_token_ms",
        "turn_first_token_ms",
        "terminal_ms",
    ):
        values = [metric[field] for metric in metrics if metric.get(field) is not None]
        summary[f"avg_{field}"] = round(sum(values) / len(values), 1) if values else None
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
        "schema_version": 3,
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
            "timing_source": "host_monotonic_stdout_arrival",
            "token_semantics": "context_prompt;golutra=raw_input_includes_cache_read;pi=raw_input_plus_cache_read;cache_write=billing_separate;total=provider_billing",
            "prompt_tokens_semantics": "context_prompt",
            "cache_write_tokens_semantics": "billing_only_excluded_from_prompt",
            "total_tokens_semantics": "provider_billing_total",
            "missing_usage_semantics": "coverage_status_partial_or_unknown",
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
