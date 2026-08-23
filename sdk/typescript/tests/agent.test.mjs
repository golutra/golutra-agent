import assert from "node:assert/strict";
import test from "node:test";

import { GolutraClient, Thread } from "../.test-dist/index.js";

const threadReference = {
  thread_id: "thread-1",
  session_id: "session-1",
  workspace_root: "/tmp/golutra-sdk-test",
};

test("Thread and TurnHandle preserve the shared agent lifecycle", async () => {
  const calls = [];
  const client = {
    async rpc(method, params) {
      calls.push([method, params]);
      if (method === "turn/start") {
        return {
          accepted: true,
          command_id: "command-1",
          cursor: 10,
          thread: threadReference,
        };
      }
      return { accepted: true };
    },
    rpcCommand(method, params) {
      calls.push([method, params]);
      return Promise.resolve({ accepted: true });
    },
    eventPage(request) {
      calls.push(["eventPage", request]);
      return Promise.resolve({
        direction: request.direction,
        events: [],
        has_more: false,
        start_cursor: null,
        end_cursor: null,
      });
    },
    subscribeAgent(request, onEvent) {
      calls.push(["subscribeAgent", request]);
      for (const event of [
        { type: "thread.started" },
        { type: "turn.started", turn_id: "turn-1" },
        {
          type: "turn.completed",
          status: "completed",
          task_id: "task-1",
          turn_id: "turn-1",
          final_message: "done",
          verification: { result: "pass" },
          last_sequence_no: 12,
        },
      ]) {
        onEvent(event);
      }
      return { done: Promise.resolve(), close() {} };
    },
  };

  const thread = new Thread(client, threadReference);
  await assert.rejects(thread.run("   "), /turn prompt cannot be empty/);
  const handle = await thread.run("inspect the workspace", {
    executionMode: "strict",
    toolProfile: "full",
    outputSchema: { type: "object" },
    taskContract: {
      workspace_change: "required",
      required_paths: ["src/main.rs"],
      verification: "independent",
      max_correction_rounds: 1,
    },
    allowNetwork: true,
    yolo: true,
    maxElapsedMs: 345_000,
    deferExternalVerification: true,
    completionCriteria: [" verified ", ""],
    externalVerifiers: [{ program: "pytest", args: ["-q"] }],
  });
  const events = [];
  for await (const event of handle.events()) {
    events.push(event);
    if (event.type === "turn.completed") {
      break;
    }
  }
  assert.equal(events.at(-1).type, "turn.completed");
  const result = await handle.wait();
  assert.equal(result.status, "completed");
  assert.equal(result.final_message, "done");
  assert.equal(result.verification.result, "pass");
  assert.equal((await handle.wait()).final_message, "done");
  assert.deepEqual(calls[0], [
    "turn/start",
    {
      thread_id: "thread-1",
      prompt: "inspect the workspace",
      execution_mode: "strict",
      tool_profile: "full",
      allow_network: true,
      yolo: true,
      max_elapsed_ms: 345_000,
      defer_external_verification: true,
      completion_criteria: [" verified "],
      external_verifiers: [{ program: "pytest", args: ["-q"] }],
      output_schema: { type: "object" },
      task_contract: {
        workspace_change: "required",
        required_paths: ["src/main.rs"],
        verification: "independent",
        max_correction_rounds: 1,
      },
    },
  ]);
  assert.equal(calls[1][0], "subscribeAgent");
  assert.equal(calls[1][1].command_id, "command-1");
  assert.equal(calls[1][1].start_cursor, 10);
  assert.equal(calls.filter(([name]) => name === "subscribeAgent").length, 1);

  assert.equal((await thread.steer("continue", { toolProfile: "full" })).accepted, true);
  assert.deepEqual(calls.find(([name]) => name === "turn/steer"), [
    "turn/steer",
    { thread_id: "thread-1", prompt: "continue", tool_profile: "full" },
  ]);
  await assert.rejects(thread.steer("   "), /steering prompt cannot be empty/);
  assert.equal((await thread.interrupt()).accepted, true);
  assert.equal((await thread.takeover()).accepted, true);
  assert.equal(
    (await thread.reconcileTask("side_effect_observed", {
      taskId: "task-1",
      note: "external change confirmed",
    })).accepted,
    true,
  );
  assert.equal((await handle.resolveApproval("approval-1", false)).accepted, true);
  const history = await thread.history({ cursor: 9, direction: "forward", limit: 25 });
  assert.equal(history.direction, "forward");
  assert.deepEqual(calls.find(([name]) => name === "eventPage"), [
    "eventPage",
    {
      session_id: "session-1",
      task_id: null,
      cursor: 9,
      direction: "forward",
      limit: 25,
    },
  ]);
  assert.deepEqual(calls.find(([name]) => name === "task/reconcile"), [
    "task/reconcile",
    {
      thread_id: "thread-1",
      decision: "side_effect_observed",
      task_id: "task-1",
      note: "external change confirmed",
    },
  ]);
  await assert.rejects(thread.eventPage({ limit: 0 }), /between 1 and 512/);
});

test("project verifier discovery distinguishes omission from an explicit opt-out", async () => {
  const calls = [];
  const client = {
    async rpc(method, params) {
      calls.push([method, params]);
      return {
        accepted: true,
        command_id: `command-${calls.length}`,
        cursor: 0,
        thread: threadReference,
      };
    },
    subscribeAgent(_request, onEvent) {
      onEvent({
        type: "turn.completed",
        status: "completed",
        task_id: "task-1",
        turn_id: "turn-1",
        final_message: "done",
        last_sequence_no: 1,
      });
      return { done: Promise.resolve(), close() {} };
    },
  };
  const thread = new Thread(client, threadReference);

  await thread.run("discover checks");
  await thread.run("disable discovery", { discoverProjectVerifiers: false });

  assert.equal(calls[0][1].execution_mode, "open");
  assert.equal(calls[0][1].tool_profile, "full");
  assert.equal("external_verifiers" in calls[0][1], false);
  assert.deepEqual(calls[1][1].external_verifiers, []);
});

test("regular runs select new defaults while legacy runs preserve server defaults", async () => {
  const calls = [];
  const client = {
    async rpc(method, params) {
      calls.push([method, params]);
      return {
        accepted: true,
        command_id: `command-${calls.length}`,
        cursor: 0,
        thread: threadReference,
      };
    },
  };
  const thread = new Thread(client, threadReference);

  await thread.run("new defaults");
  await thread.runLegacy("server defaults");
  await thread.runLegacyStreamed("streamed server defaults");

  assert.equal(calls[0][0], "turn/start");
  assert.equal(calls[0][1].execution_mode, "open");
  assert.equal(calls[0][1].tool_profile, "full");
  for (const [, params] of calls.slice(1)) {
    assert.equal("execution_mode" in params, false);
    assert.equal("tool_profile" in params, false);
  }
});

test("TurnHandle ends or fails when the backing subscription settles", async () => {
  const client = {
    async rpc() {
      return {
        accepted: true,
        command_id: "command-closed",
        cursor: 3,
        thread: threadReference,
      };
    },
    subscribeAgent() {
      return { done: Promise.resolve(), close() {} };
    },
  };
  const cleanHandle = await new Thread(client, threadReference).run("run");
  await assert.rejects(cleanHandle.wait(), /ended before turn completion/);

  const failure = new Error("HTTP 401: unauthorized");
  const failingClient = {
    ...client,
    subscribeAgent() {
      return { done: Promise.reject(failure), close() {} };
    },
  };
  const failedHandle = await new Thread(failingClient, threadReference).run("run");
  await assert.rejects(failedHandle.wait(), /HTTP 401/);
});

test("governance helpers use the shared query and command contracts", async () => {
  const client = new GolutraClient(
    "http://127.0.0.1:47831",
    "/tmp/golutra-sdk-test",
    { transportToken: "t".repeat(32) },
  );
  const queries = [];
  const commands = [];
  client.query = async (query) => {
    queries.push(query);
    return { kind: "debug_projection" };
  };
  client.sendCommand = async (command) => {
    commands.push(command);
    return { accepted: true };
  };

  assert.equal(
    (await client.debugProjection("session-1", "task-1")).kind,
    "debug_projection",
  );
  assert.equal(queries[0].kind, "debug_projection");

  await client.replay("session-1", "task-1", "capsule-1");
  assert.equal(commands.at(-1).kind, "replay");
  assert.equal(commands.at(-1).payload.capsule_id, "capsule-1");

  await client.ingestExternalEvaluation("session-1", {
    evaluation_id: "evaluation-1",
  });
  assert.equal(commands.at(-1).kind, "ingest_external_evaluation");
  assert.equal(commands.at(-1).payload.record.evaluation_id, "evaluation-1");

  await client.runRegressionCampaign("session-1", "candidate-1", {
    candidateFiles: [{ path: "src/lib.rs", content: "change" }],
    providerMatrix: ["mock"],
    seeds: [7],
    minimumTrustedExternalPairs: 2,
  });
  assert.equal(commands.at(-1).kind, "run_regression_campaign");
  assert.deepEqual(commands.at(-1).payload.seeds, [7]);
  assert.equal(commands.at(-1).payload.minimum_trusted_external_pairs, 2);
});

test("Agent SSE reconnect keeps the consumed cursor and reaches the terminal event", async () => {
  const originalFetch = globalThis.fetch;
  const agentRequests = [];
  let streamAttempt = 0;
  globalThis.fetch = async (input, init = {}) => {
    const url = new URL(
      input instanceof URL ? input.href : typeof input === "string" ? input : input.url,
    );
    if (url.pathname === "/runtime/attach") {
      return Response.json({
        attachment_id: "attachment-1",
        runtime: {
          instance_id: "runtime-1",
          pid: 1,
          base_url: "http://127.0.0.1:47831",
          cwd: "/tmp/golutra-sdk-test",
          workspace_id: "workspace-1",
          default_session_id: "session-1",
          default_thread_id: "thread-1",
          started_at: "2026-01-01T00:00:00Z",
        },
      });
    }
    assert.equal(url.pathname, "/agent/events");
    agentRequests.push({ url, headers: new Headers(init.headers) });
    streamAttempt += 1;
    const event = streamAttempt === 1
      ? {
          type: "runtime.event",
          event: { sequence_no: 11 },
        }
      : {
          type: "turn.completed",
          status: "completed",
          task_id: "task-1",
          turn_id: "turn-1",
          final_message: "done",
          last_sequence_no: 12,
        };
    return new Response(`id: ${streamAttempt + 10}\nevent: agent_event\ndata: ${JSON.stringify(event)}\n\n`, {
      status: 200,
      headers: { "content-type": "text/event-stream" },
    });
  };

  try {
    const client = new GolutraClient(
      "http://127.0.0.1:47831",
      "/tmp/golutra-sdk-test",
      { transportToken: "t".repeat(32) },
    );
    const events = [];
    const subscription = client.subscribeAgent(
      {
        session_id: "session-1",
        thread_id: "thread-1",
        command_id: "command-1",
        start_cursor: 10,
        cursor: 10,
      },
      (event) => events.push(event),
      { initialRetryMs: 1, maxRetryMs: 1 },
    );
    await subscription.done;

    assert.deepEqual(events.map((event) => event.type), ["runtime.event", "turn.completed"]);
    assert.equal(agentRequests.length, 2);
    assert.equal(agentRequests[0].url.searchParams.get("cursor"), "10");
    assert.equal(agentRequests[1].url.searchParams.get("cursor"), "11");
    assert.equal(agentRequests[1].url.searchParams.get("start_cursor"), "10");
    assert.equal(agentRequests[1].headers.get("last-event-id"), "11");
  } finally {
    globalThis.fetch = originalFetch;
  }
});
