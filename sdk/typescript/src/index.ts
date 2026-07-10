import { createParser } from "eventsource-parser";

import type {
  AppliedCandidate,
  AutomationCandidate,
  BenchmarkPromotion,
  CommandAck,
  EvaluationCase,
  EvaluationResult,
  EvaluationRun,
  EventFilter,
  GeneratedTask,
  ImprovementCandidate,
  MemoryRecord,
  PostTaskReview,
  PromotionDecision,
  RegressionResult,
  RuntimeEvent,
  RuntimeQuery,
  RuntimeQueryKind,
  SessionCommand,
  SessionCommandKind,
  SkillCandidate,
} from "./generated.js";

export * from "./generated.js";

export interface RuntimeHostInfo {
  instance_id: string;
  pid: number;
  base_url: string;
  workspace_root: string;
  workspace_id: string;
  default_session_id: string;
  default_thread_id: string;
  started_at: string;
}

export interface ThreadRecord {
  thread_id: string;
  session_id: string;
  parent_thread_id?: string | null;
  workspace_root?: string | null;
  title: string;
  preview: string;
  created_at: string;
  updated_at: string;
  recency_at: string;
  archived: boolean;
}

export interface SubscriptionOptions {
  signal?: AbortSignal;
  onError?: (error: Error) => void;
  initialRetryMs?: number;
  maxRetryMs?: number;
}

export interface RuntimeSubscription {
  readonly done: Promise<void>;
  close(): void;
}

export interface EvaluationSnapshot {
  cases: EvaluationCase[];
  runs: EvaluationRun[];
  results: EvaluationResult[];
  replays: unknown[];
  reviews: PostTaskReview[];
}

export interface AutomationSnapshot {
  candidates: AutomationCandidate[];
  generated_tasks: GeneratedTask[];
  skill_candidates: SkillCandidate[];
  benchmark_promotions: BenchmarkPromotion[];
  regressions: RegressionResult[];
  promotion_decisions: PromotionDecision[];
  applied_candidates: AppliedCandidate[];
}

export class GolutraClient {
  private readonly baseUrl: URL;

  constructor(baseUrl: string | URL) {
    this.baseUrl = new URL(baseUrl);
  }

  async runtimeInfo(): Promise<RuntimeHostInfo> {
    return this.getJson<RuntimeHostInfo>("/runtime/info");
  }

  async sendCommand(command: SessionCommand): Promise<CommandAck> {
    return this.postJson<CommandAck>("/commands", command);
  }

  async query<T = unknown>(query: RuntimeQuery): Promise<T> {
    return this.postJson<T>("/queries", query);
  }

  async listMemory(sessionId: string): Promise<MemoryRecord[]> {
    return this.query<MemoryRecord[]>(this.runtimeQuery(sessionId, "memory_list"));
  }

  async evaluationResults(sessionId: string): Promise<EvaluationSnapshot> {
    return this.query<EvaluationSnapshot>(this.runtimeQuery(sessionId, "evaluation_results"));
  }

  async improvementCandidates(sessionId: string): Promise<ImprovementCandidate[]> {
    return this.query<ImprovementCandidate[]>(
      this.runtimeQuery(sessionId, "improvement_candidates"),
    );
  }

  async automationCandidates(sessionId: string): Promise<AutomationSnapshot> {
    return this.query<AutomationSnapshot>(
      this.runtimeQuery(sessionId, "automation_candidates"),
    );
  }

  async rollbackMemory(
    sessionId: string,
    memoryId: string,
    reason = "rolled back by SDK user",
    actorId = "typescript-sdk",
  ): Promise<CommandAck> {
    return this.sendCommand(
      this.sessionCommand(sessionId, "memory_rollback", actorId, {
        memory_id: memoryId,
        reason,
      }),
    );
  }

  async runRegression(
    sessionId: string,
    candidateId: string,
    actorId = "typescript-sdk",
  ): Promise<CommandAck> {
    return this.sendCommand(
      this.sessionCommand(sessionId, "run_regression", actorId, {
        candidate_id: candidateId,
      }),
    );
  }

  async applyCandidate(
    sessionId: string,
    candidateId: string,
    actorId = "typescript-sdk",
  ): Promise<CommandAck> {
    return this.sendCommand(
      this.sessionCommand(sessionId, "apply_candidate", actorId, {
        candidate_id: candidateId,
      }),
    );
  }

  async rollbackCandidate(
    sessionId: string,
    candidateId: string,
    reason = "rolled back by SDK user",
    actorId = "typescript-sdk",
  ): Promise<CommandAck> {
    return this.sendCommand(
      this.sessionCommand(sessionId, "rollback_candidate", actorId, {
        candidate_id: candidateId,
        reason,
      }),
    );
  }

  async replayEvents(filter: EventFilter): Promise<RuntimeEvent[]> {
    return this.getJson<RuntimeEvent[]>(this.eventPath("/events/replay", filter));
  }

  async listThreads(limit = 20): Promise<ThreadRecord[]> {
    const url = new URL("/threads", this.baseUrl);
    url.searchParams.set("limit", String(limit));
    return this.getJson<ThreadRecord[]>(url);
  }

  async resumeThread(threadId: string): Promise<ThreadRecord> {
    return this.postJson<ThreadRecord>(`/threads/${encodeURIComponent(threadId)}/resume`, undefined);
  }

  async forkThread(threadId: string): Promise<ThreadRecord> {
    return this.postJson<ThreadRecord>(`/threads/${encodeURIComponent(threadId)}/fork`, undefined);
  }

  subscribe(
    filter: EventFilter,
    onEvent: (event: RuntimeEvent) => void,
    options: SubscriptionOptions = {},
  ): RuntimeSubscription {
    const controller = new AbortController();
    const abortFromCaller = () => controller.abort(options.signal?.reason);
    options.signal?.addEventListener("abort", abortFromCaller, { once: true });
    if (options.signal?.aborted) {
      abortFromCaller();
    }
    const done = this.runSubscription(filter, onEvent, options, controller.signal).finally(() => {
      options.signal?.removeEventListener("abort", abortFromCaller);
    });
    return {
      done,
      close: () => controller.abort(),
    };
  }

  private async runSubscription(
    filter: EventFilter,
    onEvent: (event: RuntimeEvent) => void,
    options: SubscriptionOptions,
    signal: AbortSignal,
  ): Promise<void> {
    let cursor = filter.after_sequence_no ?? undefined;
    let retryMs = options.initialRetryMs ?? 100;
    const maxRetryMs = options.maxRetryMs ?? 2_000;
    while (!signal.aborted) {
      try {
        const requestFilter: EventFilter =
          cursor === undefined ? { ...filter } : { ...filter, after_sequence_no: cursor };
        const headers = new Headers({ accept: "text/event-stream" });
        if (cursor !== undefined) {
          headers.set("last-event-id", String(cursor));
        }
        const response = await fetch(this.eventPath("/events", requestFilter), {
          headers,
          signal,
        });
        if (!response.ok) {
          throw new Error(`Golutra SSE failed: ${response.status} ${await response.text()}`);
        }
        if (!response.body) {
          throw new Error("Golutra SSE response has no body");
        }
        const decoder = new TextDecoder();
        const parser = createParser({
          maxBufferSize: 1_048_576,
          onEvent: (message) => {
            if (message.event === "lag") {
              return;
            }
            if (message.event === "error") {
              throw new Error(message.data);
            }
            const event = JSON.parse(message.data) as RuntimeEvent;
            if (cursor !== undefined && event.sequence_no <= cursor) {
              return;
            }
            onEvent(event);
            cursor = event.sequence_no;
            retryMs = options.initialRetryMs ?? 100;
          },
        });
        for await (const chunk of response.body) {
          parser.feed(decoder.decode(chunk, { stream: true }));
        }
        parser.feed(decoder.decode());
        if (!signal.aborted) {
          throw new Error("Golutra SSE connection closed");
        }
      } catch (cause) {
        if (signal.aborted) {
          return;
        }
        const error = cause instanceof Error ? cause : new Error(String(cause));
        options.onError?.(error);
        await abortableDelay(retryMs, signal);
        retryMs = Math.min(retryMs * 2, maxRetryMs);
      }
    }
  }

  private eventPath(path: string, filter: EventFilter): URL {
    const url = new URL(path, this.baseUrl);
    url.searchParams.set("session_id", filter.session_id);
    if (filter.task_id) {
      url.searchParams.set("task_id", filter.task_id);
    }
    if (filter.after_sequence_no !== undefined && filter.after_sequence_no !== null) {
      url.searchParams.set("cursor", String(filter.after_sequence_no));
    }
    return url;
  }

  private runtimeQuery(sessionId: string, kind: RuntimeQueryKind): RuntimeQuery {
    return {
      query_id: globalThis.crypto.randomUUID(),
      session_id: sessionId,
      task_id: null,
      kind,
      requester: "sdk",
      cursor: null,
      timestamp: new Date().toISOString(),
    };
  }

  private sessionCommand(
    sessionId: string,
    kind: SessionCommandKind,
    actorId: string,
    payload: Record<string, unknown>,
  ): SessionCommand {
    const commandId = globalThis.crypto.randomUUID();
    return {
      command_id: commandId,
      session_id: sessionId,
      kind,
      idempotency_key: `sdk-${kind}-${commandId}`,
      actor: { kind: "sdk", id: actorId },
      payload,
      timestamp: new Date().toISOString(),
    };
  }

  private async getJson<T>(path: string | URL): Promise<T> {
    const response = await fetch(path instanceof URL ? path : new URL(path, this.baseUrl));
    return decodeJson<T>(response);
  }

  private async postJson<T>(path: string, value: unknown): Promise<T> {
    const request: RequestInit = {
      method: "POST",
      headers: { "content-type": "application/json" },
    };
    if (value !== undefined) {
      request.body = JSON.stringify(value);
    }
    const response = await fetch(new URL(path, this.baseUrl), request);
    return decodeJson<T>(response);
  }
}

async function decodeJson<T>(response: Response): Promise<T> {
  if (!response.ok) {
    throw new Error(`Golutra request failed: ${response.status} ${await response.text()}`);
  }
  return (await response.json()) as T;
}

async function abortableDelay(milliseconds: number, signal: AbortSignal): Promise<void> {
  if (signal.aborted) {
    return;
  }
  await new Promise<void>((resolve) => {
    let settled = false;
    const finish = () => {
      if (settled) {
        return;
      }
      settled = true;
      clearTimeout(timeout);
      signal.removeEventListener("abort", finish);
      resolve();
    };
    const timeout = setTimeout(finish, milliseconds);
    signal.addEventListener("abort", finish, { once: true });
    if (signal.aborted) {
      finish();
    }
  });
}
