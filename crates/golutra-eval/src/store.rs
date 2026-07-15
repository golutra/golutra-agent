use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use chrono::Utc;
use fs2::FileExt;
use thiserror::Error;
use uuid::Uuid;

use crate::runner::{
    benchmark_check, benchmark_run_has_required_metadata, decide_low_risk_promotion, sanitize_text,
};
use crate::{
    AppliedCandidate, AutomationCandidate, AutomationCandidateKind, BenchmarkCheckStatus,
    BenchmarkRun, BenchmarkSuiteKind, CandidateStatus, CausalComparison, CounterfactualReplay,
    EvaluationState, EvaluationVerdict, PromotionDecision, PromotionDecisionKind,
    PromotionReviewer, RegressionCaseResult, RegressionResult, RegressionVerdict,
    TaskEvaluationBundle,
};

pub(crate) const MAX_EVALUATION_STATE_BYTES: u64 = 64 * 1024 * 1024;

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
    #[error("evaluation benchmark is invalid: {0}")]
    InvalidBenchmark(String),
    #[error("evaluation counterfactual comparison is invalid: {0}")]
    InvalidCounterfactual(String),
    #[error("evaluation human review is invalid: {0}")]
    InvalidHumanReview(String),
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
            replace_by(&mut state.reviews, bundle.review, |value| {
                format!("{}:{:?}", value.task_id, value.mode)
            });
            replace_by(&mut state.benchmark_runs, bundle.benchmark_run, |value| {
                value.benchmark_id.clone()
            });
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

    pub fn record_benchmark_run(&self, run: BenchmarkRun) -> Result<(), EvaluationError> {
        if !benchmark_run_has_required_metadata(&run) {
            return Err(EvaluationError::InvalidBenchmark(
                "required metadata or structured hardening checks are missing".to_owned(),
            ));
        }
        self.update(|state| {
            replace_by(&mut state.benchmark_runs, run, |value| {
                value.benchmark_id.clone()
            });
            Ok(())
        })
    }

    pub fn compare_counterfactual(
        &self,
        group_id: &str,
    ) -> Result<CausalComparison, EvaluationError> {
        self.update(|state| {
            let mut runs = state
                .benchmark_runs
                .iter()
                .filter(|run| run.counterfactual_group_id.as_deref() == Some(group_id))
                .cloned()
                .collect::<Vec<_>>();
            if runs.len() != 2 {
                return Err(EvaluationError::InvalidCounterfactual(format!(
                    "group {group_id} must contain exactly one baseline and one variant"
                )));
            }
            runs.sort_by(|left, right| left.benchmark_id.cmp(&right.benchmark_id));
            let baseline_index = runs
                .iter()
                .position(|run| run.changed_layer.is_none())
                .ok_or_else(|| {
                    EvaluationError::InvalidCounterfactual(format!(
                        "group {group_id} has no baseline run"
                    ))
                })?;
            let baseline = runs.remove(baseline_index);
            let variant = runs.pop().ok_or_else(|| {
                EvaluationError::InvalidCounterfactual(format!(
                    "group {group_id} has no variant run"
                ))
            })?;
            let changed_layer = variant.changed_layer.clone().ok_or_else(|| {
                EvaluationError::InvalidCounterfactual(format!(
                    "group {group_id} variant does not identify the changed layer"
                ))
            })?;
            validate_counterfactual_pair(&baseline, &variant, &changed_layer)?;
            let replay = CounterfactualReplay {
                replay_id: format!("counterfactual-{}", Uuid::now_v7()),
                group_id: group_id.to_owned(),
                baseline_benchmark_id: baseline.benchmark_id.clone(),
                variant_benchmark_id: variant.benchmark_id.clone(),
                controlled_variables: controlled_variables(&baseline, &variant, &changed_layer),
                changed_layer,
                limitations: vec![
                    "comparison uses recorded benchmark runs and does not claim provider determinism"
                        .to_owned(),
                ],
            };
            let mut comparison = benchmark_delta(&baseline, &variant);
            comparison.replay_id.clone_from(&replay.replay_id);
            replace_by(&mut state.counterfactual_replays, replay, |value| {
                value.replay_id.clone()
            });
            replace_by(&mut state.causal_comparisons, comparison.clone(), |value| {
                value.comparison_id.clone()
            });
            Ok(comparison)
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
            if !matches!(
                candidate.status,
                CandidateStatus::Proposed | CandidateStatus::NeedsHumanReview
            ) {
                return Err(EvaluationError::InvalidCandidateState {
                    candidate_id: candidate.id,
                    status: candidate.status,
                    action: "run regression".to_owned(),
                });
            }
            let (case_results, mut regressions) =
                execute_durable_regression_suite(state, &candidate);
            if candidate.evidence_refs.is_empty() {
                regressions.push("candidate has no durable evidence".to_owned());
            }
            if candidate.rollback_ref.trim().is_empty() {
                regressions.push("candidate has no rollback reference".to_owned());
            }
            if case_results.is_empty() {
                regressions.push("candidate has no executable regression cases".to_owned());
            }
            let failed_cases = case_results
                .iter()
                .filter(|case_result| !case_result.passed)
                .count();
            let needs_review = candidate.kind == AutomationCandidateKind::RuntimeChange
                && !state.benchmark_runs.iter().any(|run| {
                    run.counterfactual_group_id.as_deref() == Some(candidate.id.as_str())
                        && run.changed_layer.is_some()
                });
            if needs_review {
                regressions.push(
                    "runtime-change candidate has no controlled counterfactual benchmark"
                        .to_owned(),
                );
            }
            regressions.sort();
            regressions.dedup();
            let passed = failed_cases == 0 && regressions.is_empty();
            let baseline = state.benchmark_runs.iter().find(|run| {
                run.benchmark_id == format!("benchmark-run-{}", candidate.source_task_id)
            });
            let candidate_run =
                state.benchmark_runs.iter().rev().find(|run| {
                    run.counterfactual_group_id.as_deref() == Some(candidate.id.as_str())
                });
            let comparison = candidate_run
                .zip(baseline)
                .map(|(candidate_run, baseline)| benchmark_delta(baseline, candidate_run));
            let regression = RegressionResult {
                regression_id: format!("regression-{}", Uuid::now_v7()),
                candidate_id: candidate.id.clone(),
                baseline_version: env!("CARGO_PKG_VERSION").to_owned(),
                candidate_version: format!("candidate-{}", candidate.id),
                cases_run: u32::try_from(case_results.len()).unwrap_or(u32::MAX),
                passed_cases: u32::try_from(case_results.len().saturating_sub(failed_cases))
                    .unwrap_or(u32::MAX),
                failed_cases: u32::try_from(failed_cases).unwrap_or(u32::MAX),
                regressions,
                cost_delta: comparison.as_ref().and_then(|value| value.cost_delta_usd),
                latency_delta: comparison.as_ref().and_then(|value| value.latency_delta_ms),
                quality_delta: comparison.as_ref().and_then(|value| value.quality_delta),
                security_delta: comparison.as_ref().and_then(|value| value.security_delta),
                causal_comparison_refs: vec![format!("replay-{}", candidate.source_task_id)],
                suite_kind: BenchmarkSuiteKind::Regression,
                case_results,
                baseline_benchmark_refs: baseline
                    .map(|run| vec![run.benchmark_id.clone()])
                    .unwrap_or_default(),
                candidate_benchmark_refs: candidate_run
                    .map(|run| vec![run.benchmark_id.clone()])
                    .unwrap_or_default(),
                verdict: if passed {
                    RegressionVerdict::Pass
                } else if needs_review {
                    RegressionVerdict::NeedsReview
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
            stored_candidate.status = match regression.verdict {
                RegressionVerdict::Pass => CandidateStatus::RegressionPassed,
                RegressionVerdict::NeedsReview => CandidateStatus::NeedsHumanReview,
                RegressionVerdict::Fail => CandidateStatus::Rejected,
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

    pub fn review_promotion(
        &self,
        candidate_id: &str,
        decision: PromotionDecisionKind,
        reviewer_id: &str,
        reason: &str,
    ) -> Result<PromotionDecision, EvaluationError> {
        if decision == PromotionDecisionKind::NeedsHumanReview {
            return Err(EvaluationError::InvalidHumanReview(
                "a human review must approve or reject the candidate".to_owned(),
            ));
        }
        if reviewer_id.trim().is_empty() || reason.trim().is_empty() {
            return Err(EvaluationError::InvalidHumanReview(
                "reviewer id and reason are required".to_owned(),
            ));
        }
        self.update(|state| {
            let candidate = state
                .automation_candidates
                .iter()
                .find(|candidate| candidate.id == candidate_id)
                .cloned()
                .ok_or_else(|| EvaluationError::CandidateNotFound(candidate_id.to_owned()))?;
            if !matches!(
                candidate.status,
                CandidateStatus::RegressionPassed | CandidateStatus::NeedsHumanReview
            ) {
                return Err(EvaluationError::InvalidCandidateState {
                    candidate_id: candidate.id,
                    status: candidate.status,
                    action: "human review".to_owned(),
                });
            }
            let clean_regression = state.regressions.iter().rev().any(|regression| {
                regression.candidate_id == candidate_id
                    && regression.verdict == RegressionVerdict::Pass
                    && regression.failed_cases == 0
                    && regression.regressions.is_empty()
            });
            if decision == PromotionDecisionKind::Approve && !clean_regression {
                return Err(EvaluationError::RegressionRequired(candidate_id.to_owned()));
            }
            let promotion = PromotionDecision {
                decision_id: format!("promotion-{}", Uuid::now_v7()),
                candidate_id: candidate_id.to_owned(),
                decision,
                reason: format!("{}: {}", sanitize_text(reviewer_id), sanitize_text(reason)),
                reviewer: PromotionReviewer::Human,
                applied_version: None,
                rollback_ref: Some(candidate.rollback_ref.clone()),
                expires_at: None,
                created_at: Utc::now(),
            };
            let stored_candidate = state
                .automation_candidates
                .iter_mut()
                .find(|candidate| candidate.id == candidate_id)
                .ok_or_else(|| EvaluationError::CandidateNotFound(candidate_id.to_owned()))?;
            stored_candidate.status = match decision {
                PromotionDecisionKind::Approve => CandidateStatus::Approved,
                PromotionDecisionKind::Reject => CandidateStatus::Rejected,
                PromotionDecisionKind::NeedsHumanReview => unreachable!(),
            };
            replace_by(&mut state.promotion_decisions, promotion.clone(), |value| {
                value.decision_id.clone()
            });
            Ok(promotion)
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

fn execute_durable_regression_suite(
    state: &EvaluationState,
    candidate: &AutomationCandidate,
) -> (Vec<RegressionCaseResult>, Vec<String>) {
    let cases = state
        .cases
        .iter()
        .filter(|case| case.source_task_id == Some(candidate.source_task_id))
        .collect::<Vec<_>>();
    let mut suite_failures = Vec::new();
    let case_results = cases
        .into_iter()
        .map(|case| {
            let result = state
                .results
                .iter()
                .rev()
                .find(|result| result.case_id == case.case_id);
            let replay = state
                .replays
                .iter()
                .rev()
                .find(|replay| replay.source_task_id == candidate.source_task_id);
            let expected_verdict = match candidate.kind {
                AutomationCandidateKind::Skill => EvaluationVerdict::Pass,
                AutomationCandidateKind::Benchmark
                | AutomationCandidateKind::GeneratedTask
                | AutomationCandidateKind::RuntimeChange => result
                    .map(|result| result.verdict)
                    .unwrap_or(EvaluationVerdict::Unknown),
            };
            let observed_verdict = result
                .map(|result| result.verdict)
                .unwrap_or(EvaluationVerdict::Unknown);
            let result_evidence = result
                .map(|result| {
                    result
                        .evidence_refs
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let evidence_complete = case
                .required_evidence
                .iter()
                .all(|required| result_evidence.contains(required));
            let replay_id = replay
                .map(|replay| replay.replay_id.clone())
                .unwrap_or_else(|| format!("replay-{}", candidate.source_task_id));
            let fixture_linked = case.fixture_refs.contains(&replay_id);
            let replay_has_facts = replay.is_some_and(|replay| replay.event_count > 0);
            let security_check = result
                .and_then(|result| result.security_utility.as_ref())
                .is_none_or(|security| security.policy_violations == 0);
            let evidence_checks = vec![
                benchmark_check(
                    "durable_replay_facts",
                    pass_fail(replay_has_facts),
                    if replay_has_facts {
                        "durable replay contains runtime events"
                    } else {
                        "durable replay is missing runtime events"
                    },
                    vec![replay_id.clone()],
                ),
                benchmark_check(
                    "fixture_linkage",
                    pass_fail(fixture_linked),
                    if fixture_linked {
                        "evaluation case references the replay fixture"
                    } else {
                        "evaluation case does not reference the replay fixture"
                    },
                    case.fixture_refs.clone(),
                ),
                benchmark_check(
                    "required_evidence",
                    pass_fail(evidence_complete),
                    if evidence_complete {
                        "all required evidence is present in the durable result"
                    } else {
                        "one or more required evidence records are missing"
                    },
                    result_evidence,
                ),
                benchmark_check(
                    "security_policy",
                    pass_fail(security_check),
                    if security_check {
                        "the replay has no recorded policy violation"
                    } else {
                        "the replay contains a recorded policy violation"
                    },
                    result
                        .map(|result| result.result_id.clone())
                        .into_iter()
                        .collect(),
                ),
            ];
            let passed = observed_verdict == expected_verdict
                && evidence_checks
                    .iter()
                    .all(|check| check.status == BenchmarkCheckStatus::Pass);
            if !passed {
                suite_failures.push(format!("regression case {} failed", case.case_id));
            }
            RegressionCaseResult {
                case_id: case.case_id.clone(),
                replay_id,
                passed,
                expected_verdict,
                observed_verdict,
                evidence_checks,
                failure_taxonomy: result
                    .map(|result| result.failure_taxonomy.clone())
                    .unwrap_or_default(),
            }
        })
        .collect();

    if candidate.kind == AutomationCandidateKind::Benchmark {
        let source_result = state
            .results
            .iter()
            .rev()
            .find(|result| result.source_task_id == candidate.source_task_id);
        let fixture_matches = state
            .benchmark_promotions
            .iter()
            .find(|benchmark| benchmark.source_task_id == candidate.source_task_id)
            .is_some_and(|benchmark| {
                !benchmark.fixture.is_empty()
                    && !benchmark.evaluator.is_empty()
                    && source_result
                        .is_some_and(|result| result.failure_taxonomy == benchmark.failure_taxonomy)
            });
        if !fixture_matches {
            suite_failures
                .push("benchmark fixture does not reproduce the source taxonomy".to_owned());
        }
    }

    (case_results, suite_failures)
}

fn pass_fail(passed: bool) -> BenchmarkCheckStatus {
    if passed {
        BenchmarkCheckStatus::Pass
    } else {
        BenchmarkCheckStatus::Fail
    }
}

fn benchmark_delta(baseline: &BenchmarkRun, variant: &BenchmarkRun) -> CausalComparison {
    let quality_delta = subtract_f32(variant.score, baseline.score);
    let scaffold_changed = baseline.scaffold_id != variant.scaffold_id
        || baseline.scaffold_version != variant.scaffold_version;
    let scaffold_inflation = scaffold_changed && quality_delta.is_some_and(|delta| delta > 0.0);
    CausalComparison {
        comparison_id: format!("comparison-{}", Uuid::now_v7()),
        replay_id: variant
            .counterfactual_group_id
            .clone()
            .unwrap_or_else(|| format!("compare-{}", variant.benchmark_id)),
        quality_delta,
        utility_delta: subtract_f32(variant.utility_score, baseline.utility_score),
        security_delta: subtract_f32(variant.security_score, baseline.security_score),
        token_delta: subtract_u64(variant.total_tokens, baseline.total_tokens),
        cost_delta_usd: subtract_f64(variant.cost_usd, baseline.cost_usd),
        latency_delta_ms: subtract_u64(Some(variant.runtime_ms), Some(baseline.runtime_ms)),
        scaffold_inflation,
        conclusion: if scaffold_inflation {
            "quality increased while the harness or scaffold changed; capability gain is not isolated"
                .to_owned()
        } else if quality_delta.is_some_and(|delta| delta > 0.0) {
            "the controlled variant improved quality without detected scaffold inflation".to_owned()
        } else {
            "the controlled variant did not demonstrate a quality improvement".to_owned()
        },
    }
}

fn validate_counterfactual_pair(
    baseline: &BenchmarkRun,
    variant: &BenchmarkRun,
    changed_layer: &str,
) -> Result<(), EvaluationError> {
    if baseline.dataset_version != variant.dataset_version
        || baseline.harness_version != variant.harness_version
    {
        return Err(EvaluationError::InvalidCounterfactual(
            "dataset and harness versions must remain controlled".to_owned(),
        ));
    }
    if changed_layer != "scaffold"
        && (baseline.scaffold_id != variant.scaffold_id
            || baseline.scaffold_version != variant.scaffold_version)
    {
        return Err(EvaluationError::InvalidCounterfactual(
            "scaffold changed without being declared as the changed layer".to_owned(),
        ));
    }
    if changed_layer != "provider"
        && (baseline.provider_id != variant.provider_id || baseline.model_id != variant.model_id)
    {
        return Err(EvaluationError::InvalidCounterfactual(
            "provider or model changed without being declared as the changed layer".to_owned(),
        ));
    }
    if baseline.tool_budget != variant.tool_budget && changed_layer != "tool_policy" {
        return Err(EvaluationError::InvalidCounterfactual(
            "tool budget changed without being declared as the changed layer".to_owned(),
        ));
    }
    Ok(())
}

fn controlled_variables(
    baseline: &BenchmarkRun,
    variant: &BenchmarkRun,
    changed_layer: &str,
) -> Vec<String> {
    let mut variables = vec![
        format!("dataset_version={}", baseline.dataset_version),
        format!("harness_version={}", baseline.harness_version),
        format!("attempt_count={}", baseline.attempt_count),
    ];
    if changed_layer != "scaffold" {
        variables.push(format!(
            "scaffold={}:{}",
            baseline.scaffold_id, baseline.scaffold_version
        ));
    }
    if changed_layer != "provider" {
        variables.push(format!(
            "provider_model={}:{}",
            baseline.provider_id, baseline.model_id
        ));
    }
    if baseline.runtime_ms != variant.runtime_ms {
        variables.push("runtime_ms=observed_not_controlled".to_owned());
    }
    variables
}

fn subtract_f32(left: Option<f32>, right: Option<f32>) -> Option<f32> {
    left.zip(right).map(|(left, right)| left - right)
}

fn subtract_f64(left: Option<f64>, right: Option<f64>) -> Option<f64> {
    left.zip(right).map(|(left, right)| left - right)
}

fn subtract_u64(left: Option<u64>, right: Option<u64>) -> Option<i64> {
    left.zip(right).map(|(left, right)| {
        let difference = i128::from(left) - i128::from(right);
        difference.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64
    })
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
