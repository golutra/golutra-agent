use golutra_core::TaskId;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct EvaluationCase {
    pub case_id: String,
    pub source: String,
    pub task_type: String,
    pub success_criteria: Vec<String>,
    pub required_evidence: Vec<String>,
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
pub struct ImprovementCandidate {
    pub id: String,
    pub source_task_id: TaskId,
    pub target_type: String,
    pub proposed_change: String,
    pub expected_effect: String,
    pub risk_level: String,
    pub rollback_plan: String,
    pub status: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct EvaluationRun {
    pub run_id: String,
    pub dataset_id: String,
    pub case_ids: Vec<String>,
    pub system_version: String,
    pub provider_config_ref: String,
    pub runtime_config_ref: String,
    pub cost: Option<f64>,
    pub latency_ms: Option<u64>,
    pub result_refs: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct EvaluationResult {
    pub run_id: String,
    pub case_id: String,
    pub verdict: String,
    pub quality_score: Option<f32>,
    pub cost: Option<f64>,
    pub latency_ms: Option<u64>,
    pub failure_taxonomy: Vec<String>,
    pub residual_risks: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct BenchmarkRun {
    pub benchmark_id: String,
    pub dataset_version: String,
    pub harness_version: String,
    pub scaffold_id: String,
    pub model_id: String,
    pub provider_id: String,
    pub tool_budget: u32,
    pub attempt_count: u32,
    pub runtime_ms: u64,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
    pub cost_usd: Option<f64>,
    pub cost_source: String,
    pub security_score: Option<f32>,
    pub utility_score: Option<f32>,
    pub artifact_delivery_status: String,
    pub score: Option<f32>,
    pub failure_taxonomy: Vec<String>,
    pub leakage_checks: Vec<String>,
    pub judge_checks: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RegressionResult {
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
    pub verdict: RegressionVerdict,
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
    pub candidate_id: String,
    pub decision: PromotionDecisionKind,
    pub reason: String,
    pub reviewer: PromotionReviewer,
    pub applied_version: Option<String>,
    pub rollback_ref: Option<String>,
    pub expires_at: Option<String>,
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
    pub source: String,
    pub objective: String,
    pub novelty_score: Option<f32>,
    pub difficulty_score: Option<f32>,
    pub expected_learning_value: String,
    pub environment_recipe: String,
    pub safety_constraints: Vec<String>,
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
    pub source_trajectory: String,
    pub reusable_pattern: String,
    pub evidence_refs: Vec<String>,
    pub regression_refs: Vec<String>,
    pub scope: String,
    pub promotion_status: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct BenchmarkPromotion {
    pub source_task_id: TaskId,
    pub failure_taxonomy: Vec<String>,
    pub fixture: String,
    pub evaluator: String,
    pub anti_overfit_notes: Vec<String>,
    pub accepted_by: Option<String>,
}

#[must_use]
pub fn replay_summary(
    source_task_id: TaskId,
    event_count: usize,
    artifact_count: usize,
) -> TrajectoryReplay {
    TrajectoryReplay {
        replay_id: format!("replay-{source_task_id}"),
        source_task_id,
        event_count,
        artifact_count,
        determinism_level: "event_artifact_replay".to_owned(),
        limitations: vec![
            "P0 replay summarizes durable facts without provider re-execution".to_owned(),
        ],
    }
}

#[must_use]
pub fn improvement_candidate_from_failure(
    source_task_id: TaskId,
    failure_summary: impl Into<String>,
) -> ImprovementCandidate {
    ImprovementCandidate {
        id: format!("candidate-{source_task_id}"),
        source_task_id,
        target_type: "runtime_rule".to_owned(),
        proposed_change: failure_summary.into(),
        expected_effect: "make the failure mode visible and testable".to_owned(),
        risk_level: "low".to_owned(),
        rollback_plan: "remove the proposed runtime rule or fixture".to_owned(),
        status: "proposed".to_owned(),
    }
}

#[must_use]
pub fn benchmark_run_has_required_metadata(run: &BenchmarkRun) -> bool {
    !run.dataset_version.is_empty()
        && !run.harness_version.is_empty()
        && !run.scaffold_id.is_empty()
        && !run.model_id.is_empty()
        && !run.provider_id.is_empty()
        && run.tool_budget > 0
        && run.attempt_count > 0
        && run.total_tokens.is_some()
        && run.runtime_ms > 0
        && !run.cost_source.is_empty()
}

#[must_use]
pub fn decide_low_risk_promotion(
    candidate: &ImprovementCandidate,
    regression: &RegressionResult,
) -> PromotionDecision {
    let auto_approvable = candidate.risk_level == "low"
        && candidate.status == "proposed"
        && regression.verdict == RegressionVerdict::Pass
        && regression.failed_cases == 0
        && regression.regressions.is_empty()
        && !candidate.rollback_plan.is_empty();

    if auto_approvable {
        PromotionDecision {
            candidate_id: candidate.id.clone(),
            decision: PromotionDecisionKind::Approve,
            reason: "low risk candidate passed regression without failures".to_owned(),
            reviewer: PromotionReviewer::System,
            applied_version: Some(regression.candidate_version.clone()),
            rollback_ref: Some(candidate.rollback_plan.clone()),
            expires_at: None,
        }
    } else if regression.verdict == RegressionVerdict::Fail || !regression.regressions.is_empty() {
        PromotionDecision {
            candidate_id: candidate.id.clone(),
            decision: PromotionDecisionKind::Reject,
            reason: "regression failed or reported regressions".to_owned(),
            reviewer: PromotionReviewer::System,
            applied_version: None,
            rollback_ref: Some(candidate.rollback_plan.clone()),
            expires_at: None,
        }
    } else {
        PromotionDecision {
            candidate_id: candidate.id.clone(),
            decision: PromotionDecisionKind::NeedsHumanReview,
            reason: "candidate is outside the low risk automatic promotion boundary".to_owned(),
            reviewer: PromotionReviewer::System,
            applied_version: None,
            rollback_ref: Some(candidate.rollback_plan.clone()),
            expires_at: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_replay_summary_and_improvement_candidate() {
        let task_id = TaskId::new();
        let replay = replay_summary(task_id, 3, 1);
        let candidate = improvement_candidate_from_failure(task_id, "add regression case");

        assert_eq!(replay.source_task_id, task_id);
        assert_eq!(candidate.status, "proposed");
    }

    #[test]
    fn benchmark_metadata_gate_requires_cost_and_runtime_context() {
        let run = BenchmarkRun {
            benchmark_id: "bench".to_owned(),
            dataset_version: "v1".to_owned(),
            harness_version: "h1".to_owned(),
            scaffold_id: "runtime".to_owned(),
            model_id: "mock-model".to_owned(),
            provider_id: "mock".to_owned(),
            tool_budget: 4,
            attempt_count: 1,
            runtime_ms: 10,
            input_tokens: Some(10),
            output_tokens: Some(5),
            total_tokens: Some(15),
            cost_usd: Some(0.0),
            cost_source: "provider".to_owned(),
            security_score: Some(1.0),
            utility_score: Some(1.0),
            artifact_delivery_status: "delivered".to_owned(),
            score: Some(1.0),
            failure_taxonomy: Vec::new(),
            leakage_checks: vec!["no_answer_leakage".to_owned()],
            judge_checks: vec!["evidence_backed".to_owned()],
        };

        assert!(benchmark_run_has_required_metadata(&run));
    }

    #[test]
    fn low_risk_candidate_can_be_promoted_after_clean_regression() {
        let task_id = TaskId::new();
        let candidate = improvement_candidate_from_failure(task_id, "add regression case");
        let regression = RegressionResult {
            candidate_id: candidate.id.clone(),
            baseline_version: "base".to_owned(),
            candidate_version: "candidate".to_owned(),
            cases_run: 3,
            passed_cases: 3,
            failed_cases: 0,
            regressions: Vec::new(),
            cost_delta: Some(0.0),
            latency_delta: Some(0),
            quality_delta: Some(0.1),
            security_delta: Some(0.0),
            causal_comparison_refs: vec!["replay-1".to_owned()],
            verdict: RegressionVerdict::Pass,
        };

        let decision = decide_low_risk_promotion(&candidate, &regression);

        assert_eq!(decision.decision, PromotionDecisionKind::Approve);
        assert_eq!(decision.reviewer, PromotionReviewer::System);
    }

    #[test]
    fn open_endedness_records_keep_runtime_boundaries_explicit() {
        let task = GeneratedTask {
            id: "generated-1".to_owned(),
            source: "near_miss".to_owned(),
            objective: "cover missing verification edge".to_owned(),
            novelty_score: Some(0.8),
            difficulty_score: Some(0.4),
            expected_learning_value: "improve verification coverage".to_owned(),
            environment_recipe: "fixture-only".to_owned(),
            safety_constraints: vec!["no_external_side_effects".to_owned()],
        };
        let item = CurriculumItem {
            task_id: task.id.clone(),
            selected: true,
            selected_reason: Some("near frontier".to_owned()),
            rejected_reason: None,
            frontier_ref: Some("frontier-1".to_owned()),
        };

        assert!(item.selected);
        assert_eq!(task.safety_constraints, vec!["no_external_side_effects"]);
    }
}
