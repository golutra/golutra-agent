from __future__ import annotations

import json
import os
import sys
import threading
import time


READY = {
    "request_id": "ready",
    "type": "ready",
    "protocol_version": 1,
    "minimum_protocol_version": 1,
    "instance_id": "fake-driver",
    "workspace_id": "fake-workspace",
    "workspace_path": os.getcwd(),
    "thread_id": "fake-thread",
    "session_id": "fake-session",
    "controller_mode": "controller",
}


def write(value: dict) -> None:
    sys.stdout.write(json.dumps(value, separators=(",", ":")) + "\n")
    sys.stdout.flush()


def state() -> dict:
    return {
        "instance_id": "fake-driver",
        "thread_id": "fake-thread",
        "session_id": "fake-session",
        "task_id": None,
        "turn_id": None,
        "status": "idle",
        "width": 80,
        "height": 24,
        "facts_expanded": False,
        "controller_mode": "controller",
        "closed": False,
    }


def delayed_wait(request: dict) -> None:
    delay = 0.04 if request["until"]["kind"] == "idle" else 0.005
    time.sleep(delay)
    write(
        {
            "request_id": f"event:{request['request_id']}",
            "type": "event",
            "event": {
                "kind": "state_changed",
                "sequence_no": 7,
                "status": "idle",
            },
        }
    )
    write(
        {
            "request_id": request["request_id"],
            "type": "wait_result",
            "condition": request["until"],
            "state": state(),
        }
    )


def snapshot(request: dict) -> dict:
    frozen = request.get("frame_id") == "sha256:fake"
    lines = (
        [
            {"row": 3, "text": "three", "display_width": 5, "pane": "transcript"},
            {"row": 4, "text": "four", "display_width": 4, "pane": "transcript"},
        ]
        if frozen
        else [
            {"row": 1, "text": "one", "display_width": 3, "pane": "transcript"},
            {"row": 2, "text": "two", "display_width": 3, "pane": "transcript"},
        ]
    )
    return {
        "request_id": request["request_id"],
        "type": "snapshot",
        "frame_id": "sha256:fake",
        "instance_id": "fake-driver",
        "workspace_id": "fake-workspace",
        "session_id": "fake-session",
        "task_id": None,
        "turn_id": None,
        "event_high_watermark": 7,
        "width": request["width"],
        "height": request["height"],
        "scope": request.get("scope", "current_turn"),
        "panes": request.get("panes", "transcript"),
        "total_rows": 4,
        "returned_range": {"start": 3, "end": 4} if frozen else {"start": 1, "end": 2},
        "lines": lines,
        "complete": True,
        "missing_sections": [],
        "redaction_status": "redacted",
        "next_range": None if frozen else {"start": 3, "end": 4},
        "hit_regions": [],
        "cells": None,
    }


write(READY)
for raw_line in sys.stdin:
    request = json.loads(raw_line)
    request_type = request["type"]
    if request_type == "hello":
        write({**READY, "request_id": request["request_id"]})
    elif request_type == "capabilities":
        write(
            {
                "request_id": request["request_id"],
                "type": "capabilities",
                "capabilities": ["fake"],
            }
        )
    elif request_type == "state":
        write({"request_id": request["request_id"], "type": "state", **state()})
    elif request_type == "ping":
        write({"request_id": request["request_id"], "type": "pong"})
    elif request_type == "wait":
        if request["until"].get("event_type") != "never":
            threading.Thread(target=delayed_wait, args=(request,), daemon=True).start()
    elif request_type == "snapshot":
        write(snapshot(request))
    elif request_type == "input_prompt" and request["text"] == "disconnect":
        sys.exit(0)
    elif request_type == "close":
        write({"request_id": request["request_id"], "type": "closed"})
        sys.exit(0)
    elif request_type in {
        "input_prompt",
        "input_slash",
        "input_key",
        "input_paste",
        "input_mouse",
        "resize",
        "takeover",
        "abort",
    }:
        write(
            {
                "request_id": request["request_id"],
                "type": "accepted",
                "message": "accepted",
            }
        )
    else:
        write(
            {
                "request_id": request["request_id"],
                "type": "error",
                "code": "unsupported_request",
                "message": f"unsupported {request_type}",
            }
        )
