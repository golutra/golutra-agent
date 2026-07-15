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
  | "verify"
  | "replay"
  | "export";
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
  | "storage_maintenance_completed";
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
export type ToolResultStatus = "ok" | "error" | "blocked" | "cancelled" | "timeout";
export type VerificationCheckKind =
  "tool_execution" | "workspace_change" | "objective_validation" | "assistant_response";
export type VerificationResult = "pass" | "fail" | "partial" | "unknown";
export type EvaluationVerdict = "pass" | "fail" | "partial" | "unknown";
export type EventPageDirection = "forward" | "backward";
export type OpenEndedRunStatus = "planned" | "running" | "completed" | "blocked";
export type SkillLifecycleStatus =
  "proposed" | "reviewed" | "rejected" | "installed" | "rolled_back";
export type GovernorAction = "allow" | "warn" | "ask_user" | "block";
export type GovernorPhase = "provider" | "tool" | "tool_result" | "completion";
export type MemoryScope = "project" | "user" | "global";
export type MemoryStatus = "active" | "rolled_back";
export type ReviewMode = "minimal" | "deep";
export type PromotionDecisionKind = "approve" | "reject" | "needs_human_review";
export type PromotionReviewer = "system" | "human" | "agent";
export type RuntimeQueryKind =
  | "session_state"
  | "task_state"
  | "user_projection"
  | "debug_projection"
  | "replay_cursor"
  | "memory_list"
  | "evaluation_results"
  | "improvement_candidates"
  | "automation_candidates"
  | "evolution_state"
  | "provider_state"
  | "storage_status";
export type RegressionVerdict = "pass" | "fail" | "needs_review";
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

export interface SdkProtocolBundle {
  applied_candidate: AppliedCandidate;
  automation_candidate: AutomationCandidate;
  benchmark_promotion: BenchmarkPromotion;
  benchmark_run: BenchmarkRun;
  causal_comparison: CausalComparison;
  command: SessionCommand;
  command_ack: CommandAck;
  cost_record: CostRecord;
  counterfactual_replay: CounterfactualReplay;
  debug_projection: DebugProjection;
  environment_recipe: EnvironmentRecipe;
  evaluation_case: EvaluationCase;
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
  regression_result: RegressionResult;
  security_utility_result: SecurityUtilityResult;
  skill_candidate: SkillCandidate;
  skill_lifecycle_record: SkillLifecycleRecord;
  skill_manifest: SkillManifest;
  state_projection: StateProjection;
  storage_maintenance_report: StorageMaintenanceReport;
  storage_stats: StorageStats;
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
  session_id: string;
  task_id?: string | null;
  tool_results: ToolResultEnvelope[];
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
export interface MemoryRecord {
  access_count?: number;
  confidence: number;
  content: string;
  contradiction_ids: string[];
  created_at: string;
  evidence_ids: string[];
  expires_at?: string | null;
  helpful_count?: number;
  incorrect_count?: number;
  irrelevant_count?: number;
  last_accessed_at?: string | null;
  memory_id: string;
  promotion_reviewer?: string | null;
  rollback_reason?: string | null;
  scope: MemoryScope;
  source_task_id: string;
  status: MemoryStatus;
  version: number;
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
