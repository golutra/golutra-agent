//! Runtime queries and pagination for the developer projection.

use golutra_client::{RuntimeClient, RuntimeTransport};
use golutra_core::{ActorKind, QueryId, SessionId, TaskId};
use golutra_protocol::{
    DebugProjection, EventPageDirection, EventPageRequest, RuntimeQuery, RuntimeQueryKind,
};

pub(crate) async fn load_debug_projection(
    transport: &RuntimeTransport,
    session_id: SessionId,
    task_id: Option<TaskId>,
) -> Result<DebugProjection, String> {
    let value = transport
        .query(RuntimeQuery {
            query_id: QueryId::new(),
            session_id,
            task_id,
            kind: RuntimeQueryKind::DebugProjection,
            requester: ActorKind::Tui,
            cursor: None,
            timestamp: chrono::Utc::now(),
        })
        .await
        .map_err(|error| error.to_string())?;
    let mut projection: DebugProjection = serde_json::from_value(value)
        .map_err(|error| format!("debug projection is invalid: {error}"))?;
    let page = transport
        .event_page(EventPageRequest {
            session_id,
            task_id,
            cursor: None,
            direction: EventPageDirection::Backward,
            limit: 256,
        })
        .await
        .map_err(|error| error.to_string())?;
    projection.events = page.events;
    projection.event_window.start_cursor = page.start_cursor;
    projection.event_window.end_cursor = page.end_cursor;
    projection.event_window.has_more_before = page.has_more;
    projection.event_window.limit = 256;
    refresh_debug_completeness(&mut projection);
    Ok(projection)
}

pub(crate) fn merge_debug_projection(
    mut previous: DebugProjection,
    latest: DebugProjection,
) -> DebugProjection {
    previous.events.extend(latest.events);
    previous.events.sort_by_key(|event| event.sequence_no);
    previous.events.dedup_by_key(|event| event.sequence_no);
    previous.event_window.start_cursor = previous
        .event_window
        .start_cursor
        .into_iter()
        .chain(latest.event_window.start_cursor)
        .min();
    previous.event_window.end_cursor = previous
        .event_window
        .end_cursor
        .into_iter()
        .chain(latest.event_window.end_cursor)
        .max();
    previous.event_window.has_more_before =
        previous.event_window.has_more_before && latest.event_window.has_more_before;
    previous.event_window.limit = latest.event_window.limit;
    previous
        .busy_policy_decisions
        .extend(latest.busy_policy_decisions);
    previous
        .busy_policy_decisions
        .sort_by_key(|decision| decision.decision_id);
    previous
        .busy_policy_decisions
        .dedup_by_key(|decision| decision.decision_id);
    previous.tool_results.extend(latest.tool_results);
    previous.tool_results.dedup();
    previous.artifacts.extend(latest.artifacts);
    previous
        .artifacts
        .sort_by_key(|artifact| artifact.artifact_id);
    previous
        .artifacts
        .dedup_by_key(|artifact| artifact.artifact_id);
    previous.evidence.extend(latest.evidence);
    previous.evidence.sort_by_key(|record| record.evidence_id);
    previous.evidence.dedup_by_key(|record| record.evidence_id);
    if latest.verification.is_some() {
        previous.verification = latest.verification;
    }
    previous.loop_decisions.extend(latest.loop_decisions);
    previous
        .loop_decisions
        .sort_by_key(|decision| decision.decision_id);
    previous
        .loop_decisions
        .dedup_by_key(|decision| decision.decision_id);
    previous.post_task_jobs.extend(latest.post_task_jobs);
    previous.post_task_jobs.sort_by_key(|job| job.created_at);
    previous.post_task_jobs.dedup_by_key(|job| job.job_id);
    if latest.failure_diagnosis.is_some() {
        previous.failure_diagnosis = latest.failure_diagnosis;
    }
    if latest.diagnostic_slice.is_some() {
        previous.diagnostic_slice = latest.diagnostic_slice;
    }
    if latest.replay_execution.is_some() {
        previous.replay_execution = latest.replay_execution;
    }
    previous.external_evaluations = latest.external_evaluations;
    previous.causal_comparisons = latest.causal_comparisons;
    previous.trace_complete = latest.trace_complete;
    previous.missing_sections = latest.missing_sections;
    previous.retention_losses = latest.retention_losses;
    refresh_debug_completeness(&mut previous);
    previous
}

pub(crate) async fn load_older_debug_events(
    transport: &RuntimeTransport,
    projection: &mut DebugProjection,
) -> Result<bool, String> {
    if !projection.event_window.has_more_before {
        return Ok(false);
    }
    let page = transport
        .event_page(EventPageRequest {
            session_id: projection.session_id,
            task_id: projection.task_id,
            cursor: projection.event_window.start_cursor,
            direction: EventPageDirection::Backward,
            limit: projection.event_window.limit.max(1),
        })
        .await
        .map_err(|error| error.to_string())?;
    if page.events.is_empty() {
        projection.event_window.has_more_before = false;
        return Ok(false);
    }
    let mut older = page.events;
    older.append(&mut projection.events);
    older.sort_by_key(|event| event.sequence_no);
    older.dedup_by_key(|event| event.sequence_no);
    projection.events = older;
    projection.event_window.start_cursor = page.start_cursor;
    projection.event_window.has_more_before = page.has_more;
    refresh_debug_completeness(projection);
    Ok(true)
}

fn refresh_debug_completeness(projection: &mut DebugProjection) {
    projection
        .missing_sections
        .retain(|section| section != "event_window");
    if projection.event_window.has_more_before {
        projection.missing_sections.push("event_window".to_owned());
    }
    projection.missing_sections.sort();
    projection.missing_sections.dedup();
    projection.trace_complete = projection.missing_sections.is_empty()
        && projection.retention_losses.is_empty()
        && projection.post_task_jobs.iter().all(|job| {
            matches!(
                job.status,
                golutra_core::PostTaskJobStatus::Succeeded
                    | golutra_core::PostTaskJobStatus::Failed
                    | golutra_core::PostTaskJobStatus::Cancelled
            )
        });
}
