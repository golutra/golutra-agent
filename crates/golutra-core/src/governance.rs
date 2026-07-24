use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{
    ArtifactId, ContextSnapshotId, EvidenceId, MemoryCandidateId, PostTaskJobId, ProviderRequestId,
    RegressionCampaignId, RegressionExecutionId, SessionId, TaskId, TaskStatus,
    TokenBudgetSnapshot, TurnId, VerificationAssertionId, VerificationId, VerificationPlanId,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TaskClass {
    PlainConversation,
    ReadOnlyAnalysis,
    WorkspaceChange,
    CodeChange,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TraceView {
    Summary,
    Full,
    Forensic,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ContextContributorSnapshot {
    pub name: String,
    pub role: String,
    pub source_refs: Vec<String>,
    pub included: bool,
    pub trimmed: bool,
    #[serde(default)]
    pub original_estimated_tokens: u64,
    #[serde(default)]
    pub retained_estimated_tokens: u64,
    #[serde(default)]
    pub strategy: String,
    pub estimated_tokens: u64,
    pub content_digest: String,
    pub redacted_content_ref: Option<ArtifactId>,
    pub invalidation_refs: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ContextMessageSnapshot {
    pub index: u32,
    pub role: String,
    pub content_digest: String,
    pub estimated_tokens: u64,
    pub tool_call_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ContextSnapshot {
    pub snapshot_id: ContextSnapshotId,
    pub session_id: SessionId,
    pub task_id: TaskId,
    pub turn_id: TurnId,
    pub provider_request_id: ProviderRequestId,
    pub provider_id: String,
    pub model_id: String,
    pub contributor_manifest: Vec<ContextContributorSnapshot>,
    pub message_manifest: Vec<ContextMessageSnapshot>,
    pub tool_schema_digests: Vec<String>,
    pub generation_config_digest: Option<String>,
    pub budget_snapshot: TokenBudgetSnapshot,
    pub canonical_request_digest: String,
    pub redacted_request_artifact_ref: Option<ArtifactId>,
    pub restricted_request_artifact_ref: Option<ArtifactId>,
    pub estimate_source: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PostTaskJobKind {
    DeepEvaluation,
    CandidateGeneration,
    RegressionExecution,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PostTaskJobStatus {
    Queued,
    Leased,
    Running,
    Succeeded,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PostTaskJob {
    pub job_id: PostTaskJobId,
    pub kind: PostTaskJobKind,
    pub workspace_id: String,
    pub session_id: String,
    pub task_id: TaskId,
    pub input_refs: Vec<String>,
    pub status: PostTaskJobStatus,
    pub attempt: u32,
    pub max_attempts: u32,
    pub lease_owner: Option<String>,
    pub lease_expires_at: Option<DateTime<Utc>>,
    pub result_refs: Vec<String>,
    pub last_error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum VerificationAssertionKind {
    FileState,
    Diff,
    CommandExit,
    Test,
    Diagnostic,
    Schema,
    Policy,
    Delivery,
    AssistantResponse,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum VerificationAssertionStatus {
    Pending,
    Pass,
    Fail,
    Unknown,
    NotApplicable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct VerificationAssertion {
    pub assertion_id: VerificationAssertionId,
    pub criterion_id: String,
    pub kind: VerificationAssertionKind,
    pub subject: String,
    pub expected: String,
    pub verifier_id: String,
    pub required_evidence_strength: String,
    pub blocking: bool,
    pub status: VerificationAssertionStatus,
    pub evidence_refs: Vec<EvidenceId>,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct VerificationPlan {
    pub plan_id: VerificationPlanId,
    pub task_id: TaskId,
    pub task_class: TaskClass,
    pub criteria: Vec<String>,
    pub assertions: Vec<VerificationAssertion>,
    pub policy_assertions: Vec<VerificationAssertion>,
    pub required_artifact_types: Vec<String>,
    pub generated_by: String,
    pub verifier_versions: Vec<String>,
    #[serde(default)]
    pub dimensions: VerificationDimensions,
    pub revision: u32,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum VerificationDimensionStatus {
    Pass,
    Fail,
    Partial,
    #[default]
    Unknown,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct VerificationDimensions {
    pub evidence_status: VerificationDimensionStatus,
    pub objective_status: VerificationDimensionStatus,
    pub policy_status: VerificationDimensionStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct MemoryClaim {
    pub candidate_id: MemoryCandidateId,
    pub subject: String,
    pub predicate: String,
    pub object: String,
    pub scope: String,
    pub source_task_refs: Vec<TaskId>,
    pub evidence_refs: Vec<EvidenceId>,
    pub confidence: u8,
    pub valid_from: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
    pub invalidation_refs: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MemoryLifecycle {
    Proposed,
    Quarantined,
    Active,
    Deprecated,
    RolledBack,
    Expired,
}

#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum EvaluationPartitionKind {
    #[default]
    Source,
    Historical,
    Generated,
    Holdout,
    Adversarial,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RegressionExecutionRole {
    Baseline,
    Candidate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RegressionExecutionStatus {
    Queued,
    Running,
    Succeeded,
    Failed,
    Inconclusive,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RegressionCampaign {
    pub campaign_id: RegressionCampaignId,
    pub candidate_id: String,
    pub candidate_digest: String,
    pub baseline_version: String,
    pub environment_recipe: String,
    pub case_refs: Vec<String>,
    #[serde(default)]
    pub case_partitions: BTreeMap<String, EvaluationPartitionKind>,
    #[serde(default)]
    pub required_partitions: Vec<EvaluationPartitionKind>,
    pub replay_modes: Vec<String>,
    pub provider_matrix: Vec<String>,
    pub seeds: Vec<u64>,
    #[serde(default, alias = "minimum_trusted_external_evaluations")]
    pub minimum_trusted_external_pairs: u32,
    pub resource_budget: String,
    pub hard_gates: Vec<String>,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RegressionExecution {
    pub execution_id: RegressionExecutionId,
    pub campaign_id: RegressionCampaignId,
    #[serde(default)]
    pub case_ref: String,
    #[serde(default)]
    pub partition: EvaluationPartitionKind,
    #[serde(default)]
    pub provider_variant: String,
    #[serde(default)]
    pub seed: u64,
    pub role: RegressionExecutionRole,
    pub runtime_version: String,
    pub workspace_snapshot_digest: String,
    pub task_trace_ref: Option<String>,
    pub verification_ref: Option<VerificationId>,
    pub cost_latency_ref: Option<String>,
    pub status: RegressionExecutionStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TaskTerminalSummary {
    pub task_id: TaskId,
    pub status: TaskStatus,
    pub objective: String,
    pub completed_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TraceIntegrity {
    pub event_count: u64,
    pub first_sequence: Option<u64>,
    pub last_sequence: Option<u64>,
    pub event_chain_digest: String,
    pub unresolved_refs: Vec<String>,
    pub missing_sections: Vec<String>,
    pub retention_losses: Vec<String>,
    pub redacted_fields: Vec<String>,
    #[serde(default)]
    pub missing_causal_links: Vec<String>,
    #[serde(default)]
    pub orphan_events: Vec<String>,
    #[serde(default)]
    pub broken_lifecycle_pairs: Vec<String>,
    #[serde(default)]
    pub provenance_mismatches: Vec<String>,
    #[serde(default)]
    pub artifact_checksum_failures: Vec<String>,
    #[serde(default)]
    pub external_overlay_failures: Vec<String>,
    pub complete: bool,
}
