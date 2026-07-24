import { createParser } from "eventsource-parser";

import type {
  AgentStreamEvent,
  AgentThreadRef,
  AgentTurnResult,
  AgentTurnStartResponse,
  AppliedCandidate,
  ArtifactChunk,
  ArtifactReadRequest,
  AutomationCandidate,
  BenchmarkRun,
  BenchmarkPromotion,
  CausalComparison,
  CommandAck,
  ContextProjection,
  DebugProjection,
  CounterfactualReplay,
  EvaluationCase,
  EvaluationResult,
  EvaluationRun,
  ExternalVerificationSpec,
  ExternalEvaluationRecord,
  EvaluationProjection,
  EvolutionState,
  EventFilter,
  EventPage,
  EventPageDirection,
  EventPageRequest,
  GeneratedTask,
  ImprovementCandidate,
  MemoryRecord,
  OpenEndedBudget,
  PostTaskReview,
  PromotionDecision,
  RegressionResult,
  RuntimeEvent,
  RuntimeQuery,
  RuntimeQueryKind,
  SessionCommand,
  SessionCommandKind,
  SessionPage,
  SessionPageRequest,
  SessionWindow,
  SessionWindowRequest,
  SkillCandidate,
  StorageStats,
  TaskTracePage,
  TaskTraceRequest,
  TaskReconciliationDecision,
} from "./generated.js";

const JSON_REQUEST_TIMEOUT_MS = 30_000;
const MAX_JSON_RESPONSE_BYTES = 16 * 1024 * 1024;
const MAX_COMPLETE_TRACE_PAGES = 4096;
export const RUNTIME_PROTOCOL_VERSION = 7;

class HttpStatusError extends Error {
  constructor(
    readonly status: number,
    message: string,
  ) {
    super(`HTTP ${status}: ${message}`);
    this.name = "HttpStatusError";
  }

  get retryable(): boolean {
    return this.status === 408 || this.status === 409 || this.status === 410 ||
      this.status === 425 || this.status === 429 || this.status >= 500;
  }
}

export * from "./generated.js";

export interface RuntimeHostInfo {
  instance_id: string;
  pid: number;
  base_url: string;
  ipc_path?: string | null;
  cwd: string;
  workspace_id: string;
  default_session_id: string;
  default_thread_id: string;
  started_at: string;
}

export interface AppServerInfo {
  instance_id: string;
  pid: number;
  base_url: string;
  protocol_versions: {
    minimum: number;
    current: number;
  };
  started_at: string;
}

export interface RuntimeAttachment {
  attachment_id: string;
  runtime: RuntimeHostInfo;
}

export interface ThreadRecord {
  thread_id: string;
  session_id: string;
  parent_thread_id?: string | null;
  forked_from_turn_id?: string | null;
  forked_from_sequence_no?: number | null;
  workspace_root?: string | null;
  rebound_from_workspace_root?: string | null;
  rollout_path?: string | null;
  title: string;
  preview: string;
  created_at: string;
  updated_at: string;
  recency_at: string;
  archived: boolean;
}

export interface ForkThreadOptions {
  fromTurnId?: string;
}

export interface RolloutExport {
  thread_id: string;
  session_id: string;
  path: string;
  event_count: number;
  last_sequence_no: number | null;
}

export interface ThreadRebindResult {
  thread: ThreadRecord;
  previous_workspace_root: string;
  rollout_rebuilt: boolean;
  checkpoint_compatibility: string;
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

export interface AgentSubscriptionRequest {
  session_id: string;
  thread_id: string;
  command_id: string;
  start_cursor?: number | null;
  cursor?: number | null;
}

export interface ThreadRunOptions {
  outputSchema?: Record<string, unknown>;
  completionCriteria?: readonly string[];
  externalVerifiers?: readonly ExternalVerificationSpec[];
}

export interface ReconcileTaskOptions {
  taskId?: string;
  note?: string;
}

/** Optional bounds for one durable runtime event history page. */
export interface EventPageOptions {
  cursor?: number | null;
  direction?: EventPageDirection;
  limit?: number;
  task_id?: string | null;
}

/** A durable thread controlled by the shared app-server runtime. */
export class Thread {
  readonly thread: AgentThreadRef;

  constructor(
    readonly client: GolutraClient,
    reference: AgentThreadRef,
  ) {
    this.thread = { ...reference };
  }

  get threadId(): string {
    return this.thread.thread_id;
  }

  get sessionId(): string {
    return this.thread.session_id;
  }

  async run(prompt: string, options: ThreadRunOptions = {}): Promise<TurnHandle> {
    if (!prompt.trim()) {
      throw new Error("turn prompt cannot be empty");
    }
    const params: Record<string, unknown> = {
      thread_id: this.threadId,
      prompt,
      completion_criteria: [...(options.completionCriteria ?? [])].filter((value) => value.trim()),
      external_verifiers: [...(options.externalVerifiers ?? [])],
    };
    if (options.outputSchema !== undefined) {
      params.output_schema = options.outputSchema;
    }
    const start = await this.client.rpc<AgentTurnStartResponse>("turn/start", params);
    if (start.accepted !== true) {
      throw new Error(start.reason ?? "turn was rejected");
    }
    return new TurnHandle(this, start);
  }

  runStreamed(prompt: string, options: ThreadRunOptions = {}): Promise<TurnHandle> {
    return this.run(prompt, options);
  }

  async steer(prompt: string): Promise<CommandAck> {
    if (!prompt.trim()) {
      throw new Error("steering prompt cannot be empty");
    }
    return this.client.rpcCommand("turn/steer", {
      thread_id: this.threadId,
      prompt,
    });
  }

  async interrupt(): Promise<CommandAck> {
    return this.client.rpcCommand("turn/interrupt", { thread_id: this.threadId });
  }

  async takeover(): Promise<CommandAck> {
    return this.client.rpcCommand("turn/takeover", { thread_id: this.threadId });
  }

  async reconcileTask(
    decision: TaskReconciliationDecision,
    options: ReconcileTaskOptions = {},
  ): Promise<CommandAck> {
    return this.client.rpcCommand("task/reconcile", {
      thread_id: this.threadId,
      decision,
      ...(options.taskId !== undefined ? { task_id: options.taskId } : {}),
      ...(options.note !== undefined ? { note: options.note } : {}),
    });
  }

  async eventPage(
    request: EventPageOptions = {},
  ): Promise<EventPage> {
    const limit = request.limit ?? 128;
    if (!Number.isInteger(limit) || limit < 1 || limit > 512) {
      throw new Error("event page limit must be between 1 and 512");
    }
    const direction: EventPageDirection = request.direction ?? "backward";
    return this.client.eventPage({
      session_id: this.sessionId,
      task_id: request.task_id ?? null,
      cursor: request.cursor ?? null,
      direction,
      limit,
    });
  }

  async history(
    request: EventPageOptions = {},
  ): Promise<EventPage> {
    return this.eventPage(request);
  }
}

/** A single accepted turn and its normalized streaming lifecycle. */
export class TurnHandle {
  readonly commandId: string;
  private readonly startCursor: number;
  private cursor: number | null | undefined;
  private terminal: AgentTurnResult | undefined;

  constructor(
    private readonly thread: Thread,
    private readonly start: AgentTurnStartResponse,
  ) {
    this.commandId = start.command_id;
    this.cursor = start.cursor;
    this.startCursor = start.cursor ?? 0;
  }

  get acceptedStart(): AgentTurnStartResponse {
    return this.start;
  }

  async *events(signal?: AbortSignal): AsyncGenerator<AgentStreamEvent> {
    // The terminal event is already cached; the live SSE endpoint is not a
    // history query and must not be reopened after the turn was consumed.
    if (this.terminal) {
      return;
    }
    const queue = new AsyncEventQueue<AgentStreamEvent>();
    const subscriptionOptions: SubscriptionOptions = {};
    if (signal) {
      subscriptionOptions.signal = signal;
    }
    const subscription = this.thread.client.subscribeAgent(
      {
        session_id: this.thread.sessionId,
        thread_id: this.thread.threadId,
        command_id: this.commandId,
        start_cursor: this.startCursor,
        ...(this.cursor !== undefined ? { cursor: this.cursor } : {}),
      },
      (event) => {
        const sequence = agentEventSequence(event);
        if (sequence !== undefined && this.cursor !== null && this.cursor !== undefined) {
          if (sequence <= this.cursor) {
            return;
          }
        }
        if (sequence !== undefined) {
          this.cursor = sequence;
        }
        queue.push(event);
      },
      subscriptionOptions,
    );
    void subscription.done.then(
      () => queue.close(),
      (error: unknown) =>
        queue.fail(error instanceof Error ? error : new Error(String(error))),
    );
    try {
      while (true) {
        const next = await queue.next();
        if (next.done) {
          return;
        }
        if (next.value.type === "turn.completed" || next.value.type === "turn.failed") {
          this.terminal = next.value;
          yield next.value;
          return;
        }
        yield next.value;
      }
    } finally {
      subscription.close();
    }
  }

  async wait(): Promise<AgentTurnResult> {
    if (this.terminal) {
      return this.terminal;
    }
    for await (const _event of this.events()) {
      // Drain the shared stream until the projector emits a terminal event.
    }
    if (!this.terminal) {
      throw new Error("agent event stream ended before turn completion");
    }
    return this.terminal;
  }

  steer(prompt: string): Promise<CommandAck> {
    return this.thread.steer(prompt);
  }

  interrupt(): Promise<CommandAck> {
    return this.thread.interrupt();
  }

  resolveApproval(approvalId: string, approve: boolean): Promise<CommandAck> {
    return this.thread.client.rpcCommand("approval/resolve", {
      thread_id: this.thread.threadId,
      approval_id: approvalId,
      approve,
    });
  }
}

class AsyncEventQueue<T> {
  private readonly values: T[] = [];
  private readonly waiters: Array<{
    resolve: (result: IteratorResult<T>) => void;
    reject: (error: Error) => void;
  }> = [];
  private failure: Error | undefined;
  private closed = false;

  push(value: T): void {
    const waiter = this.waiters.shift();
    if (waiter) {
      waiter.resolve({ done: false, value });
    } else {
      this.values.push(value);
    }
  }

  fail(error: Error): void {
    this.failure = error;
    while (this.waiters.length > 0) {
      this.waiters.shift()?.reject(error);
    }
  }

  close(): void {
    this.closed = true;
    while (this.waiters.length > 0) {
      this.waiters.shift()?.resolve({ done: true, value: undefined as never });
    }
  }

  next(): Promise<IteratorResult<T>> {
    if (this.failure) {
      return Promise.reject(this.failure);
    }
    const value = this.values.shift();
    if (value !== undefined) {
      return Promise.resolve({ done: false, value });
    }
    if (this.closed) {
      return Promise.resolve({ done: true, value: undefined as never });
    }
    return new Promise((resolve, reject) => this.waiters.push({ resolve, reject }));
  }
}

export interface GolutraClientOptions {
  transportToken: string;
}

export interface EvaluationSnapshot {
  cases: EvaluationCase[];
  runs: EvaluationRun[];
  results: EvaluationResult[];
  replays: unknown[];
  reviews: PostTaskReview[];
  benchmark_runs: BenchmarkRun[];
  counterfactual_replays: CounterfactualReplay[];
  causal_comparisons: CausalComparison[];
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

export type EvolutionSnapshot = EvolutionState;

export class GolutraClient {
  private readonly baseUrl: URL;
  private readonly cwd: string;
  private readonly transportToken: string;
  private readonly actorId = `typescript-sdk-${globalThis.crypto.randomUUID()}`;
  private attachment: Promise<RuntimeAttachment> | undefined;

  constructor(baseUrl: string | URL, cwd: string, options: GolutraClientOptions) {
    this.baseUrl = new URL(baseUrl);
    const normalizedCwd = cwd.trim();
    if (!normalizedCwd) {
      throw new Error("GolutraClient requires a cwd");
    }
    if (!isAbsoluteFilesystemPath(normalizedCwd)) {
      throw new Error(`GolutraClient requires an absolute cwd: ${normalizedCwd}`);
    }
    this.cwd = normalizedCwd;
    const transportToken = options.transportToken.trim();
    if (transportToken.length < 32 || transportToken.length > 512 || /\s/u.test(transportToken)) {
      throw new Error(
        "GolutraClient transportToken must contain 32..=512 non-whitespace characters",
      );
    }
    this.transportToken = transportToken;
  }

  async runtimeInfo(): Promise<RuntimeHostInfo> {
    return (await this.runtimeAttachment()).runtime;
  }

  async rpc<T = unknown>(method: string, params: Record<string, unknown> = {}): Promise<T> {
    const response = await this.postJson<{
      error?: { code: number; message: string } | null;
      result?: unknown;
    }>("/rpc", {
      jsonrpc: "2.0",
      id: globalThis.crypto.randomUUID(),
      method,
      params,
    });
    if (response.error) {
      throw new Error(`JSON-RPC ${response.error.code}: ${response.error.message}`);
    }
    return response.result as T;
  }

  async rpcCommand(method: string, params: Record<string, unknown> = {}): Promise<CommandAck> {
    const result = await this.rpc<{ ack?: CommandAck }>(method, params);
    if (!result.ack) {
      throw new Error(`JSON-RPC ${method} did not return a command acknowledgement`);
    }
    return result.ack;
  }

  async startThread(): Promise<Thread> {
    const result = await this.rpc<{ thread: AgentThreadRef }>("thread/start");
    return new Thread(this, result.thread);
  }

  async resume(threadId: string): Promise<Thread> {
    const result = await this.rpc<{ thread: AgentThreadRef }>("thread/resume", {
      thread_id: threadId,
    });
    return new Thread(this, result.thread);
  }

  async serverInfo(): Promise<AppServerInfo> {
    const info = await this.rawJson<AppServerInfo>(new URL("/runtime/info", this.baseUrl));
    if (
      RUNTIME_PROTOCOL_VERSION < info.protocol_versions.minimum ||
      RUNTIME_PROTOCOL_VERSION > info.protocol_versions.current
    ) {
      throw new Error(
        `Golutra protocol ${RUNTIME_PROTOCOL_VERSION} is incompatible with server range ${info.protocol_versions.minimum}..=${info.protocol_versions.current}`,
      );
    }
    return info;
  }

  async eventPage(request: EventPageRequest): Promise<EventPage> {
    const url = new URL("/events/page", this.baseUrl);
    url.searchParams.set("session_id", request.session_id);
    if (request.task_id) {
      url.searchParams.set("task_id", request.task_id);
    }
    if (request.cursor !== undefined && request.cursor !== null) {
      url.searchParams.set("cursor", String(request.cursor));
    }
    url.searchParams.set("direction", request.direction);
    url.searchParams.set("limit", String(request.limit));
    return this.getJson<EventPage>(url);
  }

  async contextProjection(sessionId: string, taskId: string): Promise<ContextProjection> {
    return this.query<ContextProjection>(
      this.runtimeQuery(sessionId, "context_projection", taskId),
    );
  }

  async evaluationProjection(
    sessionId: string,
    taskId: string,
  ): Promise<EvaluationProjection> {
    return this.query<EvaluationProjection>(
      this.runtimeQuery(sessionId, "evaluation_projection", taskId),
    );
  }

  async debugProjection(sessionId: string, taskId: string): Promise<DebugProjection> {
    return this.query<DebugProjection>(this.runtimeQuery(sessionId, "debug_projection", taskId));
  }

  async taskTrace(request: TaskTraceRequest): Promise<TaskTracePage> {
    return this.postJson<TaskTracePage>("/traces", request);
  }

  async completeTaskTrace(request: TaskTraceRequest): Promise<TaskTracePage> {
    let nextRequest = { ...request };
    const trace = await this.taskTrace(nextRequest);
    for (let pageCount = 1; pageCount < MAX_COMPLETE_TRACE_PAGES; pageCount += 1) {
      if (!trace.has_more) {
        return trace;
      }
      const nextCursor = trace.next_cursor;
      if (nextCursor === undefined || nextCursor === null) {
        throw new Error("task trace page has_more without a next cursor");
      }
      if (nextRequest.cursor === nextCursor) {
        throw new Error("task trace cursor did not advance");
      }
      nextRequest = {
        ...nextRequest,
        cursor: nextCursor,
        wait_for_evaluation: false,
      };
      mergeTaskTracePage(trace, await this.taskTrace(nextRequest));
    }
    if (!trace.has_more) {
      return trace;
    }
    throw new Error(`task trace exceeds ${MAX_COMPLETE_TRACE_PAGES} pages`);
  }

  async readArtifactChunk(request: ArtifactReadRequest): Promise<ArtifactChunk | null> {
    return this.postJson<ArtifactChunk | null>("/artifacts/chunk", request);
  }

  async storageStatus(sessionId: string): Promise<StorageStats> {
    return this.query<StorageStats>(this.runtimeQuery(sessionId, "storage_status"));
  }

  async runStorageMaintenance(
    sessionId: string,
    actorId?: string,
  ): Promise<CommandAck> {
    return this.sendCommand(
      this.sessionCommand(
        sessionId,
        "run_storage_maintenance",
        actorId ?? this.actorId,
        {},
      ),
    );
  }

  async sendCommand(command: SessionCommand): Promise<CommandAck> {
    return this.postJson<CommandAck>("/commands", command);
  }

  async takeover(sessionId: string, actorId?: string): Promise<CommandAck> {
    return this.sendCommand(
      this.sessionCommand(sessionId, "takeover", actorId ?? this.actorId, {}),
    );
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

  async evolutionState(sessionId: string): Promise<EvolutionSnapshot> {
    return this.query<EvolutionSnapshot>(this.runtimeQuery(sessionId, "evolution_state"));
  }

  async planEvolution(
    sessionId: string,
    objective: string,
    budget?: Partial<OpenEndedBudget>,
    actorId?: string,
  ): Promise<CommandAck> {
    const normalizedBudget: OpenEndedBudget = {
      max_generated_tasks: budget?.max_generated_tasks ?? 20,
      max_selected_tasks: budget?.max_selected_tasks ?? 3,
      max_tool_calls_per_task: budget?.max_tool_calls_per_task ?? 8,
      max_runtime_ms_per_task: budget?.max_runtime_ms_per_task ?? 120_000,
    };
    return this.sendCommand(
      this.sessionCommand(sessionId, "plan_evolution", actorId ?? this.actorId, {
        objective,
        budget: normalizedBudget,
      }),
    );
  }

  async runEvolution(
    sessionId: string,
    runId?: string,
    actorId?: string,
  ): Promise<CommandAck> {
    return this.sendCommand(
      this.sessionCommand(sessionId, "run_evolution", actorId ?? this.actorId, {
        run_id: runId ?? null,
      }),
    );
  }

  async stageSkill(
    sessionId: string,
    candidateId: string,
    actorId?: string,
  ): Promise<CommandAck> {
    return this.sendCommand(
      this.sessionCommand(sessionId, "stage_skill", actorId ?? this.actorId, {
        candidate_id: candidateId,
      }),
    );
  }

  async reviewSkill(
    sessionId: string,
    skillId: string,
    decision: "approve" | "reject",
    reason: string,
    regressionRefs: string[] = [],
    actorId?: string,
  ): Promise<CommandAck> {
    return this.sendCommand(
      this.sessionCommand(sessionId, "review_skill", actorId ?? this.actorId, {
        skill_id: skillId,
        decision,
        reason,
        regression_refs: regressionRefs,
      }),
    );
  }

  async installSkill(
    sessionId: string,
    skillId: string,
    actorId?: string,
  ): Promise<CommandAck> {
    return this.sendCommand(
      this.sessionCommand(sessionId, "install_skill", actorId ?? this.actorId, {
        skill_id: skillId,
      }),
    );
  }

  async rollbackSkill(
    sessionId: string,
    skillId: string,
    reason = "rolled back by SDK user",
    actorId?: string,
  ): Promise<CommandAck> {
    return this.sendCommand(
      this.sessionCommand(sessionId, "rollback_skill", actorId ?? this.actorId, {
        skill_id: skillId,
        reason,
      }),
    );
  }

  async rollbackMemory(
    sessionId: string,
    memoryId: string,
    reason = "rolled back by SDK user",
    actorId?: string,
  ): Promise<CommandAck> {
    return this.sendCommand(
      this.sessionCommand(sessionId, "memory_rollback", actorId ?? this.actorId, {
        memory_id: memoryId,
        reason,
      }),
    );
  }

  async recordMemoryFeedback(
    sessionId: string,
    memoryId: string,
    feedback: "helpful" | "irrelevant" | "incorrect",
    reason = "",
    actorId?: string,
  ): Promise<CommandAck> {
    return this.sendCommand(
      this.sessionCommand(sessionId, "memory_feedback", actorId ?? this.actorId, {
        memory_id: memoryId,
        feedback,
        reason,
      }),
    );
  }

  async runRegression(
    sessionId: string,
    candidateId: string,
    actorId?: string,
  ): Promise<CommandAck> {
    return this.sendCommand(
      this.sessionCommand(sessionId, "run_regression", actorId ?? this.actorId, {
        candidate_id: candidateId,
      }),
    );
  }

  async runRegressionCampaign(
    sessionId: string,
    candidateId: string,
    options: {
      candidateFiles: readonly Record<string, unknown>[];
      caseRefs?: readonly string[];
      providerMatrix?: readonly string[];
      seeds?: readonly number[];
      minimumTrustedExternalPairs?: number;
      /** @deprecated Use minimumTrustedExternalPairs. */
      minimumTrustedExternalEvaluations?: number;
    },
    actorId?: string,
  ): Promise<CommandAck> {
    if (options.candidateFiles.length === 0) {
      throw new Error("regression campaign requires candidateFiles");
    }
    if (
      options.minimumTrustedExternalPairs !== undefined &&
      options.minimumTrustedExternalEvaluations !== undefined
    ) {
      throw new Error(
        "use minimumTrustedExternalPairs or its legacy evaluations alias, not both",
      );
    }
    const minimumTrustedExternalPairs =
      options.minimumTrustedExternalPairs ?? options.minimumTrustedExternalEvaluations;
    if (
      minimumTrustedExternalPairs !== undefined &&
      (!Number.isInteger(minimumTrustedExternalPairs) || minimumTrustedExternalPairs < 0)
    ) {
      throw new Error("minimumTrustedExternalPairs must be a non-negative integer");
    }
    return this.sendCommand(
      this.sessionCommand(sessionId, "run_regression_campaign", actorId ?? this.actorId, {
        candidate_id: candidateId,
        candidate_files: options.candidateFiles.map((file) => ({ ...file })),
        ...(options.caseRefs?.length ? { case_refs: [...options.caseRefs] } : {}),
        ...(options.providerMatrix?.length
          ? { provider_matrix: [...options.providerMatrix] }
          : {}),
        ...(options.seeds?.length ? { seeds: [...options.seeds] } : {}),
        ...(minimumTrustedExternalPairs !== undefined
          ? { minimum_trusted_external_pairs: minimumTrustedExternalPairs }
          : {}),
      }),
    );
  }

  async ingestExternalEvaluation(
    sessionId: string,
    record: ExternalEvaluationRecord,
    actorId?: string,
  ): Promise<CommandAck> {
    if (!record.evaluation_id) {
      throw new Error("external evaluation requires evaluation_id");
    }
    return this.sendCommand(
      this.sessionCommand(sessionId, "ingest_external_evaluation", actorId ?? this.actorId, {
        record: { ...record },
      }),
    );
  }

  async replay(
    sessionId: string,
    taskId: string,
    capsuleId?: string,
    actorId?: string,
  ): Promise<CommandAck> {
    return this.sendCommand(
      this.sessionCommand(sessionId, "replay", actorId ?? this.actorId, {
        task_id: taskId,
        ...(capsuleId ? { capsule_id: capsuleId } : {}),
      }),
    );
  }

  async reviewCandidate(
    sessionId: string,
    candidateId: string,
    decision: "approve" | "reject",
    reason: string,
    actorId?: string,
  ): Promise<CommandAck> {
    return this.sendCommand(
      this.sessionCommand(sessionId, "review_candidate", actorId ?? this.actorId, {
        candidate_id: candidateId,
        decision,
        reason,
      }),
    );
  }

  async recordBenchmark(
    sessionId: string,
    run: BenchmarkRun,
    actorId?: string,
  ): Promise<CommandAck> {
    return this.sendCommand(
      this.sessionCommand(sessionId, "record_benchmark", actorId ?? this.actorId, { run }),
    );
  }

  async compareCounterfactual(
    sessionId: string,
    groupId: string,
    actorId?: string,
  ): Promise<CommandAck> {
    return this.sendCommand(
      this.sessionCommand(sessionId, "compare_counterfactual", actorId ?? this.actorId, {
        group_id: groupId,
      }),
    );
  }

  async applyCandidate(
    sessionId: string,
    candidateId: string,
    actorId?: string,
  ): Promise<CommandAck> {
    return this.sendCommand(
      this.sessionCommand(sessionId, "apply_candidate", actorId ?? this.actorId, {
        candidate_id: candidateId,
      }),
    );
  }

  async rollbackCandidate(
    sessionId: string,
    candidateId: string,
    reason = "rolled back by SDK user",
    actorId?: string,
  ): Promise<CommandAck> {
    return this.sendCommand(
      this.sessionCommand(sessionId, "rollback_candidate", actorId ?? this.actorId, {
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

  async sessionPage(request: SessionPageRequest): Promise<SessionPage> {
    return this.postJson<SessionPage>("/sessions/page", request);
  }

  async sessionWindow(request: SessionWindowRequest): Promise<SessionWindow> {
    return this.postJson<SessionWindow>("/sessions/window", request);
  }

  async threadForSession(sessionId: string): Promise<ThreadRecord | null> {
    return this.getJson<ThreadRecord | null>(
      `/sessions/${encodeURIComponent(sessionId)}/thread`,
    );
  }

  async resumeThread(threadId: string): Promise<ThreadRecord> {
    return this.postJson<ThreadRecord>(`/threads/${encodeURIComponent(threadId)}/resume`, undefined);
  }

  async forkThread(threadId: string, options: ForkThreadOptions = {}): Promise<ThreadRecord> {
    return this.postJson<ThreadRecord>(`/threads/${encodeURIComponent(threadId)}/fork`, {
      from_turn_id: options.fromTurnId ?? null,
    });
  }

  async exportThreadRollout(threadId: string): Promise<RolloutExport> {
    return this.postJson<RolloutExport>(
      `/threads/${encodeURIComponent(threadId)}/rollout/export`,
      undefined,
    );
  }

  async rebindThread(threadId: string, fromWorkspaceRoot: string): Promise<ThreadRebindResult> {
    if (!isAbsoluteFilesystemPath(fromWorkspaceRoot)) {
      throw new Error(`rebind source must be an absolute path: ${fromWorkspaceRoot}`);
    }
    return this.postJson<ThreadRebindResult>(
      `/threads/${encodeURIComponent(threadId)}/rebind`,
      { from_workspace_root: fromWorkspaceRoot },
    );
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

  subscribeAgent(
    request: AgentSubscriptionRequest,
    onEvent: (event: AgentStreamEvent) => void,
    options: SubscriptionOptions = {},
  ): RuntimeSubscription {
    const controller = new AbortController();
    const abortFromCaller = () => controller.abort(options.signal?.reason);
    options.signal?.addEventListener("abort", abortFromCaller, { once: true });
    if (options.signal?.aborted) {
      abortFromCaller();
    }
    const done = this.runAgentSubscription(request, onEvent, options, controller.signal).finally(
      () => options.signal?.removeEventListener("abort", abortFromCaller),
    );
    return {
      done,
      close: () => controller.abort(),
    };
  }

  private async runAgentSubscription(
    request: AgentSubscriptionRequest,
    onEvent: (event: AgentStreamEvent) => void,
    options: SubscriptionOptions,
    signal: AbortSignal,
  ): Promise<void> {
    let cursor = request.cursor ?? undefined;
    let retryMs = options.initialRetryMs ?? 100;
    const maxRetryMs = options.maxRetryMs ?? 2_000;
    while (!signal.aborted) {
      try {
        const url = new URL("/agent/events", this.baseUrl);
        url.searchParams.set("session_id", request.session_id);
        url.searchParams.set("thread_id", request.thread_id);
        url.searchParams.set("command_id", request.command_id);
        if (request.start_cursor !== undefined && request.start_cursor !== null) {
          url.searchParams.set("start_cursor", String(request.start_cursor));
        }
        if (cursor !== undefined) {
          url.searchParams.set("cursor", String(cursor));
        }
        const headers = new Headers({ accept: "text/event-stream" });
        if (cursor !== undefined) {
          headers.set("last-event-id", String(cursor));
        }
        const response = await this.fetchWithAttachment(url, { headers, signal });
        if (!response.ok) {
          throw new HttpStatusError(
            response.status,
            await readBoundedResponseText(response),
          );
        }
        if (!response.body) {
          throw new Error("Golutra Agent SSE response has no body");
        }
        const decoder = new TextDecoder();
        let terminalSeen = false;
        const parser = createParser({
          maxBufferSize: 1_048_576,
          onEvent: (message) => {
            if (message.event === "error") {
              throw new Error(message.data);
            }
            const event = JSON.parse(message.data) as AgentStreamEvent;
            const sequence = agentEventSequence(event);
            if (cursor !== undefined && sequence !== undefined && sequence <= cursor) {
              return;
            }
            onEvent(event);
            terminalSeen = event.type === "turn.completed" || event.type === "turn.failed";
            if (sequence !== undefined) {
              cursor = sequence;
            }
            retryMs = options.initialRetryMs ?? 100;
          },
        });
        for await (const chunk of response.body) {
          parser.feed(decoder.decode(chunk, { stream: true }));
        }
        parser.feed(decoder.decode());
        if (terminalSeen) {
          return;
        }
        if (!signal.aborted) {
          throw new Error("Golutra Agent SSE connection closed");
        }
      } catch (cause) {
        if (signal.aborted) {
          return;
        }
        const error = cause instanceof Error ? cause : new Error(String(cause));
        if (error instanceof HttpStatusError && !error.retryable) {
          throw error;
        }
        options.onError?.(error);
        await abortableDelay(retryMs, signal);
        retryMs = Math.min(retryMs * 2, maxRetryMs);
      }
    }
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
        const response = await this.fetchWithAttachment(this.eventPath("/events", requestFilter), {
          headers,
          signal,
        });
        if (!response.ok) {
          throw new HttpStatusError(
            response.status,
            await readBoundedResponseText(response),
          );
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
        if (error instanceof HttpStatusError && !error.retryable) {
          throw error;
        }
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

  private runtimeQuery(
    sessionId: string,
    kind: RuntimeQueryKind,
    taskId?: string,
  ): RuntimeQuery {
    return {
      query_id: globalThis.crypto.randomUUID(),
      session_id: sessionId,
      task_id: taskId ?? null,
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
    const response = await this.fetchWithAttachment(
      path instanceof URL ? path : new URL(path, this.baseUrl),
      { signal: AbortSignal.timeout(JSON_REQUEST_TIMEOUT_MS) },
    );
    return decodeJson<T>(response);
  }

  private async postJson<T>(path: string, value: unknown): Promise<T> {
    const request: RequestInit = {
      method: "POST",
      headers: { "content-type": "application/json" },
      signal: AbortSignal.timeout(JSON_REQUEST_TIMEOUT_MS),
    };
    if (value !== undefined) {
      request.body = JSON.stringify(value);
    }
    const response = await this.fetchWithAttachment(new URL(path, this.baseUrl), request);
    return decodeJson<T>(response);
  }

  private async attachRuntime(cwd: string): Promise<RuntimeAttachment> {
    const response = await fetch(new URL("/runtime/attach", this.baseUrl), {
      method: "POST",
      headers: this.transportHeaders({ "content-type": "application/json" }),
      body: JSON.stringify({ cwd, protocol_version: RUNTIME_PROTOCOL_VERSION }),
      signal: AbortSignal.timeout(JSON_REQUEST_TIMEOUT_MS),
    });
    return decodeJson<RuntimeAttachment>(response);
  }

  private runtimeAttachment(): Promise<RuntimeAttachment> {
    if (!this.attachment) {
      const pending = this.attachRuntime(this.cwd);
      this.attachment = pending;
      void pending.catch(() => {
        if (this.attachment === pending) {
          this.attachment = undefined;
        }
      });
    }
    return this.attachment;
  }

  private async refreshRuntimeAttachment(staleAttachmentId: string): Promise<RuntimeAttachment> {
    const stalePromise = this.attachment;
    if (stalePromise) {
      const current = await stalePromise;
      if (current.attachment_id !== staleAttachmentId) {
        return current;
      }
      if (this.attachment === stalePromise) {
        this.attachment = undefined;
      }
    }
    return this.runtimeAttachment();
  }

  private async fetchWithAttachment(url: URL, init: RequestInit = {}): Promise<Response> {
    const send = (attachmentId: string) => {
      const headers = this.transportHeaders(init.headers);
      headers.set("x-golutra-attachment", attachmentId);
      return fetch(url, { ...init, headers });
    };
    const attachment = await this.runtimeAttachment();
    const response = await send(attachment.attachment_id);
    if (response.status !== 410) {
      return response;
    }
    const refreshed = await this.refreshRuntimeAttachment(attachment.attachment_id);
    return send(refreshed.attachment_id);
  }

  private async rawJson<T>(url: URL): Promise<T> {
    return decodeJson<T>(
      await fetch(url, {
        headers: this.transportHeaders(),
        signal: AbortSignal.timeout(JSON_REQUEST_TIMEOUT_MS),
      }),
    );
  }

  private transportHeaders(initial?: HeadersInit): Headers {
    const headers = new Headers(initial);
    headers.set("authorization", `Bearer ${this.transportToken}`);
    headers.set("x-golutra-actor-id", this.actorId);
    headers.set("x-golutra-protocol-version", String(RUNTIME_PROTOCOL_VERSION));
    return headers;
  }
}

export function mergeTaskTracePage(target: TaskTracePage, page: TaskTracePage): void {
  if (
    target.session_id !== page.session_id ||
    target.task_id !== page.task_id ||
    target.view !== page.view
  ) {
    throw new Error("cannot merge task trace pages from different requests");
  }
  if (target.integrity.event_chain_digest !== page.integrity.event_chain_digest) {
    target.integrity.unresolved_refs.push("integrity:event_chain_digest_mismatch");
  }
  target.integrity.event_count = Math.max(
    target.integrity.event_count,
    page.integrity.event_count,
  );
  target.integrity.first_sequence = optionalMin(
    target.integrity.first_sequence,
    page.integrity.first_sequence,
  );
  target.integrity.last_sequence = optionalMax(
    target.integrity.last_sequence,
    page.integrity.last_sequence,
  );
  target.integrity.unresolved_refs.push(...page.integrity.unresolved_refs);
  target.integrity.missing_sections.push(...page.integrity.missing_sections);
  target.integrity.retention_losses.push(...page.integrity.retention_losses);
  target.integrity.redacted_fields.push(...page.integrity.redacted_fields);
  target.events = dedupeBy([...target.events, ...page.events], (event) => event.id).sort(
    (left, right) => left.sequence_no - right.sequence_no,
  );
  target.context_snapshots = dedupeBy(
    [...target.context_snapshots, ...page.context_snapshots],
    (snapshot) => snapshot.snapshot_id,
  );
  target.artifacts = dedupeBy(
    [...target.artifacts, ...page.artifacts],
    (artifact) => artifact.artifact_id,
  );
  target.evidence = dedupeBy(
    [...target.evidence, ...page.evidence],
    (record) => record.evidence_id,
  );
  if (page.verification_plan !== undefined && page.verification_plan !== null) {
    target.verification_plan = page.verification_plan;
  }
  if (page.verification !== undefined && page.verification !== null) {
    target.verification = page.verification;
  }
  target.post_task_jobs = dedupeBy(
    [...target.post_task_jobs, ...page.post_task_jobs],
    (job) => job.job_id,
  );
  target.evaluation = page.evaluation;
  target.next_cursor = page.next_cursor ?? null;
  target.has_more = page.has_more;
  target.integrity.unresolved_refs = sortedUnique(target.integrity.unresolved_refs);
  target.integrity.missing_sections = sortedUnique(target.integrity.missing_sections);
  target.integrity.retention_losses = sortedUnique(target.integrity.retention_losses);
  target.integrity.redacted_fields = sortedUnique(target.integrity.redacted_fields);
  target.integrity.complete =
    !target.has_more &&
    target.integrity.unresolved_refs.length === 0 &&
    target.integrity.missing_sections.length === 0 &&
    target.integrity.retention_losses.length === 0;
}

function dedupeBy<T>(values: T[], key: (value: T) => string): T[] {
  const seen = new Set<string>();
  return values.filter((value) => {
    const identifier = key(value);
    if (seen.has(identifier)) {
      return false;
    }
    seen.add(identifier);
    return true;
  });
}

function sortedUnique(values: string[]): string[] {
  return [...new Set(values)].sort();
}

function optionalMin(
  left: number | null | undefined,
  right: number | null | undefined,
): number | null {
  if (left === undefined || left === null) {
    return right ?? null;
  }
  if (right === undefined || right === null) {
    return left;
  }
  return Math.min(left, right);
}

function optionalMax(
  left: number | null | undefined,
  right: number | null | undefined,
): number | null {
  if (left === undefined || left === null) {
    return right ?? null;
  }
  if (right === undefined || right === null) {
    return left;
  }
  return Math.max(left, right);
}

function agentEventSequence(event: AgentStreamEvent): number | undefined {
  if (event.type === "runtime.event") {
    return event.event.sequence_no;
  }
  if (
    event.type === "item.started" ||
    event.type === "item.updated" ||
    event.type === "item.completed"
  ) {
    return event.item.sequence_no ?? undefined;
  }
  if (event.type === "turn.completed" || event.type === "turn.failed") {
    return event.last_sequence_no ?? undefined;
  }
  return undefined;
}

async function decodeJson<T>(response: Response): Promise<T> {
  const body = await readBoundedResponseText(response);
  if (!response.ok) {
    throw new Error(`Golutra request failed: ${response.status} ${body}`);
  }
  try {
    return JSON.parse(body) as T;
  } catch (cause) {
    const detail = cause instanceof Error ? cause.message : String(cause);
    throw new Error(`Golutra response was not valid JSON: ${detail}`);
  }
}

async function readBoundedResponseText(
  response: Response,
  maxBytes = MAX_JSON_RESPONSE_BYTES,
): Promise<string> {
  const contentLength = response.headers.get("content-length");
  if (contentLength && /^\d+$/u.test(contentLength) && Number(contentLength) > maxBytes) {
    await response.body?.cancel();
    throw new Error(`Golutra response exceeds ${maxBytes} bytes`);
  }
  if (!response.body) {
    return "";
  }
  const reader = response.body.getReader();
  const chunks: Uint8Array[] = [];
  let totalBytes = 0;
  while (true) {
    const { done, value } = await reader.read();
    if (done) {
      break;
    }
    totalBytes += value.byteLength;
    if (totalBytes > maxBytes) {
      await reader.cancel();
      throw new Error(`Golutra response exceeds ${maxBytes} bytes`);
    }
    chunks.push(value);
  }
  const bytes = new Uint8Array(totalBytes);
  let offset = 0;
  for (const chunk of chunks) {
    bytes.set(chunk, offset);
    offset += chunk.byteLength;
  }
  return new TextDecoder().decode(bytes);
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

function isAbsoluteFilesystemPath(value: string): boolean {
  return value.startsWith("/") || /^[A-Za-z]:[\\/]/u.test(value) || value.startsWith("\\\\");
}
