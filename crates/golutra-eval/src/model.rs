use chrono::{DateTime, Utc};
use golutra_core::{
    ArtifactId, EvaluationPartitionKind, EventId, EvidenceId, LoopAction, ProviderRequestId,
    ProviderResponseId, RegressionCampaign, RegressionCampaignId, RegressionExecution,
    RegressionExecutionRole, RunId, TaskId, TaskStatus, TokenUsageRecord, ToolCallId,
    VerificationRecord, VerificationResult,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EvaluationVerdict {
    Pass,
    Fail,
    Partial,
    Unknown,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReviewMode {
    #[default]
    Minimal,
    Deep,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BenchmarkSuiteKind {
    Release,
    Shadow,
    #[default]
    Regression,
    Adversarial,
    Counterfactual,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BenchmarkCheckStatus {
    Pass,
    Fail,
    Unknown,
    NotApplicable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct BenchmarkCheck {
    pub check_id: String,
    pub status: BenchmarkCheckStatus,
    pub reason: String,
    pub evidence_refs: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct CostRecord {
    pub provider_id: String,
    pub model_id: String,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
    pub estimated_cost_usd: Option<f64>,
    pub source: String,
    pub confidence: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct EvaluationCase {
    pub case_id: String,
    pub source: String,
    pub source_task_id: Option<TaskId>,
    pub task_type: String,
    pub objective: String,
    pub expected_outcome: String,
    pub success_criteria: Vec<String>,
    pub required_evidence: Vec<String>,
    pub policy_constraints: Vec<String>,
    pub fixture_refs: Vec<String>,
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TrajectoryReplay {
    pub replay_id: String,
    pub source_task_id: TaskId,
    pub event_count: usize,
    pub artifact_count: usize,
    pub determinism_level: String,
    pub limitations: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FailureDomain {
    RuntimeControlFlow,
    Context,
    Provider,
    Tool,
    Policy,
    Verification,
    Memory,
    ExternalEvaluation,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct FailureTaxonomy {
    pub domain: FailureDomain,
    pub code: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CodeTargetRef {
    pub crate_name: String,
    pub module_path: String,
    pub symbol: Option<String>,
    pub source_path: Option<String>,
    pub source_digest: Option<String>,
    pub owner: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct FailureDiagnosis {
    pub diagnosis_id: String,
    pub source_task_id: TaskId,
    pub taxonomy: FailureTaxonomy,
    pub summary: String,
    pub trigger_event_refs: Vec<EventId>,
    pub causal_event_refs: Vec<EventId>,
    pub expected_behavior: String,
    pub actual_behavior: String,
    pub counterfactual: String,
    pub confidence: u8,
    pub code_targets: Vec<CodeTargetRef>,
    pub regression_commands: Vec<String>,
    pub analyzer_version: String,
    #[serde(default)]
    pub failure_episode_id: Option<String>,
    #[serde(default)]
    pub revision: u32,
    #[serde(default)]
    pub supersedes_diagnosis_id: Option<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FailureSignalKind {
    Producer,
    SelfCheck,
    ExternalAssertion,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct FailureSignalRef {
    pub event_ref: EventId,
    pub kind: FailureSignalKind,
    pub signal_key: String,
    pub summary: String,
    #[serde(default)]
    pub evidence_refs: Vec<EvidenceId>,
    #[serde(default)]
    pub artifact_refs: Vec<ArtifactId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct FailureRecovery {
    pub event_ref: EventId,
    pub signal_key: String,
    pub summary: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FailureEpisodeStatus {
    #[default]
    Active,
    Recovered,
    Superseded,
}

impl FailureEpisodeStatus {
    #[must_use]
    pub const fn is_active(self) -> bool {
        matches!(self, Self::Active)
    }

    #[must_use]
    pub const fn is_recovered(self) -> bool {
        matches!(self, Self::Recovered)
    }

    #[must_use]
    pub const fn is_superseded(self) -> bool {
        matches!(self, Self::Superseded)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct FailureEpisode {
    pub episode_id: String,
    pub source_task_id: TaskId,
    pub status: FailureEpisodeStatus,
    pub primary_signal: FailureSignalRef,
    #[serde(default)]
    pub producer_failures: Vec<FailureSignalRef>,
    #[serde(default)]
    pub self_check_failures: Vec<FailureSignalRef>,
    #[serde(default)]
    pub external_assertion_failures: Vec<FailureSignalRef>,
    #[serde(default)]
    pub diagnosis_refs: Vec<String>,
    #[serde(default)]
    pub recovered_by: Option<FailureRecovery>,
    #[serde(default)]
    pub superseded_by: Option<String>,
    pub opened_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DiagnosticSliceContinuation {
    /// Cursor for `TaskTraceRequest.cursor`; `None` starts at the first event.
    #[serde(default)]
    pub after_sequence_no: Option<u64>,
    pub through_sequence_no: u64,
    pub omitted_event_count: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DiagnosticSlice {
    pub slice_id: String,
    pub source_task_id: TaskId,
    pub diagnosis: FailureDiagnosis,
    pub event_refs: Vec<EventId>,
    #[serde(default)]
    pub causal_event_refs: Vec<EventId>,
    #[serde(default)]
    pub supporting_event_refs: Vec<EventId>,
    pub artifact_refs: Vec<ArtifactId>,
    pub evidence_refs: Vec<EvidenceId>,
    pub omitted_event_count: u64,
    #[serde(default)]
    pub continuation_pages: Vec<DiagnosticSliceContinuation>,
    #[serde(default)]
    pub continuation_pages_truncated: bool,
    #[serde(default)]
    pub selection_strategy: String,
    pub complete: bool,
    pub generated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReplayMode {
    Projection,
    DeterministicControlFlow,
    LiveRegression,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ReplayProviderExchange {
    pub request_id: ProviderRequestId,
    pub response_id: ProviderResponseId,
    pub request_artifact_ref: ArtifactId,
    pub response_artifact_ref: ArtifactId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ReplayToolResult {
    pub tool_call_id: ToolCallId,
    pub provider_tool_call_id: Option<String>,
    pub result_artifact_ref: ArtifactId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ReplayCapsule {
    pub capsule_id: String,
    pub source_task_id: TaskId,
    pub source_run_id: RunId,
    pub mode: ReplayMode,
    pub provider_exchanges: Vec<ReplayProviderExchange>,
    pub tool_results: Vec<ReplayToolResult>,
    pub clock_seed: String,
    pub random_seed: u64,
    pub runtime_config_digest: String,
    pub fixture_ref: Option<String>,
    pub event_chain_digest: String,
    #[serde(default)]
    pub source_last_sequence_no: Option<u64>,
    pub complete: bool,
    pub missing_inputs: Vec<String>,
    pub limitations: Vec<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReplayExecutionStatus {
    Matched,
    Diverged,
    Incomplete,
    Failed,
}

/// Result of re-entering the ordinary AgentLoop with recorded provider and
/// tool artifacts. This is an executable replay result, not a projection-only
/// reconstruction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ReplayExecution {
    pub execution_id: String,
    pub capsule_id: String,
    pub source_task_id: TaskId,
    pub mode: ReplayMode,
    pub status: ReplayExecutionStatus,
    pub provider_exchanges_total: u32,
    pub provider_exchanges_consumed: u32,
    pub tool_results_total: u32,
    pub tool_results_consumed: u32,
    pub expected_loop_action: Option<LoopAction>,
    pub observed_loop_action: Option<LoopAction>,
    pub expected_verification: Option<VerificationResult>,
    pub observed_verification: Option<VerificationResult>,
    pub mismatches: Vec<String>,
    pub started_at: DateTime<Utc>,
    pub completed_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ExternalEvaluationTrust {
    UntrustedLocal,
    OwnerLocal,
    Signed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ExternalEvaluationAssertion {
    pub assertion_id: String,
    pub name: String,
    pub passed: bool,
    pub message: String,
    pub evidence_refs: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ExternalEvaluationPhaseKind {
    Setup,
    Agent,
    Test,
    Assertion,
    Teardown,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ExternalEvaluationPhaseStatus {
    Passed,
    Failed,
    TimedOut,
    Error,
    Skipped,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ExternalEvaluationPhase {
    pub phase_id: String,
    pub kind: ExternalEvaluationPhaseKind,
    pub status: ExternalEvaluationPhaseStatus,
    #[serde(default)]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub completed_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub duration_ms: Option<u64>,
    #[serde(default)]
    pub assertion_refs: Vec<String>,
    #[serde(default)]
    pub evidence_refs: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ExternalEvaluationTerminalCause {
    pub code: String,
    #[serde(default)]
    pub phase_id: Option<String>,
    pub message: String,
    #[serde(default)]
    pub retryable: bool,
    #[serde(default)]
    pub evidence_refs: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct EvaluationAttestation {
    pub algorithm: String,
    pub key_id: String,
    pub signature: String,
    pub signed_digest: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ExternalEvaluationRecord {
    pub evaluation_id: String,
    pub source_task_id: TaskId,
    pub evaluator_id: String,
    pub evaluator_version: String,
    pub harness_id: String,
    pub harness_version: String,
    pub dataset_id: String,
    pub dataset_version: String,
    pub case_id: String,
    pub verdict: EvaluationVerdict,
    pub score: Option<f64>,
    pub score_max: Option<f64>,
    pub assertions: Vec<ExternalEvaluationAssertion>,
    #[serde(default)]
    pub phases: Vec<ExternalEvaluationPhase>,
    #[serde(default)]
    pub terminal_cause: Option<ExternalEvaluationTerminalCause>,
    pub artifact_refs: Vec<String>,
    #[serde(default)]
    pub imported_artifacts: Vec<ImportedEvaluationArtifact>,
    #[serde(default)]
    pub imported_evidence_refs: Vec<EvidenceId>,
    #[serde(default)]
    pub partition: EvaluationPartitionKind,
    #[serde(default)]
    pub seed: Option<u64>,
    #[serde(default)]
    pub provider_variant: Option<String>,
    #[serde(default)]
    pub holdout_protected: bool,
    #[serde(default)]
    pub comparison_group_id: Option<String>,
    #[serde(default)]
    pub candidate_id: Option<String>,
    #[serde(default)]
    pub campaign_id: Option<RegressionCampaignId>,
    #[serde(default)]
    pub role: Option<RegressionExecutionRole>,
    pub base_trace_digest: String,
    pub runtime_identity: String,
    pub result_digest: String,
    pub trust: ExternalEvaluationTrust,
    pub attestation: Option<EvaluationAttestation>,
    pub ingested_at: DateTime<Utc>,
}

/// Host-derived immutable copy of evaluator evidence. These fields are not
/// part of `result_digest`; the digest authenticates evaluator-controlled
/// facts while the imported artifact checksum authenticates local retention.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ImportedEvaluationArtifact {
    pub source_ref: String,
    pub artifact_ref: ArtifactId,
    pub checksum: String,
    pub size_bytes: u64,
}

/// Digest over evaluator-controlled result facts. Local ingestion time and
/// the detached attestation are intentionally excluded.
#[must_use]
pub fn external_evaluation_result_digest(record: &ExternalEvaluationRecord) -> String {
    let value = json!({
        "evaluation_id": record.evaluation_id,
        "source_task_id": record.source_task_id,
        "evaluator_id": record.evaluator_id,
        "evaluator_version": record.evaluator_version,
        "harness_id": record.harness_id,
        "harness_version": record.harness_version,
        "dataset_id": record.dataset_id,
        "dataset_version": record.dataset_version,
        "case_id": record.case_id,
        "verdict": record.verdict,
        "score": record.score,
        "score_max": record.score_max,
        "assertions": record.assertions,
        "phases": record.phases,
        "terminal_cause": record.terminal_cause,
        "artifact_refs": record.artifact_refs,
        "partition": record.partition,
        "seed": record.seed,
        "provider_variant": record.provider_variant,
        "holdout_protected": record.holdout_protected,
        "comparison_group_id": record.comparison_group_id,
        "candidate_id": record.candidate_id,
        "campaign_id": record.campaign_id,
        "role": record.role,
        "base_trace_digest": record.base_trace_digest,
        "runtime_identity": record.runtime_identity,
        "trust": record.trust,
    });
    let bytes = serde_json::to_vec(&canonical_json(value)).unwrap_or_default();
    format!("sha256:{:x}", Sha256::digest(bytes))
}

fn canonical_json(value: Value) -> Value {
    match value {
        Value::Object(object) => {
            let mut entries = object.into_iter().collect::<Vec<_>>();
            entries.sort_by(|left, right| left.0.cmp(&right.0));
            Value::Object(
                entries
                    .into_iter()
                    .map(|(key, value)| (key, canonical_json(value)))
                    .collect(),
            )
        }
        Value::Array(values) => Value::Array(values.into_iter().map(canonical_json).collect()),
        value => value,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RegressionCaseResult {
    pub case_id: String,
    pub replay_id: String,
    pub passed: bool,
    pub expected_verdict: EvaluationVerdict,
    pub observed_verdict: EvaluationVerdict,
    pub evidence_checks: Vec<BenchmarkCheck>,
    pub failure_taxonomy: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct CounterfactualReplay {
    pub replay_id: String,
    pub group_id: String,
    pub baseline_benchmark_id: String,
    pub variant_benchmark_id: String,
    pub controlled_variables: Vec<String>,
    pub changed_layer: String,
    pub limitations: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct CausalComparison {
    pub comparison_id: String,
    pub replay_id: String,
    pub quality_delta: Option<f32>,
    pub utility_delta: Option<f32>,
    pub security_delta: Option<f32>,
    pub token_delta: Option<i64>,
    pub cost_delta_usd: Option<f64>,
    pub latency_delta_ms: Option<i64>,
    pub scaffold_inflation: bool,
    pub conclusion: String,
    #[serde(default)]
    pub baseline_evaluation_ref: Option<String>,
    #[serde(default)]
    pub candidate_evaluation_ref: Option<String>,
    #[serde(default)]
    pub partition: Option<EvaluationPartitionKind>,
    #[serde(default)]
    pub provider_variant: Option<String>,
    #[serde(default)]
    pub seed: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RegressionCoverage {
    pub required_partitions: Vec<EvaluationPartitionKind>,
    pub observed_partitions: Vec<EvaluationPartitionKind>,
    pub missing_partitions: Vec<EvaluationPartitionKind>,
    pub required_providers: Vec<String>,
    pub observed_providers: Vec<String>,
    pub missing_providers: Vec<String>,
    pub required_seeds: Vec<u64>,
    pub observed_seeds: Vec<u64>,
    pub missing_seeds: Vec<u64>,
    pub expected_cells: u32,
    pub completed_cells: u32,
    pub missing_cells: Vec<String>,
    #[serde(default)]
    pub trusted_external_pairs: u32,
    pub trusted_external_evaluation_refs: Vec<String>,
    pub untrusted_external_evaluation_refs: Vec<String>,
    pub holdout_disclosure_violations: Vec<String>,
}

impl RegressionCoverage {
    #[must_use]
    pub fn complete(&self) -> bool {
        self.expected_cells > 0
            && self.expected_cells == self.completed_cells
            && self.missing_partitions.is_empty()
            && self.missing_providers.is_empty()
            && self.missing_seeds.is_empty()
            && self.missing_cells.is_empty()
            && self.holdout_disclosure_violations.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SecurityUtilityResult {
    pub security_score: Option<f32>,
    pub utility_score: Option<f32>,
    pub policy_violations: u32,
    pub evidence_refs: Vec<EvidenceId>,
    pub verdict: EvaluationVerdict,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CandidateStatus {
    Proposed,
    RegressionPassed,
    NeedsHumanReview,
    Approved,
    Applied,
    Rejected,
    RolledBack,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CandidateRisk {
    Low,
    Medium,
    High,
    Critical,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AutomationCandidateKind {
    Benchmark,
    GeneratedTask,
    Skill,
    RuntimeChange,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ImprovementCandidate {
    pub id: String,
    pub source_task_id: TaskId,
    pub source_failure_ids: Vec<String>,
    pub target_type: String,
    pub target_id: Option<String>,
    pub proposed_change: String,
    pub expected_effect: String,
    pub risk_level: CandidateRisk,
    pub evidence_refs: Vec<EvidenceId>,
    pub causal_evidence_refs: Vec<String>,
    pub benchmark_refs: Vec<String>,
    pub rollback_plan: String,
    #[serde(default)]
    pub diagnosis_ref: Option<String>,
    #[serde(default)]
    pub proposed_commands: Vec<String>,
    #[serde(default)]
    pub validation_plan: Vec<String>,
    pub status: CandidateStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct FrozenCandidatePatch {
    pub candidate_id: String,
    pub source_task_id: TaskId,
    pub artifact_ref: ArtifactId,
    pub digest: String,
    pub format: String,
    pub file_count: u32,
    pub total_bytes: u64,
    pub frozen_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct EvaluationRun {
    pub run_id: String,
    pub dataset_id: String,
    pub case_ids: Vec<String>,
    pub system_version: String,
    pub provider_config_ref: String,
    pub runtime_config_ref: String,
    pub started_at: DateTime<Utc>,
    pub completed_at: DateTime<Utc>,
    pub cost: Option<f64>,
    #[serde(default)]
    pub cost_source: String,
    pub latency_ms: Option<u64>,
    #[serde(default)]
    pub input_tokens: Option<u64>,
    #[serde(default)]
    pub output_tokens: Option<u64>,
    #[serde(default)]
    pub reasoning_tokens: Option<u64>,
    #[serde(default)]
    pub total_tokens: Option<u64>,
    #[serde(default)]
    pub cost_records: Vec<CostRecord>,
    pub result_refs: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct EvaluationResult {
    pub result_id: String,
    pub run_id: String,
    pub case_id: String,
    pub source_task_id: TaskId,
    pub verdict: EvaluationVerdict,
    pub quality_score: Option<f32>,
    pub cost: Option<f64>,
    pub latency_ms: Option<u64>,
    pub evidence_refs: Vec<EvidenceId>,
    pub failure_taxonomy: Vec<String>,
    pub residual_risks: Vec<String>,
    #[serde(default)]
    pub security_utility: Option<SecurityUtilityResult>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct BenchmarkRun {
    pub benchmark_id: String,
    #[serde(default)]
    pub suite_kind: BenchmarkSuiteKind,
    pub dataset_version: String,
    pub harness_version: String,
    pub scaffold_id: String,
    #[serde(default)]
    pub scaffold_version: String,
    pub model_id: String,
    pub provider_id: String,
    pub tool_budget: u32,
    pub attempt_count: u32,
    pub runtime_ms: u64,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    #[serde(default)]
    pub reasoning_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
    pub cost_usd: Option<f64>,
    pub cost_source: String,
    pub security_score: Option<f32>,
    pub utility_score: Option<f32>,
    pub artifact_delivery_status: String,
    pub score: Option<f32>,
    pub failure_taxonomy: Vec<String>,
    #[serde(default)]
    pub counterfactual_group_id: Option<String>,
    #[serde(default)]
    pub changed_layer: Option<String>,
    #[serde(default)]
    pub leakage_checks: Vec<BenchmarkCheck>,
    #[serde(default)]
    pub judge_checks: Vec<BenchmarkCheck>,
    #[serde(default)]
    pub scaffold_checks: Vec<BenchmarkCheck>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RegressionResult {
    pub regression_id: String,
    pub candidate_id: String,
    pub baseline_version: String,
    pub candidate_version: String,
    pub cases_run: u32,
    pub passed_cases: u32,
    pub failed_cases: u32,
    pub regressions: Vec<String>,
    pub cost_delta: Option<f64>,
    pub latency_delta: Option<i64>,
    pub quality_delta: Option<f32>,
    pub security_delta: Option<f32>,
    pub causal_comparison_refs: Vec<String>,
    #[serde(default)]
    pub paired_execution_refs: Vec<String>,
    #[serde(default)]
    pub external_evaluation_refs: Vec<String>,
    #[serde(default)]
    pub coverage: RegressionCoverage,
    #[serde(default)]
    pub suite_kind: BenchmarkSuiteKind,
    #[serde(default)]
    pub case_results: Vec<RegressionCaseResult>,
    #[serde(default)]
    pub baseline_benchmark_refs: Vec<String>,
    #[serde(default)]
    pub candidate_benchmark_refs: Vec<String>,
    pub verdict: RegressionVerdict,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RegressionVerdict {
    Pass,
    Fail,
    NeedsReview,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PromotionDecision {
    pub decision_id: String,
    pub candidate_id: String,
    pub decision: PromotionDecisionKind,
    pub reason: String,
    pub reviewer: PromotionReviewer,
    pub applied_version: Option<String>,
    pub rollback_ref: Option<String>,
    pub expires_at: Option<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PromotionGateFacts {
    pub trace_complete: bool,
    pub unresolved_refs: Vec<String>,
    pub verification: EvaluationVerdict,
    pub paired_execution_refs: Vec<String>,
    #[serde(default)]
    pub trusted_external_evaluation_refs: Vec<String>,
    #[serde(default)]
    pub coverage_complete: bool,
    #[serde(default)]
    pub missing_coverage: Vec<String>,
    #[serde(default)]
    pub holdout_disclosure_violations: Vec<String>,
    pub candidate_mutates_control_plane: bool,
    pub mutation_reasons: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PromotionDecisionKind {
    Approve,
    Reject,
    NeedsHumanReview,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PromotionReviewer {
    System,
    Human,
    Agent,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct GeneratedTask {
    pub id: String,
    pub source_task_id: TaskId,
    pub source: String,
    pub objective: String,
    pub novelty_score: Option<f32>,
    pub difficulty_score: Option<f32>,
    pub expected_learning_value: String,
    pub environment_recipe: String,
    pub safety_constraints: Vec<String>,
    pub promotion_status: CandidateStatus,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct CurriculumItem {
    pub task_id: String,
    pub selected: bool,
    pub selected_reason: Option<String>,
    pub rejected_reason: Option<String>,
    pub frontier_ref: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CapabilityFrontier {
    pub mastered: Vec<String>,
    pub near_miss: Vec<String>,
    pub failed: Vec<String>,
    pub blocked: Vec<String>,
    pub missing_tools: Vec<String>,
    pub unstable_skills: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SkillCandidate {
    pub id: String,
    pub source_task_id: TaskId,
    pub source_trajectory: String,
    pub reusable_pattern: String,
    pub evidence_refs: Vec<EvidenceId>,
    pub regression_refs: Vec<String>,
    pub scope: String,
    pub rollback_ref: String,
    pub promotion_status: CandidateStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct BenchmarkPromotion {
    pub id: String,
    pub source_task_id: TaskId,
    pub failure_taxonomy: Vec<String>,
    pub fixture: String,
    pub evaluator: String,
    pub anti_overfit_notes: Vec<String>,
    pub rollback_ref: String,
    pub promotion_status: CandidateStatus,
    pub accepted_by: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AutomationCandidate {
    pub id: String,
    pub source_task_id: TaskId,
    pub kind: AutomationCandidateKind,
    pub summary: String,
    pub risk_level: CandidateRisk,
    pub evidence_refs: Vec<EvidenceId>,
    pub regression_plan: String,
    pub rollback_ref: String,
    pub status: CandidateStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AppliedCandidate {
    pub candidate_id: String,
    pub applied_version: String,
    pub rollback_ref: String,
    pub applied_at: DateTime<Utc>,
    pub rolled_back_at: Option<DateTime<Utc>>,
    pub rollback_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PostTaskReview {
    pub task_id: TaskId,
    pub mode: ReviewMode,
    pub outcome: String,
    pub success_reasons: Vec<String>,
    pub failure_reasons: Vec<String>,
    pub evidence_quality: String,
    pub policy_issues: Vec<String>,
    pub context_issues: Vec<String>,
    pub tool_issues: Vec<String>,
    pub provider_issues: Vec<String>,
    pub suggested_improvements: Vec<String>,
    pub promotion_candidates: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct TaskEvaluationInput {
    pub task_id: TaskId,
    pub objective: String,
    pub task_status: TaskStatus,
    pub verification: Option<VerificationRecord>,
    pub event_count: usize,
    pub artifact_count: usize,
    pub tool_count: usize,
    pub latency_ms: Option<u64>,
    pub failure_summary: Option<String>,
    pub token_usage: Vec<TokenUsageRecord>,
    pub provider_config_ref: String,
    pub runtime_config_ref: String,
    pub policy_violation_count: u32,
}

#[derive(Debug, Clone)]
pub struct TaskEvaluationBundle {
    pub case: EvaluationCase,
    pub run: EvaluationRun,
    pub result: EvaluationResult,
    pub replay: TrajectoryReplay,
    pub review: PostTaskReview,
    pub improvement_candidate: Option<ImprovementCandidate>,
    pub generated_task: Option<GeneratedTask>,
    pub skill_candidate: Option<SkillCandidate>,
    pub benchmark_promotion: Option<BenchmarkPromotion>,
    pub automation_candidates: Vec<AutomationCandidate>,
    pub benchmark_run: BenchmarkRun,
}
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct EvaluationState {
    pub cases: Vec<EvaluationCase>,
    pub runs: Vec<EvaluationRun>,
    pub results: Vec<EvaluationResult>,
    pub replays: Vec<TrajectoryReplay>,
    #[serde(default)]
    pub replay_capsules: Vec<ReplayCapsule>,
    #[serde(default)]
    pub replay_executions: Vec<ReplayExecution>,
    #[serde(default)]
    pub failure_diagnoses: Vec<FailureDiagnosis>,
    #[serde(default)]
    pub failure_episodes: Vec<FailureEpisode>,
    #[serde(default)]
    pub diagnostic_slices: Vec<DiagnosticSlice>,
    #[serde(default)]
    pub external_evaluations: Vec<ExternalEvaluationRecord>,
    pub reviews: Vec<PostTaskReview>,
    #[serde(default)]
    pub benchmark_runs: Vec<BenchmarkRun>,
    #[serde(default)]
    pub counterfactual_replays: Vec<CounterfactualReplay>,
    #[serde(default)]
    pub causal_comparisons: Vec<CausalComparison>,
    pub improvement_candidates: Vec<ImprovementCandidate>,
    #[serde(default)]
    pub frozen_candidate_patches: Vec<FrozenCandidatePatch>,
    pub generated_tasks: Vec<GeneratedTask>,
    pub skill_candidates: Vec<SkillCandidate>,
    pub benchmark_promotions: Vec<BenchmarkPromotion>,
    pub automation_candidates: Vec<AutomationCandidate>,
    pub regressions: Vec<RegressionResult>,
    pub promotion_decisions: Vec<PromotionDecision>,
    pub applied_candidates: Vec<AppliedCandidate>,
    #[serde(default)]
    pub regression_campaigns: Vec<RegressionCampaign>,
    #[serde(default)]
    pub regression_executions: Vec<RegressionExecution>,
}
