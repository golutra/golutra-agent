use std::{collections::BTreeMap, path::PathBuf};

use chrono::{DateTime, Utc};
use golutra_release::BuildReport;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProducerKind {
    Internal,
    External,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PrivacyClass {
    Public,
    Redacted,
    Restricted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EpochStatus {
    Observing,
    Planning,
    Generating,
    Evaluating,
    AwaitingPromotion,
    BuildingRelease,
    Previewing,
    Canarying,
    Promoted,
    NoOpportunity,
    NoImprovement,
    Rejected,
    BudgetExhausted,
    Inconclusive,
    PausedForReview,
    RolledBack,
}

impl EpochStatus {
    #[must_use]
    pub const fn terminal(self) -> bool {
        matches!(
            self,
            Self::Promoted
                | Self::NoOpportunity
                | Self::NoImprovement
                | Self::Rejected
                | Self::BudgetExhausted
                | Self::Inconclusive
                | Self::PausedForReview
                | Self::RolledBack
        )
    }
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
pub enum CandidateStatus {
    Frozen,
    Evaluating,
    Passed,
    Rejected,
    Built,
    Previewing,
    Canarying,
    Promoted,
    RolledBack,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum GateVerdict {
    Pass,
    Fail,
    Inconclusive,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct EvolutionEpochBudget {
    pub max_candidates: u32,
    pub max_generations: u32,
    pub max_provider_tokens: u64,
    pub max_cost_usd: f64,
    pub max_latency_delta_ms: i64,
    pub max_build_minutes: u32,
    pub max_holdout_queries: u32,
    pub max_canary_releases: u32,
    pub deadline: DateTime<Utc>,
}

impl Default for EvolutionEpochBudget {
    fn default() -> Self {
        Self {
            max_candidates: 3,
            max_generations: 3,
            max_provider_tokens: 100_000,
            max_cost_usd: 25.0,
            max_latency_delta_ms: 5_000,
            max_build_minutes: 15,
            max_holdout_queries: 2,
            max_canary_releases: 1,
            deadline: Utc::now() + chrono::Duration::hours(1),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ObservationBundle {
    pub task_id: String,
    pub source_version: String,
    pub trace_ref: String,
    pub complete: bool,
    pub unresolved_refs: Vec<String>,
    pub failure_taxonomy: Vec<String>,
    pub objective: String,
    pub observation_refs: Vec<String>,
    pub verification_pass: bool,
    pub privacy_class: PrivacyClass,
    pub independent_group: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct EvolutionOpportunity {
    pub opportunity_id: String,
    pub source_version: String,
    pub source_task_refs: Vec<String>,
    #[serde(default)]
    pub independent_groups: Vec<String>,
    pub observation_refs: Vec<String>,
    pub failure_cluster: String,
    pub suspected_layer: String,
    pub causal_hypothesis: String,
    pub expected_effect: String,
    pub confidence: u8,
    pub privacy_class: PrivacyClass,
    pub proposed_eval_slices: Vec<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct EvolutionEpoch {
    pub epoch_id: String,
    pub opportunity_id: String,
    pub parent_release_id: Option<String>,
    pub budget: EvolutionEpochBudget,
    pub status: EpochStatus,
    pub candidate_ids: Vec<String>,
    pub generation_count: u32,
    pub holdout_queries: u32,
    pub created_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
    pub terminal_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CandidateProposal {
    pub candidate_id: Option<String>,
    pub epoch_id: String,
    pub producer_kind: ProducerKind,
    pub producer_version: String,
    pub source_commit: String,
    pub worktree: PathBuf,
    pub patch_digest: String,
    pub target_paths: Vec<String>,
    pub change_class: String,
    pub generation_model: String,
    pub generation_config_digest: String,
    pub risk_level: CandidateRisk,
    pub state_migration_ref: Option<String>,
    pub rollback_plan: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct EvolutionCandidate {
    pub candidate_id: String,
    pub epoch_id: String,
    pub opportunity_id: String,
    pub producer_kind: ProducerKind,
    pub producer_version: String,
    pub source_commit: String,
    pub worktree: PathBuf,
    pub patch_digest: String,
    pub target_paths: Vec<String>,
    pub change_class: String,
    pub generation_model: String,
    pub generation_config_digest: String,
    pub risk_level: CandidateRisk,
    pub state_migration_ref: Option<String>,
    pub rollback_plan: String,
    pub status: CandidateStatus,
    pub release_id: Option<String>,
    #[serde(default)]
    pub trusted_build: bool,
    pub created_at: DateTime<Utc>,
    pub frozen_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeEvaluationPartition {
    Development,
    Security,
    Migration,
    Sealed,
    Fresh,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RuntimeEvaluationSuite {
    pub candidate_id: String,
    pub cases: Vec<RuntimeEvaluationCase>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RuntimeEvaluationCase {
    pub case_id: String,
    pub partition: RuntimeEvaluationPartition,
    pub objective: String,
    #[serde(default)]
    pub payload: serde_json::Value,
    #[serde(default)]
    pub fixture_files: BTreeMap<String, String>,
    pub assertions: Vec<RuntimeEvaluationAssertion>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TrustedEvaluationBuild {
    pub candidate_id: String,
    pub report_ref: String,
    pub report: BuildReport,
    pub completed_at: DateTime<Utc>,
}

/// Assertions remain in the trusted Supervisor and are never sent to the
/// candidate runtime worker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RuntimeEvaluationAssertion {
    VerificationPass,
    FileExists { path: String },
    FileAbsent { path: String },
    FileSha256 { path: String, checksum: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct EvaluationInput {
    pub candidate_id: String,
    pub paired_execution_refs: Vec<String>,
    pub development_verdict: GateVerdict,
    pub security_verdict: GateVerdict,
    pub migration_verdict: GateVerdict,
    pub sealed_verdict: GateVerdict,
    pub fresh_verdict: GateVerdict,
    pub quality_delta: f32,
    pub cost_delta_usd: f64,
    pub latency_delta_ms: i64,
    pub holdout_queries: u32,
    pub exact_feedback_exposed: bool,
    pub evidence_refs: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct EvaluationCampaign {
    pub campaign_id: String,
    pub candidate_id: String,
    pub baseline_release_id: Option<String>,
    pub evaluator_version: String,
    pub dataset_partition_refs: Vec<String>,
    pub disclosure_budget_ref: String,
    pub environment_digest: String,
    pub seeds: Vec<u64>,
    pub created_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
    pub sealed_verdict: GateVerdict,
    pub fresh_verdict: GateVerdict,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct GeneralizationGateResult {
    pub campaign_id: String,
    pub candidate_id: String,
    pub development_verdict: GateVerdict,
    pub sealed_verdict: GateVerdict,
    pub fresh_verdict: GateVerdict,
    pub security_verdict: GateVerdict,
    pub migration_verdict: GateVerdict,
    pub paired_execution_refs: Vec<String>,
    pub quality_delta_milli: i32,
    pub cost_delta_milli_usd: i64,
    pub latency_delta_ms: i64,
    pub verdict: GateVerdict,
    pub rejection_reasons: Vec<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CandidateArchiveEntry {
    pub candidate_id: String,
    pub lineage_parent_ids: Vec<String>,
    pub build_digest: Option<String>,
    pub capability_slice_scores: BTreeMap<String, i32>,
    pub novelty_descriptor: String,
    pub descendant_success_rate_milli: Option<i32>,
    pub improvement_cost_milli_usd: Option<i64>,
    pub rollback_rate_milli: Option<i32>,
    pub status: CandidateStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DisclosureBudget {
    pub budget_id: String,
    pub candidate_family_id: String,
    pub maximum_queries: u32,
    pub query_count: u32,
    pub aggregate_feedback_count: u32,
    pub exact_feedback_count: u32,
    pub exhausted_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DeploymentObservation {
    pub candidate_id: String,
    pub release_id: String,
    pub cohort: String,
    pub sample_count: u32,
    pub task_failure_rate_milli: i32,
    pub rollback_signal: bool,
    pub security_violation: bool,
    pub cost_delta_milli_usd: i64,
    pub latency_delta_ms: i64,
    pub observed_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SupervisorState {
    #[serde(default)]
    pub observations: Vec<ObservationBundle>,
    pub epochs: Vec<EvolutionEpoch>,
    pub opportunities: Vec<EvolutionOpportunity>,
    pub candidates: Vec<EvolutionCandidate>,
    pub campaigns: Vec<EvaluationCampaign>,
    pub gate_results: Vec<GeneralizationGateResult>,
    pub archive: Vec<CandidateArchiveEntry>,
    pub disclosure_budgets: Vec<DisclosureBudget>,
    pub deployment_observations: Vec<DeploymentObservation>,
    #[serde(default)]
    pub evaluation_builds: Vec<TrustedEvaluationBuild>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ControlEvent {
    pub sequence: u64,
    pub event_type: String,
    pub epoch_id: Option<String>,
    pub candidate_id: Option<String>,
    pub payload: serde_json::Value,
    pub at: DateTime<Utc>,
    pub previous_digest: String,
    pub digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CandidateRequest {
    pub epoch_id: String,
    pub opportunity: EvolutionOpportunity,
    pub worktree: PathBuf,
    pub source_version: String,
    pub observation_bundle_refs: Vec<String>,
}
