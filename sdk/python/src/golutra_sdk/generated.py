"""Generated from Golutra Rust protocol schemas. Do not edit manually."""

from __future__ import annotations

from typing import Any, Literal, Never, NotRequired, Required, TypeAlias, TypedDict

class Actor(TypedDict, total=False):
    id: Required[str]
    kind: Required[ActorKind]

ActorKind: TypeAlias = Literal['user', 'api', 'tui', 'cli', 'sdk', 'web', 'ide', 'runtime']

class AgentItem(TypedDict, total=False):
    content: NotRequired[str | None]
    data: Required[Any]
    id: Required[str]
    kind: Required[AgentItemKind]
    runtime_event_id: NotRequired[str | None]
    sequence_no: NotRequired[int | None]
    status: Required[AgentItemStatus]
    title: Required[str]

AgentItemKind: TypeAlias = Literal['user_message', 'assistant_message', 'model', 'tool', 'approval', 'verification', 'runtime']

AgentItemStatus: TypeAlias = Literal['in_progress', 'completed', 'failed', 'cancelled']

class AgentStreamEventThreadStarted(TypedDict, total=False):
    session_id: Required[str]
    thread_id: Required[str]
    timestamp: Required[str]
    type: Required[Literal['thread.started']]
    workspace_root: NotRequired[str | None]

class AgentStreamEventTurnStarted(TypedDict, total=False):
    session_id: Required[str]
    task_id: NotRequired[str | None]
    thread_id: Required[str]
    timestamp: Required[str]
    turn_id: NotRequired[str | None]
    type: Required[Literal['turn.started']]

class AgentStreamEventItemStarted(TypedDict, total=False):
    item: Required[AgentItem]
    type: Required[Literal['item.started']]

class AgentStreamEventItemUpdated(TypedDict, total=False):
    item: Required[AgentItem]
    type: Required[Literal['item.updated']]

class AgentStreamEventItemCompleted(TypedDict, total=False):
    item: Required[AgentItem]
    type: Required[Literal['item.completed']]

class AgentStreamEventRuntimeEvent(TypedDict, total=False):
    event: Required[RuntimeEvent]
    type: Required[Literal['runtime.event']]

class AgentStreamEventTurnCompleted(TypedDict, total=False):
    final_message: NotRequired[str | None]
    last_sequence_no: NotRequired[int | None]
    session_id: Required[str]
    status: Required[TaskStatus]
    task_id: NotRequired[str | None]
    thread_id: Required[str]
    timestamp: Required[str]
    turn_id: NotRequired[str | None]
    type: Required[Literal['turn.completed']]
    verification: NotRequired[VerificationRecord | None]

class AgentStreamEventTurnFailed(TypedDict, total=False):
    error: Required[str]
    final_message: NotRequired[str | None]
    last_sequence_no: NotRequired[int | None]
    session_id: Required[str]
    status: Required[TaskStatus]
    task_id: NotRequired[str | None]
    thread_id: Required[str]
    timestamp: Required[str]
    turn_id: NotRequired[str | None]
    type: Required[Literal['turn.failed']]
    verification: NotRequired[VerificationRecord | None]

AgentStreamEvent: TypeAlias = AgentStreamEventThreadStarted | AgentStreamEventTurnStarted | AgentStreamEventItemStarted | AgentStreamEventItemUpdated | AgentStreamEventItemCompleted | AgentStreamEventRuntimeEvent | AgentStreamEventTurnCompleted | AgentStreamEventTurnFailed

class AgentThreadRef(TypedDict, total=False):
    session_id: Required[str]
    thread_id: Required[str]
    workspace_root: NotRequired[str | None]

class AgentTurnOptions(TypedDict, total=False):
    completion_criteria: NotRequired[list[str]]
    external_verifiers: NotRequired[list[ExternalVerificationSpec]]
    output_schema: NotRequired[Any]

class AgentTurnResult(TypedDict, total=False):
    final_message: NotRequired[str | None]
    last_sequence_no: NotRequired[int | None]
    session_id: Required[str]
    status: Required[TaskStatus]
    task_id: NotRequired[str | None]
    thread_id: Required[str]
    turn_id: NotRequired[str | None]
    verification: NotRequired[VerificationRecord | None]

class AgentTurnStart(TypedDict, total=False):
    accepted: Required[bool]
    command_id: Required[str]
    reason: NotRequired[str | None]
    session_id: Required[str]
    task_id: NotRequired[str | None]
    thread_id: Required[str]
    turn_id: NotRequired[str | None]

class AgentTurnStartResponse(TypedDict, total=False):
    accepted: Required[bool]
    attachment_id: Required[str]
    command_id: Required[str]
    cursor: NotRequired[int | None]
    reason: NotRequired[str | None]
    thread: Required[AgentThreadRef]

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
    redaction_status: NotRequired[RedactionStatus]
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

class BuildProvenance(TypedDict, total=False):
    binary_checksum: NotRequired[str | None]
    cargo_lock_digest: NotRequired[str | None]
    dirty: Required[bool]
    features: Required[list[str]]
    git_commit: NotRequired[str | None]
    package_version: Required[str]
    profile: Required[str]
    rustc_version: Required[str]
    schema_version: Required[int]
    source_digest: NotRequired[str | None]
    target: Required[str]

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
    baseline_evaluation_ref: NotRequired[str | None]
    candidate_evaluation_ref: NotRequired[str | None]
    comparison_id: Required[str]
    conclusion: Required[str]
    cost_delta_usd: NotRequired[float | None]
    latency_delta_ms: NotRequired[int | None]
    partition: NotRequired[EvaluationPartitionKind | None]
    provider_variant: NotRequired[str | None]
    quality_delta: NotRequired[float | None]
    replay_id: Required[str]
    scaffold_inflation: Required[bool]
    security_delta: NotRequired[float | None]
    seed: NotRequired[int | None]
    token_delta: NotRequired[int | None]
    utility_delta: NotRequired[float | None]

class CausalContext(TypedDict, total=False):
    candidate_id: NotRequired[str | None]
    provider_request_id: NotRequired[str | None]
    provider_response_id: NotRequired[str | None]
    provider_round_id: NotRequired[str | None]
    provider_tool_call_id: NotRequired[str | None]
    regression_campaign_id: NotRequired[str | None]
    run_id: NotRequired[str | None]
    session_id: NotRequired[str | None]
    step_id: NotRequired[str | None]
    step_no: NotRequired[int | None]
    task_id: NotRequired[str | None]
    tool_call_id: NotRequired[str | None]
    turn_id: NotRequired[str | None]
    verification_id: NotRequired[str | None]
    workspace_id: NotRequired[str | None]

class CausalLink(TypedDict, total=False):
    event_id: Required[str]
    relation: Required[CausalRelation]

CausalRelation: TypeAlias = Literal['parent', 'triggered_by', 'responds_to', 'derived_from', 'verifies', 'compares', 'supersedes']

class CodeTargetRef(TypedDict, total=False):
    crate_name: Required[str]
    module_path: Required[str]
    owner: Required[str]
    source_digest: NotRequired[str | None]
    source_path: NotRequired[str | None]
    symbol: NotRequired[str | None]

class CommandAck(TypedDict, total=False):
    accepted: Required[bool]
    command_id: Required[str]
    reason: NotRequired[str | None]

class ContextContributorSnapshot(TypedDict, total=False):
    content_digest: Required[str]
    estimated_tokens: Required[int]
    included: Required[bool]
    invalidation_refs: Required[list[str]]
    message_indexes: NotRequired[list[int]]
    name: Required[str]
    original_estimated_tokens: NotRequired[int]
    redacted_content_ref: NotRequired[str | None]
    retained_estimated_tokens: NotRequired[int]
    role: Required[str]
    source_refs: Required[list[str]]
    strategy: NotRequired[str]
    trimmed: Required[bool]

class ContextMessageSnapshot(TypedDict, total=False):
    content_digest: Required[str]
    contributor: NotRequired[str]
    estimated_tokens: Required[int]
    index: Required[int]
    origin: NotRequired[str]
    role: Required[str]
    source_refs: NotRequired[list[str]]
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
    causal_comparisons: NotRequired[list[CausalComparison]]
    diagnostic_slice: NotRequired[DiagnosticSlice | None]
    event_window: Required[DebugEventWindow]
    events: Required[list[RuntimeEvent]]
    evidence: Required[list[EvidenceRecord]]
    external_evaluations: NotRequired[list[ExternalEvaluationRecord]]
    failure_diagnosis: NotRequired[FailureDiagnosis | None]
    failure_episodes: NotRequired[list[FailureEpisode]]
    loop_decisions: Required[list[LoopDecision]]
    missing_sections: NotRequired[list[str]]
    post_task_jobs: NotRequired[list[PostTaskJob]]
    replay_execution: NotRequired[ReplayExecution | None]
    retention_losses: NotRequired[list[str]]
    session_id: Required[str]
    task_id: NotRequired[str | None]
    tool_results: Required[list[ToolResultEnvelope]]
    trace_complete: NotRequired[bool]
    verification: NotRequired[VerificationRecord | None]

class DiagnosticSlice(TypedDict, total=False):
    artifact_refs: Required[list[str]]
    complete: Required[bool]
    diagnosis: Required[FailureDiagnosis]
    event_refs: Required[list[str]]
    evidence_refs: Required[list[str]]
    generated_at: Required[str]
    omitted_event_count: Required[int]
    slice_id: Required[str]
    source_task_id: Required[str]

DriverControllerMode: TypeAlias = Literal['controller', 'observer']

class DriverEnvelopeHello(TypedDict, total=False):
    request_id: Required[str]
    protocol_version: NotRequired[int | None]
    type: Required[Literal['hello']]

class DriverEnvelopeCapabilities(TypedDict, total=False):
    request_id: Required[str]
    type: Required[Literal['capabilities']]

class DriverEnvelopeState(TypedDict, total=False):
    request_id: Required[str]
    type: Required[Literal['state']]

class DriverEnvelopePing(TypedDict, total=False):
    request_id: Required[str]
    type: Required[Literal['ping']]

class DriverEnvelopeInputPrompt(TypedDict, total=False):
    request_id: Required[str]
    text: Required[str]
    type: Required[Literal['input_prompt']]

class DriverEnvelopeInputSlash(TypedDict, total=False):
    request_id: Required[str]
    text: Required[str]
    type: Required[Literal['input_slash']]

class DriverEnvelopeInputKey(TypedDict, total=False):
    request_id: Required[str]
    key: Required[DriverKey]
    type: Required[Literal['input_key']]

class DriverEnvelopeInputPaste(TypedDict, total=False):
    request_id: Required[str]
    text: Required[str]
    type: Required[Literal['input_paste']]

class DriverEnvelopeInputMouse(TypedDict, total=False):
    request_id: Required[str]
    event: Required[DriverMouseEvent]
    type: Required[Literal['input_mouse']]

class DriverEnvelopeResize(TypedDict, total=False):
    request_id: Required[str]
    height: Required[int]
    type: Required[Literal['resize']]
    width: Required[int]

class DriverEnvelopeWait(TypedDict, total=False):
    request_id: Required[str]
    timeout_ms: NotRequired[int | None]
    type: Required[Literal['wait']]
    until: Required[WaitCondition]

class DriverEnvelopeSnapshot(TypedDict, total=False):
    request_id: Required[str]
    detail: NotRequired[SnapshotDetail]
    frame_id: NotRequired[str | None]
    height: Required[int]
    panes: NotRequired[SnapshotPanes]
    rows: NotRequired[RowRange | None]
    scope: NotRequired[SnapshotScope]
    type: Required[Literal['snapshot']]
    width: Required[int]

class DriverEnvelopeMetrics(TypedDict, total=False):
    request_id: Required[str]
    type: Required[Literal['metrics']]

class DriverEnvelopeTakeover(TypedDict, total=False):
    request_id: Required[str]
    type: Required[Literal['takeover']]

class DriverEnvelopeAbort(TypedDict, total=False):
    request_id: Required[str]
    type: Required[Literal['abort']]

class DriverEnvelopeClose(TypedDict, total=False):
    request_id: Required[str]
    abort_active_task: NotRequired[bool]
    type: Required[Literal['close']]

DriverEnvelope: TypeAlias = DriverEnvelopeHello | DriverEnvelopeCapabilities | DriverEnvelopeState | DriverEnvelopePing | DriverEnvelopeInputPrompt | DriverEnvelopeInputSlash | DriverEnvelopeInputKey | DriverEnvelopeInputPaste | DriverEnvelopeInputMouse | DriverEnvelopeResize | DriverEnvelopeWait | DriverEnvelopeSnapshot | DriverEnvelopeMetrics | DriverEnvelopeTakeover | DriverEnvelopeAbort | DriverEnvelopeClose

class DriverKeyChar(TypedDict, total=False):
    char: Required[str]

DriverKey: TypeAlias = Literal['enter', 'escape', 'up', 'down', 'left', 'right', 'page_up', 'page_down', 'home', 'end', 'backspace', 'delete', 'tab', 'ctrl_c'] | DriverKeyChar

class DriverLatencyMetrics(TypedDict, total=False):
    last_ms: Required[int]
    max_ms: Required[int]
    samples: Required[int]
    total_ms: Required[int]

class DriverMetrics(TypedDict, total=False):
    connections: Required[int]
    frame_cache_entries: Required[int]
    frozen_frame_hits: Required[int]
    frozen_frame_misses: Required[int]
    instance_id: Required[str]
    pending_waits: Required[int]
    reconnects: Required[int]
    rejected_connections: Required[int]
    request_errors: Required[int]
    requests: Required[int]
    snapshot_latency: Required[DriverLatencyMetrics]
    snapshot_renders: Required[int]
    snapshot_requests: Required[int]
    sync_attempts: Required[int]
    sync_errors: Required[int]
    sync_latency: Required[DriverLatencyMetrics]
    wait_cancelled: Required[int]
    wait_latency: Required[DriverLatencyMetrics]
    wait_requests: Required[int]
    wait_results: Required[int]
    wait_timeouts: Required[int]

class DriverMouseEvent(TypedDict, total=False):
    column: Required[int]
    kind: Required[DriverMouseKind]
    row: Required[int]

DriverMouseKind: TypeAlias = Literal['left_click', 'scroll_up', 'scroll_down']

class DriverNotification(TypedDict, total=False):
    kind: Required[DriverNotificationKind]
    sequence_no: NotRequired[int | None]
    status: NotRequired[DriverTaskStatus | None]

DriverNotificationKind: TypeAlias = Literal['heartbeat', 'runtime_event_available', 'state_changed', 'task_terminal']

class DriverResponseEnvelopeReady(TypedDict, total=False):
    request_id: Required[str]
    controller_mode: Required[DriverControllerMode]
    instance_id: Required[str]
    minimum_protocol_version: Required[int]
    protocol_version: Required[int]
    session_id: Required[str]
    thread_id: Required[str]
    type: Required[Literal['ready']]
    workspace_id: Required[str]
    workspace_path: Required[str]

class DriverResponseEnvelopeCapabilities(TypedDict, total=False):
    request_id: Required[str]
    capabilities: Required[list[str]]
    type: Required[Literal['capabilities']]

class DriverResponseEnvelopeState(TypedDict, total=False):
    request_id: Required[str]
    closed: Required[bool]
    controller_mode: Required[DriverControllerMode]
    facts_expanded: Required[bool]
    height: Required[int]
    instance_id: Required[str]
    session_id: Required[str]
    status: Required[DriverTaskStatus]
    task_id: NotRequired[str | None]
    thread_id: Required[str]
    turn_id: NotRequired[str | None]
    type: Required[Literal['state']]
    width: Required[int]

class DriverResponseEnvelopePong(TypedDict, total=False):
    request_id: Required[str]
    type: Required[Literal['pong']]

class DriverResponseEnvelopeSnapshot(TypedDict, total=False):
    request_id: Required[str]
    cells: NotRequired[list[TuiFrameCell] | None]
    complete: Required[bool]
    event_high_watermark: NotRequired[int | None]
    frame_id: Required[str]
    height: Required[int]
    hit_regions: NotRequired[list[TuiHitRegion]]
    instance_id: Required[str]
    lines: Required[list[TuiFrameLine]]
    missing_sections: Required[list[str]]
    next_range: NotRequired[RowRange | None]
    panes: Required[SnapshotPanes]
    redaction_status: Required[RedactionStatus]
    returned_range: Required[RowRange]
    scope: Required[SnapshotScope]
    session_id: Required[str]
    task_id: NotRequired[str | None]
    total_rows: Required[int]
    turn_id: NotRequired[str | None]
    type: Required[Literal['snapshot']]
    width: Required[int]
    workspace_id: Required[str]

class DriverResponseEnvelopeMetrics(TypedDict, total=False):
    request_id: Required[str]
    metrics: Required[DriverMetrics]
    type: Required[Literal['metrics']]

class DriverResponseEnvelopeAccepted(TypedDict, total=False):
    request_id: Required[str]
    message: Required[str]
    type: Required[Literal['accepted']]

class DriverResponseEnvelopeWaitResult(TypedDict, total=False):
    request_id: Required[str]
    condition: Required[WaitCondition]
    state: Required[DriverState]
    type: Required[Literal['wait_result']]

class DriverResponseEnvelopeWaitTimeout(TypedDict, total=False):
    request_id: Required[str]
    condition: Required[WaitCondition]
    state: Required[DriverState]
    type: Required[Literal['wait_timeout']]

class DriverResponseEnvelopeEvent(TypedDict, total=False):
    request_id: Required[str]
    event: Required[DriverNotification]
    type: Required[Literal['event']]

class DriverResponseEnvelopeClosed(TypedDict, total=False):
    request_id: Required[str]
    type: Required[Literal['closed']]

class DriverResponseEnvelopeError(TypedDict, total=False):
    request_id: Required[str]
    code: Required[str]
    message: Required[str]
    type: Required[Literal['error']]

DriverResponseEnvelope: TypeAlias = DriverResponseEnvelopeReady | DriverResponseEnvelopeCapabilities | DriverResponseEnvelopeState | DriverResponseEnvelopePong | DriverResponseEnvelopeSnapshot | DriverResponseEnvelopeMetrics | DriverResponseEnvelopeAccepted | DriverResponseEnvelopeWaitResult | DriverResponseEnvelopeWaitTimeout | DriverResponseEnvelopeEvent | DriverResponseEnvelopeClosed | DriverResponseEnvelopeError

class DriverState(TypedDict, total=False):
    closed: Required[bool]
    controller_mode: Required[DriverControllerMode]
    facts_expanded: Required[bool]
    height: Required[int]
    instance_id: Required[str]
    session_id: Required[str]
    status: Required[DriverTaskStatus]
    task_id: NotRequired[str | None]
    thread_id: Required[str]
    turn_id: NotRequired[str | None]
    width: Required[int]

DriverTaskStatus: TypeAlias = Literal['connecting', 'idle', 'running', 'waiting_approval', 'waiting_authentication', 'pausing', 'paused', 'aborting', 'completed', 'partial', 'failed', 'blocked', 'cancelled', 'interrupted', 'uncertain']

class EnvironmentRecipe(TypedDict, total=False):
    dependency_snapshot: Required[str]
    fixture_refs: Required[list[str]]
    generated_task_id: Required[str]
    permission_profile: Required[str]
    provider_profile: Required[str]
    recipe_id: Required[str]
    replay_seed: Required[str]
    repo_ref: Required[str]

class EvaluationAttestation(TypedDict, total=False):
    algorithm: Required[str]
    key_id: Required[str]
    signature: Required[str]
    signed_digest: Required[str]

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

EvaluationPartitionKind: TypeAlias = Literal['source', 'historical', 'generated', 'holdout', 'adversarial']

class EvaluationProjection(TypedDict, total=False):
    automation_candidates: Required[list[AutomationCandidate]]
    causal_comparisons: NotRequired[list[CausalComparison]]
    diagnostic_slices: NotRequired[list[DiagnosticSlice]]
    external_evaluations: NotRequired[list[ExternalEvaluationRecord]]
    failure_diagnoses: NotRequired[list[FailureDiagnosis]]
    failure_episodes: NotRequired[list[FailureEpisode]]
    improvement_candidates: Required[list[ImprovementCandidate]]
    integrity_warnings: Required[list[str]]
    post_task_jobs: Required[list[PostTaskJob]]
    promotion_decisions: Required[list[PromotionDecision]]
    regressions: Required[list[RegressionResult]]
    replay_capsules: NotRequired[list[ReplayCapsule]]
    replay_executions: NotRequired[list[ReplayExecution]]
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

class ExternalEvaluationAssertion(TypedDict, total=False):
    assertion_id: Required[str]
    evidence_refs: Required[list[str]]
    message: Required[str]
    name: Required[str]
    passed: Required[bool]

class ExternalEvaluationRecord(TypedDict, total=False):
    artifact_refs: Required[list[str]]
    assertions: Required[list[ExternalEvaluationAssertion]]
    attestation: NotRequired[EvaluationAttestation | None]
    base_trace_digest: Required[str]
    campaign_id: NotRequired[str | None]
    candidate_id: NotRequired[str | None]
    case_id: Required[str]
    comparison_group_id: NotRequired[str | None]
    dataset_id: Required[str]
    dataset_version: Required[str]
    evaluation_id: Required[str]
    evaluator_id: Required[str]
    evaluator_version: Required[str]
    harness_id: Required[str]
    harness_version: Required[str]
    holdout_protected: NotRequired[bool]
    imported_artifacts: NotRequired[list[ImportedEvaluationArtifact]]
    imported_evidence_refs: NotRequired[list[str]]
    ingested_at: Required[str]
    partition: NotRequired[EvaluationPartitionKind]
    provider_variant: NotRequired[str | None]
    result_digest: Required[str]
    role: NotRequired[RegressionExecutionRole | None]
    runtime_identity: Required[str]
    score: NotRequired[float | None]
    score_max: NotRequired[float | None]
    seed: NotRequired[int | None]
    source_task_id: Required[str]
    trust: Required[ExternalEvaluationTrust]
    verdict: Required[EvaluationVerdict]

ExternalEvaluationTrust: TypeAlias = Literal['untrusted_local', 'owner_local', 'signed']

class ExternalVerificationSpec(TypedDict, total=False):
    args: NotRequired[list[str]]
    cwd: NotRequired[str]
    expected_exit_code: NotRequired[int]
    max_output_bytes: NotRequired[int]
    program: Required[str]
    timeout_ms: NotRequired[int]

class FailureDiagnosis(TypedDict, total=False):
    actual_behavior: Required[str]
    analyzer_version: Required[str]
    causal_event_refs: Required[list[str]]
    code_targets: Required[list[CodeTargetRef]]
    confidence: Required[int]
    counterfactual: Required[str]
    created_at: Required[str]
    diagnosis_id: Required[str]
    expected_behavior: Required[str]
    failure_episode_id: NotRequired[str | None]
    regression_commands: Required[list[str]]
    revision: NotRequired[int]
    source_task_id: Required[str]
    summary: Required[str]
    supersedes_diagnosis_id: NotRequired[str | None]
    taxonomy: Required[FailureTaxonomy]
    trigger_event_refs: Required[list[str]]

FailureDomain: TypeAlias = Literal['runtime_control_flow', 'context', 'provider', 'tool', 'policy', 'verification', 'memory', 'external_evaluation', 'unknown']

class FailureEpisode(TypedDict, total=False):
    diagnosis_refs: NotRequired[list[str]]
    episode_id: Required[str]
    external_assertion_failures: NotRequired[list[FailureSignalRef]]
    opened_at: Required[str]
    primary_signal: Required[FailureSignalRef]
    producer_failures: NotRequired[list[FailureSignalRef]]
    recovered_by: NotRequired[FailureRecovery | None]
    self_check_failures: NotRequired[list[FailureSignalRef]]
    source_task_id: Required[str]
    status: Required[FailureEpisodeStatus]
    superseded_by: NotRequired[str | None]
    updated_at: Required[str]

FailureEpisodeStatus: TypeAlias = Literal['active', 'recovered', 'superseded']

class FailureRecovery(TypedDict, total=False):
    event_ref: Required[str]
    signal_key: Required[str]
    summary: Required[str]

FailureSignalKind: TypeAlias = Literal['producer', 'self_check', 'external_assertion']

class FailureSignalRef(TypedDict, total=False):
    artifact_refs: NotRequired[list[str]]
    event_ref: Required[str]
    evidence_refs: NotRequired[list[str]]
    kind: Required[FailureSignalKind]
    signal_key: Required[str]
    summary: Required[str]

class FailureTaxonomy(TypedDict, total=False):
    code: Required[str]
    domain: Required[FailureDomain]

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

class ImportedEvaluationArtifact(TypedDict, total=False):
    artifact_ref: Required[str]
    checksum: Required[str]
    size_bytes: Required[int]
    source_ref: Required[str]

class ImprovementCandidate(TypedDict, total=False):
    benchmark_refs: Required[list[str]]
    causal_evidence_refs: Required[list[str]]
    diagnosis_ref: NotRequired[str | None]
    evidence_refs: Required[list[str]]
    expected_effect: Required[str]
    id: Required[str]
    proposed_change: Required[str]
    proposed_commands: NotRequired[list[str]]
    risk_level: Required[CandidateRisk]
    rollback_plan: Required[str]
    source_failure_ids: Required[list[str]]
    source_task_id: Required[str]
    status: Required[CandidateStatus]
    target_id: NotRequired[str | None]
    target_type: Required[str]
    validation_plan: NotRequired[list[str]]

class IncompleteToolCall(TypedDict, total=False):
    side_effect_possible: Required[bool]
    started_event_ref: Required[str]
    tool_call_id: Required[str]
    tool_name: Required[str]

class JsonRpcErrorObject(TypedDict, total=False):
    code: Required[int]
    data: NotRequired[Any]
    message: Required[str]

class JsonRpcNotification(TypedDict, total=False):
    jsonrpc: Required[str]
    method: Required[str]
    params: Required[Any]

class JsonRpcRequest(TypedDict, total=False):
    id: NotRequired[Any]
    jsonrpc: Required[str]
    method: Required[str]
    params: NotRequired[Any]

class JsonRpcResponse(TypedDict, total=False):
    error: NotRequired[JsonRpcErrorObject | None]
    id: NotRequired[Any]
    jsonrpc: Required[str]
    result: NotRequired[Any]

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
    case_partitions: NotRequired[dict[str, EvaluationPartitionKind]]
    case_refs: Required[list[str]]
    completed_at: NotRequired[str | None]
    created_at: Required[str]
    environment_recipe: Required[str]
    hard_gates: Required[list[str]]
    minimum_trusted_external_pairs: NotRequired[int]
    provider_matrix: Required[list[str]]
    replay_modes: Required[list[str]]
    required_partitions: NotRequired[list[EvaluationPartitionKind]]
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

class RegressionCoverage(TypedDict, total=False):
    completed_cells: Required[int]
    expected_cells: Required[int]
    holdout_disclosure_violations: Required[list[str]]
    missing_cells: Required[list[str]]
    missing_partitions: Required[list[EvaluationPartitionKind]]
    missing_providers: Required[list[str]]
    missing_seeds: Required[list[int]]
    observed_partitions: Required[list[EvaluationPartitionKind]]
    observed_providers: Required[list[str]]
    observed_seeds: Required[list[int]]
    required_partitions: Required[list[EvaluationPartitionKind]]
    required_providers: Required[list[str]]
    required_seeds: Required[list[int]]
    trusted_external_evaluation_refs: Required[list[str]]
    trusted_external_pairs: NotRequired[int]
    untrusted_external_evaluation_refs: Required[list[str]]

class RegressionExecution(TypedDict, total=False):
    campaign_id: Required[str]
    case_ref: NotRequired[str]
    cost_latency_ref: NotRequired[str | None]
    execution_id: Required[str]
    partition: NotRequired[EvaluationPartitionKind]
    provider_variant: NotRequired[str]
    role: Required[RegressionExecutionRole]
    runtime_version: Required[str]
    seed: NotRequired[int]
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
    coverage: NotRequired[RegressionCoverage]
    created_at: Required[str]
    external_evaluation_refs: NotRequired[list[str]]
    failed_cases: Required[int]
    latency_delta: NotRequired[int | None]
    paired_execution_refs: NotRequired[list[str]]
    passed_cases: Required[int]
    quality_delta: NotRequired[float | None]
    regression_id: Required[str]
    regressions: Required[list[str]]
    security_delta: NotRequired[float | None]
    suite_kind: NotRequired[BenchmarkSuiteKind]
    verdict: Required[RegressionVerdict]

RegressionVerdict: TypeAlias = Literal['pass', 'fail', 'needs_review']

class ReplayCapsule(TypedDict, total=False):
    capsule_id: Required[str]
    clock_seed: Required[str]
    complete: Required[bool]
    created_at: Required[str]
    event_chain_digest: Required[str]
    fixture_ref: NotRequired[str | None]
    limitations: Required[list[str]]
    missing_inputs: Required[list[str]]
    mode: Required[ReplayMode]
    provider_exchanges: Required[list[ReplayProviderExchange]]
    random_seed: Required[int]
    runtime_config_digest: Required[str]
    source_last_sequence_no: NotRequired[int | None]
    source_run_id: Required[str]
    source_task_id: Required[str]
    tool_results: Required[list[ReplayToolResult]]

class ReplayExecution(TypedDict, total=False):
    capsule_id: Required[str]
    completed_at: Required[str]
    execution_id: Required[str]
    expected_loop_action: NotRequired[LoopAction | None]
    expected_verification: NotRequired[VerificationResult | None]
    mismatches: Required[list[str]]
    mode: Required[ReplayMode]
    observed_loop_action: NotRequired[LoopAction | None]
    observed_verification: NotRequired[VerificationResult | None]
    provider_exchanges_consumed: Required[int]
    provider_exchanges_total: Required[int]
    source_task_id: Required[str]
    started_at: Required[str]
    status: Required[ReplayExecutionStatus]
    tool_results_consumed: Required[int]
    tool_results_total: Required[int]

ReplayExecutionStatus: TypeAlias = Literal['matched', 'diverged', 'incomplete', 'failed']

ReplayMode: TypeAlias = Literal['projection', 'deterministic_control_flow', 'live_regression']

class ReplayProviderExchange(TypedDict, total=False):
    request_artifact_ref: Required[str]
    request_id: Required[str]
    response_artifact_ref: Required[str]
    response_id: Required[str]

class ReplayToolResult(TypedDict, total=False):
    provider_tool_call_id: NotRequired[str | None]
    result_artifact_ref: Required[str]
    tool_call_id: Required[str]

ReviewMode: TypeAlias = Literal['minimal', 'deep']

class RowRange(TypedDict, total=False):
    end: Required[int]
    start: Required[int]

class RunProvenance(TypedDict, total=False):
    build: Required[BuildProvenance]
    captured_at: Required[str]
    policy_digest: NotRequired[str | None]
    provider_config_digest: NotRequired[str | None]
    run_id: Required[str]
    runtime_config_digest: NotRequired[str | None]
    runtime_identity: Required[str]
    schema_version: Required[int]
    tool_manifest_digest: NotRequired[str | None]
    verifier_digest: NotRequired[str | None]
    workspace_initial_digest: NotRequired[str | None]

class RuntimeEvent(TypedDict, total=False):
    causal_context: NotRequired[CausalContext]
    causal_links: NotRequired[list[CausalLink]]
    durable: Required[bool]
    event_type: Required[RuntimeEventType]
    id: Required[str]
    parent_event_id: NotRequired[str | None]
    payload: Required[Any]
    payload_ref: NotRequired[str | None]
    schema_version: NotRequired[int]
    sequence_no: Required[int]
    session_id: Required[str]
    source: Required[RuntimeEventSource]
    task_id: NotRequired[str | None]
    timestamp: Required[str]
    turn_id: NotRequired[str | None]

RuntimeEventSource: TypeAlias = Literal['runtime', 'provider', 'tool', 'policy', 'verifier', 'memory', 'evaluator', 'governor', 'evolution', 'user']

RuntimeEventType: TypeAlias = Literal['command_received', 'command_completed', 'command_accepted', 'command_rejected', 'session_created', 'thread_forked', 'thread_rebound', 'task_created', 'turn_started', 'step_started', 'step_completed', 'step_checkpointed', 'turn_queued', 'busy_policy_decided', 'controller_changed', 'context_built', 'provider_started', 'provider_streamed', 'provider_completed', 'provider_failed', 'token_usage_recorded', 'assistant_message', 'tool_started', 'tool_progress', 'tool_completed', 'policy_evaluated', 'verification_completed', 'loop_decided', 'checkpoint_created', 'task_completed', 'task_abort_requested', 'task_aborted', 'task_interrupted', 'task_uncertain', 'task_reconciled', 'task_paused', 'task_resumed', 'approval_requested', 'approval_resolved', 'retry_scheduled', 'provider_fallback', 'provider_transport_fallback', 'provider_auth_required', 'provider_auth_submitted', 'provider_auth_cancelled', 'provider_configured', 'provider_probe_started', 'provider_probe_completed', 'provider_auth_failed', 'provider_rate_limited', 'provider_credential_refreshed', 'loop_guard_triggered', 'compaction_started', 'compaction_completed', 'compaction_failed', 'memory_retrieved', 'memory_promoted', 'memory_promotion_rejected', 'memory_rolled_back', 'memory_feedback_recorded', 'post_task_reviewed', 'evaluation_completed', 'improvement_candidate_created', 'automation_candidate_created', 'regression_completed', 'promotion_decided', 'candidate_applied', 'candidate_rolled_back', 'benchmark_recorded', 'counterfactual_compared', 'evolution_planned', 'evolution_task_started', 'evolution_task_completed', 'evolution_completed', 'skill_staged', 'skill_reviewed', 'skill_installed', 'skill_rolled_back', 'governor_decided', 'storage_maintenance_completed', 'context_snapshot_created', 'post_task_job_queued', 'post_task_job_started', 'post_task_job_completed', 'post_task_job_failed', 'verification_planned', 'verification_assertion_completed', 'regression_campaign_started', 'regression_execution_completed', 'memory_candidate_quarantined', 'memory_activated', 'memory_invalidated', 'failure_diagnosed', 'failure_episode_recorded', 'diagnostic_slice_created', 'replay_capsule_created', 'replay_executed', 'external_evaluation_ingested', 'external_evaluation_compared']

class RuntimeGovernorDecision(TypedDict, total=False):
    action: Required[GovernorAction]
    alignment: Required[GoalAlignmentCheck]
    budget_risk: Required[str]
    consecutive_failed_tool_calls: Required[int]
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

SessionCommandKind: TypeAlias = Literal['create', 'prompt', 'approve', 'deny', 'pause', 'resume', 'abort', 'reconcile_task', 'takeover', 'compact', 'memory_rollback', 'memory_feedback', 'run_regression', 'review_candidate', 'apply_candidate', 'rollback_candidate', 'record_benchmark', 'ingest_external_evaluation', 'compare_counterfactual', 'plan_evolution', 'run_evolution', 'stage_skill', 'review_skill', 'install_skill', 'rollback_skill', 'provider_configured', 'provider_auth_submitted', 'provider_auth_cancelled', 'run_storage_maintenance', 'wait_post_task_job', 'retry_post_task_job', 'run_regression_campaign', 'review_memory_candidate', 'expire_memory', 'verify', 'replay', 'export']

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

SnapshotDetail: TypeAlias = Literal['text', 'cells']

SnapshotPanes: TypeAlias = Literal['transcript', 'developer', 'response_and_developer', 'full_screen']

SnapshotScope: TypeAlias = Literal['current_turn', 'task', 'session', 'screen']

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

TaskReconciliationDecision: TypeAlias = Literal['no_side_effect_observed', 'side_effect_observed', 'abandon']

class TaskReconciliationRecord(TypedDict, total=False):
    decision: Required[TaskReconciliationDecision]
    note: NotRequired[str | None]
    reconciled_at: Required[str]
    reconciled_by: Required[Actor]
    recovery_event_ref: Required[str]
    resulting_status: Required[TaskStatus]
    resumed_pending_turns: Required[bool]
    task_id: Required[str]

TaskRecoveryDisposition: TypeAlias = Literal['interrupted', 'uncertain']

class TaskRecoveryRecord(TypedDict, total=False):
    checkpoint_event_refs: Required[list[str]]
    detected_at: Required[str]
    disposition: Required[TaskRecoveryDisposition]
    incomplete_tool_calls: Required[list[IncompleteToolCall]]
    interrupted_turn_ids: Required[list[str]]
    last_event_ref: NotRequired[str | None]
    previous_runtime_identity: NotRequired[str | None]
    reason: Required[str]
    reconciliation_required: Required[bool]
    recovering_runtime_identity: Required[str]
    running_process_ids: Required[list[str]]
    safe_to_replay: Required[bool]
    task_id: Required[str]

TaskStatus: TypeAlias = Literal['idle', 'running', 'waiting_approval', 'waiting_authentication', 'pausing', 'paused', 'aborting', 'completed', 'partial', 'failed', 'blocked', 'cancelled', 'interrupted', 'uncertain']

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
    run_provenance: NotRequired[RunProvenance | None]
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
    artifact_checksum_failures: NotRequired[list[str]]
    broken_lifecycle_pairs: NotRequired[list[str]]
    complete: Required[bool]
    event_chain_digest: Required[str]
    event_count: Required[int]
    external_overlay_failures: NotRequired[list[str]]
    first_sequence: NotRequired[int | None]
    last_sequence: NotRequired[int | None]
    missing_causal_links: NotRequired[list[str]]
    missing_sections: Required[list[str]]
    orphan_events: NotRequired[list[str]]
    provenance_mismatches: NotRequired[list[str]]
    redacted_fields: Required[list[str]]
    retention_losses: Required[list[str]]
    unresolved_refs: Required[list[str]]

TraceView: TypeAlias = Literal['summary', 'full', 'forensic']

class TuiDriverProtocolBundle(TypedDict, total=False):
    request: Required[DriverEnvelope]
    response: Required[DriverResponseEnvelope]
    snapshot: Required[TuiFrame]

class TuiFrame(TypedDict, total=False):
    cells: NotRequired[list[TuiFrameCell] | None]
    complete: Required[bool]
    event_high_watermark: NotRequired[int | None]
    frame_id: Required[str]
    height: Required[int]
    hit_regions: NotRequired[list[TuiHitRegion]]
    instance_id: Required[str]
    lines: Required[list[TuiFrameLine]]
    missing_sections: Required[list[str]]
    next_range: NotRequired[RowRange | None]
    panes: Required[SnapshotPanes]
    redaction_status: Required[RedactionStatus]
    returned_range: Required[RowRange]
    scope: Required[SnapshotScope]
    session_id: Required[str]
    task_id: NotRequired[str | None]
    total_rows: Required[int]
    turn_id: NotRequired[str | None]
    width: Required[int]
    workspace_id: Required[str]

class TuiFrameCell(TypedDict, total=False):
    background: Required[str]
    column: Required[int]
    foreground: Required[str]
    modifiers: Required[str]
    pane: Required[TuiFramePane]
    row: Required[int]
    symbol: Required[str]

class TuiFrameLine(TypedDict, total=False):
    display_width: Required[int]
    pane: Required[TuiFramePane]
    row: Required[int]
    text: Required[str]

TuiFramePane: TypeAlias = Literal['transcript', 'developer', 'response_and_developer', 'screen']

TuiHitPane: TypeAlias = Literal['transcript', 'bottom', 'developer']

class TuiHitRegion(TypedDict, total=False):
    height: Required[int]
    id: Required[str]
    pane: Required[TuiHitPane]
    width: Required[int]
    x: Required[int]
    y: Required[int]

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

VerificationCheckKind: TypeAlias = Literal['tool_execution', 'workspace_change', 'objective_validation', 'assistant_response', 'schema']

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

class WaitConditionReady(TypedDict, total=False):
    kind: Required[Literal['ready']]

class WaitConditionIdle(TypedDict, total=False):
    kind: Required[Literal['idle']]

class WaitConditionTaskStarted(TypedDict, total=False):
    kind: Required[Literal['task_started']]

class WaitConditionTaskTerminal(TypedDict, total=False):
    kind: Required[Literal['task_terminal']]

class WaitConditionTurnTerminal(TypedDict, total=False):
    kind: Required[Literal['turn_terminal']]

class WaitConditionApprovalRequired(TypedDict, total=False):
    kind: Required[Literal['approval_required']]

class WaitConditionAuthenticationRequired(TypedDict, total=False):
    kind: Required[Literal['authentication_required']]

class WaitConditionEvaluationTerminal(TypedDict, total=False):
    kind: Required[Literal['evaluation_terminal']]

class WaitConditionEvent(TypedDict, total=False):
    event_type: Required[str]
    kind: Required[Literal['event']]
    sequence_at_least: NotRequired[int | None]

WaitCondition: TypeAlias = WaitConditionReady | WaitConditionIdle | WaitConditionTaskStarted | WaitConditionTaskTerminal | WaitConditionTurnTerminal | WaitConditionApprovalRequired | WaitConditionAuthenticationRequired | WaitConditionEvaluationTerminal | WaitConditionEvent

__all__ = [
    "Actor",
    "ActorKind",
    "AgentItem",
    "AgentItemKind",
    "AgentItemStatus",
    "AgentStreamEvent",
    "AgentStreamEventItemCompleted",
    "AgentStreamEventItemStarted",
    "AgentStreamEventItemUpdated",
    "AgentStreamEventRuntimeEvent",
    "AgentStreamEventThreadStarted",
    "AgentStreamEventTurnCompleted",
    "AgentStreamEventTurnFailed",
    "AgentStreamEventTurnStarted",
    "AgentThreadRef",
    "AgentTurnOptions",
    "AgentTurnResult",
    "AgentTurnStart",
    "AgentTurnStartResponse",
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
    "BuildProvenance",
    "BusyPolicy",
    "BusyPolicyDecision",
    "CandidateRisk",
    "CandidateStatus",
    "CapabilityFrontier",
    "CausalComparison",
    "CausalContext",
    "CausalLink",
    "CausalRelation",
    "CodeTargetRef",
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
    "DiagnosticSlice",
    "DriverControllerMode",
    "DriverEnvelope",
    "DriverEnvelopeAbort",
    "DriverEnvelopeCapabilities",
    "DriverEnvelopeClose",
    "DriverEnvelopeHello",
    "DriverEnvelopeInputKey",
    "DriverEnvelopeInputMouse",
    "DriverEnvelopeInputPaste",
    "DriverEnvelopeInputPrompt",
    "DriverEnvelopeInputSlash",
    "DriverEnvelopeMetrics",
    "DriverEnvelopePing",
    "DriverEnvelopeResize",
    "DriverEnvelopeSnapshot",
    "DriverEnvelopeState",
    "DriverEnvelopeTakeover",
    "DriverEnvelopeWait",
    "DriverKey",
    "DriverKeyChar",
    "DriverLatencyMetrics",
    "DriverMetrics",
    "DriverMouseEvent",
    "DriverMouseKind",
    "DriverNotification",
    "DriverNotificationKind",
    "DriverResponseEnvelope",
    "DriverResponseEnvelopeAccepted",
    "DriverResponseEnvelopeCapabilities",
    "DriverResponseEnvelopeClosed",
    "DriverResponseEnvelopeError",
    "DriverResponseEnvelopeEvent",
    "DriverResponseEnvelopeMetrics",
    "DriverResponseEnvelopePong",
    "DriverResponseEnvelopeReady",
    "DriverResponseEnvelopeSnapshot",
    "DriverResponseEnvelopeState",
    "DriverResponseEnvelopeWaitResult",
    "DriverResponseEnvelopeWaitTimeout",
    "DriverState",
    "DriverTaskStatus",
    "EnvironmentRecipe",
    "EvaluationAttestation",
    "EvaluationCase",
    "EvaluationPartitionKind",
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
    "ExternalEvaluationAssertion",
    "ExternalEvaluationRecord",
    "ExternalEvaluationTrust",
    "ExternalVerificationSpec",
    "FailureDiagnosis",
    "FailureDomain",
    "FailureEpisode",
    "FailureEpisodeStatus",
    "FailureRecovery",
    "FailureSignalKind",
    "FailureSignalRef",
    "FailureTaxonomy",
    "GeneratedTask",
    "GeneratedTaskExecution",
    "GoalAlignmentCheck",
    "GovernorAction",
    "GovernorPhase",
    "ImportedEvaluationArtifact",
    "ImprovementCandidate",
    "IncompleteToolCall",
    "JsonRpcErrorObject",
    "JsonRpcNotification",
    "JsonRpcRequest",
    "JsonRpcResponse",
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
    "RegressionCoverage",
    "RegressionExecution",
    "RegressionExecutionRole",
    "RegressionExecutionStatus",
    "RegressionResult",
    "RegressionVerdict",
    "ReplayCapsule",
    "ReplayExecution",
    "ReplayExecutionStatus",
    "ReplayMode",
    "ReplayProviderExchange",
    "ReplayToolResult",
    "ReviewMode",
    "RowRange",
    "RunProvenance",
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
    "SnapshotDetail",
    "SnapshotPanes",
    "SnapshotScope",
    "StateProjection",
    "StorageMaintenanceReport",
    "StorageStats",
    "TaskClass",
    "TaskReconciliationDecision",
    "TaskReconciliationRecord",
    "TaskRecoveryDisposition",
    "TaskRecoveryRecord",
    "TaskStatus",
    "TaskTracePage",
    "TaskTraceRequest",
    "TokenBudgetSnapshot",
    "ToolResultEnvelope",
    "ToolResultStatus",
    "TraceIntegrity",
    "TraceView",
    "TuiDriverProtocolBundle",
    "TuiFrame",
    "TuiFrameCell",
    "TuiFrameLine",
    "TuiFramePane",
    "TuiHitPane",
    "TuiHitRegion",
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
    "WaitCondition",
    "WaitConditionApprovalRequired",
    "WaitConditionAuthenticationRequired",
    "WaitConditionEvaluationTerminal",
    "WaitConditionEvent",
    "WaitConditionIdle",
    "WaitConditionReady",
    "WaitConditionTaskStarted",
    "WaitConditionTaskTerminal",
    "WaitConditionTurnTerminal",
]
