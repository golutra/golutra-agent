"""按真实执行身份核对子代理证据，避免 fork 重放和父会话用量造成假优势。"""
from __future__ import annotations

from collections import Counter, defaultdict


def golutra_parent_answer(events: list[dict]) -> str:
    parents = [e for e in events if e.get("event_type") == "task_created"
               and not e.get("payload", {}).get("payload", {}).get("_delegated_task")]
    if len(parents) != 1:
        return ""
    parent = parents[0]
    return next((e["payload"].get("content", "") for e in reversed(events)
                 if e.get("event_type") == "assistant_message"
                 and e.get("session_id") == parent["session_id"] and e.get("task_id") == parent["task_id"]), "")


def codex_sessions(records: list[dict]) -> list[dict]:
    grouped = defaultdict(list)
    for record in records:
        grouped[record["_rollout"]].append(record)
    metadata_by_file = {name: next(row["payload"] for row in rows if row["type"] == "session_meta")
                        for name, rows in grouped.items()}
    turn_owners = {}
    # 被继承的 turn 同时存在于较早创建的源文件；保留原归属，包括无 usage 的失败。
    for name in sorted(grouped, key=lambda name: metadata_by_file[name]["timestamp"]):
        for row in grouped[name]:
            if row["type"] == "event_msg" and row["payload"].get("type") == "task_started":
                turn_owners.setdefault(row["payload"]["turn_id"], metadata_by_file[name]["id"])
    sessions = []
    for rows in grouped.values():
        metadata = next(row["payload"] for row in rows if row["type"] == "session_meta")
        owner = metadata["id"]
        # fork 文件含父 meta/turn；只认明确归属当前 thread 的原生执行。
        own_turns = {r["payload"]["turn_id"] for r in rows
                     if r["type"] == "event_msg" and r["payload"].get("thread_id") == owner
                     and r["payload"].get("turn_id")}
        own_turns.update(r["payload"]["turn_id"] for r in rows
                         if r["type"] == "token_usage_record" and r["payload"].get("thread_id") == owner)
        own_turns.update(turn for turn, thread in turn_owners.items() if thread == owner)
        calls, outputs, turns = {}, {}, {}
        for row in rows:
            payload = row["payload"]
            if row["type"] == "event_msg" and payload.get("turn_id") in own_turns:
                turn = turns.setdefault(payload["turn_id"], {})
                if payload.get("type") == "task_started":
                    turn["start"] = row["timestamp"]
                elif payload.get("type") == "task_complete":
                    turn.update(end=row["timestamp"], answer=payload.get("last_agent_message") or "",
                                error=payload.get("error"))
                elif payload.get("type") == "turn_aborted":
                    turn["aborted"] = True
            if row["type"] != "response_item":
                continue
            turn_id = payload.get("internal_chat_message_metadata_passthrough", {}).get("turn_id")
            if turn_id not in own_turns:
                continue
            kind = payload.get("type")
            if kind in {"function_call", "custom_tool_call", "tool_search_call"}:
                calls[payload.get("call_id") or payload["id"]] = payload
            elif kind in {"function_call_output", "custom_tool_call_output"}:
                outputs[payload["call_id"]] = payload.get("output", "")
        sessions.append({"id": owner, "metadata": metadata, "turns": turns,
                         "calls": calls, "outputs": outputs})
    return sessions


def codex_usage(records: list[dict], sessions: list[dict]) -> dict:
    by_response = {}
    for row in records:
        if row["type"] == "token_usage_record":
            payload = row["payload"]
            key = (payload["thread_id"], payload["response_id"])
            if key in by_response and by_response[key] != payload["usage"]:
                raise ValueError("conflicting usage for same response")
            by_response[key] = payload["usage"]
    mapping = {"total_tokens": "total_tokens", "cache_read_tokens": "cached_input_tokens",
               "output_tokens": "output_tokens", "input_tokens": "input_tokens"}
    result = {target: sum(record[source] for record in by_response.values())
              if by_response and all(source in record for record in by_response.values()) else None
              for target, source in mapping.items()}
    result["uncached_input_tokens"] = (result["input_tokens"] - result["cache_read_tokens"]
        if result["input_tokens"] is not None and result["cache_read_tokens"] is not None else None)
    complete_turns = bool(sessions) and all(session["turns"] and all(
        turn.get("end") and not turn.get("aborted") and not turn.get("error") for turn in session["turns"].values())
        for session in sessions)
    # 完成记录覆盖不等于请求覆盖：流中断/重试可能不写 usage，不能填零。
    result.update(recorded_response_count=len(by_response), usage_complete=None,
                  all_observed_turns_completed=complete_turns,
                  scope="deduplicated recorded Responses completions; failed attempts may lack usage",
                  cache_write_tokens=None, provider_ttft_ms=None)
    if not complete_turns:
        for field in (*mapping, "uncached_input_tokens"):
            result[field + "_partial"] = result[field]
            result[field] = None
    return result


def cancellation_checks(engine: str, events: list[dict], metrics: dict) -> dict:
    if engine == "golutra":
        child_tasks = [e for e in events if e.get("event_type") == "task_created"
                       and e.get("payload", {}).get("payload", {}).get("_delegated_task")]
        children = {e["session_id"] for e in child_tasks}
        terminal = [e for e in events if e.get("event_type") in ("task_completed", "task_aborted", "task_interrupted")
                    and e["session_id"] in children]
        first_id = child_tasks[0]["task_id"] if child_tasks else None
        second_id = child_tasks[1]["task_id"] if len(child_tasks) == 2 else None
        recovered = second_id is not None and any(e.get("event_type") == "assistant_message"
            and e["session_id"] in children and e.get("task_id") == second_id
            and "RECOVERED_91" in e["payload"].get("content", "") for e in events)
        stopped = any(e.get("task_id") == first_id and e.get("event_type")
                      in ("task_aborted", "task_interrupted") for e in terminal)
        child_count, turn_count = len(children), len(child_tasks)
    else:
        children = [s for s in codex_sessions(events) if isinstance(s["metadata"].get("source"), dict)]
        turns = list(children[0]["turns"].values()) if len(children) == 1 else []
        recovered = len(turns) == 2 and "RECOVERED_91" in turns[1].get("answer", "")
        stopped = bool(turns) and bool(turns[0].get("aborted"))
        child_count, turn_count = len(children), len(turns)
    return {"parent_succeeded": metrics.get("completed") is True,
            "one_child_two_executions": child_count == 1 and turn_count == 2,
            "original_execution_stopped": stopped, "resumed_child_returned_token": recovered}


def fanout_checks(engine: str, events: list[dict], metrics: dict, markers: list[str]) -> dict:
    if engine == "golutra":
        launches = [e for e in events if e.get("event_type") == "task_created"
                    and e.get("payload", {}).get("payload", {}).get("_delegated_task")]
        sessions = {e["session_id"] for e in launches}
        tasks = {e["task_id"] for e in launches}
        answers = [e["payload"].get("content", "") for e in events
                   if e.get("event_type") == "assistant_message" and e.get("task_id") in tasks]
        final_answers = {e["task_id"]: e["payload"].get("content", "") for e in events
                         if e.get("event_type") == "assistant_message" and e.get("task_id") in tasks}
        completed = {e["task_id"] for e in events if e.get("event_type") == "task_completed"
                     and e["payload"].get("status") == "completed" and e.get("task_id") in tasks}
        child_calls = [e for e in events if e.get("event_type") == "tool_started"
                       and e["session_id"] in sessions and e["payload"].get("tool_name") == "subagent"]
        boundaries = [(e["timestamp"], 1) for e in launches] + [
            (e["timestamp"], -1) for e in events if e.get("event_type") == "task_completed" and e.get("task_id") in tasks]
        session_count, executions = len(sessions), len(launches)
        success = len(completed) == 10
    else:
        sessions = [s for s in codex_sessions(events) if isinstance(s["metadata"].get("source"), dict)]
        turns = [t for s in sessions for t in s["turns"].values()]
        answers = [t.get("answer", "") for t in turns]
        final_answers = dict(enumerate(answers))
        child_calls = [c for s in sessions for c in s["calls"].values() if c.get("name") == "spawn_agent"]
        boundaries = [(t["start"], 1) for t in turns if t.get("start")] + [(t["end"], -1) for t in turns if t.get("end")]
        session_count, executions = len(sessions), len(turns)
        success = len(turns) == 10 and all(t.get("end") and not t.get("error") and not t.get("aborted") for t in turns)
    active = peak = 0
    for _, change in sorted(boundaries):
        active += change
        peak = max(active, peak)
    checks = {"parent_succeeded": metrics.get("completed") is True,
              "ten_children_ten_executions": session_count == 10 and executions == 10,
              "all_children_succeeded": success, "no_recursive_delegation": not child_calls,
              "all_file_facts_returned_by_children": all(any(marker in answer for answer in answers) for marker in markers),
              "one_assigned_finding_per_child": len(final_answers) == 10 and all(
                  sum(marker in answer for marker in markers) == 1 for answer in final_answers.values()),
              "parent_reported_all_findings": all(marker in (metrics.get("final_message") or "") for marker in markers),
              "executions_overlap_within_limit": 2 <= peak <= 10}
    return {"checks": checks, "passed": all(checks.values()), "peak_child_executions": peak}


def summarize_codex(records: list[dict], metrics: dict) -> dict:
    sessions = codex_sessions(records)
    parents = [s for s in sessions if s["metadata"].get("source") == "exec"]
    children = [s for s in sessions if isinstance(s["metadata"].get("source"), dict)]
    calls = [call for session in sessions for call in session["calls"].values()]
    spawn = [call for call in calls if call.get("name") == "spawn_agent"]
    forked = [s for s in children if s["metadata"].get("forked_from_id")]
    independent = [s for s in children if not s["metadata"].get("forked_from_id")]
    child_turns = [turn for child in children for turn in child["turns"].values()]
    first_turns = [next(iter(child["turns"].values()), {}) for child in children]
    fork_answers = "\n".join(t.get("answer", "") for s in forked for t in s["turns"].values())
    independent_turns = list(independent[0]["turns"].values()) if len(independent) == 1 else []
    checks = {
        "parent_succeeded": metrics.get("completed") is True and len(parents) == 1,
        "exactly_two_child_sessions": len(children) == 2,
        "child_execution_intervals_overlap": len(first_turns) == 2
            and all(t.get("start") and t.get("end") for t in first_turns)
            and max(t["start"] for t in first_turns) < min(t["end"] for t in first_turns),
        "three_child_executions": len(child_turns) == 3,
        "all_child_executions_succeeded": len(child_turns) == 3
            and all(t.get("end") and not t.get("aborted") and not t.get("error") for t in child_turns),
        "exactly_two_spawn_attempts": len(spawn) == 2,
        "fork_returned_assigned_facts": all(s in fork_answers for s in ("RIGHT_SENTINEL_84", "FORK_CONTEXT_73X")),
        "marker_not_copied_in_spawn": len(forked) == 1 and all(
            "FORK_CONTEXT_73X" not in call.get("arguments", "") for call in spawn),
        "independent_child_resumed": len(independent_turns) == 2 and all(
            "LEFT_SENTINEL_42" in t.get("answer", "") for t in independent_turns),
        "no_recursive_delegation": not any(call.get("name") == "spawn_agent"
            for s in children for call in s["calls"].values()),
    }
    return {"passed": all(checks.values()), "checks": checks,
            "all_session_tool_calls": len(calls),
            "tool_names": dict(Counter(call.get("name", call["type"]) for call in calls)),
            "child_executions": len(child_turns), "parent_metrics": metrics,
            "usage": codex_usage(records, sessions)}
