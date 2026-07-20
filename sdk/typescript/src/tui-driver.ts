import { randomUUID } from "node:crypto";
import { lstat } from "node:fs/promises";
import { createConnection, type Socket } from "node:net";
import {
  spawn as spawnChild,
  type ChildProcessWithoutNullStreams,
  type SpawnOptionsWithoutStdio,
} from "node:child_process";
import { StringDecoder } from "node:string_decoder";

import type {
  DriverEnvelope,
  DriverNotification,
  DriverResponseEnvelope,
  DriverState,
  RowRange,
  TuiFrame,
  WaitCondition,
} from "./generated.js";

export const TUI_DRIVER_PROTOCOL_VERSION = 1;
const DEFAULT_REQUEST_TIMEOUT_MS = 30_000;
const DEFAULT_STARTUP_TIMEOUT_MS = 30_000;
const MAX_DRIVER_LINE_BYTES = 1024 * 1024;
const MAX_SNAPSHOT_PAGES = 4096;

type WithoutRequestId<T> = T extends unknown ? Omit<T, "request_id"> : never;
export type TuiDriverRequest = WithoutRequestId<DriverEnvelope>;
export type TuiDriverResponse = DriverResponseEnvelope;
export type TuiDriverReady = Extract<TuiDriverResponse, { type: "ready" }>;
export type TuiDriverWaitResponse = Extract<
  TuiDriverResponse,
  { type: "wait_result" | "wait_timeout" }
>;

export interface TuiDriverRequestOptions {
  timeoutMs?: number;
}

export interface TuiDriverConnectionOptions {
  startupTimeoutMs?: number;
  requestTimeoutMs?: number;
  onNotification?: (notification: DriverNotification) => void;
  onDiagnostic?: (response: TuiDriverResponse) => void;
}

export interface TuiDriverSpawnOptions extends TuiDriverConnectionOptions {
  workspacePath: string;
  binaryPath?: string;
  session?: string;
  taskId?: string;
  debug?: boolean;
  embedded?: boolean;
  daemon?: boolean;
  connect?: string;
  width?: number;
  height?: number;
  idleTimeoutSeconds?: number;
  heartbeatSeconds?: number;
  env?: NodeJS.ProcessEnv;
  onStderr?: (text: string) => void;
}

export interface TuiDriverCommandOptions extends TuiDriverConnectionOptions {
  cwd?: string;
  env?: NodeJS.ProcessEnv;
  onStderr?: (text: string) => void;
}

export interface SnapshotPageOptions {
  maxPages?: number;
  timeoutMs?: number;
}

export interface TuiSnapshotRequest {
  width: number;
  height: number;
  scope?: "current_turn" | "task" | "session" | "screen";
  panes?: "transcript" | "developer" | "response_and_developer" | "full_screen";
  detail?: "text" | "cells";
  rows?: RowRange | null;
  frame_id?: string | null;
}

export class TuiDriverError extends Error {
  constructor(
    message: string,
    readonly code = "driver_error",
  ) {
    super(message);
    this.name = "TuiDriverError";
  }
}

export class TuiDriverDisconnectedError extends TuiDriverError {
  constructor(message = "TUI Driver connection closed") {
    super(message, "driver_disconnected");
    this.name = "TuiDriverDisconnectedError";
  }
}

interface PendingRequest {
  resolve: (response: TuiDriverResponse) => void;
  reject: (error: Error) => void;
  timeout: ReturnType<typeof setTimeout>;
}

interface ByteWriter {
  write(data: Uint8Array | string): boolean;
  destroy(error?: Error): void;
  end(): void;
}

interface ActiveTransport {
  writer: ByteWriter;
  close(): void;
}

export class TuiDriverClient {
  private readonly requestTimeoutMs: number;
  private readonly startupTimeoutMs: number;
  private readonly notificationListeners = new Set<
    (notification: DriverNotification) => void
  >();
  private readonly diagnosticListeners = new Set<
    (response: TuiDriverResponse) => void
  >();
  private readonly pending = new Map<string, PendingRequest>();
  private transport: ActiveTransport | undefined;
  private process: ChildProcessWithoutNullStreams | undefined;
  private socketPath: string | undefined;
  private decoder = new StringDecoder("utf8");
  private buffered = "";
  private bufferedBytes = 0;
  private readyWaiter: Promise<TuiDriverReady> | undefined;
  private resolveReady: ((ready: TuiDriverReady) => void) | undefined;
  private rejectReady: ((error: Error) => void) | undefined;
  private _ready: TuiDriverReady | undefined;
  private closing = false;

  private constructor(options: TuiDriverConnectionOptions = {}) {
    this.requestTimeoutMs = positiveTimeout(
      options.requestTimeoutMs,
      DEFAULT_REQUEST_TIMEOUT_MS,
      "requestTimeoutMs",
    );
    this.startupTimeoutMs = positiveTimeout(
      options.startupTimeoutMs,
      DEFAULT_STARTUP_TIMEOUT_MS,
      "startupTimeoutMs",
    );
    if (options.onNotification) {
      this.notificationListeners.add(options.onNotification);
    }
    if (options.onDiagnostic) {
      this.diagnosticListeners.add(options.onDiagnostic);
    }
  }

  static async spawn(options: TuiDriverSpawnOptions): Promise<TuiDriverClient> {
    const workspacePath = requireAbsolutePath(
      options.workspacePath,
      "workspacePath",
    );
    if (options.embedded && (options.daemon || options.connect)) {
      throw new TuiDriverError(
        "embedded cannot be combined with daemon or connect",
        "invalid_transport",
      );
    }
    if (options.daemon && options.connect) {
      throw new TuiDriverError(
        "daemon cannot be combined with connect",
        "invalid_transport",
      );
    }
    const args = ["--cwd", workspacePath];
    if (options.daemon) args.push("--daemon");
    if (options.connect) args.push("--connect", options.connect);
    if (options.taskId) args.push("--task-id", options.taskId);
    if (options.debug) args.push("--debug");
    args.push("driver", "--stdio");
    if (options.embedded) args.push("--embedded");
    if (options.session) args.push("--session", options.session);
    if (options.width !== undefined)
      args.push("--width", String(options.width));
    if (options.height !== undefined)
      args.push("--height", String(options.height));
    if (options.idleTimeoutSeconds !== undefined) {
      args.push("--idle-timeout-secs", String(options.idleTimeoutSeconds));
    }
    if (options.heartbeatSeconds !== undefined) {
      args.push("--heartbeat-secs", String(options.heartbeatSeconds));
    }
    return TuiDriverClient.spawnCommand(
      options.binaryPath ?? "golutra-tui",
      args,
      {
        ...options,
        cwd: workspacePath,
      },
    );
  }

  static async spawnCommand(
    command: string,
    args: readonly string[],
    options: TuiDriverCommandOptions = {},
  ): Promise<TuiDriverClient> {
    const client = new TuiDriverClient(options);
    client.armReadyWaiter();
    const spawnOptions: SpawnOptionsWithoutStdio = {
      cwd: options.cwd,
      env: options.env,
      stdio: "pipe",
    };
    const child = spawnChild(
      command,
      [...args],
      spawnOptions,
    ) as ChildProcessWithoutNullStreams;
    client.process = child;
    client.transport = {
      writer: child.stdin,
      close: () => {
        child.stdin.end();
        if (child.exitCode === null && child.signalCode === null) {
          child.kill();
        }
      },
    };
    child.stdout.on("data", (chunk: Buffer) => client.receiveBytes(chunk));
    child.stderr.on("data", (chunk: Buffer) =>
      options.onStderr?.(chunk.toString("utf8")),
    );
    child.once("error", (error) => client.transportFailed(error));
    child.once("exit", (code, signal) => {
      const detail = signal
        ? `signal ${signal}`
        : `status ${code ?? "unknown"}`;
      client.transportFailed(
        new TuiDriverDisconnectedError(`TUI Driver exited with ${detail}`),
      );
    });
    try {
      await client.waitUntilReady();
      return client;
    } catch (error) {
      client.disconnect();
      throw error;
    }
  }

  static async connectSocket(
    socketPath: string,
    options: TuiDriverConnectionOptions = {},
  ): Promise<TuiDriverClient> {
    if (process.platform === "win32") {
      throw new TuiDriverError(
        "Unix socket TUI Driver connections are unavailable on Windows",
        "unsupported_transport",
      );
    }
    const client = new TuiDriverClient(options);
    client.socketPath = requireAbsolutePath(socketPath, "socketPath");
    await client.openSocket();
    return client;
  }

  get ready(): TuiDriverReady {
    if (!this._ready) {
      throw new TuiDriverDisconnectedError(
        "TUI Driver has not completed its ready handshake",
      );
    }
    return this._ready;
  }

  get connected(): boolean {
    return this.transport !== undefined && this._ready !== undefined;
  }

  onNotification(
    listener: (notification: DriverNotification) => void,
  ): () => void {
    this.notificationListeners.add(listener);
    return () => this.notificationListeners.delete(listener);
  }

  onDiagnostic(listener: (response: TuiDriverResponse) => void): () => void {
    this.diagnosticListeners.add(listener);
    return () => this.diagnosticListeners.delete(listener);
  }

  async reconnect(): Promise<TuiDriverReady> {
    if (!this.socketPath) {
      throw new TuiDriverError(
        "Only Unix socket clients support explicit reconnect",
        "unsupported_reconnect",
      );
    }
    if (this.connected) {
      return this.ready;
    }
    await this.openSocket();
    return this.ready;
  }

  async request(
    request: TuiDriverRequest,
    options: TuiDriverRequestOptions = {},
  ): Promise<TuiDriverResponse> {
    const transport = this.transport;
    if (!transport || !this._ready) {
      throw new TuiDriverDisconnectedError();
    }
    const requestId = randomUUID();
    const timeoutMs = positiveTimeout(
      options.timeoutMs,
      this.requestTimeoutMs,
      "timeoutMs",
    );
    const response = new Promise<TuiDriverResponse>((resolve, reject) => {
      const timeout = setTimeout(() => {
        this.pending.delete(requestId);
        reject(
          new TuiDriverError(
            `TUI Driver request ${requestId} timed out after ${timeoutMs}ms`,
            "request_timeout",
          ),
        );
      }, timeoutMs);
      this.pending.set(requestId, { resolve, reject, timeout });
    });
    const envelope = { ...request, request_id: requestId } as DriverEnvelope;
    const encoded = `${JSON.stringify(envelope)}\n`;
    if (Buffer.byteLength(encoded, "utf8") > MAX_DRIVER_LINE_BYTES) {
      this.rejectPending(
        requestId,
        new TuiDriverError(
          `TUI Driver request exceeds ${MAX_DRIVER_LINE_BYTES} bytes`,
          "request_too_large",
        ),
      );
      return response;
    }
    try {
      transport.writer.write(encoded);
    } catch (cause) {
      const error = asError(cause);
      this.rejectPending(requestId, error);
      this.transportFailed(error);
    }
    return response;
  }

  async hello(
    protocolVersion = TUI_DRIVER_PROTOCOL_VERSION,
  ): Promise<TuiDriverReady> {
    return expectResponse(
      await this.request({ type: "hello", protocol_version: protocolVersion }),
      "ready",
    );
  }

  async capabilities(): Promise<string[]> {
    return expectResponse(
      await this.request({ type: "capabilities" }),
      "capabilities",
    ).capabilities;
  }

  async state(): Promise<DriverState> {
    return responseState(
      expectResponse(await this.request({ type: "state" }), "state"),
    );
  }

  async ping(): Promise<void> {
    expectResponse(await this.request({ type: "ping" }), "pong");
  }

  async prompt(text: string, options?: TuiDriverRequestOptions): Promise<void> {
    expectResponse(
      await this.request({ type: "input_prompt", text }, options),
      "accepted",
    );
  }

  async slash(text: string, options?: TuiDriverRequestOptions): Promise<void> {
    expectResponse(
      await this.request({ type: "input_slash", text }, options),
      "accepted",
    );
  }

  async wait(
    until: WaitCondition,
    timeoutMs?: number,
    options: TuiDriverRequestOptions = {},
  ): Promise<TuiDriverWaitResponse> {
    const clientTimeout =
      options.timeoutMs ??
      Math.max(this.requestTimeoutMs, (timeoutMs ?? 0) + 1000);
    const response = await this.request(
      { type: "wait", until, timeout_ms: timeoutMs },
      { timeoutMs: clientTimeout },
    );
    if (response.type !== "wait_result" && response.type !== "wait_timeout") {
      throw unexpectedResponse(response, "wait_result or wait_timeout");
    }
    return response;
  }

  async snapshot(
    request: TuiSnapshotRequest,
    options?: TuiDriverRequestOptions,
  ): Promise<TuiFrame> {
    const response = expectResponse(
      await this.request({ type: "snapshot", ...request }, options),
      "snapshot",
    );
    return responseFrame(response);
  }

  async *snapshotPages(
    request: Omit<TuiSnapshotRequest, "frame_id">,
    options: SnapshotPageOptions = {},
  ): AsyncGenerator<TuiFrame, void, void> {
    const maxPages = options.maxPages ?? MAX_SNAPSHOT_PAGES;
    if (!Number.isSafeInteger(maxPages) || maxPages < 1) {
      throw new TuiDriverError(
        "maxPages must be a positive safe integer",
        "invalid_pagination",
      );
    }
    const requestOptions =
      options.timeoutMs === undefined
        ? undefined
        : { timeoutMs: options.timeoutMs };
    let page = await this.snapshot(request, requestOptions);
    yield page;
    for (
      let pageCount = 1;
      page.next_range && pageCount < maxPages;
      pageCount += 1
    ) {
      const nextRange = page.next_range;
      const previousEnd = page.returned_range.end;
      if (nextRange.start <= previousEnd || nextRange.end < nextRange.start) {
        throw new TuiDriverError(
          "frozen snapshot pagination did not advance",
          "invalid_pagination",
        );
      }
      page = await this.snapshot(
        {
          ...request,
          rows: nextRange,
          frame_id: page.frame_id,
        },
        requestOptions,
      );
      yield page;
    }
    if (page.next_range) {
      throw new TuiDriverError(
        `frozen snapshot exceeds ${maxPages} pages`,
        "pagination_limit",
      );
    }
  }

  async completeSnapshot(
    request: Omit<TuiSnapshotRequest, "frame_id">,
    options: SnapshotPageOptions = {},
  ): Promise<TuiFrame> {
    let combined: TuiFrame | undefined;
    for await (const page of this.snapshotPages(request, options)) {
      if (!combined) {
        combined = structuredClone(page);
        continue;
      }
      if (page.frame_id !== combined.frame_id) {
        throw new TuiDriverError(
          "snapshot frame changed during pagination",
          "frame_mismatch",
        );
      }
      combined.lines.push(...page.lines);
      if (combined.cells && page.cells) combined.cells.push(...page.cells);
      combined.returned_range.end = page.returned_range.end;
      if (page.next_range === undefined) {
        delete combined.next_range;
      } else {
        combined.next_range = page.next_range;
      }
    }
    if (!combined) {
      throw new TuiDriverError(
        "snapshot pagination returned no pages",
        "invalid_pagination",
      );
    }
    return combined;
  }

  async takeover(): Promise<void> {
    expectResponse(await this.request({ type: "takeover" }), "accepted");
  }

  async abort(): Promise<void> {
    expectResponse(await this.request({ type: "abort" }), "accepted");
  }

  async close(abortActiveTask = false): Promise<void> {
    if (this.closing) return;
    this.closing = true;
    try {
      if (this.connected) {
        expectResponse(
          await this.request(
            { type: "close", abort_active_task: abortActiveTask },
            { timeoutMs: this.requestTimeoutMs },
          ),
          "closed",
        );
      }
    } finally {
      this.disconnect();
    }
  }

  disconnect(): void {
    const transport = this.transport;
    this.transport = undefined;
    this._ready = undefined;
    this.closing = false;
    transport?.close();
    this.rejectAll(new TuiDriverDisconnectedError());
  }

  private async openSocket(): Promise<void> {
    if (!this.socketPath) throw new TuiDriverError("socket path is missing");
    await validateUnixSocket(this.socketPath);
    this.disconnect();
    this.armReadyWaiter();
    const socket = await connectUnixSocket(this.socketPath);
    this.attachSocket(socket);
    try {
      await this.waitUntilReady();
    } catch (error) {
      this.disconnect();
      throw error;
    }
  }

  private attachSocket(socket: Socket): void {
    this.transport = {
      writer: socket,
      close: () => socket.destroy(),
    };
    socket.on("data", (chunk: Buffer) => this.receiveBytes(chunk));
    socket.once("error", (error) => this.transportFailed(error));
    socket.once("close", () =>
      this.transportFailed(new TuiDriverDisconnectedError()),
    );
  }

  private armReadyWaiter(): void {
    this._ready = undefined;
    this.decoder = new StringDecoder("utf8");
    this.buffered = "";
    this.bufferedBytes = 0;
    this.readyWaiter = new Promise<TuiDriverReady>((resolve, reject) => {
      this.resolveReady = resolve;
      this.rejectReady = reject;
    });
  }

  private async waitUntilReady(): Promise<TuiDriverReady> {
    if (!this.readyWaiter) throw new TuiDriverDisconnectedError();
    let timeout: ReturnType<typeof setTimeout> | undefined;
    try {
      return await Promise.race([
        this.readyWaiter,
        new Promise<never>((_, reject) => {
          timeout = setTimeout(
            () =>
              reject(
                new TuiDriverError(
                  "TUI Driver ready handshake timed out",
                  "startup_timeout",
                ),
              ),
            this.startupTimeoutMs,
          );
        }),
      ]);
    } finally {
      if (timeout) clearTimeout(timeout);
    }
  }

  private receiveBytes(chunk: Uint8Array): void {
    this.bufferedBytes += chunk.byteLength;
    if (this.bufferedBytes > MAX_DRIVER_LINE_BYTES && !chunk.includes(0x0a)) {
      this.transportFailed(
        new TuiDriverError(
          `TUI Driver response exceeds ${MAX_DRIVER_LINE_BYTES} bytes`,
          "response_too_large",
        ),
      );
      return;
    }
    this.buffered += this.decoder.write(Buffer.from(chunk));
    let newline = this.buffered.indexOf("\n");
    while (newline >= 0) {
      const line = this.buffered.slice(0, newline).replace(/\r$/u, "");
      this.buffered = this.buffered.slice(newline + 1);
      this.bufferedBytes = Buffer.byteLength(this.buffered, "utf8");
      if (Buffer.byteLength(line, "utf8") > MAX_DRIVER_LINE_BYTES) {
        this.transportFailed(
          new TuiDriverError(
            `TUI Driver response exceeds ${MAX_DRIVER_LINE_BYTES} bytes`,
            "response_too_large",
          ),
        );
        return;
      }
      if (line) this.receiveLine(line);
      newline = this.buffered.indexOf("\n");
    }
  }

  private receiveLine(line: string): void {
    let response: TuiDriverResponse;
    try {
      response = JSON.parse(line) as TuiDriverResponse;
    } catch (cause) {
      this.transportFailed(
        new TuiDriverError(
          `TUI Driver emitted invalid JSON: ${asError(cause).message}`,
          "invalid_json",
        ),
      );
      return;
    }
    if (
      !response ||
      typeof response !== "object" ||
      typeof response.request_id !== "string"
    ) {
      this.transportFailed(
        new TuiDriverError(
          "TUI Driver emitted an invalid envelope",
          "invalid_envelope",
        ),
      );
      return;
    }
    if (response.type === "ready" && !this._ready) {
      if (
        response.minimum_protocol_version > TUI_DRIVER_PROTOCOL_VERSION ||
        response.protocol_version < TUI_DRIVER_PROTOCOL_VERSION
      ) {
        this.transportFailed(
          new TuiDriverError(
            `TUI Driver protocol ${TUI_DRIVER_PROTOCOL_VERSION} is incompatible with ${response.minimum_protocol_version}..=${response.protocol_version}`,
            "incompatible_protocol",
          ),
        );
        return;
      }
      this._ready = response;
      this.resolveReady?.(response);
      this.resolveReady = undefined;
      this.rejectReady = undefined;
      return;
    }
    if (response.type === "event") {
      for (const listener of this.notificationListeners)
        listener(response.event);
      return;
    }
    const pending = this.pending.get(response.request_id);
    if (!pending) {
      for (const listener of this.diagnosticListeners) listener(response);
      return;
    }
    this.pending.delete(response.request_id);
    clearTimeout(pending.timeout);
    if (response.type === "error") {
      pending.reject(new TuiDriverError(response.message, response.code));
    } else {
      pending.resolve(response);
    }
  }

  private rejectPending(requestId: string, error: Error): void {
    const pending = this.pending.get(requestId);
    if (!pending) return;
    this.pending.delete(requestId);
    clearTimeout(pending.timeout);
    pending.reject(error);
  }

  private rejectAll(error: Error): void {
    for (const requestId of [...this.pending.keys()])
      this.rejectPending(requestId, error);
  }

  private transportFailed(cause: unknown): void {
    const error =
      cause instanceof TuiDriverError
        ? cause
        : new TuiDriverDisconnectedError(asError(cause).message);
    const transport = this.transport;
    this.transport = undefined;
    this._ready = undefined;
    transport?.close();
    this.rejectReady?.(error);
    this.resolveReady = undefined;
    this.rejectReady = undefined;
    this.rejectAll(error);
  }
}

function expectResponse<T extends TuiDriverResponse["type"]>(
  response: TuiDriverResponse,
  type: T,
): Extract<TuiDriverResponse, { type: T }> {
  if (response.type !== type) throw unexpectedResponse(response, type);
  return response as Extract<TuiDriverResponse, { type: T }>;
}

function unexpectedResponse(
  response: TuiDriverResponse,
  expected: string,
): TuiDriverError {
  return new TuiDriverError(
    `TUI Driver returned ${response.type}; expected ${expected}`,
    "unexpected_response",
  );
}

function responseState(
  response: Extract<TuiDriverResponse, { type: "state" }>,
): DriverState {
  const { request_id: _requestId, type: _type, ...state } = response;
  return state as DriverState;
}

function responseFrame(
  response: Extract<TuiDriverResponse, { type: "snapshot" }>,
): TuiFrame {
  const { request_id: _requestId, type: _type, ...frame } = response;
  return frame as TuiFrame;
}

function positiveTimeout(
  value: number | undefined,
  fallback: number,
  name: string,
): number {
  const normalized = value ?? fallback;
  if (!Number.isSafeInteger(normalized) || normalized < 1) {
    throw new TuiDriverError(
      `${name} must be a positive safe integer`,
      "invalid_timeout",
    );
  }
  return normalized;
}

function requireAbsolutePath(value: string, name: string): string {
  const normalized = value.trim();
  if (
    !normalized ||
    (!normalized.startsWith("/") && !/^[A-Za-z]:[\\/]/u.test(normalized))
  ) {
    throw new TuiDriverError(
      `${name} must be an absolute path`,
      "invalid_path",
    );
  }
  return normalized;
}

async function validateUnixSocket(path: string): Promise<void> {
  let metadata;
  try {
    metadata = await lstat(path);
  } catch (cause) {
    throw new TuiDriverError(
      `cannot inspect Driver socket ${path}: ${asError(cause).message}`,
      "socket_unavailable",
    );
  }
  if (!metadata.isSocket()) {
    throw new TuiDriverError(
      `Driver path is not a Unix socket: ${path}`,
      "invalid_socket",
    );
  }
  if ((metadata.mode & 0o077) !== 0) {
    throw new TuiDriverError(
      `Driver socket must not grant group or world access: ${path}`,
      "insecure_socket",
    );
  }
  const getuid = process.geteuid ?? process.getuid;
  if (getuid && metadata.uid !== getuid()) {
    throw new TuiDriverError(
      `Driver socket is owned by another user: ${path}`,
      "insecure_socket",
    );
  }
}

function connectUnixSocket(path: string): Promise<Socket> {
  return new Promise((resolve, reject) => {
    const socket = createConnection({ path });
    const onError = (error: Error) => {
      socket.destroy();
      reject(
        new TuiDriverDisconnectedError(
          `cannot connect to Driver socket: ${error.message}`,
        ),
      );
    };
    socket.once("error", onError);
    socket.once("connect", () => {
      socket.removeListener("error", onError);
      resolve(socket);
    });
  });
}

function asError(cause: unknown): Error {
  return cause instanceof Error ? cause : new Error(String(cause));
}

export type { RowRange };
