// Generated from Golutra Rust protocol schemas. Do not edit manually.

/**
 * Selects how much deterministic completion policy is allowed to shape one
 * turn.  The open path leaves planning, tool order, and stopping to the
 * provider while retaining the runtime's safety, budget, and audit gates.
 */
export type AgentExecutionMode = "open" | "strict";
export type AgentItemKind =
  "user_message" | "assistant_message" | "model" | "tool" | "approval" | "verification" | "runtime";
export type AgentItemStatus = "in_progress" | "completed" | "failed" | "cancelled";
/**
 * Controls the model-visible tool surface without removing the underlying
 * executor capability or its policy checks.
 */
export type AgentToolProfile = "coding" | "full";
export type AgentStreamEvent =
  | {
      session_id: string;
      thread_id: string;
      timestamp: string;
      type: "thread.started";
      workspace_root?: string | null;
      [k: string]: unknown;
    }
  | {
      session_id: string;
      task_id?: string | null;
      thread_id: string;
      timestamp: string;
      turn_id?: string | null;
      type: "turn.started";
      [k: string]: unknown;
    }
  | {
      item: AgentItem;
      type: "item.started";
      [k: string]: unknown;
    }
  | {
      item: AgentItem;
      type: "item.updated";
      [k: string]: unknown;
    }
  | {
      item: AgentItem;
      type: "item.completed";
      [k: string]: unknown;
    }
  | {
      event: RuntimeEvent;
      type: "runtime.event";
      [k: string]: unknown;
    }
  | {
      final_message?: string | null;
      last_sequence_no?: number | null;
      session_id: string;
      status: TaskStatus;
      task_id?: string | null;
      thread_id: string;
      timestamp: string;
      turn_id?: string | null;
      type: "turn.completed";
      verification?: VerificationRecord | null;
      [k: string]: unknown;
    }
  | {
      error: string;
      final_message?: string | null;
      last_sequence_no?: number | null;
      session_id: string;
      status: TaskStatus;
      task_id?: string | null;
      thread_id: string;
      timestamp: string;
      turn_id?: string | null;
      type: "turn.failed";
      verification?: VerificationRecord | null;
      [k: string]: unknown;
    };
export type CausalRelation =
  | "parent"
  | "triggered_by"
  | "responds_to"
  | "derived_from"
  | "verifies"
  | "compares"
  | "supersedes";
export type RuntimeEventType =
  | "command_received"
  | "command_completed"
  | "command_accepted"
  | "command_rejected"
  | "session_created"
  | "thread_forked"
  | "thread_rebound"
  | "thread_renamed"
  | "thread_archived"
  | "thread_deleted"
  | "task_created"
  | "turn_started"
  | "step_started"
  | "step_completed"
  | "step_checkpointed"
  | "turn_queued"
  | "turn_updated"
  | "turn_cancelled"
  | "busy_policy_decided"
  | "controller_changed"
  | "context_built"
  | "provider_started"
  | "provider_streamed"
  | "provider_completed"
  | "provider_failed"
  | "token_usage_recorded"
  | "assistant_message"
  | "tool_started"
  | "tool_progress"
  | "tool_completed"
  | "policy_evaluated"
  | "verification_completed"
  | "loop_decided"
  | "checkpoint_created"
  | "task_completed"
  | "task_abort_requested"
  | "task_aborted"
  | "task_interrupted"
  | "task_uncertain"
  | "task_reconciled"
  | "task_paused"
  | "task_resumed"
  | "approval_requested"
  | "approval_resolved"
  | "user_question_requested"
  | "user_question_resolved"
  | "retry_scheduled"
  | "provider_fallback"
  | "provider_transport_fallback"
  | "provider_auth_required"
  | "provider_auth_submitted"
  | "provider_auth_cancelled"
  | "provider_configured"
  | "provider_probe_started"
  | "provider_probe_completed"
  | "provider_auth_failed"
  | "provider_rate_limited"
  | "provider_credential_refreshed"
  | "loop_guard_triggered"
  | "compaction_started"
  | "compaction_completed"
  | "compaction_failed"
  | "memory_retrieved"
  | "memory_promoted"
  | "memory_promotion_rejected"
  | "memory_rolled_back"
  | "memory_feedback_recorded"
  | "post_task_reviewed"
  | "evaluation_completed"
  | "improvement_candidate_created"
  | "automation_candidate_created"
  | "candidate_patch_frozen"
  | "regression_blocked"
  | "regression_completed"
  | "promotion_decided"
  | "candidate_applied"
  | "candidate_rolled_back"
  | "benchmark_recorded"
  | "counterfactual_compared"
  | "evolution_planned"
  | "evolution_task_started"
  | "evolution_task_completed"
  | "evolution_completed"
  | "skill_staged"
  | "skill_reviewed"
  | "skill_installed"
  | "skill_rolled_back"
  | "governor_decided"
  | "storage_maintenance_completed"
  | "context_snapshot_created"
  | "post_task_job_queued"
  | "post_task_job_started"
  | "post_task_job_completed"
  | "post_task_job_failed"
  | "post_task_stage_failed"
  | "verification_planned"
  | "verification_assertion_completed"
  | "continuation_decided"
  | "regression_campaign_started"
  | "regression_execution_completed"
  | "memory_candidate_quarantined"
  | "memory_activated"
  | "memory_invalidated"
  | "failure_diagnosed"
  | "failure_episode_recorded"
  | "diagnostic_slice_created"
  | "replay_capsule_created"
  | "replay_executed"
  | "external_evaluation_ingested"
  | "external_evaluation_compared"
  | "candidate_ready"
  | "verification_ready"
  | "external_verification_requested"
  | "external_verification_feedback";
export type RuntimeEventSource =
  | "runtime"
  | "provider"
  | "tool"
  | "policy"
  | "verifier"
  | "memory"
  | "evaluator"
  | "governor"
  | "evolution"
  | "user";
export type TaskStatus =
  | "idle"
  | "running"
  | "waiting_approval"
  | "waiting_authentication"
  | "pausing"
  | "paused"
  | "aborting"
  | "completed"
  | "partial"
  | "failed"
  | "blocked"
  | "cancelled"
  | "interrupted"
  | "uncertain";
export type VerificationAssertionKind =
  | "file_state"
  | "diff"
  | "command_exit"
  | "test"
  | "diagnostic"
  | "schema"
  | "policy"
  | "delivery"
  | "assistant_response";
export type VerificationAssertionStatus =
  "pending" | "pass" | "fail" | "unknown" | "not_applicable";
export type VerificationCheckKind =
  | "tool_execution"
  | "workspace_change"
  | "objective_validation"
  | "assistant_response"
  | "schema"
  | "policy";
export type VerificationResult = "pass" | "fail" | "partial" | "unknown";
export type ExecutionOutcome =
  | "running"
  | "candidate_ready"
  | "completed"
  | "partial"
  | "failed"
  | "aborted"
  | "blocked"
  | "cancelled"
  | "interrupted"
  | "uncertain";
export type FailureClass =
  | "runtime_control_flow"
  | "context"
  | "provider"
  | "tool"
  | "policy"
  | "verification"
  | "external_evaluation"
  | "environment"
  | "timeout"
  | "unknown";
export type AutomationCandidateKind = "benchmark" | "generated_task" | "skill" | "runtime_change";
export type CandidateRisk = "low" | "medium" | "high" | "critical";
export type CandidateStatus =
  | "proposed"
  | "regression_passed"
  | "needs_human_review"
  | "approved"
  | "applied"
  | "rejected"
  | "rolled_back";
export type BenchmarkCheckStatus = "pass" | "fail" | "unknown" | "not_applicable";
export type EvaluationPartitionKind =
  "source" | "historical" | "generated" | "holdout" | "adversarial";
export type ActorKind = "user" | "api" | "tui" | "cli" | "sdk" | "web" | "ide" | "runtime";
export type SessionCommandKind =
  | "create"
  | "rename_thread"
  | "archive_thread"
  | "delete_thread"
  | "prompt"
  | "update_queued_turn"
  | "cancel_queued_turn"
  | "approve"
  | "deny"
  | "answer_question"
  | "pause"
  | "resume"
  | "abort"
  | "reconcile_task"
  | "takeover"
  | "compact"
  | "memory_rollback"
  | "memory_feedback"
  | "run_regression"
  | "review_candidate"
  | "apply_candidate"
  | "rollback_candidate"
  | "record_benchmark"
  | "ingest_external_evaluation"
  | "compare_counterfactual"
  | "plan_evolution"
  | "run_evolution"
  | "stage_skill"
  | "review_skill"
  | "install_skill"
  | "rollback_skill"
  | "provider_configured"
  | "provider_auth_submitted"
  | "provider_auth_cancelled"
  | "run_storage_maintenance"
  | "wait_post_task_job"
  | "retry_post_task_job"
  | "run_regression_campaign"
  | "review_memory_candidate"
  | "expire_memory"
  | "verify"
  | "replay"
  | "export";
export type BudgetOverflowAction = "trim" | "compact" | "ask_user" | "block";
export type RedactionStatus = "raw" | "redacted" | "not_required";
export type BusyPolicy = "append" | "inject" | "interrupt" | "reject";
export type FailureDomain =
  | "runtime_control_flow"
  | "context"
  | "provider"
  | "tool"
  | "policy"
  | "verification"
  | "memory"
  | "external_evaluation"
  | "unknown";
export type EvidenceStrength = "weak" | "medium" | "strong";
export type ExternalEvaluationPhaseKind =
  "setup" | "agent" | "test" | "assertion" | "teardown" | "other";
export type ExternalEvaluationPhaseStatus =
  "passed" | "failed" | "timed_out" | "error" | "skipped" | "unknown";
export type RegressionExecutionRole = "baseline" | "candidate";
export type ExternalEvaluationTrust = "untrusted_local" | "owner_local" | "signed";
export type EvaluationVerdict = "pass" | "fail" | "partial" | "unknown";
export type FailureSignalKind = "producer" | "self_check" | "external_assertion";
export type FailureEpisodeStatus = "active" | "recovered" | "superseded";
export type LoopAction =
  | "continue"
  | "compact"
  | "retry"
  | "fallback"
  | "ask_user"
  | "verify"
  | "stop_success"
  | "stop_partial"
  | "stop_failed"
  | "blocked";
export type PostTaskJobKind = "deep_evaluation" | "candidate_generation" | "regression_execution";
export type PostTaskJobStatus =
  "queued" | "leased" | "running" | "succeeded" | "failed" | "cancelled";
export type ReplayMode = "projection" | "deterministic_control_flow" | "live_regression";
export type ReplayExecutionStatus = "matched" | "diverged" | "incomplete" | "failed";
export type ToolResultStatus = "ok" | "error" | "blocked" | "cancelled" | "timeout";
export type PromotionDecisionKind = "approve" | "reject" | "needs_human_review";
export type PromotionReviewer = "system" | "human" | "agent";
export type RegressionVerdict = "pass" | "fail" | "needs_review";
export type ReviewMode = "minimal" | "deep";
export type EventPageDirection = "forward" | "backward";
export type OpenEndedRunStatus = "planned" | "running" | "completed" | "blocked";
export type SkillLifecycleStatus =
  "proposed" | "reviewed" | "rejected" | "installed" | "rolled_back";
export type GovernorAction = "allow" | "warn" | "ask_user" | "block";
export type GovernorPhase = "provider" | "tool" | "tool_result" | "completion";
export type MemoryScope = "project" | "user" | "global";
export type MemoryStatus =
  "proposed" | "quarantined" | "active" | "deprecated" | "rolled_back" | "expired";
export type RuntimeQueryKind =
  | "session_state"
  | "task_state"
  | "user_projection"
  | "debug_projection"
  | "context_projection"
  | "evaluation_projection"
  | "replay_cursor"
  | "memory_list"
  | "evaluation_results"
  | "improvement_candidates"
  | "automation_candidates"
  | "evolution_state"
  | "provider_state"
  | "storage_status"
  | "task_trace"
  | "post_task_jobs"
  | "artifact_chunk";
export type RegressionExecutionStatus =
  "queued" | "running" | "succeeded" | "failed" | "inconclusive";
export type SessionRangeDirection = "single" | "newer" | "older";
export type TaskReconciliationDecision =
  "no_side_effect_observed" | "side_effect_observed" | "abandon";
export type TaskRecoveryDisposition = "interrupted" | "uncertain";
export type InterruptedToolAction = "replay_safe" | "reconcile_before_retry" | "replay_forbidden";
export type SideEffectType = "none" | "file" | "process" | "network" | "external_system";
export type VerificationDimensionStatus = "pass" | "fail" | "partial" | "unknown";
export type TaskClass =
  "plain_conversation" | "read_only_analysis" | "workspace_change" | "code_change";
export type TraceView = "summary" | "full" | "forensic";
export type DriverEnvelope = {
  request_id: string;
  [k: string]: unknown;
} & DriverEnvelope1;
export type DriverEnvelope1 =
  | {
      protocol_version?: number | null;
      type: "hello";
      [k: string]: unknown;
    }
  | {
      type: "capabilities";
      [k: string]: unknown;
    }
  | {
      type: "state";
      [k: string]: unknown;
    }
  | {
      type: "ping";
      [k: string]: unknown;
    }
  | {
      text: string;
      type: "input_prompt";
      [k: string]: unknown;
    }
  | {
      text: string;
      type: "input_slash";
      [k: string]: unknown;
    }
  | {
      key: DriverKey;
      type: "input_key";
      [k: string]: unknown;
    }
  | {
      text: string;
      type: "input_paste";
      [k: string]: unknown;
    }
  | {
      event: DriverMouseEvent;
      type: "input_mouse";
      [k: string]: unknown;
    }
  | {
      height: number;
      type: "resize";
      width: number;
      [k: string]: unknown;
    }
  | {
      timeout_ms?: number | null;
      type: "wait";
      until: WaitCondition;
      [k: string]: unknown;
    }
  | {
      detail?: "text" | "cells";
      frame_id?: string | null;
      height: number;
      panes?: "transcript" | "developer" | "response_and_developer" | "full_screen";
      rows?: RowRange | null;
      scope?: "current_turn" | "task" | "session" | "screen";
      type: "snapshot";
      width: number;
      [k: string]: unknown;
    }
  | {
      type: "metrics";
      [k: string]: unknown;
    }
  | {
      type: "takeover";
      [k: string]: unknown;
    }
  | {
      type: "abort";
      [k: string]: unknown;
    }
  | {
      abort_active_task?: boolean;
      type: "close";
      [k: string]: unknown;
    };
export type DriverKey =
  | (
      | "enter"
      | "escape"
      | "up"
      | "down"
      | "left"
      | "right"
      | "page_up"
      | "page_down"
      | "home"
      | "end"
      | "backspace"
      | "delete"
      | "tab"
      | "ctrl_c"
    )
  | {
      char: string;
    };
export type DriverMouseKind = "left_click" | "scroll_up" | "scroll_down";
export type WaitCondition =
  | {
      kind: "ready";
      [k: string]: unknown;
    }
  | {
      kind: "idle";
      [k: string]: unknown;
    }
  | {
      kind: "task_started";
      [k: string]: unknown;
    }
  | {
      kind: "task_terminal";
      [k: string]: unknown;
    }
  | {
      kind: "turn_terminal";
      [k: string]: unknown;
    }
  | {
      kind: "approval_required";
      [k: string]: unknown;
    }
  | {
      kind: "authentication_required";
      [k: string]: unknown;
    }
  | {
      kind: "evaluation_terminal";
      [k: string]: unknown;
    }
  | {
      event_type: string;
      kind: "event";
      sequence_at_least?: number | null;
      [k: string]: unknown;
    };
export type DriverResponseEnvelope = {
  request_id: string;
  [k: string]: unknown;
} & DriverResponseEnvelope1;
export type DriverResponseEnvelope1 =
  | {
      controller_mode: DriverControllerMode;
      instance_id: string;
      minimum_protocol_version: number;
      protocol_version: number;
      session_id: string;
      thread_id: string;
      type: "ready";
      workspace_id: string;
      workspace_path: string;
      [k: string]: unknown;
    }
  | {
      capabilities: string[];
      type: "capabilities";
      [k: string]: unknown;
    }
  | {
      closed: boolean;
      controller_mode: DriverControllerMode;
      facts_expanded: boolean;
      height: number;
      instance_id: string;
      session_id: string;
      status: DriverTaskStatus;
      task_id?: string | null;
      thread_id: string;
      turn_id?: string | null;
      type: "state";
      width: number;
      [k: string]: unknown;
    }
  | {
      type: "pong";
      [k: string]: unknown;
    }
  | {
      cells?: TuiFrameCell[] | null;
      complete: boolean;
      event_high_watermark?: number | null;
      frame_id: string;
      height: number;
      hit_regions?: TuiHitRegion[];
      instance_id: string;
      lines: TuiFrameLine[];
      missing_sections: string[];
      next_range?: RowRange | null;
      panes: SnapshotPanes;
      redaction_status: RedactionStatus;
      returned_range: RowRange;
      scope: SnapshotScope;
      session_id: string;
      task_id?: string | null;
      total_rows: number;
      turn_id?: string | null;
      type: "snapshot";
      width: number;
      workspace_id: string;
      [k: string]: unknown;
    }
  | {
      metrics: DriverMetrics;
      type: "metrics";
      [k: string]: unknown;
    }
  | {
      message: string;
      type: "accepted";
      [k: string]: unknown;
    }
  | {
      condition: WaitCondition;
      state: DriverState;
      type: "wait_result";
      [k: string]: unknown;
    }
  | {
      condition: WaitCondition;
      state: DriverState;
      type: "wait_timeout";
      [k: string]: unknown;
    }
  | {
      event: DriverNotification;
      type: "event";
      [k: string]: unknown;
    }
  | {
      type: "closed";
      [k: string]: unknown;
    }
  | {
      code: string;
      message: string;
      type: "error";
      [k: string]: unknown;
    };
export type DriverControllerMode = "controller" | "observer";
export type DriverTaskStatus =
  | "connecting"
  | "idle"
  | "running"
  | "waiting_approval"
  | "waiting_authentication"
  | "pausing"
  | "paused"
  | "aborting"
  | "completed"
  | "partial"
  | "failed"
  | "blocked"
  | "cancelled"
  | "interrupted"
  | "uncertain";
export type TuiFramePane = "transcript" | "developer" | "response_and_developer" | "screen";
export type TuiHitPane = "transcript" | "bottom" | "developer" | "overlay";
export type SnapshotPanes = "transcript" | "developer" | "response_and_developer" | "full_screen";
export type SnapshotScope = "current_turn" | "task" | "session" | "screen";
export type DriverNotificationKind =
  "heartbeat" | "runtime_event_available" | "state_changed" | "task_terminal";

export interface SdkProtocolBundle {
  agent_execution_mode: AgentExecutionMode;
  agent_item: AgentItem;
  agent_steer_options: AgentSteerOptions;
  agent_stream_event: AgentStreamEvent;
  agent_thread_ref: AgentThreadRef;
  agent_tool_profile: AgentToolProfile;
  agent_turn_execution_options: AgentTurnExecutionOptions;
  agent_turn_options: AgentTurnOptions;
  agent_turn_result: AgentTurnResult;
  agent_turn_start: AgentTurnStart;
  agent_turn_start_response: AgentTurnStartResponse;
  applied_candidate: AppliedCandidate;
  artifact_chunk: ArtifactChunk;
  artifact_read_request: ArtifactReadRequest;
  automation_candidate: AutomationCandidate;
  benchmark_promotion: BenchmarkPromotion;
  benchmark_run: BenchmarkRun;
  causal_comparison: CausalComparison;
  command: SessionCommand;
  command_ack: CommandAck;
  context_projection: ContextProjection;
  cost_record: CostRecord;
  counterfactual_replay: CounterfactualReplay;
  debug_projection: DebugProjection;
  environment_recipe: EnvironmentRecipe;
  evaluation_case: EvaluationCase;
  evaluation_projection: EvaluationProjection;
  evaluation_result: EvaluationResult;
  evaluation_run: EvaluationRun;
  event: RuntimeEvent;
  event_filter: EventFilter;
  event_page: EventPage;
  event_page_request: EventPageRequest;
  evolution_state: EvolutionState;
  generated_task: GeneratedTask;
  generated_task_execution: GeneratedTaskExecution;
  governor_decision: RuntimeGovernorDecision;
  improvement_candidate: ImprovementCandidate;
  json_rpc_notification: JsonRpcNotification;
  json_rpc_request: JsonRpcRequest;
  json_rpc_response: JsonRpcResponse;
  memory_record: MemoryRecord;
  novelty_record: NoveltyRecord;
  open_ended_budget: OpenEndedBudget;
  open_ended_run: OpenEndedRun;
  post_task_review: PostTaskReview;
  promotion_decision: PromotionDecision;
  protocol_handshake: ProtocolHandshake;
  query: RuntimeQuery;
  regression_campaign: RegressionCampaign;
  regression_execution: RegressionExecution;
  regression_result: RegressionResult;
  replay_capsule: ReplayCapsule;
  replay_execution: ReplayExecution;
  security_utility_result: SecurityUtilityResult;
  session_page: SessionPage;
  session_page_request: SessionPageRequest;
  session_window: SessionWindow;
  session_window_request: SessionWindowRequest;
  skill_candidate: SkillCandidate;
  skill_lifecycle_record: SkillLifecycleRecord;
  skill_manifest: SkillManifest;
  state_projection: StateProjection;
  storage_maintenance_report: StorageMaintenanceReport;
  storage_stats: StorageStats;
  task_reconciliation_decision: TaskReconciliationDecision;
  task_reconciliation_record: TaskReconciliationRecord;
  task_recovery_record: TaskRecoveryRecord;
  task_trace_page: TaskTracePage;
  task_trace_request: TaskTraceRequest;
  tui_driver: TuiDriverProtocolBundle;
  user_projection: UserProjection;
  user_question_request: UserQuestionRequest;
  user_question_resolution: UserQuestionResolution;
  [k: string]: unknown;
}
export interface AgentItem {
  content?: string | null;
  data: unknown;
  id: string;
  kind: AgentItemKind;
  runtime_event_id?: string | null;
  sequence_no?: number | null;
  status: AgentItemStatus;
  title: string;
  [k: string]: unknown;
}
/**
 * Optional execution-surface override for a steering continuation. Steering
 * cannot replace the active task contract or execution mode.
 */
export interface AgentSteerOptions {
  tool_profile?: AgentToolProfile | null;
  [k: string]: unknown;
}
export interface RuntimeEvent {
  causal_context?: CausalContext;
  causal_links?: CausalLink[];
  durable: boolean;
  event_type: RuntimeEventType;
  id: string;
  parent_event_id?: string | null;
  payload: unknown;
  payload_ref?: string | null;
  schema_version?: number;
  sequence_no: number;
  session_id: string;
  source: RuntimeEventSource;
  task_id?: string | null;
  timestamp: string;
  turn_id?: string | null;
  [k: string]: unknown;
}
/**
 * Correlation identifiers propagated through one governed runtime execution.
 *
 * The event envelope remains authoritative for session/task/turn ownership.
 * Repeating those identifiers here makes detached facts self-describing and
 * lets integrity validation reject mismatched context rather than guessing.
 */
export interface CausalContext {
  candidate_id?: string | null;
  provider_request_id?: string | null;
  provider_response_id?: string | null;
  provider_round_id?: string | null;
  provider_tool_call_id?: string | null;
  regression_campaign_id?: string | null;
  run_id?: string | null;
  session_id?: string | null;
  step_id?: string | null;
  step_no?: number | null;
  task_id?: string | null;
  tool_call_id?: string | null;
  turn_id?: string | null;
  verification_id?: string | null;
  workspace_id?: string | null;
  [k: string]: unknown;
}
export interface CausalLink {
  event_id: string;
  relation: CausalRelation;
  [k: string]: unknown;
}
export interface VerificationRecord {
  assertions?: VerificationAssertion[];
  checks: VerificationCheck[];
  completion_criteria: string[];
  environment_digest?: string | null;
  evidence_refs: string[];
  independence?: "unspecified" | "runtime_evidence" | "independent";
  objective: string;
  /**
   * The fixed plan and assertion statuses are copied into the record so a
   * consumer can validate a terminal result without guessing from event
   * prose or loading a second mutable object.
   */
  plan_id?: string | null;
  policy_status: string;
  residual_risks: string[];
  result: VerificationResult;
  source?: "runtime" | "external_verifier" | "mixed";
  task_id: string;
  verification_id: string;
  [k: string]: unknown;
}
export interface VerificationAssertion {
  assertion_id: string;
  blocking: boolean;
  criterion_id: string;
  evidence_refs: string[];
  expected: string;
  kind: VerificationAssertionKind;
  message: string;
  required_evidence_strength: string;
  status: VerificationAssertionStatus;
  subject: string;
  verifier_id: string;
  [k: string]: unknown;
}
export interface VerificationCheck {
  command?: string | null;
  evidence_refs: string[];
  kind: VerificationCheckKind;
  message: string;
  name: string;
  passed: boolean;
  [k: string]: unknown;
}
export interface AgentThreadRef {
  session_id: string;
  thread_id: string;
  workspace_root?: string | null;
  [k: string]: unknown;
}
/**
 * Selects the model-facing execution surface for a newly started turn.
 *
 * This is separate from [`AgentTurnOptions`] so adding execution profiles does
 * not break existing Rust callers that construct that options type directly.
 */
export interface AgentTurnExecutionOptions {
  /**
   * Selects how much deterministic completion policy is allowed to shape one
   * turn.  The open path leaves planning, tool order, and stopping to the
   * provider while retaining the runtime's safety, budget, and audit gates.
   */
  execution_mode?: "open" | "strict";
  /**
   * Controls the model-visible tool surface without removing the underlying
   * executor capability or its policy checks.
   */
  tool_profile?: "coding" | "full";
  [k: string]: unknown;
}
export interface AgentTurnOptions {
  /**
   * Request network access for child tools. The runtime host may still
   * reject this request when its capability is disabled.
   */
  allow_network?: boolean;
  completion_criteria?: string[];
  /**
   * Keep the typed outcome open for a later evaluator overlay.
   */
  defer_external_verification?: boolean;
  /**
   * Discover conservative project checks when no explicit verifier list is
   * supplied. Set this to false to send an explicit empty list.
   */
  discover_project_verifiers?: boolean;
  /**
   * Caller-owned commands that objectively verify the candidate workspace
   * after the model stops. These commands are argv-based and are never
   * interpreted by a shell.
   */
  external_verifiers?: ExternalVerificationSpec[];
  /**
   * Optional wall-clock budget for this turn. Active provider sessions and
   * newly scheduled provider, tool, verifier, or correction work are
   * bounded by this deadline so callers can retain a terminal candidate
   * before an outer harness timeout.
   */
  max_elapsed_ms?: number | null;
  output_schema?: {
    [k: string]: unknown;
  };
  /**
   * Explicit runtime completion/verification contract.  `None` keeps wire
   * compatibility for older clients; the application adapter supplies a
   * normalized default before execution.
   */
  task_contract?: TaskContract | null;
  /**
   * Disable workspace, sensitive-path, shell and OS sandbox restrictions
   * for this turn. Network environment remains a separate host capability,
   * but process-only execution cannot enforce OS-level network isolation.
   */
  yolo?: boolean;
  [k: string]: unknown;
}
export interface ExternalVerificationSpec {
  args?: string[];
  cwd?: string;
  expected_exit_code?: number;
  max_output_bytes?: number;
  program: string;
  timeout_ms?: number;
  [k: string]: unknown;
}
export interface TaskContract {
  completion_criteria?: string[];
  max_correction_rounds?: number;
  require_objective_validation?: boolean;
  required_file_contents?: RequiredFileContent[];
  required_paths?: string[];
  schema_version?: number;
  verification?: "best_effort" | "required" | "independent";
  /**
   * Explicitly describes what a turn must deliver.  The runtime uses this
   * contract for verification; it never infers the requirement from prompt
   * wording.
   */
  workspace_change?: "optional" | "required" | "forbidden";
  [k: string]: unknown;
}
export interface RequiredFileContent {
  content: string;
  path: string;
  [k: string]: unknown;
}
export interface AgentTurnResult {
  final_message?: string | null;
  last_sequence_no?: number | null;
  outcome?: TaskOutcome | null;
  session_id: string;
  status: TaskStatus;
  task_id?: string | null;
  thread_id: string;
  turn_id?: string | null;
  verification?: VerificationRecord | null;
  [k: string]: unknown;
}
export interface TaskOutcome {
  confidence: number;
  evidence_refs?: string[];
  execution: ExecutionOutcome;
  external_verification?: "not_requested" | "pending" | "pass" | "partial" | "fail" | "unknown";
  failure_class?: FailureClass | null;
  next_action?: string | null;
  scorable: boolean;
  verification: VerificationResult;
  [k: string]: unknown;
}
export interface AgentTurnStart {
  accepted: boolean;
  command_id: string;
  reason?: string | null;
  session_id: string;
  task_id?: string | null;
  thread_id: string;
  turn_id?: string | null;
  [k: string]: unknown;
}
export interface AgentTurnStartResponse {
  accepted: boolean;
  attachment_id: string;
  command_id: string;
  cursor?: number | null;
  reason?: string | null;
  thread: AgentThreadRef;
  [k: string]: unknown;
}
export interface AppliedCandidate {
  applied_at: string;
  applied_version: string;
  candidate_id: string;
  rollback_reason?: string | null;
  rollback_ref: string;
  rolled_back_at?: string | null;
  [k: string]: unknown;
}
export interface ArtifactChunk {
  artifact_id: string;
  checksum: string;
  content_base64: string;
  eof: boolean;
  length: number;
  offset: number;
  redaction_status?: "raw" | "redacted" | "not_required";
  total_size: number;
  [k: string]: unknown;
}
export interface ArtifactReadRequest {
  artifact_id: string;
  length: number;
  offset: number;
  [k: string]: unknown;
}
export interface AutomationCandidate {
  evidence_refs: string[];
  id: string;
  kind: AutomationCandidateKind;
  regression_plan: string;
  risk_level: CandidateRisk;
  rollback_ref: string;
  source_task_id: string;
  status: CandidateStatus;
  summary: string;
  [k: string]: unknown;
}
export interface BenchmarkPromotion {
  accepted_by?: string | null;
  anti_overfit_notes: string[];
  evaluator: string;
  failure_taxonomy: string[];
  fixture: string;
  id: string;
  promotion_status: CandidateStatus;
  rollback_ref: string;
  source_task_id: string;
  [k: string]: unknown;
}
export interface BenchmarkRun {
  artifact_delivery_status: string;
  attempt_count: number;
  benchmark_id: string;
  changed_layer?: string | null;
  cost_source: string;
  cost_usd?: number | null;
  counterfactual_group_id?: string | null;
  dataset_version: string;
  failure_taxonomy: string[];
  harness_version: string;
  input_tokens?: number | null;
  judge_checks?: BenchmarkCheck[];
  leakage_checks?: BenchmarkCheck[];
  model_id: string;
  output_tokens?: number | null;
  provider_id: string;
  reasoning_tokens?: number | null;
  runtime_ms: number;
  scaffold_checks?: BenchmarkCheck[];
  scaffold_id: string;
  scaffold_version?: string;
  score?: number | null;
  security_score?: number | null;
  suite_kind?: "release" | "shadow" | "regression" | "adversarial" | "counterfactual";
  tool_budget: number;
  total_tokens?: number | null;
  utility_score?: number | null;
  [k: string]: unknown;
}
export interface BenchmarkCheck {
  check_id: string;
  evidence_refs: string[];
  reason: string;
  status: BenchmarkCheckStatus;
  [k: string]: unknown;
}
export interface CausalComparison {
  baseline_evaluation_ref?: string | null;
  candidate_evaluation_ref?: string | null;
  comparison_id: string;
  conclusion: string;
  cost_delta_usd?: number | null;
  latency_delta_ms?: number | null;
  partition?: EvaluationPartitionKind | null;
  provider_variant?: string | null;
  quality_delta?: number | null;
  replay_id: string;
  scaffold_inflation: boolean;
  security_delta?: number | null;
  seed?: number | null;
  token_delta?: number | null;
  utility_delta?: number | null;
  [k: string]: unknown;
}
export interface SessionCommand {
  actor: Actor;
  command_id: string;
  idempotency_key: string;
  kind: SessionCommandKind;
  payload: unknown;
  session_id?: string | null;
  timestamp: string;
  [k: string]: unknown;
}
export interface Actor {
  id: string;
  kind: ActorKind;
  [k: string]: unknown;
}
export interface CommandAck {
  accepted: boolean;
  command_id: string;
  reason?: string | null;
  [k: string]: unknown;
}
/**
 * 一个任务实际发送给 provider 的模型输入审计投影。
 *
 * 这是对 `ModelInputEnvelope` 的脱敏、可查询读模型，不是 provider request 本身，也不会
 * 因为被持久化或被开发者读取而自动进入下一轮模型上下文。provider 原始请求仍受 artifact
 * 权限控制。
 */
export interface ContextProjection {
  complete: boolean;
  integrity_warnings: string[];
  latest?: ContextSnapshot | null;
  session_id: string;
  snapshots: ContextSnapshot[];
  task_id: string;
  [k: string]: unknown;
}
export interface ContextSnapshot {
  budget_snapshot: TokenBudgetSnapshot;
  canonical_request_digest: string;
  contributor_manifest: ContextContributorSnapshot[];
  created_at: string;
  estimate_source: string;
  generation_config_digest?: string | null;
  message_manifest: ContextMessageSnapshot[];
  model_id: string;
  provider_id: string;
  provider_request_id: string;
  redacted_request_artifact_ref?: string | null;
  restricted_request_artifact_ref?: string | null;
  session_id: string;
  snapshot_id: string;
  task_id: string;
  tool_schema_digests: string[];
  turn_id: string;
  [k: string]: unknown;
}
export interface TokenBudgetSnapshot {
  action_if_exceeded: BudgetOverflowAction;
  budget_limit: number;
  budget_policy: string;
  context_window: number;
  max_output: number;
  planned_input_tokens: number;
  planned_summary_tokens: number;
  planned_tool_tokens: number;
  reserved_output_tokens: number;
  snapshot_id: string;
  task_id: string;
  turn_id: string;
  [k: string]: unknown;
}
export interface ContextContributorSnapshot {
  content_digest: string;
  estimated_tokens: number;
  included: boolean;
  invalidation_refs: string[];
  message_indexes?: number[];
  name: string;
  original_estimated_tokens?: number;
  redacted_content_ref?: string | null;
  retained_estimated_tokens?: number;
  role: string;
  source_refs: string[];
  strategy?: string;
  trimmed: boolean;
  [k: string]: unknown;
}
export interface ContextMessageSnapshot {
  content_digest: string;
  contributor?: string;
  estimated_tokens: number;
  index: number;
  origin?: string;
  role: string;
  source_refs?: string[];
  tool_call_ids: string[];
  [k: string]: unknown;
}
export interface CostRecord {
  confidence: string;
  estimated_cost_usd?: number | null;
  input_tokens?: number | null;
  model_id: string;
  output_tokens?: number | null;
  provider_id: string;
  reasoning_tokens?: number | null;
  source: string;
  total_tokens?: number | null;
  [k: string]: unknown;
}
export interface CounterfactualReplay {
  baseline_benchmark_id: string;
  changed_layer: string;
  controlled_variables: string[];
  group_id: string;
  limitations: string[];
  replay_id: string;
  variant_benchmark_id: string;
  [k: string]: unknown;
}
export interface DebugProjection {
  artifacts: ArtifactRecord[];
  busy_policy_decisions: BusyPolicyDecision[];
  causal_comparisons?: CausalComparison[];
  diagnostic_slice?: DiagnosticSlice | null;
  event_window: DebugEventWindow;
  events: RuntimeEvent[];
  evidence: EvidenceRecord[];
  external_evaluations?: ExternalEvaluationRecord[];
  failure_diagnosis?: FailureDiagnosis | null;
  failure_episodes?: FailureEpisode[];
  loop_decisions: LoopDecision[];
  missing_sections?: string[];
  post_task_jobs?: PostTaskJob[];
  replay_execution?: ReplayExecution | null;
  retention_losses?: string[];
  session_id: string;
  task_id?: string | null;
  tool_results: ToolResultEnvelope[];
  trace_complete?: boolean;
  verification?: VerificationRecord | null;
  [k: string]: unknown;
}
export interface ArtifactRecord {
  artifact_id: string;
  artifact_type: string;
  checksum: string;
  created_at: string;
  producer: string;
  provenance_refs: string[];
  redaction_status: RedactionStatus;
  retention_policy: string;
  session_id: string;
  size_bytes: number;
  tool_call_id?: string | null;
  turn_id?: string | null;
  uri: string;
  [k: string]: unknown;
}
export interface BusyPolicyDecision {
  affected_turn_id?: string | null;
  applied_policy: BusyPolicy;
  command_id: string;
  decision_id: string;
  lane_id: string;
  reason: string;
  requested_policy: BusyPolicy;
  safe_to_inject: boolean;
  [k: string]: unknown;
}
export interface DiagnosticSlice {
  artifact_refs: string[];
  causal_event_refs?: string[];
  complete: boolean;
  continuation_pages?: DiagnosticSliceContinuation[];
  continuation_pages_truncated?: boolean;
  diagnosis: FailureDiagnosis;
  event_refs: string[];
  evidence_refs: string[];
  generated_at: string;
  omitted_event_count: number;
  selection_strategy?: string;
  slice_id: string;
  source_task_id: string;
  supporting_event_refs?: string[];
  [k: string]: unknown;
}
export interface DiagnosticSliceContinuation {
  /**
   * Cursor for `TaskTraceRequest.cursor`; `None` starts at the first event.
   */
  after_sequence_no?: number | null;
  omitted_event_count: number;
  through_sequence_no: number;
  [k: string]: unknown;
}
export interface FailureDiagnosis {
  actual_behavior: string;
  analyzer_version: string;
  causal_event_refs: string[];
  code_targets: CodeTargetRef[];
  confidence: number;
  counterfactual: string;
  created_at: string;
  diagnosis_id: string;
  expected_behavior: string;
  failure_episode_id?: string | null;
  layer?: "causal" | "outcome";
  regression_commands: string[];
  revision?: number;
  source_task_id: string;
  summary: string;
  supersedes_diagnosis_id?: string | null;
  taxonomy: FailureTaxonomy;
  trigger_event_refs: string[];
  [k: string]: unknown;
}
export interface CodeTargetRef {
  crate_name: string;
  module_path: string;
  owner: string;
  source_digest?: string | null;
  source_path?: string | null;
  symbol?: string | null;
  [k: string]: unknown;
}
export interface FailureTaxonomy {
  code: string;
  domain: FailureDomain;
  [k: string]: unknown;
}
export interface DebugEventWindow {
  end_cursor?: number | null;
  has_more_before: boolean;
  limit: number;
  start_cursor?: number | null;
  [k: string]: unknown;
}
export interface EvidenceRecord {
  artifact_refs: string[];
  claim: string;
  confidence: number;
  evidence_id: string;
  evidence_strength: EvidenceStrength;
  limitations: string;
  source_event_refs: string[];
  verifier: string;
  [k: string]: unknown;
}
export interface ExternalEvaluationRecord {
  artifact_refs: string[];
  assertions: ExternalEvaluationAssertion[];
  attestation?: EvaluationAttestation | null;
  base_trace_digest: string;
  campaign_id?: string | null;
  candidate_id?: string | null;
  case_id: string;
  comparison_group_id?: string | null;
  dataset_id: string;
  dataset_version: string;
  evaluation_id: string;
  evaluator_id: string;
  evaluator_version: string;
  harness_id: string;
  harness_version: string;
  holdout_protected?: boolean;
  imported_artifacts?: ImportedEvaluationArtifact[];
  imported_evidence_refs?: string[];
  ingested_at: string;
  partition?: "source" | "historical" | "generated" | "holdout" | "adversarial";
  phases?: ExternalEvaluationPhase[];
  provider_variant?: string | null;
  result_digest: string;
  role?: RegressionExecutionRole | null;
  runtime_identity: string;
  score?: number | null;
  score_max?: number | null;
  seed?: number | null;
  source_task_id: string;
  terminal_cause?: ExternalEvaluationTerminalCause | null;
  trust: ExternalEvaluationTrust;
  verdict: EvaluationVerdict;
  [k: string]: unknown;
}
export interface ExternalEvaluationAssertion {
  assertion_id: string;
  evidence_refs: string[];
  message: string;
  name: string;
  passed: boolean;
  [k: string]: unknown;
}
export interface EvaluationAttestation {
  algorithm: string;
  key_id: string;
  signature: string;
  signed_digest: string;
  [k: string]: unknown;
}
/**
 * Host-derived immutable copy of evaluator evidence. These fields are not
 * part of `result_digest`; the digest authenticates evaluator-controlled
 * facts while the imported artifact checksum authenticates local retention.
 */
export interface ImportedEvaluationArtifact {
  artifact_ref: string;
  checksum: string;
  size_bytes: number;
  source_ref: string;
  [k: string]: unknown;
}
export interface ExternalEvaluationPhase {
  assertion_refs?: string[];
  completed_at?: string | null;
  duration_ms?: number | null;
  evidence_refs?: string[];
  kind: ExternalEvaluationPhaseKind;
  phase_id: string;
  started_at?: string | null;
  status: ExternalEvaluationPhaseStatus;
  [k: string]: unknown;
}
export interface ExternalEvaluationTerminalCause {
  code: string;
  evidence_refs?: string[];
  message: string;
  phase_id?: string | null;
  retryable?: boolean;
  [k: string]: unknown;
}
export interface FailureEpisode {
  diagnosis_refs?: string[];
  episode_id: string;
  external_assertion_failures?: FailureSignalRef[];
  opened_at: string;
  primary_signal: FailureSignalRef;
  producer_failures?: FailureSignalRef[];
  recovered_by?: FailureRecovery | null;
  self_check_failures?: FailureSignalRef[];
  source_task_id: string;
  status: FailureEpisodeStatus;
  superseded_by?: string | null;
  updated_at: string;
  [k: string]: unknown;
}
export interface FailureSignalRef {
  artifact_refs?: string[];
  event_ref: string;
  evidence_refs?: string[];
  kind: FailureSignalKind;
  signal_key: string;
  summary: string;
  [k: string]: unknown;
}
export interface FailureRecovery {
  event_ref: string;
  signal_key: string;
  summary: string;
  [k: string]: unknown;
}
export interface LoopDecision {
  action: LoopAction;
  budget_state: BudgetState;
  decision_id: string;
  evidence_refs: string[];
  model_state: string;
  next_step?: string | null;
  policy_ref?: string | null;
  reason: string;
  task_id: string;
  tool_state: string;
  turn_id: string;
  verification_ref?: string | null;
  [k: string]: unknown;
}
export interface BudgetState {
  actual_input_tokens?: number | null;
  budget_remaining?: number | null;
  compact_recommended: boolean;
  cost_risk: string;
  estimated_cost?: string | null;
  output_tokens?: number | null;
  planned_input_tokens?: number | null;
  total_tokens?: number | null;
  [k: string]: unknown;
}
export interface PostTaskJob {
  attempt: number;
  completed_at?: string | null;
  created_at: string;
  input_refs: string[];
  job_id: string;
  kind: PostTaskJobKind;
  last_error?: string | null;
  lease_expires_at?: string | null;
  lease_owner?: string | null;
  max_attempts: number;
  result_refs: string[];
  session_id: string;
  started_at?: string | null;
  status: PostTaskJobStatus;
  task_id: string;
  workspace_id: string;
  [k: string]: unknown;
}
/**
 * Result of re-entering the ordinary AgentLoop with recorded provider and
 * tool artifacts. This is an executable replay result, not a projection-only
 * reconstruction.
 */
export interface ReplayExecution {
  capsule_id: string;
  completed_at: string;
  execution_id: string;
  expected_loop_action?: LoopAction | null;
  expected_verification?: VerificationResult | null;
  mismatches: string[];
  mode: ReplayMode;
  observed_loop_action?: LoopAction | null;
  observed_verification?: VerificationResult | null;
  provider_exchanges_consumed: number;
  provider_exchanges_total: number;
  source_task_id: string;
  started_at: string;
  status: ReplayExecutionStatus;
  tool_results_consumed: number;
  tool_results_total: number;
  [k: string]: unknown;
}
export interface ToolResultEnvelope {
  evidence_refs: string[];
  model_visible_excerpt?: string | null;
  raw_artifact_ref?: string | null;
  risk: string;
  status: ToolResultStatus;
  structured_facts: unknown;
  summary: string;
  tool_call_id: string;
  tool_name: string;
  verification_hint?: string | null;
  [k: string]: unknown;
}
export interface EnvironmentRecipe {
  dependency_snapshot: string;
  fixture_refs: string[];
  generated_task_id: string;
  permission_profile: string;
  provider_profile: string;
  recipe_id: string;
  replay_seed: string;
  repo_ref: string;
  [k: string]: unknown;
}
export interface EvaluationCase {
  case_id: string;
  expected_outcome: string;
  fixture_refs: string[];
  objective: string;
  policy_constraints: string[];
  required_evidence: string[];
  source: string;
  source_task_id?: string | null;
  success_criteria: string[];
  tags: string[];
  task_type: string;
  [k: string]: unknown;
}
/**
 * 一个任务完成后治理生命周期的类型化读模型。
 *
 * 开发工具无需解析事件文案即可区分 review、candidate、regression 和 promotion。
 */
export interface EvaluationProjection {
  automation_candidates: AutomationCandidate[];
  causal_comparisons?: CausalComparison[];
  diagnostic_slices?: DiagnosticSlice[];
  external_evaluations?: ExternalEvaluationRecord[];
  failure_diagnoses?: FailureDiagnosis[];
  failure_episodes?: FailureEpisode[];
  frozen_candidate_patches?: FrozenCandidatePatch[];
  improvement_candidates: ImprovementCandidate[];
  integrity_warnings: string[];
  post_task_jobs: PostTaskJob[];
  promotion_decisions: PromotionDecision[];
  regressions: RegressionResult[];
  replay_capsules?: ReplayCapsule[];
  replay_executions?: ReplayExecution[];
  results: EvaluationResult[];
  reviews: PostTaskReview[];
  session_id: string;
  task_id: string;
  terminal: boolean;
  [k: string]: unknown;
}
export interface FrozenCandidatePatch {
  artifact_ref: string;
  candidate_id: string;
  digest: string;
  file_count: number;
  format: string;
  frozen_at: string;
  source_task_id: string;
  total_bytes: number;
  [k: string]: unknown;
}
export interface ImprovementCandidate {
  benchmark_refs: string[];
  causal_evidence_refs: string[];
  diagnosis_ref?: string | null;
  evidence_refs: string[];
  expected_effect: string;
  id: string;
  proposed_change: string;
  proposed_commands?: string[];
  risk_level: CandidateRisk;
  rollback_plan: string;
  source_failure_ids: string[];
  source_task_id: string;
  status: CandidateStatus;
  target_id?: string | null;
  target_type: string;
  validation_plan?: string[];
  [k: string]: unknown;
}
export interface PromotionDecision {
  applied_version?: string | null;
  candidate_id: string;
  created_at: string;
  decision: PromotionDecisionKind;
  decision_id: string;
  expires_at?: string | null;
  reason: string;
  reviewer: PromotionReviewer;
  rollback_ref?: string | null;
  [k: string]: unknown;
}
export interface RegressionResult {
  baseline_benchmark_refs?: string[];
  baseline_version: string;
  candidate_benchmark_refs?: string[];
  candidate_id: string;
  candidate_version: string;
  case_results?: RegressionCaseResult[];
  cases_run: number;
  causal_comparison_refs: string[];
  cost_delta?: number | null;
  coverage?: RegressionCoverage;
  created_at: string;
  external_evaluation_refs?: string[];
  failed_cases: number;
  latency_delta?: number | null;
  paired_execution_refs?: string[];
  passed_cases: number;
  quality_delta?: number | null;
  regression_id: string;
  regressions: string[];
  security_delta?: number | null;
  suite_kind?: "release" | "shadow" | "regression" | "adversarial" | "counterfactual";
  verdict: RegressionVerdict;
  [k: string]: unknown;
}
export interface RegressionCaseResult {
  case_id: string;
  evidence_checks: BenchmarkCheck[];
  expected_verdict: EvaluationVerdict;
  failure_taxonomy: string[];
  observed_verdict: EvaluationVerdict;
  passed: boolean;
  replay_id: string;
  [k: string]: unknown;
}
export interface RegressionCoverage {
  completed_cells: number;
  expected_cells: number;
  holdout_disclosure_violations: string[];
  missing_cells: string[];
  missing_partitions: EvaluationPartitionKind[];
  missing_providers: string[];
  missing_seeds: number[];
  observed_partitions: EvaluationPartitionKind[];
  observed_providers: string[];
  observed_seeds: number[];
  required_partitions: EvaluationPartitionKind[];
  required_providers: string[];
  required_seeds: number[];
  trusted_external_evaluation_refs: string[];
  trusted_external_pairs?: number;
  untrusted_external_evaluation_refs: string[];
  [k: string]: unknown;
}
export interface ReplayCapsule {
  capsule_id: string;
  clock_seed: string;
  complete: boolean;
  created_at: string;
  event_chain_digest: string;
  fixture_ref?: string | null;
  limitations: string[];
  missing_inputs: string[];
  mode: ReplayMode;
  provider_exchanges: ReplayProviderExchange[];
  random_seed: number;
  runtime_config_digest: string;
  source_last_sequence_no?: number | null;
  source_run_id: string;
  source_task_id: string;
  tool_results: ReplayToolResult[];
  [k: string]: unknown;
}
export interface ReplayProviderExchange {
  request_artifact_ref: string;
  request_id: string;
  response_artifact_ref: string;
  response_id: string;
  [k: string]: unknown;
}
export interface ReplayToolResult {
  provider_tool_call_id?: string | null;
  result_artifact_ref: string;
  tool_call_id: string;
  [k: string]: unknown;
}
export interface EvaluationResult {
  case_id: string;
  cost?: number | null;
  evidence_refs: string[];
  failure_taxonomy: string[];
  latency_ms?: number | null;
  quality_score?: number | null;
  residual_risks: string[];
  result_id: string;
  run_id: string;
  security_utility?: SecurityUtilityResult | null;
  source_task_id: string;
  verdict: EvaluationVerdict;
  [k: string]: unknown;
}
export interface SecurityUtilityResult {
  evidence_refs: string[];
  policy_violations: number;
  security_score?: number | null;
  utility_score?: number | null;
  verdict: EvaluationVerdict;
  [k: string]: unknown;
}
export interface PostTaskReview {
  context_issues: string[];
  evidence_quality: string;
  failure_reasons: string[];
  mode: ReviewMode;
  outcome: string;
  policy_issues: string[];
  promotion_candidates: string[];
  provider_issues: string[];
  success_reasons: string[];
  suggested_improvements: string[];
  task_id: string;
  tool_issues: string[];
  trajectory_summary?: TrajectorySummary;
  [k: string]: unknown;
}
export interface TrajectorySummary {
  approval_requests: number;
  context_growth_tokens: number;
  context_pressure: boolean;
  failed_tool_calls: number;
  failure_clusters: TrajectoryFailureCluster[];
  final_context_tokens?: number | null;
  initial_context_tokens?: number | null;
  max_context_tokens?: number | null;
  provider_calls: number;
  tool_calls: number;
  tool_duration_ms: number;
  tool_output_bytes: number;
  workspace_changes_observed: boolean;
  [k: string]: unknown;
}
export interface TrajectoryFailureCluster {
  duration_ms: number;
  failures: number;
  family: string;
  output_bytes: number;
  [k: string]: unknown;
}
export interface EvaluationRun {
  case_ids: string[];
  completed_at: string;
  cost?: number | null;
  cost_records?: CostRecord[];
  cost_source?: string;
  dataset_id: string;
  input_tokens?: number | null;
  latency_ms?: number | null;
  output_tokens?: number | null;
  provider_config_ref: string;
  reasoning_tokens?: number | null;
  result_refs: string[];
  run_id: string;
  runtime_config_ref: string;
  started_at: string;
  system_version: string;
  total_tokens?: number | null;
  [k: string]: unknown;
}
export interface EventFilter {
  after_sequence_no?: number | null;
  session_id: string;
  task_id?: string | null;
  [k: string]: unknown;
}
export interface EventPage {
  direction: EventPageDirection;
  end_cursor?: number | null;
  events: RuntimeEvent[];
  has_more: boolean;
  start_cursor?: number | null;
  [k: string]: unknown;
}
export interface EventPageRequest {
  cursor?: number | null;
  direction: EventPageDirection;
  limit: number;
  session_id: string;
  task_id?: string | null;
  [k: string]: unknown;
}
export interface EvolutionState {
  curriculum: CurriculumItem[];
  executions: GeneratedTaskExecution[];
  frontier?: CapabilityFrontier | null;
  generated_tasks: GeneratedTask[];
  novelty: NoveltyRecord[];
  recipes: EnvironmentRecipe[];
  runs: OpenEndedRun[];
  skills: SkillLifecycleRecord[];
  [k: string]: unknown;
}
export interface CurriculumItem {
  frontier_ref?: string | null;
  rejected_reason?: string | null;
  selected: boolean;
  selected_reason?: string | null;
  task_id: string;
  [k: string]: unknown;
}
export interface GeneratedTaskExecution {
  completed_at?: string | null;
  execution_id: string;
  generated_task_id: string;
  run_id: string;
  runtime_session_id: string;
  runtime_task_id?: string | null;
  sandbox_workspace: string;
  started_at: string;
  status: string;
  verification_ref?: string | null;
  [k: string]: unknown;
}
export interface CapabilityFrontier {
  blocked: string[];
  failed: string[];
  mastered: string[];
  missing_tools: string[];
  near_miss: string[];
  unstable_skills: string[];
  [k: string]: unknown;
}
export interface GeneratedTask {
  difficulty_score?: number | null;
  environment_recipe: string;
  expected_learning_value: string;
  id: string;
  novelty_score?: number | null;
  objective: string;
  promotion_status: CandidateStatus;
  safety_constraints: string[];
  source: string;
  source_task_id: string;
  [k: string]: unknown;
}
export interface NoveltyRecord {
  duplicate_risk: string;
  explanation: string;
  novelty_score: number;
  similar_tasks: string[];
  task_id: string;
  [k: string]: unknown;
}
export interface OpenEndedRun {
  blocked_reason?: string | null;
  budget: OpenEndedBudget;
  completed_at?: string | null;
  created_at: string;
  generated_task_ids: string[];
  objective: string;
  promoted_benchmark_ids: string[];
  promoted_skill_ids: string[];
  run_id: string;
  selected_task_ids: string[];
  source_scope: string;
  status: OpenEndedRunStatus;
  [k: string]: unknown;
}
export interface OpenEndedBudget {
  max_generated_tasks: number;
  max_runtime_ms_per_task: number;
  max_selected_tasks: number;
  max_tool_calls_per_task: number;
  [k: string]: unknown;
}
export interface SkillLifecycleRecord {
  candidate_path: string;
  checksum: string;
  created_at: string;
  installed_at?: string | null;
  installed_path?: string | null;
  manifest: SkillManifest;
  review_reason?: string | null;
  reviewed_at?: string | null;
  reviewer?: string | null;
  rollback_reason?: string | null;
  rolled_back_at?: string | null;
  status: SkillLifecycleStatus;
  [k: string]: unknown;
}
export interface SkillManifest {
  description: string;
  evidence_refs: string[];
  failure_cases: string[];
  name: string;
  prerequisites: string[];
  regression_refs: string[];
  rollback_ref: string;
  scope: string;
  skill_id: string;
  source_task_id: string;
  source_trajectory: string;
  steps: string[];
  [k: string]: unknown;
}
export interface RuntimeGovernorDecision {
  action: GovernorAction;
  advisories?: GovernorAdvisory[];
  alignment: GoalAlignmentCheck;
  budget_risk: string;
  consecutive_failed_tool_calls: number;
  failed_tool_calls: number;
  iteration: number;
  phase: GovernorPhase;
  reason: string;
  security_risk: string;
  task_id: string;
  tool_calls: number;
  [k: string]: unknown;
}
export interface GovernorAdvisory {
  code: string;
  reason: string;
  [k: string]: unknown;
}
export interface GoalAlignmentCheck {
  aligned: boolean;
  alignment_score: number;
  drift_type?: string | null;
  evidence_refs: string[];
  reason: string;
  task_id: string;
  [k: string]: unknown;
}
export interface JsonRpcNotification {
  jsonrpc: string;
  method: string;
  params: unknown;
  [k: string]: unknown;
}
export interface JsonRpcRequest {
  id?: {
    [k: string]: unknown;
  };
  jsonrpc: string;
  method: string;
  params?: {
    [k: string]: unknown;
  };
  [k: string]: unknown;
}
export interface JsonRpcResponse {
  error?: JsonRpcErrorObject | null;
  id?: unknown;
  jsonrpc: string;
  result?: unknown;
  [k: string]: unknown;
}
export interface JsonRpcErrorObject {
  code: number;
  data?: unknown;
  message: string;
  [k: string]: unknown;
}
export interface MemoryRecord {
  access_count?: number;
  claim?: MemoryClaim | null;
  confidence: number;
  content: string;
  contradiction_ids: string[];
  created_at: string;
  evidence_ids: string[];
  expires_at?: string | null;
  helpful_count?: number;
  incorrect_count?: number;
  invalidation_refs?: string[];
  irrelevant_count?: number;
  last_accessed_at?: string | null;
  memory_id: string;
  promotion_reviewer?: string | null;
  rollback_reason?: string | null;
  scope: MemoryScope;
  source_task_id: string;
  status: MemoryStatus;
  supporting_task_ids?: string[];
  version: number;
  [k: string]: unknown;
}
export interface MemoryClaim {
  candidate_id: string;
  confidence: number;
  evidence_refs: string[];
  expires_at?: string | null;
  invalidation_refs: string[];
  object: string;
  predicate: string;
  scope: string;
  source_task_refs: string[];
  subject: string;
  valid_from: string;
  [k: string]: unknown;
}
export interface ProtocolHandshake {
  name: string;
  versions: ProtocolVersionRange;
  [k: string]: unknown;
}
export interface ProtocolVersionRange {
  current: number;
  minimum: number;
  [k: string]: unknown;
}
export interface RuntimeQuery {
  cursor?: number | null;
  kind: RuntimeQueryKind;
  query_id: string;
  requester: ActorKind;
  session_id: string;
  task_id?: string | null;
  timestamp: string;
  [k: string]: unknown;
}
export interface RegressionCampaign {
  baseline_version: string;
  campaign_id: string;
  candidate_artifact_ref?: string | null;
  candidate_digest: string;
  candidate_id: string;
  case_partitions?: {
    [k: string]: EvaluationPartitionKind;
  };
  case_refs: string[];
  completed_at?: string | null;
  created_at: string;
  environment_recipe: string;
  hard_gates: string[];
  minimum_trusted_external_pairs?: number;
  provider_matrix: string[];
  replay_modes: string[];
  required_partitions?: EvaluationPartitionKind[];
  resource_budget: string;
  seeds: number[];
  started_at?: string | null;
  [k: string]: unknown;
}
export interface RegressionExecution {
  campaign_id: string;
  case_ref?: string;
  cost_latency_ref?: string | null;
  execution_id: string;
  partition?: "source" | "historical" | "generated" | "holdout" | "adversarial";
  provider_variant?: string;
  role: RegressionExecutionRole;
  runtime_version: string;
  seed?: number;
  status: RegressionExecutionStatus;
  task_trace_ref?: string | null;
  verification_ref?: string | null;
  workspace_snapshot_digest: string;
  [k: string]: unknown;
}
export interface SessionPage {
  has_more: boolean;
  next_cursor?: SessionCursor | null;
  sessions: SessionSummary[];
  [k: string]: unknown;
}
export interface SessionCursor {
  recency_at: string;
  thread_id: string;
  [k: string]: unknown;
}
export interface SessionSummary {
  created_at: string;
  forked_from_turn_id?: string | null;
  parent_thread_id?: string | null;
  preview: string;
  recency_at: string;
  session_id: string;
  thread_id: string;
  title: string;
  updated_at: string;
  [k: string]: unknown;
}
export interface SessionPageRequest {
  cursor?: SessionCursor | null;
  limit: number;
  [k: string]: unknown;
}
export interface SessionWindow {
  anchor_thread_id: string;
  range: SessionRangeSpec;
  reached_boundary: boolean;
  sessions: SessionSummary[];
  [k: string]: unknown;
}
export interface SessionRangeSpec {
  count: number;
  direction: SessionRangeDirection;
  [k: string]: unknown;
}
export interface SessionWindowRequest {
  anchor_thread_id: string;
  range: SessionRangeSpec;
  [k: string]: unknown;
}
export interface SkillCandidate {
  evidence_refs: string[];
  id: string;
  promotion_status: CandidateStatus;
  regression_refs: string[];
  reusable_pattern: string;
  rollback_ref: string;
  scope: string;
  source_task_id: string;
  source_trajectory: string;
  [k: string]: unknown;
}
export interface StateProjection {
  active_task_id?: string | null;
  final_message?: string | null;
  last_loop_decision?: LoopDecision | null;
  last_sequence_no: number;
  last_verification?: VerificationRecord | null;
  pending_approval?: string | null;
  runtime_lane?: RuntimeLane | null;
  session_id: string;
  task_status: TaskStatus;
  visible_steps: VisibleStep[];
  [k: string]: unknown;
}
export interface RuntimeLane {
  active_controller: Actor;
  active_turn_id?: string | null;
  busy_policy_default: BusyPolicy;
  injected_inputs: string[];
  lane_id: string;
  pending_turns: string[];
  session_id: string;
  status: TaskStatus;
  task_id: string;
  workspace_id: string;
  [k: string]: unknown;
}
export interface VisibleStep {
  label: string;
  status: string;
  summary: string;
  [k: string]: unknown;
}
export interface StorageMaintenanceReport {
  artifact_blobs_removed: number;
  checkpoint_directories_removed: number;
  completed_at: string;
  protected_artifacts_skipped: number;
  stats: StorageStats;
  temporary_artifacts_removed: number;
  [k: string]: unknown;
}
export interface StorageStats {
  artifact_records: number;
  checkpoint_directories: number;
  expired_artifact_blobs: number;
  live_artifact_blobs: number;
  live_artifact_bytes: number;
  rollout_files: number;
  [k: string]: unknown;
}
export interface TaskReconciliationRecord {
  decision: TaskReconciliationDecision;
  note?: string | null;
  reconciled_at: string;
  reconciled_by: Actor;
  recovery_event_ref: string;
  resulting_status: TaskStatus;
  resumed_pending_turns: boolean;
  task_id: string;
  [k: string]: unknown;
}
export interface TaskRecoveryRecord {
  checkpoint_event_refs: string[];
  detected_at: string;
  disposition: TaskRecoveryDisposition;
  incomplete_provider_request_ids?: string[];
  incomplete_tool_calls: IncompleteToolCall[];
  interrupted_turn_ids: string[];
  last_event_ref?: string | null;
  previous_runtime_identity?: string | null;
  reason: string;
  reconciliation_required: boolean;
  recovering_runtime_identity: string;
  running_process_ids: string[];
  safe_to_replay: boolean;
  task_id: string;
  [k: string]: unknown;
}
export interface IncompleteToolCall {
  recovery_policy?: ToolRecoveryPolicy;
  side_effect_possible: boolean;
  started_event_ref: string;
  tool_call_id: string;
  tool_name: string;
  [k: string]: unknown;
}
export interface ToolRecoveryPolicy {
  idempotency_key_policy: string;
  interrupted_action: InterruptedToolAction;
  retry_policy: string;
  side_effect_type: SideEffectType;
  [k: string]: unknown;
}
export interface TaskTracePage {
  artifacts: ArtifactRecord[];
  context_snapshots: ContextSnapshot[];
  evaluation: EvaluationProjection;
  events: RuntimeEvent[];
  evidence: EvidenceRecord[];
  has_more: boolean;
  integrity: TraceIntegrity;
  next_cursor?: number | null;
  post_task_jobs: PostTaskJob[];
  run_provenance?: RunProvenance | null;
  runtime_identity: string;
  session_id: string;
  task_id: string;
  verification?: VerificationRecord | null;
  verification_plan?: VerificationPlan | null;
  view: TraceView;
  [k: string]: unknown;
}
export interface TraceIntegrity {
  artifact_checksum_failures?: string[];
  broken_lifecycle_pairs?: string[];
  complete: boolean;
  event_chain_digest: string;
  event_count: number;
  external_overlay_failures?: string[];
  first_sequence?: number | null;
  last_sequence?: number | null;
  missing_causal_links?: string[];
  missing_sections: string[];
  orphan_events?: string[];
  provenance_mismatches?: string[];
  redacted_fields: string[];
  retention_losses: string[];
  unresolved_refs: string[];
  [k: string]: unknown;
}
export interface RunProvenance {
  build: BuildProvenance;
  captured_at: string;
  policy_digest?: string | null;
  provider_config_digest?: string | null;
  run_id: string;
  runtime_config_digest?: string | null;
  runtime_identity: string;
  schema_version: number;
  tool_manifest_digest?: string | null;
  verifier_digest?: string | null;
  workspace_initial_digest?: string | null;
  [k: string]: unknown;
}
export interface BuildProvenance {
  binary_checksum?: string | null;
  cargo_lock_digest?: string | null;
  dirty: boolean;
  features: string[];
  git_commit?: string | null;
  package_version: string;
  profile: string;
  rustc_version: string;
  schema_version: number;
  source_digest?: string | null;
  target: string;
  [k: string]: unknown;
}
export interface VerificationPlan {
  assertions: VerificationAssertion[];
  created_at: string;
  criteria: string[];
  dimensions?: VerificationDimensions;
  generated_by: string;
  plan_id: string;
  policy_assertions: VerificationAssertion[];
  required_artifact_types: string[];
  revision: number;
  task_class: TaskClass;
  task_id: string;
  verifier_versions: string[];
  [k: string]: unknown;
}
export interface VerificationDimensions {
  evidence_status: VerificationDimensionStatus;
  objective_status: VerificationDimensionStatus;
  policy_status: VerificationDimensionStatus;
  [k: string]: unknown;
}
export interface TaskTraceRequest {
  cursor?: number | null;
  limit: number;
  session_id: string;
  task_id: string;
  view: TraceView;
  wait_for_evaluation: boolean;
  [k: string]: unknown;
}
/**
 * Schema root for generated clients. The values are never instantiated at
 * runtime; grouping them keeps the versioned request and response contract in
 * one generated SDK namespace.
 */
export interface TuiDriverProtocolBundle {
  request: DriverEnvelope;
  response: DriverResponseEnvelope;
  snapshot: TuiFrame;
  [k: string]: unknown;
}
export interface DriverMouseEvent {
  column: number;
  kind: DriverMouseKind;
  row: number;
  [k: string]: unknown;
}
export interface RowRange {
  end: number;
  start: number;
  [k: string]: unknown;
}
export interface TuiFrameCell {
  background: string;
  column: number;
  foreground: string;
  modifiers: string;
  pane: TuiFramePane;
  row: number;
  symbol: string;
  [k: string]: unknown;
}
export interface TuiHitRegion {
  height: number;
  id: string;
  pane: TuiHitPane;
  width: number;
  x: number;
  y: number;
  [k: string]: unknown;
}
export interface TuiFrameLine {
  display_width: number;
  pane: TuiFramePane;
  row: number;
  text: string;
  [k: string]: unknown;
}
/**
 * Operational counters for diagnosing a long-lived Driver instance.
 *
 * The values are process-local and cumulative until the Driver exits. The
 * `pending_waits` and `frame_cache_entries` fields are live gauges.
 */
export interface DriverMetrics {
  connections: number;
  frame_cache_entries: number;
  frozen_frame_hits: number;
  frozen_frame_misses: number;
  instance_id: string;
  pending_waits: number;
  reconnects: number;
  rejected_connections: number;
  request_errors: number;
  requests: number;
  snapshot_latency: DriverLatencyMetrics;
  snapshot_renders: number;
  snapshot_requests: number;
  sync_attempts: number;
  sync_errors: number;
  sync_latency: DriverLatencyMetrics;
  wait_cancelled: number;
  wait_latency: DriverLatencyMetrics;
  wait_requests: number;
  wait_results: number;
  wait_timeouts: number;
  [k: string]: unknown;
}
/**
 * Redacted, low-cardinality timing aggregates exposed by the native Driver.
 *
 * This deliberately contains no request payloads, rendered text, workspace
 * paths, provider identifiers, or credential material.
 */
export interface DriverLatencyMetrics {
  last_ms: number;
  max_ms: number;
  samples: number;
  total_ms: number;
  [k: string]: unknown;
}
export interface DriverState {
  closed: boolean;
  controller_mode: DriverControllerMode;
  facts_expanded: boolean;
  height: number;
  instance_id: string;
  session_id: string;
  status: DriverTaskStatus;
  task_id?: string | null;
  thread_id: string;
  turn_id?: string | null;
  width: number;
  [k: string]: unknown;
}
export interface DriverNotification {
  kind: DriverNotificationKind;
  sequence_no?: number | null;
  status?: DriverTaskStatus | null;
  [k: string]: unknown;
}
export interface TuiFrame {
  cells?: TuiFrameCell[] | null;
  complete: boolean;
  event_high_watermark?: number | null;
  frame_id: string;
  height: number;
  hit_regions?: TuiHitRegion[];
  instance_id: string;
  lines: TuiFrameLine[];
  missing_sections: string[];
  next_range?: RowRange | null;
  panes: SnapshotPanes;
  redaction_status: RedactionStatus;
  returned_range: RowRange;
  scope: SnapshotScope;
  session_id: string;
  task_id?: string | null;
  total_rows: number;
  turn_id?: string | null;
  width: number;
  workspace_id: string;
  [k: string]: unknown;
}
export interface UserProjection {
  final_message?: string | null;
  pending_approval?: string | null;
  residual_risks: string[];
  session_id: string;
  status: TaskStatus;
  task_id?: string | null;
  visible_steps: VisibleStep[];
  [k: string]: unknown;
}
export interface UserQuestionRequest {
  question_id: string;
  questions: UserQuestionPrompt[];
  task_id: string;
  tool_call_id: string;
  turn_id: string;
  [k: string]: unknown;
}
export interface UserQuestionPrompt {
  header: string;
  id: string;
  mode?: "single" | "multiple";
  options: UserQuestionOption[];
  question: string;
  [k: string]: unknown;
}
export interface UserQuestionOption {
  description?: string | null;
  id: string;
  label: string;
  [k: string]: unknown;
}
export interface UserQuestionResolution {
  answers: UserQuestionAnswer[];
  question_id: string;
  reason: string;
  [k: string]: unknown;
}
export interface UserQuestionAnswer {
  free_text?: string | null;
  question_id: string;
  selected_option_ids: string[];
  [k: string]: unknown;
}
