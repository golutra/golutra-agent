//! View models for the developer pane.

use golutra_protocol::{DebugProjection, RuntimeEvent};

use super::{DeveloperFactsProjection, developer_facts_projection};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DeveloperPanelRow {
    Summary(String),
    Event {
        sequence_no: u64,
        label: String,
        summary: String,
    },
}

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
    rows.extend(projection.events[start..].iter().map(developer_event_row));
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

fn developer_event_row(event: &RuntimeEvent) -> DeveloperPanelRow {
    DeveloperPanelRow::Event {
        sequence_no: event.sequence_no,
        label: format!("{:?}/{:?}", event.event_type, event.source),
        summary: event_summary(event),
    }
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
