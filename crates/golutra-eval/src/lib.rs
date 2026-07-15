use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use chrono::{DateTime, Utc};
use fs2::FileExt;
use golutra_core::{EvidenceId, TaskId, TaskStatus, VerificationRecord, VerificationResult};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

const MAX_EVALUATION_STATE_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EvaluationVerdict {
    Pass,
    Fail,
    Partial,
    Unknown,
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
    pub latency_ms: Option<u64>,
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
    pub mode: String,
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
}

#[derive(Debug, Default, Clone, Copy)]
pub struct EvaluationRunner;

impl EvaluationRunner {
    #[must_use]
    pub fn evaluate_task(&self, input: TaskEvaluationInput) -> TaskEvaluationBundle {
        let now = Utc::now();
        let case_id = format!("case-{}", input.task_id);
        let run_id = format!("run-{}", Uuid::now_v7());
        let evidence_refs = input
            .verification
            .as_ref()
            .map(|record| record.evidence_refs.clone())
            .unwrap_or_default();
        let verification_result = input.verification.as_ref().map(|record| record.result);
        let verdict = evaluation_verdict(input.task_status, verification_result);
        let failure_taxonomy = failure_taxonomy(&input, verdict);
        let residual_risks = input
            .verification
            .as_ref()
            .map(|record| record.residual_risks.clone())
            .unwrap_or_else(|| {
                input
                    .failure_summary
                    .as_deref()
                    .map(sanitize_text)
                    .into_iter()
                    .collect()
            });
        let result_id = format!("result-{}", Uuid::now_v7());
        let case = EvaluationCase {
            case_id: case_id.clone(),
            source: "live_task".to_owned(),
            source_task_id: Some(input.task_id),
            task_type: if input.tool_count > 0 {
                "workspace_task".to_owned()
            } else {
                "conversation".to_owned()
            },
            objective: sanitize_text(&input.objective),
            expected_outcome: "verification-backed terminal result".to_owned(),
            success_criteria: input
                .verification
                .as_ref()
                .map(|record| record.completion_criteria.clone())
                .unwrap_or_else(|| vec!["task reaches an explainable terminal state".to_owned()]),
            required_evidence: evidence_refs.iter().map(ToString::to_string).collect(),
            policy_constraints: vec!["no unapproved high-risk side effects".to_owned()],
            fixture_refs: vec![format!("replay-{}", input.task_id)],
            tags: failure_taxonomy.clone(),
        };
        let result = EvaluationResult {
            result_id: result_id.clone(),
            run_id: run_id.clone(),
            case_id: case_id.clone(),
            source_task_id: input.task_id,
            verdict,
            quality_score: Some(match verdict {
                EvaluationVerdict::Pass => 1.0,
                EvaluationVerdict::Partial => 0.5,
                EvaluationVerdict::Fail => 0.0,
                EvaluationVerdict::Unknown => 0.25,
            }),
            cost: None,
            latency_ms: input.latency_ms,
            evidence_refs: evidence_refs.clone(),
            failure_taxonomy: failure_taxonomy.clone(),
            residual_risks: residual_risks.clone(),
        };
        let run = EvaluationRun {
            run_id,
            dataset_id: "workspace-history".to_owned(),
            case_ids: vec![case_id],
            system_version: env!("CARGO_PKG_VERSION").to_owned(),
            provider_config_ref: "runtime-active-profile".to_owned(),
            runtime_config_ref: "workspace-runtime-host".to_owned(),
            started_at: now,
            completed_at: Utc::now(),
            cost: None,
            latency_ms: input.latency_ms,
            result_refs: vec![result_id],
        };
        let replay = replay_summary(input.task_id, input.event_count, input.artifact_count);
        let successful = verdict == EvaluationVerdict::Pass;
        let improvement_candidate = (!successful).then(|| {
            improvement_candidate_from_failure(
                input.task_id,
                evidence_refs.clone(),
                failure_taxonomy.clone(),
                input
                    .failure_summary
                    .as_deref()
                    .unwrap_or("capture the failed trajectory as a regression case"),
            )
        });
        let generated_task = (!successful).then(|| GeneratedTask {
            id: format!("generated-task-{}", input.task_id),
            source_task_id: input.task_id,
            source: "failed_trajectory".to_owned(),
            objective: format!("reproduce and resolve: {}", sanitize_text(&input.objective)),
            novelty_score: None,
            difficulty_score: None,
            expected_learning_value: "turn a production failure into a repeatable regression"
                .to_owned(),
            environment_recipe: format!("fixture://task/{}", input.task_id),
            safety_constraints: vec![
                "fixture_only".to_owned(),
                "no_external_side_effects".to_owned(),
            ],
            promotion_status: CandidateStatus::Proposed,
        });
        let benchmark_promotion = (!successful).then(|| BenchmarkPromotion {
            id: format!("benchmark-{}", input.task_id),
            source_task_id: input.task_id,
            failure_taxonomy: failure_taxonomy.clone(),
            fixture: format!("replay-{}", input.task_id),
            evaluator: "durable_verification_replay".to_owned(),
            anti_overfit_notes: vec![
                "fixture contains runtime facts and evidence references, not a target answer"
                    .to_owned(),
            ],
            rollback_ref: format!("remove-benchmark-{}", input.task_id),
            promotion_status: CandidateStatus::Proposed,
            accepted_by: None,
        });
        let skill_candidate = (successful && !evidence_refs.is_empty()).then(|| SkillCandidate {
            id: format!("skill-{}", input.task_id),
            source_task_id: input.task_id,
            source_trajectory: format!("replay-{}", input.task_id),
            reusable_pattern: sanitize_text(&input.objective),
            evidence_refs: evidence_refs.clone(),
            regression_refs: Vec::new(),
            scope: "project".to_owned(),
            rollback_ref: format!("remove-skill-{}", input.task_id),
            promotion_status: CandidateStatus::Proposed,
        });
        let mut automation_candidates = Vec::new();
        if benchmark_promotion.is_some() {
            automation_candidates.push(AutomationCandidate {
                id: format!("automation-benchmark-{}", input.task_id),
                source_task_id: input.task_id,
                kind: AutomationCandidateKind::Benchmark,
                summary: "promote failed trajectory to the workspace regression dataset".to_owned(),
                risk_level: CandidateRisk::Low,
                evidence_refs: evidence_refs.clone(),
                regression_plan: format!("replay source task {}", input.task_id),
                rollback_ref: format!("remove-benchmark-{}", input.task_id),
                status: CandidateStatus::Proposed,
            });
            automation_candidates.push(AutomationCandidate {
                id: format!("automation-generated-task-{}", input.task_id),
                source_task_id: input.task_id,
                kind: AutomationCandidateKind::GeneratedTask,
                summary: "add a fixture-only task to the capability frontier".to_owned(),
                risk_level: CandidateRisk::Medium,
                evidence_refs: evidence_refs.clone(),
                regression_plan: format!("run generated fixture for task {}", input.task_id),
                rollback_ref: format!("remove-generated-task-{}", input.task_id),
                status: CandidateStatus::Proposed,
            });
        }
        if skill_candidate.is_some() {
            automation_candidates.push(AutomationCandidate {
                id: format!("automation-skill-{}", input.task_id),
                source_task_id: input.task_id,
                kind: AutomationCandidateKind::Skill,
                summary: "extract a project-scoped skill from a successful trajectory".to_owned(),
                risk_level: CandidateRisk::Medium,
                evidence_refs: evidence_refs.clone(),
                regression_plan: format!("replay successful task {}", input.task_id),
                rollback_ref: format!("remove-skill-{}", input.task_id),
                status: CandidateStatus::Proposed,
            });
        }
        let promotion_candidates = automation_candidates
            .iter()
            .map(|candidate| candidate.id.clone())
            .collect();
        let review = PostTaskReview {
            task_id: input.task_id,
            mode: "deep".to_owned(),
            outcome: format!("{verdict:?}").to_lowercase(),
            success_reasons: if successful {
                vec!["terminal verification passed".to_owned()]
            } else {
                Vec::new()
            },
            failure_reasons: if successful {
                Vec::new()
            } else if residual_risks.is_empty() {
                failure_taxonomy.clone()
            } else {
                residual_risks.clone()
            },
            evidence_quality: if evidence_refs.is_empty() {
                "none".to_owned()
            } else {
                "durable".to_owned()
            },
            policy_issues: taxonomy_matches(&failure_taxonomy, "PolicyFailure"),
            context_issues: taxonomy_matches(&failure_taxonomy, "ContextFailure"),
            tool_issues: taxonomy_matches(&failure_taxonomy, "ToolFailure"),
            provider_issues: taxonomy_matches(&failure_taxonomy, "ProviderFailure"),
            suggested_improvements: if successful {
                Vec::new()
            } else {
                vec!["run the generated regression fixture before promotion".to_owned()]
            },
            promotion_candidates,
        };

        TaskEvaluationBundle {
            case,
            run,
            result,
            replay,
            review,
            improvement_candidate,
            generated_task,
            skill_candidate,
            benchmark_promotion,
            automation_candidates,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct EvaluationState {
    pub cases: Vec<EvaluationCase>,
    pub runs: Vec<EvaluationRun>,
    pub results: Vec<EvaluationResult>,
    pub replays: Vec<TrajectoryReplay>,
    pub reviews: Vec<PostTaskReview>,
    pub improvement_candidates: Vec<ImprovementCandidate>,
    pub generated_tasks: Vec<GeneratedTask>,
    pub skill_candidates: Vec<SkillCandidate>,
    pub benchmark_promotions: Vec<BenchmarkPromotion>,
    pub automation_candidates: Vec<AutomationCandidate>,
    pub regressions: Vec<RegressionResult>,
    pub promotion_decisions: Vec<PromotionDecision>,
    pub applied_candidates: Vec<AppliedCandidate>,
}

#[derive(Debug, Default)]
struct EvaluationStoreState {
    loaded: bool,
    data: EvaluationState,
}

#[derive(Debug, Clone)]
pub struct EvaluationStore {
    path: Option<PathBuf>,
    state: Arc<Mutex<EvaluationStoreState>>,
}

#[derive(Debug, Error)]
pub enum EvaluationError {
    #[error("evaluation store IO failed: {0}")]
    Io(String),
    #[error("evaluation store JSON failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("evaluation store lock is poisoned")]
    LockPoisoned,
    #[error("evaluation candidate not found: {0}")]
    CandidateNotFound(String),
    #[error("evaluation candidate requires a clean regression: {0}")]
    RegressionRequired(String),
    #[error("evaluation candidate requires an approving promotion decision: {0}")]
    PromotionRequired(String),
    #[error("evaluation candidate cannot be applied automatically: {0}")]
    UnsupportedAutomaticApplication(String),
    #[error("evaluation candidate is not applied: {0}")]
    CandidateNotApplied(String),
    #[error("evaluation candidate {candidate_id} cannot transition from {status:?} to {action}")]
    InvalidCandidateState {
        candidate_id: String,
        status: CandidateStatus,
        action: String,
    },
    #[error("evaluation store invariant failed: {0}")]
    Invariant(String),
    #[error("evaluation store limit exceeded: {0}")]
    Limit(String),
}

impl EvaluationStore {
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: Some(path.into()),
            state: Arc::new(Mutex::new(EvaluationStoreState::default())),
        }
    }

    #[must_use]
    pub fn in_memory() -> Self {
        Self {
            path: None,
            state: Arc::new(Mutex::new(EvaluationStoreState::default())),
        }
    }

    pub fn snapshot(&self) -> Result<EvaluationState, EvaluationError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| EvaluationError::LockPoisoned)?;
        let _file_lock = self.acquire_file_lock()?;
        self.ensure_loaded(&mut state)?;
        Ok(state.data.clone())
    }

    pub fn record_task_evaluation(
        &self,
        bundle: TaskEvaluationBundle,
    ) -> Result<(), EvaluationError> {
        self.update(|state| {
            replace_by(&mut state.cases, bundle.case, |value| value.case_id.clone());
            replace_by(&mut state.runs, bundle.run, |value| value.run_id.clone());
            replace_by(&mut state.results, bundle.result, |value| {
                value.result_id.clone()
            });
            replace_by(&mut state.replays, bundle.replay, |value| {
                value.replay_id.clone()
            });
            replace_by(&mut state.reviews, bundle.review, |value| value.task_id);
            if let Some(candidate) = bundle.improvement_candidate {
                replace_by(&mut state.improvement_candidates, candidate, |value| {
                    value.id.clone()
                });
            }
            if let Some(task) = bundle.generated_task {
                replace_by(&mut state.generated_tasks, task, |value| value.id.clone());
            }
            if let Some(skill) = bundle.skill_candidate {
                replace_by(&mut state.skill_candidates, skill, |value| value.id.clone());
            }
            if let Some(benchmark) = bundle.benchmark_promotion {
                replace_by(&mut state.benchmark_promotions, benchmark, |value| {
                    value.id.clone()
                });
            }
            for candidate in bundle.automation_candidates {
                replace_by(&mut state.automation_candidates, candidate, |value| {
                    value.id.clone()
                });
            }
            Ok(())
        })
    }

    pub fn run_regression(&self, candidate_id: &str) -> Result<RegressionResult, EvaluationError> {
        self.update(|state| {
            let candidate = state
                .automation_candidates
                .iter()
                .find(|candidate| candidate.id == candidate_id)
                .cloned()
                .ok_or_else(|| EvaluationError::CandidateNotFound(candidate_id.to_owned()))?;
            if candidate.status != CandidateStatus::Proposed {
                return Err(EvaluationError::InvalidCandidateState {
                    candidate_id: candidate.id,
                    status: candidate.status,
                    action: "run regression".to_owned(),
                });
            }
            let mut regressions = Vec::new();
            if candidate.evidence_refs.is_empty() {
                regressions.push("candidate has no durable evidence".to_owned());
            }
            if candidate.rollback_ref.trim().is_empty() {
                regressions.push("candidate has no rollback reference".to_owned());
            }
            let source_result = state
                .results
                .iter()
                .find(|result| result.source_task_id == candidate.source_task_id);
            if source_result.is_none() {
                regressions.push("source trajectory has no durable evaluation result".to_owned());
            }
            if candidate.kind == AutomationCandidateKind::Benchmark {
                match state
                    .benchmark_promotions
                    .iter()
                    .find(|benchmark| benchmark.source_task_id == candidate.source_task_id)
                {
                    Some(benchmark)
                        if !benchmark.fixture.is_empty()
                            && !benchmark.evaluator.is_empty()
                            && source_result.is_some_and(|result| {
                                result.failure_taxonomy == benchmark.failure_taxonomy
                            }) => {}
                    _ => regressions.push(
                        "benchmark fixture does not replay the source failure taxonomy".to_owned(),
                    ),
                }
            }
            let passed = regressions.is_empty();
            let regression = RegressionResult {
                regression_id: format!("regression-{}", Uuid::now_v7()),
                candidate_id: candidate.id.clone(),
                baseline_version: env!("CARGO_PKG_VERSION").to_owned(),
                candidate_version: format!("candidate-{}", candidate.id),
                cases_run: 1,
                passed_cases: u32::from(passed),
                failed_cases: u32::from(!passed),
                regressions,
                cost_delta: Some(0.0),
                latency_delta: Some(0),
                quality_delta: Some(0.0),
                security_delta: Some(0.0),
                causal_comparison_refs: vec![format!("replay-{}", candidate.source_task_id)],
                verdict: if passed {
                    RegressionVerdict::Pass
                } else {
                    RegressionVerdict::Fail
                },
                created_at: Utc::now(),
            };
            let stored_candidate = state
                .automation_candidates
                .iter_mut()
                .find(|stored| stored.id == candidate.id)
                .ok_or_else(|| {
                    EvaluationError::Invariant(format!(
                        "candidate {} disappeared during regression",
                        candidate.id
                    ))
                })?;
            stored_candidate.status = if passed {
                CandidateStatus::RegressionPassed
            } else {
                CandidateStatus::Rejected
            };
            replace_by(&mut state.regressions, regression.clone(), |value| {
                value.regression_id.clone()
            });
            Ok(regression)
        })
    }

    pub fn decide_promotion(
        &self,
        candidate_id: &str,
    ) -> Result<PromotionDecision, EvaluationError> {
        self.update(|state| {
            let candidate = state
                .automation_candidates
                .iter()
                .find(|candidate| candidate.id == candidate_id)
                .cloned()
                .ok_or_else(|| EvaluationError::CandidateNotFound(candidate_id.to_owned()))?;
            if candidate.status != CandidateStatus::RegressionPassed {
                return Err(EvaluationError::InvalidCandidateState {
                    candidate_id: candidate.id,
                    status: candidate.status,
                    action: "decide promotion".to_owned(),
                });
            }
            let regression = state
                .regressions
                .iter()
                .rev()
                .find(|regression| regression.candidate_id == candidate_id)
                .ok_or_else(|| EvaluationError::RegressionRequired(candidate_id.to_owned()))?;
            let decision = decide_low_risk_promotion(&candidate, regression);
            let stored_candidate = state
                .automation_candidates
                .iter_mut()
                .find(|stored| stored.id == candidate.id)
                .ok_or_else(|| {
                    EvaluationError::Invariant(format!(
                        "candidate {} disappeared during promotion",
                        candidate.id
                    ))
                })?;
            stored_candidate.status = match decision.decision {
                PromotionDecisionKind::Approve => CandidateStatus::Approved,
                PromotionDecisionKind::Reject => CandidateStatus::Rejected,
                PromotionDecisionKind::NeedsHumanReview => CandidateStatus::NeedsHumanReview,
            };
            replace_by(&mut state.promotion_decisions, decision.clone(), |value| {
                value.decision_id.clone()
            });
            Ok(decision)
        })
    }

    pub fn apply_candidate(&self, candidate_id: &str) -> Result<AppliedCandidate, EvaluationError> {
        self.update(|state| {
            let candidate = state
                .automation_candidates
                .iter()
                .find(|candidate| candidate.id == candidate_id)
                .cloned()
                .ok_or_else(|| EvaluationError::CandidateNotFound(candidate_id.to_owned()))?;
            let approved = state.promotion_decisions.iter().rev().any(|decision| {
                decision.candidate_id == candidate_id
                    && decision.decision == PromotionDecisionKind::Approve
            });
            if !approved || candidate.status != CandidateStatus::Approved {
                return Err(EvaluationError::PromotionRequired(candidate_id.to_owned()));
            }
            if candidate.kind != AutomationCandidateKind::Benchmark {
                return Err(EvaluationError::UnsupportedAutomaticApplication(
                    candidate_id.to_owned(),
                ));
            }
            let benchmark = state
                .benchmark_promotions
                .iter_mut()
                .find(|benchmark| benchmark.source_task_id == candidate.source_task_id)
                .ok_or_else(|| EvaluationError::CandidateNotFound(candidate_id.to_owned()))?;
            benchmark.promotion_status = CandidateStatus::Applied;
            benchmark.accepted_by = Some("system".to_owned());
            let applied = AppliedCandidate {
                candidate_id: candidate.id.clone(),
                applied_version: format!("benchmark-dataset-{}", Uuid::now_v7()),
                rollback_ref: candidate.rollback_ref.clone(),
                applied_at: Utc::now(),
                rolled_back_at: None,
                rollback_reason: None,
            };
            state
                .automation_candidates
                .iter_mut()
                .find(|stored| stored.id == candidate.id)
                .ok_or_else(|| {
                    EvaluationError::Invariant(format!(
                        "candidate {} disappeared during apply",
                        candidate.id
                    ))
                })?
                .status = CandidateStatus::Applied;
            replace_by(&mut state.applied_candidates, applied.clone(), |value| {
                value.candidate_id.clone()
            });
            Ok(applied)
        })
    }

    pub fn rollback_candidate(
        &self,
        candidate_id: &str,
        reason: impl Into<String>,
    ) -> Result<AppliedCandidate, EvaluationError> {
        let reason = reason.into();
        self.update(|state| {
            let candidate_status = state
                .automation_candidates
                .iter()
                .find(|candidate| candidate.id == candidate_id)
                .map(|candidate| candidate.status)
                .ok_or_else(|| EvaluationError::CandidateNotFound(candidate_id.to_owned()))?;
            if candidate_status != CandidateStatus::Applied {
                return Err(EvaluationError::InvalidCandidateState {
                    candidate_id: candidate_id.to_owned(),
                    status: candidate_status,
                    action: "rollback".to_owned(),
                });
            }
            let applied = state
                .applied_candidates
                .iter_mut()
                .find(|applied| applied.candidate_id == candidate_id)
                .ok_or_else(|| EvaluationError::CandidateNotApplied(candidate_id.to_owned()))?;
            applied.rolled_back_at = Some(Utc::now());
            applied.rollback_reason = Some(reason.clone());
            let rolled_back = applied.clone();
            if let Some(candidate) = state
                .automation_candidates
                .iter_mut()
                .find(|candidate| candidate.id == candidate_id)
            {
                candidate.status = CandidateStatus::RolledBack;
            }
            if let Some(benchmark) = state.benchmark_promotions.iter_mut().find(|benchmark| {
                state
                    .automation_candidates
                    .iter()
                    .find(|candidate| candidate.id == candidate_id)
                    .is_some_and(|candidate| candidate.source_task_id == benchmark.source_task_id)
            }) {
                benchmark.promotion_status = CandidateStatus::RolledBack;
                benchmark.accepted_by = None;
            }
            Ok(rolled_back)
        })
    }

    fn update<T>(
        &self,
        operation: impl FnOnce(&mut EvaluationState) -> Result<T, EvaluationError>,
    ) -> Result<T, EvaluationError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| EvaluationError::LockPoisoned)?;
        let _file_lock = self.acquire_file_lock()?;
        self.ensure_loaded(&mut state)?;
        let mut next = state.data.clone();
        let result = operation(&mut next)?;
        self.save(&next)?;
        state.data = next;
        Ok(result)
    }

    fn ensure_loaded(&self, state: &mut EvaluationStoreState) -> Result<(), EvaluationError> {
        if self.path.is_none() && state.loaded {
            return Ok(());
        }
        state.data = match &self.path {
            Some(path) => match read_bounded_evaluation_file(path)? {
                Some(bytes) => serde_json::from_slice(&bytes)?,
                None => EvaluationState::default(),
            },
            None => EvaluationState::default(),
        };
        state.loaded = true;
        Ok(())
    }

    fn acquire_file_lock(&self) -> Result<Option<File>, EvaluationError> {
        let Some(path) = &self.path else {
            return Ok(None);
        };
        let parent = path.parent().ok_or_else(|| {
            EvaluationError::Io(format!("evaluation path has no parent: {}", path.display()))
        })?;
        fs::create_dir_all(parent).map_err(|error| EvaluationError::Io(error.to_string()))?;
        set_owner_only_evaluation_dir(parent)?;
        let lock_path = path.with_extension("lock");
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)
            .map_err(|error| EvaluationError::Io(error.to_string()))?;
        set_owner_only_evaluation_file(&lock_path)?;
        file.lock_exclusive()
            .map_err(|error| EvaluationError::Io(error.to_string()))?;
        Ok(Some(file))
    }

    fn save(&self, data: &EvaluationState) -> Result<(), EvaluationError> {
        let encoded = serde_json::to_vec_pretty(data)?;
        if u64::try_from(encoded.len()).unwrap_or(u64::MAX) > MAX_EVALUATION_STATE_BYTES {
            return Err(EvaluationError::Limit(format!(
                "serialized state exceeds {MAX_EVALUATION_STATE_BYTES} bytes"
            )));
        }
        let Some(path) = &self.path else {
            return Ok(());
        };
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|error| EvaluationError::Io(error.to_string()))?;
            set_owner_only_evaluation_dir(parent)?;
        }
        let temporary = temporary_path(path);
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&temporary)
            .map_err(|error| EvaluationError::Io(error.to_string()))?;
        file.write_all(&encoded)
            .map_err(|error| EvaluationError::Io(error.to_string()))?;
        file.sync_all()
            .map_err(|error| EvaluationError::Io(error.to_string()))?;
        set_owner_only_evaluation_file(&temporary)?;
        fs::rename(&temporary, path).map_err(|error| EvaluationError::Io(error.to_string()))?;
        set_owner_only_evaluation_file(path)?;
        File::open(path)
            .and_then(|file| file.sync_all())
            .map_err(|error| EvaluationError::Io(error.to_string()))?;
        if let Some(parent) = path.parent() {
            sync_evaluation_directory(parent)?;
        }
        Ok(())
    }
}

fn read_bounded_evaluation_file(path: &Path) -> Result<Option<Vec<u8>>, EvaluationError> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(EvaluationError::Io(error.to_string())),
    };
    let metadata = file
        .metadata()
        .map_err(|error| EvaluationError::Io(error.to_string()))?;
    if metadata.len() > MAX_EVALUATION_STATE_BYTES {
        return Err(EvaluationError::Limit(format!(
            "{} exceeds {MAX_EVALUATION_STATE_BYTES} bytes",
            path.display()
        )));
    }
    let mut bytes = Vec::new();
    file.take(MAX_EVALUATION_STATE_BYTES.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| EvaluationError::Io(error.to_string()))?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_EVALUATION_STATE_BYTES {
        return Err(EvaluationError::Limit(format!(
            "{} grew beyond {MAX_EVALUATION_STATE_BYTES} bytes while reading",
            path.display()
        )));
    }
    Ok(Some(bytes))
}

#[cfg(unix)]
fn sync_evaluation_directory(path: &Path) -> Result<(), EvaluationError> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| EvaluationError::Io(error.to_string()))
}

#[cfg(not(unix))]
fn sync_evaluation_directory(_path: &Path) -> Result<(), EvaluationError> {
    Ok(())
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
            "replay uses durable runtime facts without re-executing the provider".to_owned(),
        ],
    }
}

#[must_use]
pub fn improvement_candidate_from_failure(
    source_task_id: TaskId,
    evidence_refs: Vec<EvidenceId>,
    source_failure_ids: Vec<String>,
    failure_summary: impl Into<String>,
) -> ImprovementCandidate {
    ImprovementCandidate {
        id: format!("candidate-{source_task_id}"),
        source_task_id,
        source_failure_ids,
        target_type: "benchmark_case".to_owned(),
        target_id: None,
        proposed_change: sanitize_text(&failure_summary.into()),
        expected_effect: "make the failure mode replayable and regression-tested".to_owned(),
        risk_level: CandidateRisk::Low,
        evidence_refs,
        causal_evidence_refs: vec![format!("replay-{source_task_id}")],
        benchmark_refs: vec![format!("benchmark-{source_task_id}")],
        rollback_plan: format!("remove-benchmark-{source_task_id}"),
        status: CandidateStatus::Proposed,
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
    candidate: &AutomationCandidate,
    regression: &RegressionResult,
) -> PromotionDecision {
    let clean_regression = regression.verdict == RegressionVerdict::Pass
        && regression.failed_cases == 0
        && regression.regressions.is_empty();
    let (decision, reason) = if candidate.status != CandidateStatus::RegressionPassed {
        (
            PromotionDecisionKind::Reject,
            "candidate has not reached the regression-passed state",
        )
    } else if !clean_regression {
        (
            PromotionDecisionKind::Reject,
            "regression failed or reported regressions",
        )
    } else if candidate.risk_level != CandidateRisk::Low
        || candidate.kind != AutomationCandidateKind::Benchmark
    {
        (
            PromotionDecisionKind::NeedsHumanReview,
            "candidate is outside the low-risk benchmark automation boundary",
        )
    } else if candidate.rollback_ref.trim().is_empty() || candidate.evidence_refs.is_empty() {
        (
            PromotionDecisionKind::Reject,
            "candidate lacks durable evidence or a rollback reference",
        )
    } else {
        (
            PromotionDecisionKind::Approve,
            "low-risk benchmark candidate passed clean regression",
        )
    };
    PromotionDecision {
        decision_id: format!("promotion-{}", Uuid::now_v7()),
        candidate_id: candidate.id.clone(),
        decision,
        reason: reason.to_owned(),
        reviewer: PromotionReviewer::System,
        applied_version: (decision == PromotionDecisionKind::Approve)
            .then(|| regression.candidate_version.clone()),
        rollback_ref: Some(candidate.rollback_ref.clone()),
        expires_at: None,
        created_at: Utc::now(),
    }
}

fn evaluation_verdict(
    task_status: TaskStatus,
    verification: Option<VerificationResult>,
) -> EvaluationVerdict {
    match (task_status, verification) {
        (TaskStatus::Completed, Some(VerificationResult::Pass)) => EvaluationVerdict::Pass,
        (TaskStatus::Partial, _) | (_, Some(VerificationResult::Partial)) => {
            EvaluationVerdict::Partial
        }
        (TaskStatus::Failed | TaskStatus::Cancelled, _) | (_, Some(VerificationResult::Fail)) => {
            EvaluationVerdict::Fail
        }
        _ => EvaluationVerdict::Unknown,
    }
}

fn failure_taxonomy(input: &TaskEvaluationInput, verdict: EvaluationVerdict) -> Vec<String> {
    if verdict == EvaluationVerdict::Pass {
        return Vec::new();
    }
    let summary = input
        .failure_summary
        .as_deref()
        .unwrap_or_default()
        .to_ascii_lowercase();
    let mut taxonomy = Vec::new();
    if summary.contains("provider") {
        taxonomy.push("ProviderFailure".to_owned());
    }
    if summary.contains("tool") {
        taxonomy.push("ToolFailure".to_owned());
    }
    if summary.contains("policy") || summary.contains("approval") {
        taxonomy.push("PolicyFailure".to_owned());
    }
    if summary.contains("context") || summary.contains("token") {
        taxonomy.push("ContextFailure".to_owned());
    }
    if input
        .verification
        .as_ref()
        .is_some_and(|record| record.result != VerificationResult::Pass)
    {
        taxonomy.push("VerificationFailure".to_owned());
    }
    if taxonomy.is_empty() {
        taxonomy.push(
            match input.task_status {
                TaskStatus::Cancelled => "HumanInteractionFailure",
                TaskStatus::Blocked => "StateFailure",
                _ => "GoalFailure",
            }
            .to_owned(),
        );
    }
    taxonomy.sort();
    taxonomy.dedup();
    taxonomy
}

fn taxonomy_matches(taxonomy: &[String], expected: &str) -> Vec<String> {
    taxonomy
        .iter()
        .filter(|value| value.as_str() == expected)
        .cloned()
        .collect()
}

fn sanitize_text(value: &str) -> String {
    let mut redacted = value.to_owned();
    for prefix in ["github_pat_", "ghp_", "sk-"] {
        redacted = redact_prefixed_secret(&redacted, prefix);
    }
    redacted.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn redact_prefixed_secret(value: &str, prefix: &str) -> String {
    let mut output = value.to_owned();
    let mut search_from = 0;
    loop {
        let lower = output.to_ascii_lowercase();
        let Some(relative_start) = lower[search_from..].find(prefix) else {
            break;
        };
        let start = search_from + relative_start;
        let end = output[start..]
            .char_indices()
            .take_while(|(_, character)| {
                character.is_ascii_alphanumeric() || matches!(character, '-' | '_')
            })
            .map(|(offset, character)| start + offset + character.len_utf8())
            .last()
            .unwrap_or(start);
        if end.saturating_sub(start) < 12 {
            search_from = end.max(start + prefix.len());
            continue;
        }
        output.replace_range(start..end, "[REDACTED]");
        search_from = start + "[REDACTED]".len();
    }
    output
}

fn temporary_path(path: &Path) -> PathBuf {
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| format!("{extension}.tmp"))
        .unwrap_or_else(|| "tmp".to_owned());
    path.with_extension(extension)
}

#[cfg(unix)]
fn set_owner_only_evaluation_dir(path: &Path) -> Result<(), EvaluationError> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(|error| EvaluationError::Io(error.to_string()))
}

#[cfg(not(unix))]
fn set_owner_only_evaluation_dir(_path: &Path) -> Result<(), EvaluationError> {
    Ok(())
}

#[cfg(unix)]
fn set_owner_only_evaluation_file(path: &Path) -> Result<(), EvaluationError> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .map_err(|error| EvaluationError::Io(error.to_string()))
}

#[cfg(not(unix))]
fn set_owner_only_evaluation_file(_path: &Path) -> Result<(), EvaluationError> {
    Ok(())
}

fn replace_by<T, K: PartialEq>(values: &mut Vec<T>, value: T, key: impl Fn(&T) -> K) {
    let value_key = key(&value);
    if let Some(existing) = values
        .iter_mut()
        .find(|existing| key(existing) == value_key)
    {
        *existing = value;
    } else {
        values.push(value);
    }
}

#[cfg(test)]
mod tests;
