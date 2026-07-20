import { createInterface } from "node:readline";

const ready = {
  request_id: "ready",
  type: "ready",
  protocol_version: 1,
  minimum_protocol_version: 1,
  instance_id: "fake-driver",
  workspace_id: "fake-workspace",
  workspace_path: process.cwd(),
  thread_id: "fake-thread",
  session_id: "fake-session",
  controller_mode: "controller",
};

write(ready);

const input = createInterface({ input: process.stdin, crlfDelay: Infinity });
input.on("line", (line) => {
  const request = JSON.parse(line);
  switch (request.type) {
    case "hello":
      write({ ...ready, request_id: request.request_id });
      break;
    case "capabilities":
      write({
        request_id: request.request_id,
        type: "capabilities",
        capabilities: ["fake"],
      });
      break;
    case "state":
      write({ request_id: request.request_id, type: "state", ...state() });
      break;
    case "ping":
      write({ request_id: request.request_id, type: "pong" });
      break;
    case "wait": {
      if (
        request.until.kind === "event" &&
        request.until.event_type === "never"
      )
        return;
      const delay = request.until.kind === "idle" ? 40 : 5;
      setTimeout(() => {
        write({
          request_id: `event:${request.request_id}`,
          type: "event",
          event: { kind: "state_changed", sequence_no: 7, status: "idle" },
        });
        write({
          request_id: request.request_id,
          type: "wait_result",
          condition: request.until,
          state: state(),
        });
      }, delay);
      break;
    }
    case "snapshot":
      write(snapshot(request));
      break;
    case "input_prompt":
      if (request.text === "disconnect") {
        process.exit(0);
      }
      write({
        request_id: request.request_id,
        type: "accepted",
        message: "accepted",
      });
      break;
    case "input_slash":
    case "input_key":
    case "input_paste":
    case "input_mouse":
    case "resize":
    case "takeover":
    case "abort":
      write({
        request_id: request.request_id,
        type: "accepted",
        message: "accepted",
      });
      break;
    case "close":
      write({ request_id: request.request_id, type: "closed" });
      setTimeout(() => process.exit(0), 5);
      break;
    default:
      write({
        request_id: request.request_id,
        type: "error",
        code: "unsupported_request",
        message: `unsupported ${request.type}`,
      });
  }
});

function state() {
  return {
    instance_id: "fake-driver",
    thread_id: "fake-thread",
    session_id: "fake-session",
    task_id: null,
    turn_id: null,
    status: "idle",
    width: 80,
    height: 24,
    facts_expanded: false,
    controller_mode: "controller",
    closed: false,
  };
}

function snapshot(request) {
  const frozen = request.frame_id === "sha256:fake";
  const rows = frozen
    ? [
        { row: 3, text: "three", display_width: 5, pane: "transcript" },
        { row: 4, text: "four", display_width: 4, pane: "transcript" },
      ]
    : [
        { row: 1, text: "one", display_width: 3, pane: "transcript" },
        { row: 2, text: "two", display_width: 3, pane: "transcript" },
      ];
  return {
    request_id: request.request_id,
    type: "snapshot",
    frame_id: "sha256:fake",
    instance_id: "fake-driver",
    workspace_id: "fake-workspace",
    session_id: "fake-session",
    task_id: null,
    turn_id: null,
    event_high_watermark: 7,
    width: request.width,
    height: request.height,
    scope: request.scope ?? "current_turn",
    panes: request.panes ?? "transcript",
    total_rows: 4,
    returned_range: frozen ? { start: 3, end: 4 } : { start: 1, end: 2 },
    lines: rows,
    complete: true,
    missing_sections: [],
    redaction_status: "redacted",
    next_range: frozen ? null : { start: 3, end: 4 },
    hit_regions: [],
    cells: null,
  };
}

function write(value) {
  process.stdout.write(`${JSON.stringify(value)}\n`);
}
