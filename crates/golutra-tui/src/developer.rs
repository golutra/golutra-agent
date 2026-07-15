//! 开发者模式的运行时观察数据。

use golutra_client::{RuntimeClient, RuntimeTransport};
use golutra_core::{ActorKind, QueryId, SessionId, TaskId};
use golutra_protocol::{
    DebugProjection, RuntimeEvent, RuntimeEventType, RuntimeQuery, RuntimeQueryKind,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DeveloperPanelRow {
    Summary(String),
    Event {
        sequence_no: u64,
        label: String,
        summary: String,
    },
}

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
    serde_json::from_value(value).map_err(|error| format!("debug projection is invalid: {error}"))
}

pub(crate) fn developer_panel_rows(
    projection: &DebugProjection,
    recent_event_limit: usize,
) -> Vec<DeveloperPanelRow> {
    let event_count = projection.events.len();
    let policy_count = projection
        .events
        .iter()
        .filter(|event| event.event_type == RuntimeEventType::PolicyEvaluated)
        .count();
    let checkpoint_count = count_events(&projection.events, RuntimeEventType::CheckpointCreated);
    let context_count = count_events(&projection.events, RuntimeEventType::ContextBuilt);
    let provider_count = count_events(&projection.events, RuntimeEventType::ProviderStarted);
    let token_count = count_events(&projection.events, RuntimeEventType::TokenUsageRecorded);
    let retry_count = count_events(&projection.events, RuntimeEventType::RetryScheduled);
    let fallback_count = count_events(&projection.events, RuntimeEventType::ProviderFallback);
    let verification_line = projection.verification.as_ref().map_or_else(
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
    let loop_line = projection.loop_decisions.last().map_or_else(
        || "pending".to_owned(),
        |decision| {
            format!(
                "{:?} {}",
                decision.action,
                compact_text(&decision.reason, 88)
            )
        },
    );
    let evaluation_counts = [
        (
            "reviews",
            count_events(&projection.events, RuntimeEventType::PostTaskReviewed),
        ),
        (
            "evaluations",
            count_events(&projection.events, RuntimeEventType::EvaluationCompleted),
        ),
        (
            "improvements",
            count_events(
                &projection.events,
                RuntimeEventType::ImprovementCandidateCreated,
            ),
        ),
        (
            "regressions",
            count_events(&projection.events, RuntimeEventType::RegressionCompleted),
        ),
        (
            "promotions",
            count_events(&projection.events, RuntimeEventType::PromotionDecided),
        ),
        (
            "applied",
            count_events(&projection.events, RuntimeEventType::CandidateApplied),
        ),
    ];

    let mut rows = vec![
        DeveloperPanelRow::Summary(format!(
            "facts events={} tools={} artifacts={} evidence={} checkpoints={} policy={}",
            event_count,
            projection.tool_results.len(),
            projection.artifacts.len(),
            projection.evidence.len(),
            checkpoint_count,
            policy_count
        )),
        DeveloperPanelRow::Summary(format!(
            "model context={} provider_calls={} token_records={} retries={} fallbacks={}",
            context_count, provider_count, token_count, retry_count, fallback_count
        )),
        DeveloperPanelRow::Summary(format!("verify {verification_line}")),
        DeveloperPanelRow::Summary(format!("loop {loop_line}")),
        DeveloperPanelRow::Summary(
            evaluation_counts
                .iter()
                .map(|(label, count)| format!("{label}={count}"))
                .collect::<Vec<_>>()
                .join(" "),
        ),
    ];

    let start = projection.events.len().saturating_sub(recent_event_limit);
    rows.extend(projection.events[start..].iter().map(developer_event_row));
    rows
}

fn developer_event_row(event: &RuntimeEvent) -> DeveloperPanelRow {
    DeveloperPanelRow::Event {
        sequence_no: event.sequence_no,
        label: format!("{:?}/{:?}", event.event_type, event.source),
        summary: event_summary(event),
    }
}

fn count_events(events: &[RuntimeEvent], event_type: RuntimeEventType) -> usize {
    events
        .iter()
        .filter(|event| event.event_type == event_type)
        .count()
}

fn event_summary(event: &RuntimeEvent) -> String {
    event
        .payload
        .get("summary")
        .and_then(|value| value.as_str())
        .or_else(|| event.payload.get("error").and_then(|value| value.as_str()))
        .map_or_else(
            || "runtime event recorded".to_owned(),
            |value| compact_text(value, 96),
        )
}

fn compact_text(value: &str, max_chars: usize) -> String {
    let mut text = value.chars().take(max_chars).collect::<String>();
    if value.chars().count() > max_chars {
        text.push_str("...");
    }
    text
}
