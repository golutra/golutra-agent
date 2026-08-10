//! View models for the developer pane.

use golutra_core::EventId;
use golutra_protocol::{DebugProjection, RuntimeEvent, RuntimeEventType};

use super::{DeveloperFactsProjection, developer_facts_projection};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DeveloperPanelRow {
    Summary(String),
    Event {
        sequence_no: u64,
        end_sequence_no: u64,
        label: String,
        summary: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DeveloperEventProjection {
    pub(crate) event_ids: Vec<EventId>,
    pub(crate) sequence_no: u64,
    pub(crate) end_sequence_no: u64,
    pub(crate) label: String,
    summary: String,
    event_type: RuntimeEventType,
}

impl DeveloperEventProjection {
    fn from_event(event: &RuntimeEvent) -> Self {
        Self {
            event_ids: vec![event.id],
            sequence_no: event.sequence_no,
            end_sequence_no: event.sequence_no,
            label: format!("{:?}/{:?}", event.event_type, event.source),
            summary: developer_event_summary(event),
            event_type: event.event_type,
        }
    }

    fn push(&mut self, event: &RuntimeEvent) {
        self.event_ids.push(event.id);
        self.end_sequence_no = event.sequence_no;
    }

    pub(crate) fn summary(&self) -> String {
        if self.event_ids.len() == 1 {
            self.summary.clone()
        } else {
            format!("{} ({} events)", self.summary, self.event_ids.len())
        }
    }

    pub(crate) fn is_open_provider_stream(&self) -> bool {
        self.event_type == RuntimeEventType::ProviderStreamed
    }
}

pub(crate) fn developer_event_projections<'a>(
    events: impl IntoIterator<Item = &'a RuntimeEvent>,
) -> Vec<DeveloperEventProjection> {
    let mut projections = Vec::<DeveloperEventProjection>::new();
    let mut previous = None;

    for event in events {
        if previous.is_some_and(|previous| provider_stream_events_are_contiguous(previous, event)) {
            projections
                .last_mut()
                .expect("a previous event has a developer projection")
                .push(event);
        } else {
            projections.push(DeveloperEventProjection::from_event(event));
        }
        previous = Some(event);
    }

    projections
}

fn provider_stream_events_are_contiguous(previous: &RuntimeEvent, event: &RuntimeEvent) -> bool {
    previous.event_type == RuntimeEventType::ProviderStreamed
        && event.event_type == RuntimeEventType::ProviderStreamed
        && previous.sequence_no.checked_add(1) == Some(event.sequence_no)
        && previous.session_id == event.session_id
        && previous.task_id == event.task_id
        && previous.turn_id == event.turn_id
        && previous.source == event.source
}

#[cfg(test)]
pub(crate) fn developer_panel_rows(
    projection: &DebugProjection,
    recent_event_limit: usize,
) -> Vec<DeveloperPanelRow> {
    developer_panel_rows_with_changes(projection, None, recent_event_limit)
}

pub(crate) fn developer_panel_rows_with_changes(
    projection: &DebugProjection,
    changes: Option<&golutra_core::TurnChangeSummary>,
    recent_event_limit: usize,
) -> Vec<DeveloperPanelRow> {
    let facts = developer_facts_projection(projection, changes);
    let mut rows = summary_rows(&facts);
    let start = projection.events.len().saturating_sub(recent_event_limit);
    rows.extend(
        developer_event_projections(projection.events[start..].iter())
            .into_iter()
            .map(developer_event_row),
    );
    rows
}

fn summary_rows(facts: &DeveloperFactsProjection) -> Vec<DeveloperPanelRow> {
    let verification = facts.verification.as_ref().map_or_else(
        || "pending".to_owned(),
        |verification| {
            format!(
                "{:?} checks={} evidence={} risks={}",
                verification.result,
                verification.checks.len(),
                verification.evidence_refs.len(),
                verification.residual_risks.len()
            )
        },
    );
    let loop_decision = facts.loop_decision.as_ref().map_or_else(
        || "pending".to_owned(),
        |decision| {
            format!(
                "{:?} {}",
                decision.action,
                compact_text(&decision.reason, 88)
            )
        },
    );
    let mut rows = vec![
        DeveloperPanelRow::Summary(format!(
            "facts events={} tools={} artifacts={} evidence={} checkpoints={} policy={}",
            facts.event_count,
            facts.tool_count,
            facts.artifact_count,
            facts.evidence_count,
            facts.checkpoint_count,
            facts.policy_count
        )),
        DeveloperPanelRow::Summary(format!(
            "model context={} provider_calls={} token_records={} retries={} fallbacks={} transport_fallbacks={}",
            facts.context_count,
            facts.provider_count,
            facts.token_count,
            facts.retry_count,
            facts.fallback_count,
            facts.transport_fallback_count
        )),
    ];
    if let Some(changes) = &facts.changes {
        let stats = match (changes.added_lines, changes.removed_lines) {
            (Some(added), Some(removed)) => format!("+{added} -{removed}"),
            _ => "line_stats=unavailable".to_owned(),
        };
        rows.push(DeveloperPanelRow::Summary(format!(
            "changes files={} {stats} complete={}",
            changes.files.len(),
            changes.stats_complete
        )));
    }
    if let Some(diagnosis) = &facts.diagnosis {
        rows.push(DeveloperPanelRow::Summary(format!(
            "diagnosis {diagnosis} slice_events={} omitted={} complete={}",
            facts.diagnostic_event_count,
            facts.diagnostic_omitted_event_count,
            facts.diagnostic_complete
        )));
    }
    if facts.active_failure_episodes > 0
        || facts.recovered_failure_episodes > 0
        || facts.superseded_failure_episodes > 0
    {
        rows.push(DeveloperPanelRow::Summary(format!(
            "failure_episodes active={} recovered={} superseded={}",
            facts.active_failure_episodes,
            facts.recovered_failure_episodes,
            facts.superseded_failure_episodes
        )));
    }
    if facts.replay_status.is_some()
        || facts.external_evaluation_count > 0
        || facts.causal_comparison_count > 0
    {
        rows.push(DeveloperPanelRow::Summary(format!(
            "replay={} external_evaluations={} causal_comparisons={}",
            facts.replay_status.as_deref().unwrap_or("not_run"),
            facts.external_evaluation_count,
            facts.causal_comparison_count
        )));
    }
    rows.extend([
        DeveloperPanelRow::Summary(format!("verify {verification}")),
        DeveloperPanelRow::Summary(format!("loop {loop_decision}")),
        DeveloperPanelRow::Summary(format!(
            "reviews={} evaluations={} improvements={} regressions={} promotions={} applied={}",
            facts.evaluation.reviews,
            facts.evaluation.evaluations,
            facts.evaluation.improvements,
            facts.evaluation.regressions,
            facts.evaluation.promotions,
            facts.evaluation.applied
        )),
        DeveloperPanelRow::Summary(format!(
            "jobs terminal={}/{}",
            facts.terminal_jobs, facts.job_count
        )),
        DeveloperPanelRow::Summary(format!(
            "trace complete={} missing={} retention_losses={}",
            facts.trace_complete, facts.missing_sections, facts.retention_losses
        )),
    ]);
    rows
}

fn developer_event_row(event: DeveloperEventProjection) -> DeveloperPanelRow {
    let summary = event.summary();
    DeveloperPanelRow::Event {
        sequence_no: event.sequence_no,
        end_sequence_no: event.end_sequence_no,
        label: event.label,
        summary,
    }
}

pub(crate) fn developer_event_summary(event: &RuntimeEvent) -> String {
    event
        .payload
        .get("summary")
        .and_then(|value| value.as_str())
        .or_else(|| event.payload.get("error").and_then(|value| value.as_str()))
        .map_or_else(|| "runtime event recorded".to_owned(), str::to_owned)
}

fn compact_text(value: &str, max_chars: usize) -> String {
    let mut text = value.chars().take(max_chars).collect::<String>();
    if value.chars().count() > max_chars {
        text.push_str("...");
    }
    text
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use golutra_core::{SessionId, TaskId, TurnId};
    use golutra_protocol::{RuntimeEventSource, RuntimeEventType};
    use serde_json::json;

    use super::*;

    fn event(
        sequence_no: u64,
        session_id: SessionId,
        task_id: TaskId,
        turn_id: TurnId,
        event_type: RuntimeEventType,
    ) -> RuntimeEvent {
        RuntimeEvent {
            schema_version: golutra_core::RUNTIME_EVENT_SCHEMA_VERSION,
            id: EventId::new(),
            sequence_no,
            session_id,
            turn_id: Some(turn_id),
            task_id: Some(task_id),
            parent_event_id: None,
            causal_context: Default::default(),
            causal_links: Vec::new(),
            event_type,
            timestamp: Utc::now(),
            source: RuntimeEventSource::Provider,
            payload: json!({"summary": "provider response delta received"}),
            payload_ref: None,
            durable: true,
        }
    }

    #[test]
    fn consecutive_provider_deltas_are_one_lossless_display_projection() {
        let session_id = SessionId::new();
        let task_id = TaskId::new();
        let turn_id = TurnId::new();
        let events = vec![
            event(
                10,
                session_id,
                task_id,
                turn_id,
                RuntimeEventType::ProviderStreamed,
            ),
            event(
                11,
                session_id,
                task_id,
                turn_id,
                RuntimeEventType::ProviderStreamed,
            ),
            event(
                12,
                session_id,
                task_id,
                turn_id,
                RuntimeEventType::ProviderCompleted,
            ),
        ];

        let projected = developer_event_projections(&events);

        assert_eq!(projected.len(), 2);
        assert_eq!(projected[0].sequence_no, 10);
        assert_eq!(projected[0].end_sequence_no, 11);
        assert_eq!(projected[0].event_ids, vec![events[0].id, events[1].id]);
        assert_eq!(
            projected[0].summary(),
            "provider response delta received (2 events)"
        );
        assert!(projected[0].is_open_provider_stream());
        assert_eq!(projected[1].event_ids, vec![events[2].id]);
    }

    #[test]
    fn provider_delta_groups_do_not_cross_sequence_or_turn_boundaries() {
        let session_id = SessionId::new();
        let task_id = TaskId::new();
        let first_turn = TurnId::new();
        let second_turn = TurnId::new();
        let events = vec![
            event(
                1,
                session_id,
                task_id,
                first_turn,
                RuntimeEventType::ProviderStreamed,
            ),
            event(
                3,
                session_id,
                task_id,
                first_turn,
                RuntimeEventType::ProviderStreamed,
            ),
            event(
                4,
                session_id,
                task_id,
                second_turn,
                RuntimeEventType::ProviderStreamed,
            ),
        ];

        let projected = developer_event_projections(&events);

        assert_eq!(projected.len(), 3);
        assert!(projected.iter().all(|event| event.event_ids.len() == 1));
    }
}
