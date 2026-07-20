import assert from "node:assert/strict";
import { chmod, mkdtemp, rm } from "node:fs/promises";
import { createServer } from "node:net";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import test from "node:test";

import {
  TuiDriverClient,
  TuiDriverDisconnectedError,
  TuiDriverError,
} from "../.test-dist/tui-driver.js";

const testDir = dirname(fileURLToPath(import.meta.url));
const fakeDriver = resolve(testDir, "fake-driver.mjs");

test("stdio client routes concurrent waits, notifications, and frozen pages", async () => {
  const notifications = [];
  const client = await TuiDriverClient.spawnCommand(
    process.execPath,
    [fakeDriver],
    {
      requestTimeoutMs: 500,
      startupTimeoutMs: 500,
      onNotification: (notification) => notifications.push(notification),
    },
  );
  assert.equal(client.ready.instance_id, "fake-driver");
  assert.deepEqual(await client.capabilities(), ["fake"]);

  const [slow, fast] = await Promise.all([
    client.wait({ kind: "idle" }, 200),
    client.wait({ kind: "task_terminal" }, 200),
  ]);
  assert.equal(slow.condition.kind, "idle");
  assert.equal(fast.condition.kind, "task_terminal");
  assert.equal(notifications.length, 2);

  const frame = await client.completeSnapshot({ width: 80, height: 24 });
  assert.equal(frame.frame_id, "sha256:fake");
  assert.deepEqual(
    frame.lines.map((line) => line.text),
    ["one", "two", "three", "four"],
  );
  assert.deepEqual(frame.returned_range, { start: 1, end: 4 });
  assert.equal(frame.next_range, null);
  await client.close();
});

test("timeouts and disconnect reject pending work without replay", async () => {
  const client = await TuiDriverClient.spawnCommand(
    process.execPath,
    [fakeDriver],
    {
      requestTimeoutMs: 500,
      startupTimeoutMs: 500,
    },
  );
  await assert.rejects(
    client.request(
      {
        type: "wait",
        until: { kind: "event", event_type: "never" },
        timeout_ms: 500,
      },
      { timeoutMs: 20 },
    ),
    (error) =>
      error instanceof TuiDriverError && error.code === "request_timeout",
  );

  const pending = client.request(
    {
      type: "wait",
      until: { kind: "event", event_type: "never" },
      timeout_ms: 500,
    },
    { timeoutMs: 500 },
  );
  const pendingFailure = assert.rejects(
    pending,
    (error) => error instanceof TuiDriverDisconnectedError,
  );
  await assert.rejects(client.prompt("disconnect"), TuiDriverDisconnectedError);
  await pendingFailure;
  assert.equal(client.connected, false);
});

test("Unix socket reconnect is explicit and never replays input", async (t) => {
  if (process.platform === "win32") {
    t.skip("Unix sockets are unavailable on Windows");
    return;
  }
  const directory = await mkdtemp(join(tmpdir(), "golutra-driver-sdk-"));
  const socketPath = join(directory, "driver.sock");
  let promptCount = 0;
  const server = createServer((socket) => {
    socket.write(`${JSON.stringify(readyEnvelope())}\n`);
    let buffered = "";
    socket.setEncoding("utf8");
    socket.on("data", (chunk) => {
      buffered += chunk;
      let newline = buffered.indexOf("\n");
      while (newline >= 0) {
        const request = JSON.parse(buffered.slice(0, newline));
        buffered = buffered.slice(newline + 1);
        if (request.type === "input_prompt") {
          promptCount += 1;
          socket.destroy();
        } else if (request.type === "ping") {
          socket.write(
            `${JSON.stringify({ request_id: request.request_id, type: "pong" })}\n`,
          );
        } else if (request.type === "close") {
          socket.write(
            `${JSON.stringify({ request_id: request.request_id, type: "closed" })}\n`,
          );
        }
        newline = buffered.indexOf("\n");
      }
    });
  });
  await new Promise((resolveListen, rejectListen) => {
    server.once("error", rejectListen);
    server.listen(socketPath, resolveListen);
  });
  await chmod(socketPath, 0o600);
  t.after(async () => {
    await new Promise((resolveClose) => server.close(resolveClose));
    await rm(directory, { recursive: true, force: true });
  });

  const client = await TuiDriverClient.connectSocket(socketPath, {
    requestTimeoutMs: 300,
    startupTimeoutMs: 300,
  });
  await assert.rejects(client.prompt("drop"), TuiDriverDisconnectedError);
  assert.equal(promptCount, 1);
  await client.reconnect();
  assert.equal(promptCount, 1);
  await client.ping();
  await client.close();
});

function readyEnvelope() {
  return {
    request_id: "ready",
    type: "ready",
    protocol_version: 1,
    minimum_protocol_version: 1,
    instance_id: "socket-driver",
    workspace_id: "fake-workspace",
    workspace_path: process.cwd(),
    thread_id: "fake-thread",
    session_id: "fake-session",
    controller_mode: "controller",
  };
}
