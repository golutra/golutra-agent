"""Generated from Golutra Rust protocol schemas. Do not edit manually."""

from __future__ import annotations

from typing import Any, Literal, Never, NotRequired, Required, TypeAlias, TypedDict

class Actor(TypedDict, total=False):
    id: Required[str]
    kind: Required[ActorKind]

ActorKind: TypeAlias = Literal['user', 'api', 'tui', 'cli', 'sdk', 'web', 'ide', 'runtime']

class AppliedCandidate(TypedDict, total=False):
    applied_at: Required[str]
    applied_version: Required[str]
    candidate_id: Required[str]
    rollback_reason: NotRequired[str | None]
    rollback_ref: Required[str]
    rolled_back_at: NotRequired[str | None]

class ArtifactChunk(TypedDict, total=False):
    artifact_id: Required[str]
    checksum: Required[str]
    content_base64: Required[str]
    eof: Required[bool]
    length: Required[int]
    offset: Required[int]
    total_size: Required[int]

class ArtifactReadRequest(TypedDict, total=False):
    artifact_id: Required[str]
    length: Required[int]
    offset: Required[int]

class ArtifactRecord(TypedDict, total=False):
    artifact_id: Required[str]
    artifact_type: Required[str]
    checksum: Required[str]
    created_at: Required[str]
    producer: Required[str]
    provenance_refs: Required[list[str]]
    redaction_status: Required[RedactionStatus]
    retention_policy: Required[str]
    session_id: Required[str]
    size_bytes: Required[int]
    tool_call_id: NotRequired[str | None]
    turn_id: NotRequired[str | None]
    uri: Required[str]

class AutomationCandidate(TypedDict, total=False):
    evidence_refs: Required[list[str]]
    id: Required[str]
    kind: Required[AutomationCandidateKind]
    regression_plan: Required[str]
    risk_level: Required[CandidateRisk]
    rollback_ref: Required[str]
    source_task_id: Required[str]
    status: Required[CandidateStatus]
    summary: Required[str]

AutomationCandidateKind: TypeAlias = Literal['benchmark', 'generated_task', 'skill', 'runtime_change']

class BenchmarkCheck(TypedDict, total=False):
    check_id: Required[str]
    evidence_refs: Required[list[str]]
    reason: Required[str]
    status: Required[BenchmarkCheckStatus]

BenchmarkCheckStatus: TypeAlias = Literal['pass', 'fail', 'unknown', 'not_applicable']

class BenchmarkPromotion(TypedDict, total=False):
    accepted_by: NotRequired[str | None]
    anti_overfit_notes: Required[list[str]]
    evaluator: Required[str]
    failure_taxonomy: Required[list[str]]
    fixture: Required[str]
    id: Required[str]
    promotion_status: Required[CandidateStatus]
    rollback_ref: Required[str]
    source_task_id: Required[str]

class BenchmarkRun(TypedDict, total=False):
    artifact_delivery_status: Required[str]
    attempt_count: Required[int]
    benchmark_id: Required[str]
    changed_layer: NotRequired[str | None]
    cost_source: Required[str]
    cost_usd: NotRequired[float | None]
    counterfactual_group_id: NotRequired[str | None]
    dataset_version: Required[str]
    failure_taxonomy: Required[list[str]]
    harness_version: Required[str]
    input_tokens: NotRequired[int | None]
    judge_checks: NotRequired[list[BenchmarkCheck]]
    leakage_checks: NotRequired[list[BenchmarkCheck]]
    model_id: Required[str]
    output_tokens: NotRequired[int | None]
    provider_id: Required[str]
    reasoning_tokens: NotRequired[int | None]
    runtime_ms: Required[int]
    scaffold_checks: NotRequired[list[BenchmarkCheck]]
    scaffold_id: Required[str]
    scaffold_version: NotRequired[str]
    score: NotRequired[float | None]
    security_score: NotRequired[float | None]
    suite_kind: NotRequired[BenchmarkSuiteKind]
    tool_budget: Required[int]
    total_tokens: NotRequired[int | None]
    utility_score: NotRequired[float | None]

BenchmarkSuiteKind: TypeAlias = Literal['release', 'shadow', 'regression', 'adversarial', 'counterfactual']

BudgetOverflowAction: TypeAlias = Literal['trim', 'compact', 'ask_user', 'block']

class BudgetState(TypedDict, total=False):
    actual_input_tokens: NotRequired[int | None]
    budget_remaining: NotRequired[int | None]
    compact_recommended: Required[bool]
    cost_risk: Required[str]
    estimated_cost: NotRequired[str | None]
    output_tokens: NotRequired[int | None]
    planned_input_tokens: NotRequired[int | None]
    total_tokens: NotRequired[int | None]

BusyPolicy: TypeAlias = Literal['append', 'inject', 'interrupt', 'reject']

class BusyPolicyDecision(TypedDict, total=False):
    affected_turn_id: NotRequired[str | None]
    applied_policy: Required[BusyPolicy]
    command_id: Required[str]
    decision_id: Required[str]
    lane_id: Required[str]
    reason: Required[str]
    requested_policy: Required[BusyPolicy]
    safe_to_inject: Required[bool]

CandidateRisk: TypeAlias = Literal['low', 'medium', 'high', 'critical']

CandidateStatus: TypeAlias = Literal['proposed', 'regression_passed', 'needs_human_review', 'approved', 'applied', 'rejected', 'rolled_back']

class CapabilityFrontier(TypedDict, total=False):
    blocked: Required[list[str]]
    failed: Required[list[str]]
    mastered: Required[list[str]]
    missing_tools: Required[list[str]]
    near_miss: Required[list[str]]
    unstable_skills: Required[list[str]]

class CausalComparison(TypedDict, total=False):
    comparison_id: Required[str]
    conclusion: Required[str]
    cost_delta_usd: NotRequired[float | None]
    latency_delta_ms: NotRequired[int | None]
    quality_delta: NotRequired[float | None]
    replay_id: Required[str]
    scaffold_inflation: Required[bool]
    security_delta: NotRequired[float | None]
    token_delta: NotRequired[int | None]
    utility_delta: NotRequired[float | None]

class CommandAck(TypedDict, total=False):
    accepted: Required[bool]
    command_id: Required[str]
    reason: NotRequired[str | None]

class ContextContributorSnapshot(TypedDict, total=False):
    content_digest: Required[str]
    estimated_tokens: Required[int]
    included: Required[bool]
    invalidation_refs: Required[list[str]]
    name: Required[str]
    redacted_content_ref: NotRequired[str | None]
    role: Required[str]
    source_refs: Required[list[str]]
    trimmed: Required[bool]

class ContextMessageSnapshot(TypedDict, total=False):
    content_digest: Required[str]
    estimated_tokens: Required[int]
    index: Required[int]
    role: Required[str]
    tool_call_ids: Required[list[str]]

class ContextProjection(TypedDict, total=False):
    complete: Required[bool]
    integrity_warnings: Required[list[str]]
    latest: NotRequired[ContextSnapshot | None]
    session_id: Required[str]
    snapshots: Required[list[ContextSnapshot]]
    task_id: Required[str]

class ContextSnapshot(TypedDict, total=False):
    budget_snapshot: Required[TokenBudgetSnapshot]
    canonical_request_digest: Required[str]
    contributor_manifest: Required[list[ContextContributorSnapshot]]
    created_at: Required[str]
    estimate_source: Required[str]
    generation_config_digest: NotRequired[str | None]
    message_manifest: Required[list[ContextMessageSnapshot]]
    model_id: Required[str]
    provider_id: Required[str]
    provider_request_id: Required[str]
    redacted_request_artifact_ref: NotRequired[str | None]
    restricted_request_artifact_ref: NotRequired[str | None]
    session_id: Required[str]
    snapshot_id: Required[str]
    task_id: Required[str]
    tool_schema_digests: Required[list[str]]
    turn_id: Required[str]

class CostRecord(TypedDict, total=False):
    confidence: Required[str]
    estimated_cost_usd: NotRequired[float | None]
    input_tokens: NotRequired[int | None]
    model_id: Required[str]
    output_tokens: NotRequired[int | None]
    provider_id: Required[str]
    reasoning_tokens: NotRequired[int | None]
    source: Required[str]
    total_tokens: NotRequired[int | None]

class CounterfactualReplay(TypedDict, total=False):
    baseline_benchmark_id: Required[str]
    changed_layer: Required[str]
    controlled_variables: Required[list[str]]
    group_id: Required[str]
    limitations: Required[list[str]]
    replay_id: Required[str]
    variant_benchmark_id: Required[str]

class CurriculumItem(TypedDict, total=False):
    frontier_ref: NotRequired[str | None]
    rejected_reason: NotRequired[str | None]
    selected: Required[bool]
    selected_reason: NotRequired[str | None]
    task_id: Required[str]

class DebugEventWindow(TypedDict, total=False):
    end_cursor: NotRequired[int | None]
    has_more_before: Required[bool]
    limit: Required[int]
    start_cursor: NotRequired[int | None]

class DebugProjection(TypedDict, total=False):
    artifacts: Required[list[ArtifactRecord]]
    busy_policy_decisions: Required[list[BusyPolicyDecision]]
    event_window: Required[DebugEventWindow]
    events: Required[list[RuntimeEvent]]
    evidence: Required[list[EvidenceRecord]]
    loop_decisions: Required[list[LoopDecision]]
    session_id: Required[str]
    task_id: NotRequired[str | None]
    tool_results: Required[list[ToolResultEnvelope]]
    verification: NotRequired[VerificationRecord | None]

class EnvironmentRecipe(TypedDict, total=False):
    dependency_snapshot: Required[str]
    fixture_refs: Required[list[str]]
    generated_task_id: Required[str]
    permission_profile: Required[str]
    provider_profile: Required[str]
    recipe_id: Required[str]
    replay_seed: Required[str]
    repo_ref: Required[str]

class EvaluationCase(TypedDict, total=False):
    case_id: Required[str]
    expected_outcome: Required[str]
    fixture_refs: Required[list[str]]
    objective: Required[str]
    policy_constraints: Required[list[str]]
    required_evidence: Required[list[str]]
    source: Required[str]
    source_task_id: NotRequired[str | None]
    success_criteria: Required[list[str]]
    tags: Required[list[str]]
    task_type: Required[str]

class EvaluationProjection(TypedDict, total=False):
    automation_candidates: Required[list[AutomationCandidate]]
    improvement_candidates: Required[list[ImprovementCandidate]]
    integrity_warnings: Required[list[str]]
    post_task_jobs: Required[list[PostTaskJob]]
    promotion_decisions: Required[list[PromotionDecision]]
    regressions: Required[list[RegressionResult]]
    results: Required[list[EvaluationResult]]
    reviews: Required[list[PostTaskReview]]
    session_id: Required[str]
    task_id: Required[str]
    terminal: Required[bool]

class EvaluationResult(TypedDict, total=False):
    case_id: Required[str]
    cost: NotRequired[float | None]
    evidence_refs: Required[list[str]]
    failure_taxonomy: Required[list[str]]
    latency_ms: NotRequired[int | None]
    quality_score: NotRequired[float | None]
    residual_risks: Required[list[str]]
    result_id: Required[str]
    run_id: Required[str]
    security_utility: NotRequired[SecurityUtilityResult | None]
    source_task_id: Required[str]
    verdict: Required[EvaluationVerdict]

class EvaluationRun(TypedDict, total=False):
    case_ids: Required[list[str]]
    completed_at: Required[str]
    cost: NotRequired[float | None]
    cost_records: NotRequired[list[CostRecord]]
    cost_source: NotRequired[str]
    dataset_id: Required[str]
    input_tokens: NotRequired[int | None]
    latency_ms: NotRequired[int | None]
    output_tokens: NotRequired[int | None]
    provider_config_ref: Required[str]
    reasoning_tokens: NotRequired[int | None]
    result_refs: Required[list[str]]
    run_id: Required[str]
    runtime_config_ref: Required[str]
    started_at: Required[str]
    system_version: Required[str]
    total_tokens: NotRequired[int | None]

EvaluationVerdict: TypeAlias = Literal['pass', 'fail', 'partial', 'unknown']

class EventFilter(TypedDict, total=False):
    after_sequence_no: NotRequired[int | None]
    session_id: Required[str]
    task_id: NotRequired[str | None]

class EventPage(TypedDict, total=False):
    direction: Required[EventPageDirection]
    end_cursor: NotRequired[int | None]
    events: Required[list[RuntimeEvent]]
    has_more: Required[bool]
    start_cursor: NotRequired[int | None]

EventPageDirection: TypeAlias = Literal['forward', 'backward']

class EventPageRequest(TypedDict, total=False):
    cursor: NotRequired[int | None]
    direction: Required[EventPageDirection]
    limit: Required[int]
    session_id: Required[str]
    task_id: NotRequired[str | None]

class EvidenceRecord(TypedDict, total=False):
    artifact_refs: Required[list[str]]
    claim: Required[str]
    confidence: Required[float]
    evidence_id: Required[str]
    evidence_strength: Required[EvidenceStrength]
    limitations: Required[str]
    source_event_refs: Required[list[str]]
    verifier: Required[str]

EvidenceStrength: TypeAlias = Literal['weak', 'medium', 'strong']

class EvolutionState(TypedDict, total=False):
    curriculum: Required[list[CurriculumItem]]
    executions: Required[list[GeneratedTaskExecution]]
    frontier: NotRequired[CapabilityFrontier | None]
    generated_tasks: Required[list[GeneratedTask]]
    novelty: Required[list[NoveltyRecord]]
    recipes: Required[list[EnvironmentRecipe]]
    runs: Required[list[OpenEndedRun]]
    skills: Required[list[SkillLifecycleRecord]]

class GeneratedTask(TypedDict, total=False):
    difficulty_score: NotRequired[float | None]
    environment_recipe: Required[str]
    expected_learning_value: Required[str]
    id: Required[str]
    novelty_score: NotRequired[float | None]
    objective: Required[str]
    promotion_status: Required[CandidateStatus]
    safety_constraints: Required[list[str]]
    source: Required[str]
    source_task_id: Required[str]

class GeneratedTaskExecution(TypedDict, total=False):
    completed_at: NotRequired[str | None]
    execution_id: Required[str]
    generated_task_id: Required[str]
    run_id: Required[str]
    runtime_session_id: Required[str]
    runtime_task_id: NotRequired[str | None]
    sandbox_workspace: Required[str]
    started_at: Required[str]
    status: Required[str]
    verification_ref: NotRequired[str | None]

class GoalAlignmentCheck(TypedDict, total=False):
    aligned: Required[bool]
    alignment_score: Required[int]
    drift_type: NotRequired[str | None]
    evidence_refs: Required[list[str]]
    reason: Required[str]
    task_id: Required[str]

GovernorAction: TypeAlias = Literal['allow', 'warn', 'ask_user', 'block']

GovernorPhase: TypeAlias = Literal['provider', 'tool', 'tool_result', 'completion']

class ImprovementCandidate(TypedDict, total=False):
    benchmark_refs: Required[list[str]]
    causal_evidence_refs: Required[list[str]]
    evidence_refs: Required[list[str]]
    expected_effect: Required[str]
    id: Required[str]
    proposed_change: Required[str]
    risk_level: Required[CandidateRisk]
    rollback_plan: Required[str]
    source_failure_ids: Required[list[str]]
    source_task_id: Required[str]
    status: Required[CandidateStatus]
    target_id: NotRequired[str | None]
    target_type: Required[str]

LoopAction: TypeAlias = Literal['continue', 'compact', 'retry', 'fallback', 'ask_user', 'verify', 'stop_success', 'stop_partial', 'stop_failed', 'blocked']

class LoopDecision(TypedDict, total=False):
    action: Required[LoopAction]
    budget_state: Required[BudgetState]
    decision_id: Required[str]
    evidence_refs: Required[list[str]]
    model_state: Required[str]
    next_step: NotRequired[str | None]
    policy_ref: NotRequired[str | None]
    reason: Required[str]
    task_id: Required[str]
    tool_state: Required[str]
    turn_id: Required[str]
    verification_ref: NotRequired[str | None]

class MemoryClaim(TypedDict, total=False):
    candidate_id: Required[str]
    confidence: Required[int]
    evidence_refs: Required[list[str]]
    expires_at: NotRequired[str | None]
    invalidation_refs: Required[list[str]]
    object: Required[str]
    predicate: Required[str]
    scope: Required[str]
    source_task_refs: Required[list[str]]
    subject: Required[str]
    valid_from: Required[str]

class MemoryRecord(TypedDict, total=False):
    access_count: NotRequired[int]
    claim: NotRequired[MemoryClaim | None]
    confidence: Required[int]
    content: Required[str]
    contradiction_ids: Required[list[str]]
    created_at: Required[str]
    evidence_ids: Required[list[str]]
    expires_at: NotRequired[str | None]
    helpful_count: NotRequired[int]
    incorrect_count: NotRequired[int]
    invalidation_refs: NotRequired[list[str]]
    irrelevant_count: NotRequired[int]
    last_accessed_at: NotRequired[str | None]
    memory_id: Required[str]
    promotion_reviewer: NotRequired[str | None]
    rollback_reason: NotRequired[str | None]
    scope: Required[MemoryScope]
    source_task_id: Required[str]
    status: Required[MemoryStatus]
    supporting_task_ids: NotRequired[list[str]]
    version: Required[int]

MemoryScope: TypeAlias = Literal['project', 'user', 'global']

MemoryStatus: TypeAlias = Literal['proposed', 'quarantined', 'active', 'deprecated', 'rolled_back', 'expired']

class NoveltyRecord(TypedDict, total=False):
    duplicate_risk: Required[str]
    explanation: Required[str]
    novelty_score: Required[int]
    similar_tasks: Required[list[str]]
    task_id: Required[str]

class OpenEndedBudget(TypedDict, total=False):
    max_generated_tasks: Required[int]
    max_runtime_ms_per_task: Required[int]
    max_selected_tasks: Required[int]
    max_tool_calls_per_task: Required[int]

class OpenEndedRun(TypedDict, total=False):
    blocked_reason: NotRequired[str | None]
    budget: Required[OpenEndedBudget]
    completed_at: NotRequired[str | None]
    created_at: Required[str]
    generated_task_ids: Required[list[str]]
    objective: Required[str]
    promoted_benchmark_ids: Required[list[str]]
    promoted_skill_ids: Required[list[str]]
    run_id: Required[str]
    selected_task_ids: Required[list[str]]
    source_scope: Required[str]
    status: Required[OpenEndedRunStatus]

OpenEndedRunStatus: TypeAlias = Literal['planned', 'running', 'completed', 'blocked']

class PostTaskJob(TypedDict, total=False):
    attempt: Required[int]
    completed_at: NotRequired[str | None]
    created_at: Required[str]
    input_refs: Required[list[str]]
    job_id: Required[str]
    kind: Required[PostTaskJobKind]
    last_error: NotRequired[str | None]
    lease_expires_at: NotRequired[str | None]
    lease_owner: NotRequired[str | None]
    max_attempts: Required[int]
    result_refs: Required[list[str]]
    session_id: Required[str]
    started_at: NotRequired[str | None]
    status: Required[PostTaskJobStatus]
    task_id: Required[str]
    workspace_id: Required[str]

PostTaskJobKind: TypeAlias = Literal['deep_evaluation', 'candidate_generation', 'regression_execution']

PostTaskJobStatus: TypeAlias = Literal['queued', 'leased', 'running', 'succeeded', 'failed', 'cancelled']

class PostTaskReview(TypedDict, total=False):
    context_issues: Required[list[str]]
    evidence_quality: Required[str]
    failure_reasons: Required[list[str]]
    mode: Required[ReviewMode]
    outcome: Required[str]
    policy_issues: Required[list[str]]
    promotion_candidates: Required[list[str]]
    provider_issues: Required[list[str]]
    success_reasons: Required[list[str]]
    suggested_improvements: Required[list[str]]
    task_id: Required[str]
    tool_issues: Required[list[str]]

class PromotionDecision(TypedDict, total=False):
    applied_version: NotRequired[str | None]
    candidate_id: Required[str]
    created_at: Required[str]
    decision: Required[PromotionDecisionKind]
    decision_id: Required[str]
    expires_at: NotRequired[str | None]
    reason: Required[str]
    reviewer: Required[PromotionReviewer]
    rollback_ref: NotRequired[str | None]

PromotionDecisionKind: TypeAlias = Literal['approve', 'reject', 'needs_human_review']

PromotionReviewer: TypeAlias = Literal['system', 'human', 'agent']

class ProtocolHandshake(TypedDict, total=False):
    name: Required[str]
    versions: Required[ProtocolVersionRange]

class ProtocolVersionRange(TypedDict, total=False):
    current: Required[int]
    minimum: Required[int]

RedactionStatus: TypeAlias = Literal['raw', 'redacted', 'not_required']

class RegressionCampaign(TypedDict, total=False):
    baseline_version: Required[str]
    campaign_id: Required[str]
    candidate_digest: Required[str]
    candidate_id: Required[str]
    case_refs: Required[list[str]]
    completed_at: NotRequired[str | None]
    created_at: Required[str]
    environment_recipe: Required[str]
    hard_gates: Required[list[str]]
    provider_matrix: Required[list[str]]
    replay_modes: Required[list[str]]
    resource_budget: Required[str]
    seeds: Required[list[int]]
    started_at: NotRequired[str | None]

class RegressionCaseResult(TypedDict, total=False):
    case_id: Required[str]
    evidence_checks: Required[list[BenchmarkCheck]]
    expected_verdict: Required[EvaluationVerdict]
    failure_taxonomy: Required[list[str]]
    observed_verdict: Required[EvaluationVerdict]
    passed: Required[bool]
    replay_id: Required[str]

class RegressionExecution(TypedDict, total=False):
    campaign_id: Required[str]
    case_ref: NotRequired[str]
    cost_latency_ref: NotRequired[str | None]
    execution_id: Required[str]
    role: Required[RegressionExecutionRole]
    runtime_version: Required[str]
    status: Required[RegressionExecutionStatus]
    task_trace_ref: NotRequired[str | None]
    verification_ref: NotRequired[str | None]
    workspace_snapshot_digest: Required[str]

RegressionExecutionRole: TypeAlias = Literal['baseline', 'candidate']

RegressionExecutionStatus: TypeAlias = Literal['queued', 'running', 'succeeded', 'failed', 'inconclusive']

class RegressionResult(TypedDict, total=False):
    baseline_benchmark_refs: NotRequired[list[str]]
    baseline_version: Required[str]
    candidate_benchmark_refs: NotRequired[list[str]]
    candidate_id: Required[str]
    candidate_version: Required[str]
    case_results: NotRequired[list[RegressionCaseResult]]
    cases_run: Required[int]
    causal_comparison_refs: Required[list[str]]
    cost_delta: NotRequired[float | None]
    created_at: Required[str]
    failed_cases: Required[int]
    latency_delta: NotRequired[int | None]
    passed_cases: Required[int]
    quality_delta: NotRequired[float | None]
    regression_id: Required[str]
    regressions: Required[list[str]]
    security_delta: NotRequired[float | None]
    suite_kind: NotRequired[BenchmarkSuiteKind]
    verdict: Required[RegressionVerdict]

RegressionVerdict: TypeAlias = Literal['pass', 'fail', 'needs_review']

ReviewMode: TypeAlias = Literal['minimal', 'deep']

class RuntimeEvent(TypedDict, total=False):
    durable: Required[bool]
    event_type: Required[RuntimeEventType]
    id: Required[str]
    parent_event_id: NotRequired[str | None]
    payload: Required[Any]
    payload_ref: NotRequired[str | None]
    sequence_no: Required[int]
    session_id: Required[str]
    source: Required[RuntimeEventSource]
    task_id: NotRequired[str | None]
    timestamp: Required[str]
    turn_id: NotRequired[str | None]

RuntimeEventSource: TypeAlias = Literal['runtime', 'provider', 'tool', 'policy', 'verifier', 'memory', 'evaluator', 'governor', 'evolution', 'user']

RuntimeEventType: TypeAlias = Literal['command_received', 'command_completed', 'command_accepted', 'command_rejected', 'session_created', 'thread_forked', 'thread_rebound', 'task_created', 'turn_started', 'turn_queued', 'busy_policy_decided', 'controller_changed', 'context_built', 'provider_started', 'provider_streamed', 'provider_completed', 'token_usage_recorded', 'assistant_message', 'tool_started', 'tool_completed', 'policy_evaluated', 'verification_completed', 'loop_decided', 'checkpoint_created', 'task_completed', 'task_abort_requested', 'task_aborted', 'task_paused', 'task_resumed', 'approval_requested', 'approval_resolved', 'retry_scheduled', 'provider_fallback', 'provider_auth_required', 'provider_auth_submitted', 'provider_auth_cancelled', 'provider_configured', 'provider_probe_started', 'provider_probe_completed', 'provider_auth_failed', 'provider_rate_limited', 'provider_credential_refreshed', 'loop_guard_triggered', 'compaction_completed', 'memory_retrieved', 'memory_promoted', 'memory_promotion_rejected', 'memory_rolled_back', 'memory_feedback_recorded', 'post_task_reviewed', 'evaluation_completed', 'improvement_candidate_created', 'automation_candidate_created', 'regression_completed', 'promotion_decided', 'candidate_applied', 'candidate_rolled_back', 'benchmark_recorded', 'counterfactual_compared', 'evolution_planned', 'evolution_task_started', 'evolution_task_completed', 'evolution_completed', 'skill_staged', 'skill_reviewed', 'skill_installed', 'skill_rolled_back', 'governor_decided', 'storage_maintenance_completed', 'context_snapshot_created', 'post_task_job_queued', 'post_task_job_started', 'post_task_job_completed', 'post_task_job_failed', 'verification_planned', 'verification_assertion_completed', 'regression_campaign_started', 'regression_execution_completed', 'memory_candidate_quarantined', 'memory_activated', 'memory_invalidated']

class RuntimeGovernorDecision(TypedDict, total=False):
    action: Required[GovernorAction]
    alignment: Required[GoalAlignmentCheck]
    budget_risk: Required[str]
    failed_tool_calls: Required[int]
    iteration: Required[int]
    phase: Required[GovernorPhase]
    reason: Required[str]
    security_risk: Required[str]
    task_id: Required[str]
    tool_calls: Required[int]

class RuntimeLane(TypedDict, total=False):
    active_controller: Required[Actor]
    active_turn_id: NotRequired[str | None]
    busy_policy_default: Required[BusyPolicy]
    injected_inputs: Required[list[str]]
    lane_id: Required[str]
    pending_turns: Required[list[str]]
    session_id: Required[str]
    status: Required[TaskStatus]
    task_id: Required[str]
    workspace_id: Required[str]

class RuntimeQuery(TypedDict, total=False):
    cursor: NotRequired[int | None]
    kind: Required[RuntimeQueryKind]
    query_id: Required[str]
    requester: Required[ActorKind]
    session_id: Required[str]
    task_id: NotRequired[str | None]
    timestamp: Required[str]

RuntimeQueryKind: TypeAlias = Literal['session_state', 'task_state', 'user_projection', 'debug_projection', 'context_projection', 'evaluation_projection', 'replay_cursor', 'memory_list', 'evaluation_results', 'improvement_candidates', 'automation_candidates', 'evolution_state', 'provider_state', 'storage_status', 'task_trace', 'post_task_jobs', 'artifact_chunk']

class SecurityUtilityResult(TypedDict, total=False):
    evidence_refs: Required[list[str]]
    policy_violations: Required[int]
    security_score: NotRequired[float | None]
    utility_score: NotRequired[float | None]
    verdict: Required[EvaluationVerdict]

class SessionCommand(TypedDict, total=False):
    actor: Required[Actor]
    command_id: Required[str]
    idempotency_key: Required[str]
    kind: Required[SessionCommandKind]
    payload: Required[Any]
    session_id: NotRequired[str | None]
    timestamp: Required[str]

SessionCommandKind: TypeAlias = Literal['create', 'prompt', 'approve', 'deny', 'pause', 'resume', 'abort', 'takeover', 'compact', 'memory_rollback', 'memory_feedback', 'run_regression', 'review_candidate', 'apply_candidate', 'rollback_candidate', 'record_benchmark', 'compare_counterfactual', 'plan_evolution', 'run_evolution', 'stage_skill', 'review_skill', 'install_skill', 'rollback_skill', 'provider_configured', 'provider_auth_submitted', 'provider_auth_cancelled', 'run_storage_maintenance', 'wait_post_task_job', 'retry_post_task_job', 'run_regression_campaign', 'review_memory_candidate', 'expire_memory', 'verify', 'replay', 'export']

class SessionCursor(TypedDict, total=False):
    recency_at: Required[str]
    thread_id: Required[str]

class SessionPage(TypedDict, total=False):
    has_more: Required[bool]
    next_cursor: NotRequired[SessionCursor | None]
    sessions: Required[list[SessionSummary]]

class SessionPageRequest(TypedDict, total=False):
    cursor: NotRequired[SessionCursor | None]
    limit: Required[int]

SessionRangeDirection: TypeAlias = Literal['single', 'newer', 'older']

class SessionRangeSpec(TypedDict, total=False):
    count: Required[int]
    direction: Required[SessionRangeDirection]

class SessionSummary(TypedDict, total=False):
    created_at: Required[str]
    forked_from_turn_id: NotRequired[str | None]
    parent_thread_id: NotRequired[str | None]
    preview: Required[str]
    recency_at: Required[str]
    session_id: Required[str]
    thread_id: Required[str]
    title: Required[str]
    updated_at: Required[str]

class SessionWindow(TypedDict, total=False):
    anchor_thread_id: Required[str]
    range: Required[SessionRangeSpec]
    reached_boundary: Required[bool]
    sessions: Required[list[SessionSummary]]

class SessionWindowRequest(TypedDict, total=False):
    anchor_thread_id: Required[str]
    range: Required[SessionRangeSpec]

class SkillCandidate(TypedDict, total=False):
    evidence_refs: Required[list[str]]
    id: Required[str]
    promotion_status: Required[CandidateStatus]
    regression_refs: Required[list[str]]
    reusable_pattern: Required[str]
    rollback_ref: Required[str]
    scope: Required[str]
    source_task_id: Required[str]
    source_trajectory: Required[str]

class SkillLifecycleRecord(TypedDict, total=False):
    candidate_path: Required[str]
    checksum: Required[str]
    created_at: Required[str]
    installed_at: NotRequired[str | None]
    installed_path: NotRequired[str | None]
    manifest: Required[SkillManifest]
    review_reason: NotRequired[str | None]
    reviewed_at: NotRequired[str | None]
    reviewer: NotRequired[str | None]
    rollback_reason: NotRequired[str | None]
    rolled_back_at: NotRequired[str | None]
    status: Required[SkillLifecycleStatus]

SkillLifecycleStatus: TypeAlias = Literal['proposed', 'reviewed', 'rejected', 'installed', 'rolled_back']

class SkillManifest(TypedDict, total=False):
    description: Required[str]
    evidence_refs: Required[list[str]]
    failure_cases: Required[list[str]]
    name: Required[str]
    prerequisites: Required[list[str]]
    regression_refs: Required[list[str]]
    rollback_ref: Required[str]
    scope: Required[str]
    skill_id: Required[str]
    source_task_id: Required[str]
    source_trajectory: Required[str]
    steps: Required[list[str]]

class StateProjection(TypedDict, total=False):
    active_task_id: NotRequired[str | None]
    final_message: NotRequired[str | None]
    last_loop_decision: NotRequired[LoopDecision | None]
    last_sequence_no: Required[int]
    last_verification: NotRequired[VerificationRecord | None]
    pending_approval: NotRequired[str | None]
    runtime_lane: NotRequired[RuntimeLane | None]
    session_id: Required[str]
    task_status: Required[TaskStatus]
    visible_steps: Required[list[VisibleStep]]

class StorageMaintenanceReport(TypedDict, total=False):
    artifact_blobs_removed: Required[int]
    checkpoint_directories_removed: Required[int]
    completed_at: Required[str]
    protected_artifacts_skipped: Required[int]
    stats: Required[StorageStats]
    temporary_artifacts_removed: Required[int]

class StorageStats(TypedDict, total=False):
    artifact_records: Required[int]
    checkpoint_directories: Required[int]
    expired_artifact_blobs: Required[int]
    live_artifact_blobs: Required[int]
    live_artifact_bytes: Required[int]
    rollout_files: Required[int]

TaskClass: TypeAlias = Literal['plain_conversation', 'read_only_analysis', 'workspace_change', 'code_change']

TaskStatus: TypeAlias = Literal['idle', 'running', 'waiting_approval', 'waiting_authentication', 'pausing', 'paused', 'aborting', 'completed', 'partial', 'failed', 'blocked', 'cancelled']

class TaskTracePage(TypedDict, total=False):
    artifacts: Required[list[ArtifactRecord]]
    context_snapshots: Required[list[ContextSnapshot]]
    evaluation: Required[EvaluationProjection]
    events: Required[list[RuntimeEvent]]
    evidence: Required[list[EvidenceRecord]]
    has_more: Required[bool]
    integrity: Required[TraceIntegrity]
    next_cursor: NotRequired[int | None]
    post_task_jobs: Required[list[PostTaskJob]]
    runtime_identity: Required[str]
    session_id: Required[str]
    task_id: Required[str]
    verification: NotRequired[VerificationRecord | None]
    verification_plan: NotRequired[VerificationPlan | None]
    view: Required[TraceView]

class TaskTraceRequest(TypedDict, total=False):
    cursor: NotRequired[int | None]
    limit: Required[int]
    session_id: Required[str]
    task_id: Required[str]
    view: Required[TraceView]
    wait_for_evaluation: Required[bool]

class TokenBudgetSnapshot(TypedDict, total=False):
    action_if_exceeded: Required[BudgetOverflowAction]
    budget_limit: Required[int]
    budget_policy: Required[str]
    context_window: Required[int]
    max_output: Required[int]
    planned_input_tokens: Required[int]
    planned_summary_tokens: Required[int]
    planned_tool_tokens: Required[int]
    reserved_output_tokens: Required[int]
    snapshot_id: Required[str]
    task_id: Required[str]
    turn_id: Required[str]

class ToolResultEnvelope(TypedDict, total=False):
    evidence_refs: Required[list[str]]
    model_visible_excerpt: NotRequired[str | None]
    raw_artifact_ref: NotRequired[str | None]
    risk: Required[str]
    status: Required[ToolResultStatus]
    structured_facts: Required[Any]
    summary: Required[str]
    tool_call_id: Required[str]
    tool_name: Required[str]
    verification_hint: NotRequired[str | None]

ToolResultStatus: TypeAlias = Literal['ok', 'error', 'blocked', 'cancelled', 'timeout']

class TraceIntegrity(TypedDict, total=False):
    complete: Required[bool]
    event_chain_digest: Required[str]
    event_count: Required[int]
    first_sequence: NotRequired[int | None]
    last_sequence: NotRequired[int | None]
    missing_sections: Required[list[str]]
    redacted_fields: Required[list[str]]
    retention_losses: Required[list[str]]
    unresolved_refs: Required[list[str]]

TraceView: TypeAlias = Literal['summary', 'full', 'forensic']

class UserProjection(TypedDict, total=False):
    final_message: NotRequired[str | None]
    pending_approval: NotRequired[str | None]
    residual_risks: Required[list[str]]
    session_id: Required[str]
    status: Required[TaskStatus]
    task_id: NotRequired[str | None]
    visible_steps: Required[list[VisibleStep]]

class VerificationAssertion(TypedDict, total=False):
    assertion_id: Required[str]
    blocking: Required[bool]
    criterion_id: Required[str]
    evidence_refs: Required[list[str]]
    expected: Required[str]
    kind: Required[VerificationAssertionKind]
    message: Required[str]
    required_evidence_strength: Required[str]
    status: Required[VerificationAssertionStatus]
    subject: Required[str]
    verifier_id: Required[str]

VerificationAssertionKind: TypeAlias = Literal['file_state', 'diff', 'command_exit', 'test', 'diagnostic', 'schema', 'policy', 'delivery', 'assistant_response']

VerificationAssertionStatus: TypeAlias = Literal['pending', 'pass', 'fail', 'unknown', 'not_applicable']

class VerificationCheck(TypedDict, total=False):
    command: NotRequired[str | None]
    evidence_refs: Required[list[str]]
    kind: Required[VerificationCheckKind]
    message: Required[str]
    name: Required[str]
    passed: Required[bool]

VerificationCheckKind: TypeAlias = Literal['tool_execution', 'workspace_change', 'objective_validation', 'assistant_response']

VerificationDimensionStatus: TypeAlias = Literal['pass', 'fail', 'partial', 'unknown']

class VerificationDimensions(TypedDict, total=False):
    evidence_status: Required[VerificationDimensionStatus]
    objective_status: Required[VerificationDimensionStatus]
    policy_status: Required[VerificationDimensionStatus]

class VerificationPlan(TypedDict, total=False):
    assertions: Required[list[VerificationAssertion]]
    created_at: Required[str]
    criteria: Required[list[str]]
    dimensions: NotRequired[VerificationDimensions]
    generated_by: Required[str]
    plan_id: Required[str]
    policy_assertions: Required[list[VerificationAssertion]]
    required_artifact_types: Required[list[str]]
    revision: Required[int]
    task_class: Required[TaskClass]
    task_id: Required[str]
    verifier_versions: Required[list[str]]

class VerificationRecord(TypedDict, total=False):
    checks: Required[list[VerificationCheck]]
    completion_criteria: Required[list[str]]
    evidence_refs: Required[list[str]]
    objective: Required[str]
    policy_status: Required[str]
    residual_risks: Required[list[str]]
    result: Required[VerificationResult]
    task_id: Required[str]
    verification_id: Required[str]

VerificationResult: TypeAlias = Literal['pass', 'fail', 'partial', 'unknown']

class VisibleStep(TypedDict, total=False):
    label: Required[str]
    status: Required[str]
    summary: Required[str]

__all__ = [
    "Actor",
    "ActorKind",
    "AppliedCandidate",
    "ArtifactChunk",
    "ArtifactReadRequest",
    "ArtifactRecord",
    "AutomationCandidate",
    "AutomationCandidateKind",
    "BenchmarkCheck",
    "BenchmarkCheckStatus",
    "BenchmarkPromotion",
    "BenchmarkRun",
    "BenchmarkSuiteKind",
    "BudgetOverflowAction",
    "BudgetState",
    "BusyPolicy",
    "BusyPolicyDecision",
    "CandidateRisk",
    "CandidateStatus",
    "CapabilityFrontier",
    "CausalComparison",
    "CommandAck",
    "ContextContributorSnapshot",
    "ContextMessageSnapshot",
    "ContextProjection",
    "ContextSnapshot",
    "CostRecord",
    "CounterfactualReplay",
    "CurriculumItem",
    "DebugEventWindow",
    "DebugProjection",
    "EnvironmentRecipe",
    "EvaluationCase",
    "EvaluationProjection",
    "EvaluationResult",
    "EvaluationRun",
    "EvaluationVerdict",
    "EventFilter",
    "EventPage",
    "EventPageDirection",
    "EventPageRequest",
    "EvidenceRecord",
    "EvidenceStrength",
    "EvolutionState",
    "GeneratedTask",
    "GeneratedTaskExecution",
    "GoalAlignmentCheck",
    "GovernorAction",
    "GovernorPhase",
    "ImprovementCandidate",
    "LoopAction",
    "LoopDecision",
    "MemoryClaim",
    "MemoryRecord",
    "MemoryScope",
    "MemoryStatus",
    "NoveltyRecord",
    "OpenEndedBudget",
    "OpenEndedRun",
    "OpenEndedRunStatus",
    "PostTaskJob",
    "PostTaskJobKind",
    "PostTaskJobStatus",
    "PostTaskReview",
    "PromotionDecision",
    "PromotionDecisionKind",
    "PromotionReviewer",
    "ProtocolHandshake",
    "ProtocolVersionRange",
    "RedactionStatus",
    "RegressionCampaign",
    "RegressionCaseResult",
    "RegressionExecution",
    "RegressionExecutionRole",
    "RegressionExecutionStatus",
    "RegressionResult",
    "RegressionVerdict",
    "ReviewMode",
    "RuntimeEvent",
    "RuntimeEventSource",
    "RuntimeEventType",
    "RuntimeGovernorDecision",
    "RuntimeLane",
    "RuntimeQuery",
    "RuntimeQueryKind",
    "SecurityUtilityResult",
    "SessionCommand",
    "SessionCommandKind",
    "SessionCursor",
    "SessionPage",
    "SessionPageRequest",
    "SessionRangeDirection",
    "SessionRangeSpec",
    "SessionSummary",
    "SessionWindow",
    "SessionWindowRequest",
    "SkillCandidate",
    "SkillLifecycleRecord",
    "SkillLifecycleStatus",
    "SkillManifest",
    "StateProjection",
    "StorageMaintenanceReport",
    "StorageStats",
    "TaskClass",
    "TaskStatus",
    "TaskTracePage",
    "TaskTraceRequest",
    "TokenBudgetSnapshot",
    "ToolResultEnvelope",
    "ToolResultStatus",
    "TraceIntegrity",
    "TraceView",
    "UserProjection",
    "VerificationAssertion",
    "VerificationAssertionKind",
    "VerificationAssertionStatus",
    "VerificationCheck",
    "VerificationCheckKind",
    "VerificationDimensionStatus",
    "VerificationDimensions",
    "VerificationPlan",
    "VerificationRecord",
    "VerificationResult",
    "VisibleStep",
]
