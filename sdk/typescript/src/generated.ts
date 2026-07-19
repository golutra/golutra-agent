// Generated from Golutra Rust protocol schemas. Do not edit manually.

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
export type ActorKind = "user" | "api" | "tui" | "cli" | "sdk" | "web" | "ide" | "runtime";
export type SessionCommandKind =
  | "create"
  | "prompt"
  | "approve"
  | "deny"
  | "pause"
  | "resume"
  | "abort"
  | "takeover"
  | "compact"
  | "memory_rollback"
  | "memory_feedback"
  | "run_regression"
  | "review_candidate"
  | "apply_candidate"
  | "rollback_candidate"
  | "record_benchmark"
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
export type RuntimeEventType =
  | "command_received"
  | "command_completed"
  | "command_accepted"
  | "command_rejected"
  | "session_created"
  | "thread_forked"
  | "thread_rebound"
  | "task_created"
  | "turn_started"
  | "turn_queued"
  | "busy_policy_decided"
  | "controller_changed"
  | "context_built"
  | "provider_started"
  | "provider_streamed"
  | "provider_completed"
  | "token_usage_recorded"
  | "assistant_message"
  | "tool_started"
  | "tool_completed"
  | "policy_evaluated"
  | "verification_completed"
  | "loop_decided"
  | "checkpoint_created"
  | "task_completed"
  | "task_abort_requested"
  | "task_aborted"
  | "task_paused"
  | "task_resumed"
  | "approval_requested"
  | "approval_resolved"
  | "retry_scheduled"
  | "provider_fallback"
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
  | "compaction_completed"
  | "memory_retrieved"
  | "memory_promoted"
  | "memory_promotion_rejected"
  | "memory_rolled_back"
  | "memory_feedback_recorded"
  | "post_task_reviewed"
  | "evaluation_completed"
  | "improvement_candidate_created"
  | "automation_candidate_created"
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
  | "verification_planned"
  | "verification_assertion_completed"
  | "regression_campaign_started"
  | "regression_execution_completed"
  | "memory_candidate_quarantined"
  | "memory_activated"
  | "memory_invalidated";
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
export type EvidenceStrength = "weak" | "medium" | "strong";
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
export type ToolResultStatus = "ok" | "error" | "blocked" | "cancelled" | "timeout";
export type VerificationCheckKind =
  "tool_execution" | "workspace_change" | "objective_validation" | "assistant_response";
export type VerificationResult = "pass" | "fail" | "partial" | "unknown";
export type PromotionDecisionKind = "approve" | "reject" | "needs_human_review";
export type PromotionReviewer = "system" | "human" | "agent";
export type EvaluationVerdict = "pass" | "fail" | "partial" | "unknown";
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
export type RegressionExecutionRole = "baseline" | "candidate";
export type RegressionExecutionStatus =
  "queued" | "running" | "succeeded" | "failed" | "inconclusive";
export type SessionRangeDirection = "single" | "newer" | "older";
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
  | "cancelled";
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
  | "cancelled";
export type TuiFramePane = "transcript" | "developer" | "response_and_developer" | "screen";
export type TuiHitPane = "transcript" | "bottom" | "developer";
export type SnapshotPanes = "transcript" | "developer" | "response_and_developer" | "full_screen";
export type SnapshotScope = "current_turn" | "task" | "session" | "screen";
export type DriverNotificationKind =
  "heartbeat" | "runtime_event_available" | "state_changed" | "task_terminal";

export interface SdkProtocolBundle {
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
  task_trace_page: TaskTracePage;
  task_trace_request: TaskTraceRequest;
  tui_driver: TuiDriverProtocolBundle;
  user_projection: UserProjection;
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
  comparison_id: string;
  conclusion: string;
  cost_delta_usd?: number | null;
  latency_delta_ms?: number | null;
  quality_delta?: number | null;
  replay_id: string;
  scaffold_inflation: boolean;
  security_delta?: number | null;
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
 * 一个任务实际进入模型上下文的事实投影。
 *
 * 该视图只暴露脱敏 manifest 和 digest；provider 原始请求仍受 artifact 权限控制。
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
  estimated_tokens: number;
  index: number;
  role: string;
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
  event_window: DebugEventWindow;
  events: RuntimeEvent[];
  evidence: EvidenceRecord[];
  loop_decisions: LoopDecision[];
  missing_sections?: string[];
  post_task_jobs?: PostTaskJob[];
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
export interface DebugEventWindow {
  end_cursor?: number | null;
  has_more_before: boolean;
  limit: number;
  start_cursor?: number | null;
  [k: string]: unknown;
}
export interface RuntimeEvent {
  durable: boolean;
  event_type: RuntimeEventType;
  id: string;
  parent_event_id?: string | null;
  payload: unknown;
  payload_ref?: string | null;
  sequence_no: number;
  session_id: string;
  source: RuntimeEventSource;
  task_id?: string | null;
  timestamp: string;
  turn_id?: string | null;
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
export interface VerificationRecord {
  checks: VerificationCheck[];
  completion_criteria: string[];
  evidence_refs: string[];
  objective: string;
  policy_status: string;
  residual_risks: string[];
  result: VerificationResult;
  task_id: string;
  verification_id: string;
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
  improvement_candidates: ImprovementCandidate[];
  integrity_warnings: string[];
  post_task_jobs: PostTaskJob[];
  promotion_decisions: PromotionDecision[];
  regressions: RegressionResult[];
  results: EvaluationResult[];
  reviews: PostTaskReview[];
  session_id: string;
  task_id: string;
  terminal: boolean;
  [k: string]: unknown;
}
export interface ImprovementCandidate {
  benchmark_refs: string[];
  causal_evidence_refs: string[];
  evidence_refs: string[];
  expected_effect: string;
  id: string;
  proposed_change: string;
  risk_level: CandidateRisk;
  rollback_plan: string;
  source_failure_ids: string[];
  source_task_id: string;
  status: CandidateStatus;
  target_id?: string | null;
  target_type: string;
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
  created_at: string;
  failed_cases: number;
  latency_delta?: number | null;
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
  alignment: GoalAlignmentCheck;
  budget_risk: string;
  failed_tool_calls: number;
  iteration: number;
  phase: GovernorPhase;
  reason: string;
  security_risk: string;
  task_id: string;
  tool_calls: number;
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
  candidate_digest: string;
  candidate_id: string;
  case_refs: string[];
  completed_at?: string | null;
  created_at: string;
  environment_recipe: string;
  hard_gates: string[];
  provider_matrix: string[];
  replay_modes: string[];
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
  role: RegressionExecutionRole;
  runtime_version: string;
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
  runtime_identity: string;
  session_id: string;
  task_id: string;
  verification?: VerificationRecord | null;
  verification_plan?: VerificationPlan | null;
  view: TraceView;
  [k: string]: unknown;
}
export interface TraceIntegrity {
  complete: boolean;
  event_chain_digest: string;
  event_count: number;
  first_sequence?: number | null;
  last_sequence?: number | null;
  missing_sections: string[];
  redacted_fields: string[];
  retention_losses: string[];
  unresolved_refs: string[];
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
