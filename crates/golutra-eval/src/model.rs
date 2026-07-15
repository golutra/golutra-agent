use chrono::{DateTime, Utc};
use golutra_core::{EvidenceId, TaskId, TaskStatus, TokenUsageRecord, VerificationRecord};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

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
    pub status: CandidateStatus,
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
    pub reviews: Vec<PostTaskReview>,
    #[serde(default)]
    pub benchmark_runs: Vec<BenchmarkRun>,
    #[serde(default)]
    pub counterfactual_replays: Vec<CounterfactualReplay>,
    #[serde(default)]
    pub causal_comparisons: Vec<CausalComparison>,
    pub improvement_candidates: Vec<ImprovementCandidate>,
    pub generated_tasks: Vec<GeneratedTask>,
    pub skill_candidates: Vec<SkillCandidate>,
    pub benchmark_promotions: Vec<BenchmarkPromotion>,
    pub automation_candidates: Vec<AutomationCandidate>,
    pub regressions: Vec<RegressionResult>,
    pub promotion_decisions: Vec<PromotionDecision>,
    pub applied_candidates: Vec<AppliedCandidate>,
}
