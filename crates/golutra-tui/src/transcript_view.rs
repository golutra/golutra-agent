//! Pure mapping from runtime/user projections to transcript view models.

use std::collections::{HashMap, HashSet};

use golutra_core::FileChangeSummary;
use golutra_protocol::{RuntimeEvent, RuntimeEventType, UserProjection, VisibleStep};
use serde_json::Value;

use super::{TuiApp, operation_file_changes};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TranscriptRole {
    User,
    Assistant,
    Status,
    System,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TranscriptItem {
    pub(crate) role: TranscriptRole,
    pub(crate) title: String,
    pub(crate) body: Vec<String>,
}

pub(crate) fn transcript_items(app: &TuiApp) -> Vec<TranscriptItem> {
    if app.auth_dialog.is_some() {
        return Vec::new();
    }
    let mut items = Vec::new();
    let event_items = event_transcript_items(&app.events);
    let has_event_items = !event_items.is_empty();
    items.extend(event_items);
    items.extend(app.command_messages.clone());
    if let Some(projection) = &app.projection {
        if has_event_items {
            items.extend(projection_overlay_items(projection));
        } else {
            items.extend(projection_items(projection));
        }
    } else {
        items.push(TranscriptItem {
            role: TranscriptRole::System,
            title: "Connecting".to_owned(),
            body: vec!["loading runtime state".to_owned()],
        });
    }
    items
}

pub(crate) fn event_transcript_items(events: &[RuntimeEvent]) -> Vec<TranscriptItem> {
    let mut typed_events = events.iter().collect::<Vec<_>>();
    typed_events.sort_by_key(|event| event.sequence_no);

    let mut items = Vec::new();
    let mut visible_user_turns = HashSet::new();
    let mut streamed_assistant_items = HashMap::new();
    for event in typed_events {
        match event.event_type {
            RuntimeEventType::TaskCreated | RuntimeEventType::TurnQueued => {
                let is_new_turn = event
                    .turn_id
                    .is_none_or(|turn_id| visible_user_turns.insert(turn_id));
                if is_new_turn && let Some(item) = user_event_transcript_item(event) {
                    items.push(item);
                }
            }
            RuntimeEventType::ProviderStreamed => {
                let Some(delta) = provider_stream_text_delta(event) else {
                    continue;
                };
                let Some(turn_id) = event.turn_id else {
                    continue;
                };
                if let Some(index) = streamed_assistant_items.get(&turn_id).copied() {
                    if let Some(body) = items
                        .get_mut(index)
                        .and_then(|item: &mut TranscriptItem| item.body.first_mut())
                    {
                        body.push_str(delta);
                    }
                } else {
                    let index = items.len();
                    items.push(TranscriptItem {
                        role: TranscriptRole::Assistant,
                        title: "Golutra".to_owned(),
                        body: vec![delta.to_owned()],
                    });
                    streamed_assistant_items.insert(turn_id, index);
                }
            }
            RuntimeEventType::AssistantMessage => {
                if let Some(item) = assistant_event_transcript_item(event) {
                    if let Some(index) = event
                        .turn_id
                        .and_then(|turn_id| streamed_assistant_items.remove(&turn_id))
                    {
                        items[index] = item;
                    } else {
                        items.push(item);
                    }
                }
            }
            _ => {
                if let Some(item) = status_event_transcript_item(event) {
                    items.push(item);
                }
            }
        }
    }
    items
}

fn provider_stream_text_delta(event: &RuntimeEvent) -> Option<&str> {
    let delta = event.payload.get("delta")?;
    (delta.get("kind").and_then(Value::as_str) == Some("text_delta"))
        .then(|| delta.get("text").and_then(Value::as_str))
        .flatten()
        .filter(|text| !text.is_empty())
}

pub(crate) fn user_event_transcript_item(event: &RuntimeEvent) -> Option<TranscriptItem> {
    event
        .payload
        .get("payload")
        .and_then(|payload| payload.get("prompt"))
        .and_then(Value::as_str)
        .filter(|prompt| !prompt.trim().is_empty())
        .map(|prompt| TranscriptItem {
            role: TranscriptRole::User,
            title: "You".to_owned(),
            body: vec![prompt.to_owned()],
        })
}

pub(crate) fn assistant_event_transcript_item(event: &RuntimeEvent) -> Option<TranscriptItem> {
    event
        .payload
        .get("content")
        .and_then(Value::as_str)
        .filter(|content| !content.trim().is_empty())
        .map(|content| TranscriptItem {
            role: TranscriptRole::Assistant,
            title: "Golutra".to_owned(),
            body: vec![content.to_owned()],
        })
}

pub(crate) fn status_event_transcript_item(event: &RuntimeEvent) -> Option<TranscriptItem> {
    if event.event_type == RuntimeEventType::ApprovalRequested {
        let request = event.payload.get("request")?;
        let tool_name = request
            .get("tool_name")
            .and_then(Value::as_str)
            .unwrap_or("tool");
        let resource = request
            .get("resource")
            .and_then(Value::as_str)
            .unwrap_or("unknown resource");
        let reason = request
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or("explicit approval is required");
        return Some(TranscriptItem {
            role: TranscriptRole::Status,
            title: "Approval required".to_owned(),
            body: vec![format!("{tool_name}: {resource}"), reason.to_owned()],
        });
    }
    if event.event_type == RuntimeEventType::ToolCompleted {
        return tool_event_transcript_item(event);
    }
    if event.event_type == RuntimeEventType::TaskCompleted
        && event
            .payload
            .get("status")
            .cloned()
            .and_then(|status| serde_json::from_value::<golutra_core::TaskStatus>(status).ok())
            == Some(golutra_core::TaskStatus::Completed)
    {
        return None;
    }
    let title = event_status_title(event.event_type)?;
    let summary = event_summary(event)?;
    if event.event_type == RuntimeEventType::LoopDecided
        && !summary.contains("failed")
        && !summary.contains("error")
    {
        return None;
    }
    Some(TranscriptItem {
        role: TranscriptRole::Status,
        title: title.to_owned(),
        body: vec![summary],
    })
}

fn tool_event_transcript_item(event: &RuntimeEvent) -> Option<TranscriptItem> {
    let summary = event_summary(event).unwrap_or_else(|| "tool completed".to_owned());
    let file_changes = operation_file_changes(event);
    if !file_changes.is_empty() {
        return Some(file_change_item(&file_changes));
    }

    let envelope = event.payload.get("envelope");
    let tool_name = envelope
        .and_then(|value| value.get("tool_name"))
        .and_then(Value::as_str);
    let facts = envelope.and_then(|value| value.get("structured_facts"));
    match tool_name {
        Some("shell") => Some(TranscriptItem {
            role: TranscriptRole::Status,
            title: "Ran".to_owned(),
            body: vec![
                facts
                    .and_then(|value| value.get("command"))
                    .and_then(Value::as_str)
                    .unwrap_or(&summary)
                    .to_owned(),
            ],
        }),
        Some("read_file" | "list_dir" | "rg_search" | "symbol_search" | "find_references") => {
            Some(TranscriptItem {
                role: TranscriptRole::Status,
                title: "Explored".to_owned(),
                body: vec![tool_resource(facts).unwrap_or(summary)],
            })
        }
        _ => Some(TranscriptItem {
            role: TranscriptRole::Status,
            title: "Tool Completed".to_owned(),
            body: vec![summary],
        }),
    }
}

fn tool_resource(facts: Option<&Value>) -> Option<String> {
    let facts = facts?;
    for key in ["path", "query", "pattern", "symbol"] {
        if let Some(value) = facts.get(key).and_then(Value::as_str) {
            return Some(value.to_owned());
        }
    }
    None
}

fn file_change_item(changes: &[FileChangeSummary]) -> TranscriptItem {
    let stats_complete = changes
        .iter()
        .all(|change| change.added_lines.is_some() && change.removed_lines.is_some());
    let added = changes
        .iter()
        .filter_map(|change| change.added_lines)
        .fold(0_u64, u64::saturating_add);
    let removed = changes
        .iter()
        .filter_map(|change| change.removed_lines)
        .fold(0_u64, u64::saturating_add);
    let noun = if changes.len() == 1 { "file" } else { "files" };
    let title = if stats_complete {
        format!("Edited {} {noun} (+{added} -{removed})", changes.len())
    } else {
        format!("Edited {} {noun}", changes.len())
    };
    let body = changes
        .iter()
        .map(|change| match (change.added_lines, change.removed_lines) {
            (Some(added), Some(removed)) => {
                format!("{}  +{added} -{removed}", change.path)
            }
            _ => change.path.clone(),
        })
        .collect();
    TranscriptItem {
        role: TranscriptRole::Status,
        title,
        body,
    }
}

pub(crate) fn event_status_title(event_type: RuntimeEventType) -> Option<&'static str> {
    match event_type {
        RuntimeEventType::TaskCompleted => Some("Task Completed"),
        RuntimeEventType::CommandRejected => Some("Command Rejected"),
        RuntimeEventType::ControllerChanged => Some("Controller Changed"),
        RuntimeEventType::LoopDecided => Some("Loop Decided"),
        _ => None,
    }
}

pub(crate) fn event_summary(event: &RuntimeEvent) -> Option<String> {
    event
        .payload
        .get("summary")
        .and_then(Value::as_str)
        .map_or_else(
            || {
                event
                    .payload
                    .get("error")
                    .and_then(Value::as_str)
                    .map(|error| {
                        if error.trim().is_empty() {
                            "runtime event recorded".to_owned()
                        } else {
                            error.to_owned()
                        }
                    })
            },
            |summary| {
                if summary.trim().is_empty() {
                    None
                } else {
                    Some(summary.to_owned())
                }
            },
        )
}

pub(crate) fn projection_items(projection: &UserProjection) -> Vec<TranscriptItem> {
    let mut items = projection
        .visible_steps
        .iter()
        .filter(|step| significant_step(step))
        .map(step_item)
        .collect::<Vec<_>>();
    if let Some(pending_approval) = &projection.pending_approval {
        items.push(TranscriptItem {
            role: TranscriptRole::Status,
            title: "Approval required".to_owned(),
            body: vec![pending_approval.to_owned()],
        });
    }
    if let Some(final_message) = &projection.final_message {
        items.push(TranscriptItem {
            role: TranscriptRole::Assistant,
            title: "Golutra".to_owned(),
            body: vec![final_message.to_owned()],
        });
    }
    if !projection.residual_risks.is_empty() {
        items.push(TranscriptItem {
            role: TranscriptRole::Status,
            title: "Residual risks".to_owned(),
            body: projection.residual_risks.clone(),
        });
    }
    items
}

pub(crate) fn projection_overlay_items(projection: &UserProjection) -> Vec<TranscriptItem> {
    let mut items = Vec::new();
    if !projection.residual_risks.is_empty() {
        items.push(TranscriptItem {
            role: TranscriptRole::Status,
            title: "Residual risks".to_owned(),
            body: projection.residual_risks.clone(),
        });
    }
    items
}

pub(crate) fn significant_step(step: &VisibleStep) -> bool {
    matches!(step.label.as_str(), "ToolCompleted" | "CommandRejected")
        || (step.label == "TaskCompleted" && step.status != "Completed")
        || (step.label == "LoopDecided"
            && (step.summary.contains("failed") || step.summary.contains("error")))
}

pub(crate) fn step_item(step: &VisibleStep) -> TranscriptItem {
    TranscriptItem {
        role: TranscriptRole::Status,
        title: readable_step_label(&step.label),
        body: vec![format!("{} - {}", step.status, step.summary)],
    }
}

pub(crate) fn readable_step_label(label: &str) -> String {
    label
        .chars()
        .enumerate()
        .fold(String::new(), |mut output, (index, character)| {
            if index > 0 && character.is_uppercase() {
                output.push(' ');
            }
            output.push(character);
            output
        })
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use golutra_core::{EventId, SessionId, TaskId};
    use golutra_protocol::RuntimeEventSource;
    use serde_json::json;

    use super::*;

    #[test]
    fn file_tool_events_have_a_compact_codex_style_change_summary() {
        let event = RuntimeEvent {
            id: EventId::new(),
            sequence_no: 1,
            session_id: SessionId::new(),
            turn_id: None,
            task_id: Some(TaskId::new()),
            parent_event_id: None,
            event_type: RuntimeEventType::ToolCompleted,
            timestamp: Utc::now(),
            source: RuntimeEventSource::Tool,
            payload: json!({
                "summary": "file edited",
                "file_changes": [{
                    "path": "src/lib.rs",
                    "kind": "modified",
                    "added_lines": 3,
                    "removed_lines": 1
                }]
            }),
            payload_ref: None,
            durable: true,
        };

        let item = status_event_transcript_item(&event).expect("change item");

        assert_eq!(item.title, "Edited 1 file (+3 -1)");
        assert_eq!(item.body, vec!["src/lib.rs  +3 -1"]);
    }

    #[test]
    fn legacy_changed_files_remain_visible_without_fake_line_counts() {
        let event = RuntimeEvent {
            id: EventId::new(),
            sequence_no: 1,
            session_id: SessionId::new(),
            turn_id: None,
            task_id: Some(TaskId::new()),
            parent_event_id: None,
            event_type: RuntimeEventType::ToolCompleted,
            timestamp: Utc::now(),
            source: RuntimeEventSource::Tool,
            payload: json!({
                "summary": "file edited",
                "changed_files": ["src/legacy.rs"]
            }),
            payload_ref: None,
            durable: true,
        };

        let item = status_event_transcript_item(&event).expect("legacy change item");

        assert_eq!(item.title, "Edited 1 file");
        assert_eq!(item.body, vec!["src/legacy.rs"]);
    }
}
