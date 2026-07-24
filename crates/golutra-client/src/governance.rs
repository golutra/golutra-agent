//! 任务完成后的评估、投影与记忆隔离用例。
//!
//! RuntimeHost 只负责执行顺序和事件提交；该模块持有治理存储依赖，统一生成
//! minimal/deep evaluation，并把 context/evaluation facts 组装为类型化投影。

use std::collections::HashSet;

use golutra_core::{EvidenceId, PostTaskJobStatus, SessionId, TaskId};
use golutra_eval::{
    AutomationCandidate, EvaluationResult, EvaluationRunner, EvaluationStore, ImprovementCandidate,
    PostTaskReview, TaskEvaluationBundle, TaskEvaluationInput,
};
use golutra_memory::{MemoryError, MemoryRecord, MemoryStore, propose_project_memory};
use golutra_protocol::{ContextProjection, EvaluationProjection, RuntimeEventType};
use golutra_store::RuntimeRepositories;

use super::{ClientError, compact_history_text, run_blocking};

#[derive(Debug, Clone)]
pub(crate) struct GovernanceService {
    repositories: RuntimeRepositories,
    evaluation_store: EvaluationStore,
    memory_store: MemoryStore,
}

#[derive(Debug, Clone)]
pub(crate) struct RecordedTaskEvaluation {
    pub(crate) result: EvaluationResult,
    pub(crate) review: PostTaskReview,
    pub(crate) improvement_candidate: Option<ImprovementCandidate>,
    pub(crate) automation_candidates: Vec<AutomationCandidate>,
}

impl GovernanceService {
    #[must_use]
    pub(crate) fn new(
        repositories: RuntimeRepositories,
        evaluation_store: EvaluationStore,
        memory_store: MemoryStore,
    ) -> Self {
        Self {
            repositories,
            evaluation_store,
            memory_store,
        }
    }

    #[must_use]
    pub(crate) fn evaluate_minimal(&self, input: TaskEvaluationInput) -> TaskEvaluationBundle {
        EvaluationRunner.evaluate_minimal(input)
    }

    #[must_use]
    pub(crate) fn evaluate_deep(&self, input: TaskEvaluationInput) -> TaskEvaluationBundle {
        EvaluationRunner.evaluate_task(input)
    }

    pub(crate) async fn persist_evaluation(
        &self,
        bundle: TaskEvaluationBundle,
    ) -> Result<RecordedTaskEvaluation, ClientError> {
        let recorded = RecordedTaskEvaluation {
            result: bundle.result.clone(),
            review: bundle.review.clone(),
            improvement_candidate: bundle.improvement_candidate.clone(),
            automation_candidates: bundle.automation_candidates.clone(),
        };
        let store = self.evaluation_store.clone();
        run_blocking(move || store.record_task_evaluation(bundle)).await??;
        Ok(recorded)
    }

    pub(crate) async fn candidate_source_task_id(
        &self,
        candidate_id: &str,
    ) -> Result<TaskId, ClientError> {
        let store = self.evaluation_store.clone();
        let candidate_id = candidate_id.to_owned();
        let task_id = run_blocking(move || {
            let state = store.snapshot()?;
            state
                .automation_candidates
                .iter()
                .find(|candidate| candidate.id == candidate_id)
                .map(|candidate| candidate.source_task_id)
                .or_else(|| {
                    state
                        .improvement_candidates
                        .iter()
                        .find(|candidate| candidate.id == candidate_id)
                        .map(|candidate| candidate.source_task_id)
                })
                .ok_or(golutra_eval::EvaluationError::CandidateNotFound(
                    candidate_id,
                ))
        })
        .await??;
        Ok(task_id)
    }

    pub(crate) async fn context_projection(
        &self,
        session_id: SessionId,
        task_id: TaskId,
    ) -> Result<ContextProjection, ClientError> {
        let snapshots = self.repositories.artifacts.contexts(task_id).await?;
        let mut integrity_warnings = Vec::new();
        for snapshot in &snapshots {
            if snapshot.session_id != session_id {
                integrity_warnings.push(format!(
                    "context snapshot {} belongs to another session",
                    snapshot.snapshot_id
                ));
            }
            if snapshot.canonical_request_digest.trim().is_empty() {
                integrity_warnings.push(format!(
                    "context snapshot {} has no canonical request digest",
                    snapshot.snapshot_id
                ));
            }
            if snapshot.redacted_request_artifact_ref.is_none() {
                integrity_warnings.push(format!(
                    "context snapshot {} has no redacted request artifact",
                    snapshot.snapshot_id
                ));
            }
        }
        let latest = snapshots.last().cloned();
        let complete = !snapshots.is_empty() && integrity_warnings.is_empty();
        Ok(ContextProjection {
            session_id,
            task_id,
            snapshots,
            latest,
            complete,
            integrity_warnings,
        })
    }

    pub(crate) async fn evaluation_projection(
        &self,
        session_id: SessionId,
        task_id: TaskId,
    ) -> Result<EvaluationProjection, ClientError> {
        let store = self.evaluation_store.clone();
        let state = run_blocking(move || store.snapshot()).await??;
        let reviews = state
            .reviews
            .into_iter()
            .filter(|review| review.task_id == task_id)
            .collect::<Vec<_>>();
        let results = state
            .results
            .into_iter()
            .filter(|result| result.source_task_id == task_id)
            .collect::<Vec<_>>();
        let improvement_candidates = state
            .improvement_candidates
            .into_iter()
            .filter(|candidate| candidate.source_task_id == task_id)
            .collect::<Vec<_>>();
        let automation_candidates = state
            .automation_candidates
            .into_iter()
            .filter(|candidate| candidate.source_task_id == task_id)
            .collect::<Vec<_>>();
        let candidate_ids = automation_candidates
            .iter()
            .map(|candidate| candidate.id.as_str())
            .chain(
                improvement_candidates
                    .iter()
                    .map(|candidate| candidate.id.as_str()),
            )
            .collect::<HashSet<_>>();
        let regressions = state
            .regressions
            .into_iter()
            .filter(|regression| candidate_ids.contains(regression.candidate_id.as_str()))
            .collect::<Vec<_>>();
        let promotion_decisions = state
            .promotion_decisions
            .into_iter()
            .filter(|decision| candidate_ids.contains(decision.candidate_id.as_str()))
            .collect::<Vec<_>>();
        let failure_diagnoses = state
            .failure_diagnoses
            .into_iter()
            .filter(|diagnosis| diagnosis.source_task_id == task_id)
            .collect::<Vec<_>>();
        let diagnostic_slices = state
            .diagnostic_slices
            .into_iter()
            .filter(|slice| slice.source_task_id == task_id)
            .collect::<Vec<_>>();
        let replay_capsules = state
            .replay_capsules
            .into_iter()
            .filter(|capsule| capsule.source_task_id == task_id)
            .collect::<Vec<_>>();
        let replay_executions = state
            .replay_executions
            .into_iter()
            .filter(|execution| execution.source_task_id == task_id)
            .collect::<Vec<_>>();
        let external_evaluations = state
            .external_evaluations
            .into_iter()
            .filter(|evaluation| evaluation.source_task_id == task_id)
            .collect::<Vec<_>>();
        let external_evaluation_ids = external_evaluations
            .iter()
            .map(|evaluation| evaluation.evaluation_id.as_str())
            .collect::<HashSet<_>>();
        let causal_comparison_ids = regressions
            .iter()
            .flat_map(|regression| regression.causal_comparison_refs.iter().map(String::as_str))
            .collect::<HashSet<_>>();
        let causal_comparisons = state
            .causal_comparisons
            .into_iter()
            .filter(|comparison| {
                causal_comparison_ids.contains(comparison.comparison_id.as_str())
                    || comparison
                        .baseline_evaluation_ref
                        .as_deref()
                        .is_some_and(|reference| external_evaluation_ids.contains(reference))
                    || comparison
                        .candidate_evaluation_ref
                        .as_deref()
                        .is_some_and(|reference| external_evaluation_ids.contains(reference))
            })
            .collect::<Vec<_>>();
        let events = self
            .repositories
            .events
            .load(session_id, Some(task_id), None)
            .await?;
        let mut integrity_warnings = Vec::new();
        let event_contains_id = |event_type: RuntimeEventType, field: &str, id: &str| {
            events.iter().any(|event| {
                event.event_type == event_type
                    && (event
                        .payload
                        .get("record")
                        .and_then(|record| record.get(field))
                        .and_then(serde_json::Value::as_str)
                        == Some(id)
                        || event
                            .payload
                            .get("records")
                            .and_then(serde_json::Value::as_array)
                            .is_some_and(|records| {
                                records.iter().any(|record| {
                                    record.get(field).and_then(serde_json::Value::as_str)
                                        == Some(id)
                                })
                            }))
            })
        };
        for review in &reviews {
            let mode = serde_json::to_value(review.mode)?;
            let review_task_id = review.task_id.to_string();
            let has_event = events.iter().any(|event| {
                event.event_type == RuntimeEventType::PostTaskReviewed
                    && event.payload.get("record").is_some_and(|record| {
                        record.get("task_id").and_then(serde_json::Value::as_str)
                            == Some(review_task_id.as_str())
                            && record.get("mode") == Some(&mode)
                    })
            });
            if !has_event {
                integrity_warnings.push(format!(
                    "PostTaskReview for task {} has no canonical event",
                    review.task_id
                ));
            }
        }
        for result in &results {
            if !event_contains_id(
                RuntimeEventType::EvaluationCompleted,
                "result_id",
                &result.result_id,
            ) {
                integrity_warnings.push(format!(
                    "EvaluationResult {} has no canonical event",
                    result.result_id
                ));
            }
        }
        for candidate in &improvement_candidates {
            if !event_contains_id(
                RuntimeEventType::ImprovementCandidateCreated,
                "id",
                &candidate.id,
            ) {
                integrity_warnings.push(format!(
                    "ImprovementCandidate {} has no canonical event",
                    candidate.id
                ));
            }
        }
        for candidate in &automation_candidates {
            if !event_contains_id(
                RuntimeEventType::AutomationCandidateCreated,
                "id",
                &candidate.id,
            ) {
                integrity_warnings.push(format!(
                    "AutomationCandidate {} has no canonical event",
                    candidate.id
                ));
            }
        }
        for regression in &regressions {
            if !event_contains_id(
                RuntimeEventType::RegressionCompleted,
                "regression_id",
                &regression.regression_id,
            ) {
                integrity_warnings.push(format!(
                    "RegressionResult {} has no canonical event",
                    regression.regression_id
                ));
            }
        }
        for decision in &promotion_decisions {
            if !event_contains_id(
                RuntimeEventType::PromotionDecided,
                "decision_id",
                &decision.decision_id,
            ) {
                integrity_warnings.push(format!(
                    "PromotionDecision {} has no canonical event",
                    decision.decision_id
                ));
            }
        }
        for diagnosis in &failure_diagnoses {
            if !event_contains_id(
                RuntimeEventType::FailureDiagnosed,
                "diagnosis_id",
                &diagnosis.diagnosis_id,
            ) {
                integrity_warnings.push(format!(
                    "FailureDiagnosis {} has no canonical event",
                    diagnosis.diagnosis_id
                ));
            }
        }
        for slice in &diagnostic_slices {
            if !event_contains_id(
                RuntimeEventType::DiagnosticSliceCreated,
                "slice_id",
                &slice.slice_id,
            ) {
                integrity_warnings.push(format!(
                    "DiagnosticSlice {} has no canonical event",
                    slice.slice_id
                ));
            }
        }
        for capsule in &replay_capsules {
            if !event_contains_id(
                RuntimeEventType::ReplayCapsuleCreated,
                "capsule_id",
                &capsule.capsule_id,
            ) {
                integrity_warnings.push(format!(
                    "ReplayCapsule {} has no canonical event",
                    capsule.capsule_id
                ));
            }
        }
        for execution in &replay_executions {
            if !event_contains_id(
                RuntimeEventType::ReplayExecuted,
                "execution_id",
                &execution.execution_id,
            ) {
                integrity_warnings.push(format!(
                    "ReplayExecution {} has no canonical event",
                    execution.execution_id
                ));
            }
        }
        for evaluation in &external_evaluations {
            if !event_contains_id(
                RuntimeEventType::ExternalEvaluationIngested,
                "evaluation_id",
                &evaluation.evaluation_id,
            ) {
                integrity_warnings.push(format!(
                    "ExternalEvaluationRecord {} has no canonical event",
                    evaluation.evaluation_id
                ));
            }
        }
        for comparison in &causal_comparisons {
            if !event_contains_id(
                RuntimeEventType::ExternalEvaluationCompared,
                "comparison_id",
                &comparison.comparison_id,
            ) && comparison.baseline_evaluation_ref.is_some()
            {
                integrity_warnings.push(format!(
                    "CausalComparison {} has no canonical event",
                    comparison.comparison_id
                ));
            }
        }
        let post_task_jobs = self.repositories.jobs.list_for_task(task_id).await?;
        let terminal = !post_task_jobs.is_empty()
            && post_task_jobs.iter().all(|job| {
                matches!(
                    job.status,
                    PostTaskJobStatus::Succeeded
                        | PostTaskJobStatus::Failed
                        | PostTaskJobStatus::Cancelled
                )
            });
        if terminal && reviews.is_empty() {
            integrity_warnings
                .push("post-task job is terminal but no PostTaskReview was persisted".to_owned());
        }
        if terminal && results.is_empty() {
            integrity_warnings
                .push("post-task job is terminal but no EvaluationResult was persisted".to_owned());
        }
        for decision in &promotion_decisions {
            if !regressions
                .iter()
                .any(|regression| regression.candidate_id == decision.candidate_id)
            {
                integrity_warnings.push(format!(
                    "promotion decision {} has no regression result",
                    decision.decision_id
                ));
            }
        }
        Ok(EvaluationProjection {
            session_id,
            task_id,
            reviews,
            results,
            improvement_candidates,
            automation_candidates,
            regressions,
            promotion_decisions,
            failure_diagnoses,
            diagnostic_slices,
            replay_capsules,
            replay_executions,
            external_evaluations,
            causal_comparisons,
            post_task_jobs,
            terminal,
            integrity_warnings,
        })
    }

    pub(crate) async fn quarantine_verified_memory(
        &self,
        task_id: TaskId,
        objective: &str,
        final_message: &str,
        tool_facts: &str,
        evidence_refs: Vec<EvidenceId>,
    ) -> Result<Result<MemoryRecord, MemoryError>, ClientError> {
        let content = format!(
            "Objective: {}\nVerified outcome: {}\nEvidence-backed facts: {}",
            compact_history_text(objective, 320),
            compact_history_text(final_message, 480),
            compact_history_text(tool_facts, 480),
        );
        let candidate = propose_project_memory(task_id, evidence_refs);
        let store = self.memory_store.clone();
        run_blocking(move || store.quarantine(&candidate, content)).await
    }
}
