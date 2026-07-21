//! Typed facts projected from the canonical developer trace.

use golutra_core::{LoopDecision, TurnChangeSummary, VerificationRecord};
use golutra_protocol::{DebugProjection, RuntimeEvent, RuntimeEventType};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct EvaluationFactCounts {
    pub(crate) reviews: usize,
    pub(crate) evaluations: usize,
    pub(crate) improvements: usize,
    pub(crate) regressions: usize,
    pub(crate) promotions: usize,
    pub(crate) applied: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DeveloperFactsProjection {
    pub(crate) event_count: usize,
    pub(crate) tool_count: usize,
    pub(crate) artifact_count: usize,
    pub(crate) evidence_count: usize,
    pub(crate) checkpoint_count: usize,
    pub(crate) policy_count: usize,
    pub(crate) context_count: usize,
    pub(crate) provider_count: usize,
    pub(crate) token_count: usize,
    pub(crate) retry_count: usize,
    pub(crate) fallback_count: usize,
    pub(crate) verification: Option<VerificationRecord>,
    pub(crate) loop_decision: Option<LoopDecision>,
    pub(crate) evaluation: EvaluationFactCounts,
    pub(crate) terminal_jobs: usize,
    pub(crate) job_count: usize,
    pub(crate) trace_complete: bool,
    pub(crate) missing_sections: usize,
    pub(crate) retention_losses: usize,
    pub(crate) changes: Option<TurnChangeSummary>,
}

pub(crate) fn developer_facts_projection(
    projection: &DebugProjection,
    changes: Option<&TurnChangeSummary>,
) -> DeveloperFactsProjection {
    let terminal_jobs = projection
        .post_task_jobs
        .iter()
        .filter(|job| {
            matches!(
                job.status,
                golutra_core::PostTaskJobStatus::Succeeded
                    | golutra_core::PostTaskJobStatus::Failed
                    | golutra_core::PostTaskJobStatus::Cancelled
            )
        })
        .count();
    DeveloperFactsProjection {
        event_count: projection.events.len(),
        tool_count: projection.tool_results.len(),
        artifact_count: projection.artifacts.len(),
        evidence_count: projection.evidence.len(),
        checkpoint_count: count_events(&projection.events, RuntimeEventType::CheckpointCreated),
        policy_count: count_events(&projection.events, RuntimeEventType::PolicyEvaluated),
        context_count: count_events(&projection.events, RuntimeEventType::ContextBuilt),
        provider_count: count_events(&projection.events, RuntimeEventType::ProviderStarted),
        token_count: count_events(&projection.events, RuntimeEventType::TokenUsageRecorded),
        retry_count: count_events(&projection.events, RuntimeEventType::RetryScheduled),
        fallback_count: count_events(&projection.events, RuntimeEventType::ProviderFallback),
        verification: projection.verification.clone(),
        loop_decision: projection.loop_decisions.last().cloned(),
        evaluation: EvaluationFactCounts {
            reviews: count_events(&projection.events, RuntimeEventType::PostTaskReviewed),
            evaluations: count_events(&projection.events, RuntimeEventType::EvaluationCompleted),
            improvements: count_events(
                &projection.events,
                RuntimeEventType::ImprovementCandidateCreated,
            ),
            regressions: count_events(&projection.events, RuntimeEventType::RegressionCompleted),
            promotions: count_events(&projection.events, RuntimeEventType::PromotionDecided),
            applied: count_events(&projection.events, RuntimeEventType::CandidateApplied),
        },
        terminal_jobs,
        job_count: projection.post_task_jobs.len(),
        trace_complete: projection.trace_complete,
        missing_sections: projection.missing_sections.len(),
        retention_losses: projection.retention_losses.len(),
        changes: changes.cloned(),
    }
}

fn count_events(events: &[RuntimeEvent], event_type: RuntimeEventType) -> usize {
    events
        .iter()
        .filter(|event| event.event_type == event_type)
        .count()
}
