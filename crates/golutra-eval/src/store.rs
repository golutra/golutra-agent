use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use chrono::Utc;
use fs2::FileExt;
use golutra_core::{
    EvaluationPartitionKind, RegressionCampaign, RegressionExecution, RegressionExecutionRole,
    RegressionExecutionStatus,
};
use sha2::{Digest, Sha256};
use thiserror::Error;
use uuid::Uuid;

use crate::runner::{
    benchmark_check, benchmark_run_has_required_metadata, candidate_mutates_control_plane,
    decide_governed_promotion, sanitize_text,
};
use crate::{
    AppliedCandidate, AutomationCandidate, AutomationCandidateKind, BenchmarkCheckStatus,
    BenchmarkRun, BenchmarkSuiteKind, CandidateStatus, CausalComparison, CounterfactualReplay,
    DiagnosticSlice, EvaluationState, EvaluationVerdict, ExternalEvaluationRecord,
    ExternalEvaluationTrust, FailureDiagnosis, PromotionDecision, PromotionDecisionKind,
    PromotionReviewer, RegressionCaseResult, RegressionCoverage, RegressionResult,
    RegressionVerdict, ReplayCapsule, ReplayExecution, TaskEvaluationBundle,
    external_evaluation_result_digest,
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
    #[error("evaluation candidate has no paired baseline/candidate execution: {0}")]
    RegressionExecutionRequired(String),
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

    pub fn record_diagnostic(
        &self,
        diagnosis: FailureDiagnosis,
        slice: DiagnosticSlice,
    ) -> Result<bool, EvaluationError> {
        if diagnosis.source_task_id != slice.source_task_id
            || diagnosis.diagnosis_id != slice.diagnosis.diagnosis_id
            || slice.event_refs.is_empty()
            || diagnosis.trigger_event_refs.is_empty()
        {
            return Err(EvaluationError::Invariant(
                "diagnostic slice must reference one matching diagnosis and trigger".to_owned(),
            ));
        }
        self.update(|state| {
            let inserted = !state
                .failure_diagnoses
                .iter()
                .any(|value| value.diagnosis_id == diagnosis.diagnosis_id);
            replace_by(&mut state.failure_diagnoses, diagnosis, |value| {
                value.diagnosis_id.clone()
            });
            replace_by(&mut state.diagnostic_slices, slice, |value| {
                value.slice_id.clone()
            });
            Ok(inserted)
        })
    }

    pub fn record_replay_capsule(&self, capsule: ReplayCapsule) -> Result<bool, EvaluationError> {
        let complete_inputs_valid = !capsule.provider_exchanges.is_empty()
            && capsule.runtime_config_digest.starts_with("sha256:")
            && capsule.source_last_sequence_no.is_some()
            && capsule.missing_inputs.is_empty();
        if !capsule.event_chain_digest.starts_with("sha256:")
            || capsule.runtime_config_digest.trim().is_empty()
            || (capsule.complete && !complete_inputs_valid)
            || (!capsule.complete && capsule.missing_inputs.is_empty())
        {
            return Err(EvaluationError::Invariant(
                "replay capsule completeness or digest is invalid".to_owned(),
            ));
        }
        self.update(|state| {
            let inserted = !state
                .replay_capsules
                .iter()
                .any(|value| value.capsule_id == capsule.capsule_id);
            replace_by(&mut state.replay_capsules, capsule, |value| {
                value.capsule_id.clone()
            });
            Ok(inserted)
        })
    }

    pub fn record_replay_execution(
        &self,
        execution: ReplayExecution,
    ) -> Result<bool, EvaluationError> {
        self.update(|state| {
            let capsule = state
                .replay_capsules
                .iter()
                .find(|capsule| capsule.capsule_id == execution.capsule_id)
                .ok_or_else(|| {
                    EvaluationError::Invariant(format!(
                        "replay capsule {} is not recorded",
                        execution.capsule_id
                    ))
                })?;
            if capsule.source_task_id != execution.source_task_id
                || capsule.mode != execution.mode
                || execution.provider_exchanges_total
                    != u32::try_from(capsule.provider_exchanges.len()).unwrap_or(u32::MAX)
                || execution.tool_results_total
                    != u32::try_from(capsule.tool_results.len()).unwrap_or(u32::MAX)
                || execution.provider_exchanges_consumed > execution.provider_exchanges_total
                || execution.tool_results_consumed > execution.tool_results_total
                || execution.completed_at < execution.started_at
            {
                return Err(EvaluationError::Invariant(
                    "replay execution source or consumption counts are invalid".to_owned(),
                ));
            }
            let status_valid = match execution.status {
                crate::ReplayExecutionStatus::Matched => {
                    capsule.complete
                        && execution.provider_exchanges_consumed
                            == execution.provider_exchanges_total
                        && execution.tool_results_consumed == execution.tool_results_total
                        && execution.expected_loop_action.is_some()
                        && execution.expected_loop_action == execution.observed_loop_action
                        && execution.expected_verification.is_some()
                        && execution.expected_verification == execution.observed_verification
                        && execution.mismatches.is_empty()
                }
                crate::ReplayExecutionStatus::Diverged => !execution.mismatches.is_empty(),
                crate::ReplayExecutionStatus::Incomplete => {
                    !capsule.complete || !execution.mismatches.is_empty()
                }
                crate::ReplayExecutionStatus::Failed => !execution.mismatches.is_empty(),
            };
            if !status_valid {
                return Err(EvaluationError::Invariant(
                    "replay execution status is inconsistent with its evidence".to_owned(),
                ));
            }
            let inserted = !state
                .replay_executions
                .iter()
                .any(|value| value.execution_id == execution.execution_id);
            replace_by(&mut state.replay_executions, execution, |value| {
                value.execution_id.clone()
            });
            Ok(inserted)
        })
    }

    pub fn record_external_evaluation(
        &self,
        record: ExternalEvaluationRecord,
    ) -> Result<bool, EvaluationError> {
        let association_fields = [
            record.comparison_group_id.is_some(),
            record.candidate_id.is_some(),
            record.campaign_id.is_some(),
            record.role.is_some(),
        ];
        let association_valid = association_fields.iter().all(|value| *value)
            || association_fields.iter().all(|value| !*value);
        let score_valid = record.score.is_none_or(f64::is_finite)
            && record
                .score_max
                .is_none_or(|value| value.is_finite() && value > 0.0)
            && record
                .score
                .zip(record.score_max)
                .is_none_or(|(score, maximum)| score >= 0.0 && score <= maximum);
        let signed_attestation_valid = record.trust != ExternalEvaluationTrust::Signed
            || record.attestation.as_ref().is_some_and(|attestation| {
                attestation.algorithm == "ed25519"
                    && !attestation.key_id.trim().is_empty()
                    && !attestation.signature.trim().is_empty()
                    && attestation.signed_digest == record.result_digest
            });
        if record.evaluator_id.trim().is_empty()
            || record.case_id.trim().is_empty()
            || !record.base_trace_digest.starts_with("sha256:")
            || !record.result_digest.starts_with("sha256:")
            || record.result_digest != external_evaluation_result_digest(&record)
            || !score_valid
            || !association_valid
            || !signed_attestation_valid
            || holdout_disclosure_violation(&record).is_some()
        {
            return Err(EvaluationError::Invariant(
                "external evaluation identity, digest, or attestation is invalid".to_owned(),
            ));
        }
        self.update(|state| {
            let existing = state
                .external_evaluations
                .iter()
                .find(|value| value.evaluation_id == record.evaluation_id);
            if existing.is_some_and(|existing| existing != &record) {
                return Err(EvaluationError::Invariant(format!(
                    "external evaluation {} already exists with different facts",
                    record.evaluation_id
                )));
            }
            let inserted = existing.is_none();
            replace_by(&mut state.external_evaluations, record.clone(), |value| {
                value.evaluation_id.clone()
            });
            if let Some(comparison) = external_causal_comparison(state, &record) {
                replace_by(&mut state.causal_comparisons, comparison, |value| {
                    value.comparison_id.clone()
                });
            }
            Ok(inserted)
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

    pub fn record_regression_campaign(
        &self,
        mut campaign: RegressionCampaign,
    ) -> Result<(), EvaluationError> {
        if campaign.required_partitions.is_empty() {
            campaign.required_partitions = campaign
                .case_refs
                .iter()
                .map(|case_ref| {
                    campaign
                        .case_partitions
                        .get(case_ref)
                        .copied()
                        .unwrap_or_default()
                })
                .collect();
            campaign.required_partitions.sort();
            campaign.required_partitions.dedup();
        }
        if campaign.candidate_id.trim().is_empty()
            || campaign.candidate_digest.trim().is_empty()
            || campaign.case_refs.is_empty()
            || campaign.provider_matrix.is_empty()
            || campaign.seeds.is_empty()
            || campaign.required_partitions.is_empty()
            || campaign
                .case_partitions
                .keys()
                .any(|case_ref| !campaign.case_refs.contains(case_ref))
        {
            return Err(EvaluationError::InvalidBenchmark(
                "regression campaign requires candidate digest, executable cases, partitions, providers, and seeds"
                    .to_owned(),
            ));
        }
        self.update(|state| {
            replace_by(&mut state.regression_campaigns, campaign, |value| {
                value.campaign_id
            });
            Ok(())
        })
    }

    pub fn record_regression_execution(
        &self,
        mut execution: RegressionExecution,
    ) -> Result<(), EvaluationError> {
        self.update(|state| {
            let campaign = state
                .regression_campaigns
                .iter()
                .find(|campaign| campaign.campaign_id == execution.campaign_id)
                .ok_or_else(|| {
                    EvaluationError::RegressionExecutionRequired(format!(
                        "campaign {} is not recorded",
                        execution.campaign_id
                    ))
                })?;
            if execution.case_ref.trim().is_empty() && campaign.case_refs.len() == 1 {
                execution.case_ref.clone_from(&campaign.case_refs[0]);
            }
            if execution.provider_variant.trim().is_empty() && campaign.provider_matrix.len() == 1 {
                execution
                    .provider_variant
                    .clone_from(&campaign.provider_matrix[0]);
            }
            if !campaign.case_refs.contains(&execution.case_ref) {
                return Err(EvaluationError::RegressionExecutionRequired(format!(
                    "execution case `{}` is not part of campaign {}",
                    execution.case_ref, execution.campaign_id
                )));
            }
            let expected_partition = campaign
                .case_partitions
                .get(&execution.case_ref)
                .copied()
                .unwrap_or_default();
            if execution.partition != expected_partition
                || !campaign
                    .provider_matrix
                    .contains(&execution.provider_variant)
                || !campaign.seeds.contains(&execution.seed)
            {
                return Err(EvaluationError::RegressionExecutionRequired(format!(
                    "execution metadata is outside campaign {} partition/provider/seed matrix",
                    execution.campaign_id
                )));
            }
            if execution.status == RegressionExecutionStatus::Succeeded
                && (execution.task_trace_ref.is_none() || execution.verification_ref.is_none())
            {
                return Err(EvaluationError::RegressionExecutionRequired(
                    "execution must contain task trace and verification references".to_owned(),
                ));
            }
            replace_by(&mut state.regression_executions, execution, |value| {
                value.execution_id
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
            let campaign = state
                .regression_campaigns
                .iter()
                .rev()
                .find(|campaign| campaign.candidate_id == candidate_id)
                .ok_or_else(|| {
                    EvaluationError::RegressionExecutionRequired(candidate_id.to_owned())
                })?;
            let executions = state
                .regression_executions
                .iter()
                .filter(|execution| execution.campaign_id == campaign.campaign_id)
                .collect::<Vec<_>>();
            let mut paired_execution_refs = Vec::new();
            let mut execution_regressions = Vec::new();
            let mut needs_review = false;
            for case_ref in &campaign.case_refs {
                let pairs =
                    completed_execution_pairs(&executions, case_ref, campaign.case_refs.len());
                if pairs.is_empty() {
                    needs_review = true;
                    execution_regressions.push(format!(
                        "regression case {case_ref} has no completed baseline/candidate pair"
                    ));
                    continue;
                }
                let valid_refs = pairs
                    .into_iter()
                    .filter_map(|(baseline, candidate)| {
                        baseline
                            .task_trace_ref
                            .as_ref()
                            .zip(candidate.task_trace_ref.as_ref())
                    })
                    .filter(|(baseline, candidate)| {
                        baseline != candidate
                            && valid_execution_trace_ref(baseline)
                            && valid_execution_trace_ref(candidate)
                    })
                    .flat_map(|(baseline, candidate)| [baseline.clone(), candidate.clone()])
                    .collect::<Vec<_>>();
                if valid_refs.is_empty() {
                    needs_review = true;
                    execution_regressions.push(format!(
                        "regression case {case_ref} has invalid paired execution trace references"
                    ));
                    continue;
                }
                paired_execution_refs.extend(valid_refs);
            }
            paired_execution_refs.sort();
            paired_execution_refs.dedup();
            let coverage_evidence = regression_coverage(state, campaign, &executions);
            if !coverage_evidence.coverage.complete() {
                needs_review = true;
                execution_regressions.extend(
                    coverage_evidence
                        .coverage
                        .missing_cells
                        .iter()
                        .map(|cell| format!("missing regression coverage: {cell}")),
                );
                if !coverage_evidence.coverage.missing_partitions.is_empty() {
                    execution_regressions.push(format!(
                        "missing evaluation partitions: {:?}",
                        coverage_evidence.coverage.missing_partitions
                    ));
                }
                if !coverage_evidence.coverage.missing_providers.is_empty() {
                    execution_regressions.push(format!(
                        "missing provider variants: {}",
                        coverage_evidence.coverage.missing_providers.join(", ")
                    ));
                }
                if !coverage_evidence.coverage.missing_seeds.is_empty() {
                    execution_regressions.push(format!(
                        "missing evaluation seeds: {:?}",
                        coverage_evidence.coverage.missing_seeds
                    ));
                }
                execution_regressions.extend(
                    coverage_evidence
                        .coverage
                        .holdout_disclosure_violations
                        .iter()
                        .map(|violation| format!("holdout disclosure violation: {violation}")),
                );
            }
            let (case_results, mut regressions) =
                execute_durable_regression_suite(state, &candidate, campaign, &executions);
            regressions.extend(execution_regressions);
            if candidate.evidence_refs.is_empty() {
                regressions.push("candidate has no durable evidence".to_owned());
            }
            if candidate.rollback_ref.trim().is_empty() {
                regressions.push("candidate has no rollback reference".to_owned());
            }
            if case_results.is_empty() {
                regressions.push("candidate has no executable regression cases".to_owned());
            }
            if case_results.len() != campaign.case_refs.len() {
                needs_review = true;
                regressions.push(
                    "one or more campaign case refs have no durable evaluation definition"
                        .to_owned(),
                );
            }
            if candidate_mutates_control_plane(&candidate) {
                regressions
                    .push("candidate attempts to modify the sealed control plane".to_owned());
            }
            let failed_cases = case_results
                .iter()
                .filter(|case_result| !case_result.passed)
                .count();
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
                causal_comparison_refs: coverage_evidence.causal_comparison_refs,
                paired_execution_refs,
                external_evaluation_refs: coverage_evidence.external_evaluation_refs,
                coverage: coverage_evidence.coverage,
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
            let decision = decide_governed_promotion(
                &candidate,
                regression,
                &crate::PromotionGateFacts {
                    trace_complete: regression.paired_execution_refs.len() >= 2,
                    unresolved_refs: Vec::new(),
                    verification: EvaluationVerdict::Pass,
                    paired_execution_refs: regression.paired_execution_refs.clone(),
                    trusted_external_evaluation_refs: regression
                        .coverage
                        .trusted_external_evaluation_refs
                        .clone(),
                    coverage_complete: regression.coverage.complete(),
                    missing_coverage: regression.coverage.missing_cells.clone(),
                    holdout_disclosure_violations: regression
                        .coverage
                        .holdout_disclosure_violations
                        .clone(),
                    candidate_mutates_control_plane: candidate_mutates_control_plane(&candidate),
                    mutation_reasons: Vec::new(),
                },
            );
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

    /// 每次完成 regression 都形成显式治理结论；失败不能只靠 candidate status 隐式表达。
    pub fn decide_after_regression(
        &self,
        candidate_id: &str,
    ) -> Result<PromotionDecision, EvaluationError> {
        let status = self
            .snapshot()?
            .automation_candidates
            .into_iter()
            .find(|candidate| candidate.id == candidate_id)
            .map(|candidate| candidate.status)
            .ok_or_else(|| EvaluationError::CandidateNotFound(candidate_id.to_owned()))?;
        if status == CandidateStatus::RegressionPassed {
            return self.decide_promotion(candidate_id);
        }
        self.update(|state| {
            let candidate = state
                .automation_candidates
                .iter()
                .find(|candidate| candidate.id == candidate_id)
                .cloned()
                .ok_or_else(|| EvaluationError::CandidateNotFound(candidate_id.to_owned()))?;
            let regression = state
                .regressions
                .iter()
                .rev()
                .find(|regression| regression.candidate_id == candidate_id)
                .ok_or_else(|| EvaluationError::RegressionRequired(candidate_id.to_owned()))?;
            let (decision, reason) = match (candidate.status, regression.verdict) {
                (CandidateStatus::Rejected, RegressionVerdict::Fail) => (
                    PromotionDecisionKind::Reject,
                    format!(
                        "regression rejected candidate: {}",
                        regression.regressions.join("; ")
                    ),
                ),
                (CandidateStatus::NeedsHumanReview, RegressionVerdict::NeedsReview) => (
                    PromotionDecisionKind::NeedsHumanReview,
                    "regression requires explicit human review".to_owned(),
                ),
                (status, verdict) => {
                    return Err(EvaluationError::InvalidCandidateState {
                        candidate_id: candidate.id,
                        status,
                        action: format!("decide after {verdict:?} regression"),
                    });
                }
            };
            let promotion = PromotionDecision {
                decision_id: format!("promotion-{}", Uuid::now_v7()),
                candidate_id: candidate_id.to_owned(),
                decision,
                reason: sanitize_text(&reason),
                reviewer: PromotionReviewer::System,
                applied_version: None,
                rollback_ref: Some(candidate.rollback_ref),
                expires_at: None,
                created_at: Utc::now(),
            };
            replace_by(&mut state.promotion_decisions, promotion.clone(), |value| {
                value.decision_id.clone()
            });
            Ok(promotion)
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
                    && regression.paired_execution_refs.len() >= 2
                    && regression
                        .paired_execution_refs
                        .iter()
                        .all(|reference| !reference.trim().is_empty())
                    && regression.coverage.complete()
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
    campaign: &RegressionCampaign,
    executions: &[&RegressionExecution],
) -> (Vec<RegressionCaseResult>, Vec<String>) {
    let cases = state
        .cases
        .iter()
        .filter(|case| campaign.case_refs.contains(&case.case_id))
        .collect::<Vec<_>>();
    let mut suite_failures = Vec::new();
    let case_results = cases
        .into_iter()
        .map(|case| {
            let baseline_execution = executions.iter().rev().copied().find(|execution| {
                execution.role == RegressionExecutionRole::Baseline
                    && execution_matches_case(execution, &case.case_id, campaign.case_refs.len())
            });
            let candidate_execution = executions.iter().rev().copied().find(|execution| {
                execution.role == RegressionExecutionRole::Candidate
                    && execution_matches_case(execution, &case.case_id, campaign.case_refs.len())
            });
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
            let expected_verdict = execution_verdict(baseline_execution);
            let observed_verdict = execution_verdict(candidate_execution);
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
            let paired_traces = baseline_execution
                .and_then(|execution| execution.task_trace_ref.as_ref())
                .zip(candidate_execution.and_then(|execution| execution.task_trace_ref.as_ref()))
                .filter(|(baseline, candidate)| {
                    baseline != candidate
                        && valid_execution_trace_ref(baseline)
                        && valid_execution_trace_ref(candidate)
                });
            let workspace_changed =
                baseline_execution
                    .zip(candidate_execution)
                    .is_some_and(|(baseline, candidate)| {
                        baseline.workspace_snapshot_digest != candidate.workspace_snapshot_digest
                    });
            let evidence_checks = vec![
                benchmark_check(
                    "paired_execution_traces",
                    pass_fail(paired_traces.is_some()),
                    if paired_traces.is_some() {
                        "baseline and candidate have distinct durable execution traces"
                    } else {
                        "baseline/candidate durable execution traces are missing or invalid"
                    },
                    paired_traces
                        .into_iter()
                        .flat_map(|(baseline, candidate)| [baseline.clone(), candidate.clone()])
                        .collect(),
                ),
                benchmark_check(
                    "candidate_workspace_delta",
                    pass_fail(workspace_changed),
                    if workspace_changed {
                        "candidate execution used a changed workspace snapshot"
                    } else {
                        "candidate execution workspace is identical to baseline"
                    },
                    candidate_execution
                        .map(|execution| vec![execution.workspace_snapshot_digest.clone()])
                        .unwrap_or_default(),
                ),
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
            let passed = expected_verdict == EvaluationVerdict::Pass
                && observed_verdict == EvaluationVerdict::Pass
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

#[derive(Debug)]
struct RegressionCoverageEvidence {
    coverage: RegressionCoverage,
    causal_comparison_refs: Vec<String>,
    external_evaluation_refs: Vec<String>,
}

fn regression_coverage(
    state: &EvaluationState,
    campaign: &RegressionCampaign,
    executions: &[&RegressionExecution],
) -> RegressionCoverageEvidence {
    let mut required_partitions = campaign.required_partitions.clone();
    required_partitions.extend(campaign.case_refs.iter().map(|case_ref| {
        campaign
            .case_partitions
            .get(case_ref)
            .copied()
            .unwrap_or_default()
    }));
    required_partitions.sort();
    required_partitions.dedup();

    let associated = state
        .external_evaluations
        .iter()
        .filter(|record| {
            record.campaign_id == Some(campaign.campaign_id)
                && record.candidate_id.as_deref() == Some(campaign.candidate_id.as_str())
        })
        .collect::<Vec<_>>();
    let external_pairs = trusted_external_pairs(&associated);
    let mut observed_partitions = Vec::new();
    let mut observed_providers = Vec::new();
    let mut observed_seeds = Vec::new();
    let mut missing_cells = Vec::new();
    let mut completed_cells = 0_u32;
    let mut trusted_external_pairs = 0_u32;
    let mut trusted_external_evaluation_refs = Vec::new();
    let mut used_external_evaluation_refs = Vec::new();

    let mut required_providers = campaign.provider_matrix.clone();
    required_providers.sort();
    required_providers.dedup();
    let mut required_seeds = campaign.seeds.clone();
    required_seeds.sort_unstable();
    required_seeds.dedup();
    for case_ref in &campaign.case_refs {
        let partition = campaign
            .case_partitions
            .get(case_ref)
            .copied()
            .unwrap_or_default();
        for provider in &required_providers {
            for seed in &required_seeds {
                let local_pair = completed_execution_pair_for_cell(
                    executions, case_ref, partition, provider, *seed,
                );
                let external_pair = external_pairs.iter().find(|(baseline, candidate)| {
                    external_pair_matches_cell(
                        baseline, candidate, case_ref, partition, provider, *seed,
                    )
                });
                if local_pair.is_none() && external_pair.is_none() {
                    missing_cells.push(format!(
                        "case:{case_ref}|partition:{partition:?}|provider:{provider}|seed:{seed}"
                    ));
                    continue;
                }
                completed_cells = completed_cells.saturating_add(1);
                observed_partitions.push(partition);
                observed_providers.push(provider.clone());
                observed_seeds.push(*seed);
                if let Some((baseline, candidate)) = external_pair {
                    trusted_external_pairs = trusted_external_pairs.saturating_add(1);
                    trusted_external_evaluation_refs.extend([
                        baseline.evaluation_id.clone(),
                        candidate.evaluation_id.clone(),
                    ]);
                    used_external_evaluation_refs.extend([
                        baseline.evaluation_id.clone(),
                        candidate.evaluation_id.clone(),
                    ]);
                }
            }
        }
    }

    let mut untrusted_external_evaluation_refs = Vec::new();
    let mut external_evaluation_refs = associated
        .iter()
        .map(|record| record.evaluation_id.clone())
        .collect::<Vec<_>>();
    let mut holdout_disclosure_violations = associated
        .iter()
        .filter_map(|record| holdout_disclosure_violation(record))
        .collect::<Vec<_>>();
    for record in &associated {
        if !used_external_evaluation_refs.contains(&record.evaluation_id) {
            untrusted_external_evaluation_refs.push(record.evaluation_id.clone());
        }
    }

    observed_partitions.sort();
    observed_partitions.dedup();
    observed_providers.retain(|provider| !provider.trim().is_empty());
    observed_providers.sort();
    observed_providers.dedup();
    observed_seeds.sort_unstable();
    observed_seeds.dedup();
    trusted_external_evaluation_refs.sort();
    trusted_external_evaluation_refs.dedup();
    untrusted_external_evaluation_refs.sort();
    untrusted_external_evaluation_refs.dedup();
    external_evaluation_refs.sort();
    external_evaluation_refs.dedup();
    holdout_disclosure_violations.sort();
    holdout_disclosure_violations.dedup();

    let missing_partitions = required_partitions
        .iter()
        .copied()
        .filter(|partition| !observed_partitions.contains(partition))
        .collect::<Vec<_>>();
    let missing_providers = required_providers
        .iter()
        .filter(|provider| !observed_providers.contains(provider))
        .cloned()
        .collect::<Vec<_>>();
    let missing_seeds = required_seeds
        .iter()
        .copied()
        .filter(|seed| !observed_seeds.contains(seed))
        .collect::<Vec<_>>();
    let minimum_external =
        usize::try_from(campaign.minimum_trusted_external_pairs).unwrap_or(usize::MAX);
    if usize::try_from(trusted_external_pairs).unwrap_or(usize::MAX) < minimum_external {
        missing_cells.push(format!(
            "trusted_external_pairs:{}/{}",
            trusted_external_pairs, minimum_external
        ));
    }

    let mut causal_comparison_refs = state
        .causal_comparisons
        .iter()
        .filter(|comparison| {
            comparison
                .baseline_evaluation_ref
                .as_ref()
                .is_some_and(|reference| trusted_external_evaluation_refs.contains(reference))
                && comparison
                    .candidate_evaluation_ref
                    .as_ref()
                    .is_some_and(|reference| trusted_external_evaluation_refs.contains(reference))
        })
        .map(|comparison| comparison.comparison_id.clone())
        .collect::<Vec<_>>();
    causal_comparison_refs.sort();
    causal_comparison_refs.dedup();
    let expected_cells = u32::try_from(
        campaign
            .case_refs
            .len()
            .saturating_mul(required_providers.len())
            .saturating_mul(required_seeds.len()),
    )
    .unwrap_or(u32::MAX);

    RegressionCoverageEvidence {
        coverage: RegressionCoverage {
            required_partitions,
            observed_partitions,
            missing_partitions,
            required_providers,
            observed_providers,
            missing_providers,
            required_seeds,
            observed_seeds,
            missing_seeds,
            expected_cells,
            completed_cells,
            missing_cells,
            trusted_external_pairs,
            trusted_external_evaluation_refs,
            untrusted_external_evaluation_refs,
            holdout_disclosure_violations,
        },
        causal_comparison_refs,
        external_evaluation_refs,
    }
}

fn completed_execution_pair_for_cell<'a>(
    executions: &'a [&'a RegressionExecution],
    case_ref: &str,
    partition: EvaluationPartitionKind,
    provider: &str,
    seed: u64,
) -> Option<(&'a RegressionExecution, &'a RegressionExecution)> {
    let candidate = executions.iter().rev().copied().find(|execution| {
        execution.role == RegressionExecutionRole::Candidate
            && execution.status == RegressionExecutionStatus::Succeeded
            && execution.case_ref == case_ref
            && execution.partition == partition
            && execution.provider_variant == provider
            && execution.seed == seed
    })?;
    executions
        .iter()
        .rev()
        .copied()
        .find(|execution| {
            execution.role == RegressionExecutionRole::Baseline
                && execution.status == RegressionExecutionStatus::Succeeded
                && execution.case_ref == case_ref
                && execution.partition == partition
                && execution.provider_variant == provider
                && execution.seed == seed
        })
        .map(|baseline| (baseline, candidate))
}

fn external_pair_matches_cell(
    baseline: &ExternalEvaluationRecord,
    candidate: &ExternalEvaluationRecord,
    case_ref: &str,
    partition: EvaluationPartitionKind,
    provider: &str,
    seed: u64,
) -> bool {
    baseline.case_id == case_ref
        && candidate.case_id == case_ref
        && baseline.partition == partition
        && candidate.partition == partition
        && baseline.provider_variant.as_deref() == Some(provider)
        && candidate.provider_variant.as_deref() == Some(provider)
        && baseline.seed == Some(seed)
        && candidate.seed == Some(seed)
}

fn completed_execution_pairs<'a>(
    executions: &'a [&'a RegressionExecution],
    case_ref: &str,
    campaign_case_count: usize,
) -> Vec<(&'a RegressionExecution, &'a RegressionExecution)> {
    executions
        .iter()
        .rev()
        .copied()
        .filter(|execution| {
            execution.role == RegressionExecutionRole::Candidate
                && execution.status == RegressionExecutionStatus::Succeeded
                && execution_matches_case(execution, case_ref, campaign_case_count)
        })
        .filter_map(|candidate| {
            executions
                .iter()
                .rev()
                .copied()
                .find(|baseline| {
                    baseline.role == RegressionExecutionRole::Baseline
                        && baseline.status == RegressionExecutionStatus::Succeeded
                        && execution_matches_case(baseline, case_ref, campaign_case_count)
                        && baseline.partition == candidate.partition
                        && baseline.provider_variant == candidate.provider_variant
                        && baseline.seed == candidate.seed
                })
                .map(|baseline| (baseline, candidate))
        })
        .collect()
}

fn trusted_external_pairs<'a>(
    records: &[&'a ExternalEvaluationRecord],
) -> Vec<(&'a ExternalEvaluationRecord, &'a ExternalEvaluationRecord)> {
    records
        .iter()
        .copied()
        .filter(|record| {
            record.role == Some(RegressionExecutionRole::Candidate)
                && external_evaluation_is_trusted(record)
        })
        .filter_map(|candidate| {
            records
                .iter()
                .copied()
                .find(|baseline| {
                    baseline.role == Some(RegressionExecutionRole::Baseline)
                        && external_evaluation_is_trusted(baseline)
                        && external_evaluation_pair_matches(baseline, candidate)
                })
                .map(|baseline| (baseline, candidate))
        })
        .collect()
}

fn external_evaluation_is_trusted(record: &ExternalEvaluationRecord) -> bool {
    record.trust != ExternalEvaluationTrust::UntrustedLocal
        && (record.partition != EvaluationPartitionKind::Holdout
            || (record.trust == ExternalEvaluationTrust::Signed && record.holdout_protected))
}

fn external_evaluation_pair_matches(
    baseline: &ExternalEvaluationRecord,
    candidate: &ExternalEvaluationRecord,
) -> bool {
    baseline.comparison_group_id == candidate.comparison_group_id
        && baseline.candidate_id == candidate.candidate_id
        && baseline.campaign_id == candidate.campaign_id
        && baseline.dataset_id == candidate.dataset_id
        && baseline.dataset_version == candidate.dataset_version
        && baseline.harness_id == candidate.harness_id
        && baseline.harness_version == candidate.harness_version
        && baseline.case_id == candidate.case_id
        && baseline.partition == candidate.partition
        && baseline.seed == candidate.seed
        && baseline.provider_variant == candidate.provider_variant
}

fn holdout_disclosure_violation(record: &ExternalEvaluationRecord) -> Option<String> {
    if record.partition != EvaluationPartitionKind::Holdout {
        return None;
    }
    if !record.holdout_protected {
        return Some(format!(
            "evaluation {} is not marked holdout_protected",
            record.evaluation_id
        ));
    }
    if record.trust != ExternalEvaluationTrust::Signed {
        return Some(format!(
            "evaluation {} is not signed by a trusted evaluator",
            record.evaluation_id
        ));
    }
    if record.score.is_some()
        || record.score_max.is_some()
        || !record.artifact_refs.is_empty()
        || record
            .assertions
            .iter()
            .any(|assertion| !assertion.message.is_empty() || !assertion.evidence_refs.is_empty())
    {
        return Some(format!(
            "evaluation {} discloses holdout score, artifacts, or assertion details",
            record.evaluation_id
        ));
    }
    None
}

fn external_causal_comparison(
    state: &EvaluationState,
    record: &ExternalEvaluationRecord,
) -> Option<CausalComparison> {
    if record.comparison_group_id.is_none() || !external_evaluation_is_trusted(record) {
        return None;
    }
    let counterpart = state.external_evaluations.iter().find(|candidate| {
        candidate.evaluation_id != record.evaluation_id
            && candidate.role != record.role
            && external_evaluation_is_trusted(candidate)
            && external_evaluation_pair_matches(candidate, record)
    })?;
    let (baseline, candidate) = if record.role == Some(RegressionExecutionRole::Baseline) {
        (record, counterpart)
    } else {
        (counterpart, record)
    };
    if candidate.role != Some(RegressionExecutionRole::Candidate) {
        return None;
    }
    let quality_delta = normalized_external_score(candidate)
        .zip(normalized_external_score(baseline))
        .map(|(candidate, baseline)| candidate - baseline);
    let mut ids = [
        baseline.evaluation_id.as_str(),
        candidate.evaluation_id.as_str(),
    ];
    ids.sort_unstable();
    let digest = Sha256::digest(format!("{}\0{}", ids[0], ids[1]).as_bytes());
    let conclusion = match (baseline.verdict, candidate.verdict, quality_delta) {
        (EvaluationVerdict::Pass, EvaluationVerdict::Pass, Some(delta)) if delta > 0.0 => {
            "candidate retained a passing verdict and improved normalized external score"
        }
        (EvaluationVerdict::Pass, EvaluationVerdict::Pass, _) => {
            "candidate retained the baseline passing external verdict"
        }
        (EvaluationVerdict::Pass, _, _) => {
            "candidate regressed from a passing baseline external verdict"
        }
        (_, EvaluationVerdict::Pass, _) => "candidate improved to a passing external verdict",
        _ => "external evaluator did not establish a passing candidate result",
    };
    Some(CausalComparison {
        comparison_id: format!("external-comparison-{digest:x}"),
        replay_id: record.comparison_group_id.clone().unwrap_or_default(),
        quality_delta,
        utility_delta: None,
        security_delta: None,
        token_delta: None,
        cost_delta_usd: None,
        latency_delta_ms: None,
        scaffold_inflation: false,
        conclusion: conclusion.to_owned(),
        baseline_evaluation_ref: Some(baseline.evaluation_id.clone()),
        candidate_evaluation_ref: Some(candidate.evaluation_id.clone()),
        partition: Some(record.partition),
        provider_variant: record.provider_variant.clone(),
        seed: record.seed,
    })
}

fn normalized_external_score(record: &ExternalEvaluationRecord) -> Option<f32> {
    record
        .score
        .zip(record.score_max)
        .map(|(score, maximum)| (score / maximum) as f32)
        .filter(|score| score.is_finite())
}

fn execution_matches_case(
    execution: &RegressionExecution,
    case_ref: &str,
    campaign_case_count: usize,
) -> bool {
    execution.case_ref == case_ref || (execution.case_ref.is_empty() && campaign_case_count == 1)
}

fn execution_verdict(execution: Option<&RegressionExecution>) -> EvaluationVerdict {
    match execution.map(|execution| execution.status) {
        Some(RegressionExecutionStatus::Succeeded) => EvaluationVerdict::Pass,
        Some(RegressionExecutionStatus::Failed) => EvaluationVerdict::Fail,
        Some(
            RegressionExecutionStatus::Queued
            | RegressionExecutionStatus::Running
            | RegressionExecutionStatus::Inconclusive,
        )
        | None => EvaluationVerdict::Unknown,
    }
}

fn valid_execution_trace_ref(reference: &str) -> bool {
    reference.starts_with("runtime://")
        || reference.starts_with("execution://")
        || reference.starts_with("artifact://regression-trace/")
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
        baseline_evaluation_ref: None,
        candidate_evaluation_ref: None,
        partition: None,
        provider_variant: None,
        seed: None,
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
