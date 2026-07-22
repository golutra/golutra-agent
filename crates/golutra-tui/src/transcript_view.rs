//! Pure mapping from runtime/user projections to transcript view models.

use std::collections::{HashMap, HashSet};

use golutra_core::{FileChangeKind, FileChangeSummary, ToolResultStatus, TurnId};
use golutra_protocol::{RuntimeEvent, RuntimeEventType, UserProjection, VisibleStep};
use serde_json::Value;

use super::{TuiApp, operation_file_changes};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TranscriptRole {
    User,
    Assistant,
    Status,
    Activity,
    Success,
    Warning,
    Error,
    System,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TranscriptItem {
    pub(crate) role: TranscriptRole,
    pub(crate) title: String,
    pub(crate) body: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct OperationId(String);

impl OperationId {
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum OperationProjection {
    Message {
        item: TranscriptItem,
    },
    ToolActivity {
        id: OperationId,
        item: TranscriptItem,
        details: Vec<String>,
    },
    FileChange {
        id: OperationId,
        item: TranscriptItem,
        details: Vec<String>,
    },
    Notice {
        item: TranscriptItem,
    },
}

impl OperationProjection {
    pub(crate) fn id(&self) -> Option<&OperationId> {
        match self {
            Self::ToolActivity { id, .. } | Self::FileChange { id, .. } => Some(id),
            Self::Message { .. } | Self::Notice { .. } => None,
        }
    }

    pub(crate) fn is_expandable(&self) -> bool {
        match self {
            Self::ToolActivity { details, .. } | Self::FileChange { details, .. } => {
                !details.is_empty()
            }
            Self::Message { .. } | Self::Notice { .. } => false,
        }
    }

    pub(crate) fn item(&self, expanded: bool) -> TranscriptItem {
        let (item, details) = match self {
            Self::Message { item } | Self::Notice { item } => return item.clone(),
            Self::ToolActivity { item, details, .. } | Self::FileChange { item, details, .. } => {
                (item, details)
            }
        };
        let mut item = item.clone();
        if expanded {
            item.body.extend(details.clone());
        }
        item
    }

    fn item_mut(&mut self) -> &mut TranscriptItem {
        match self {
            Self::Message { item }
            | Self::ToolActivity { item, .. }
            | Self::FileChange { item, .. }
            | Self::Notice { item } => item,
        }
    }
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn transcript_items(app: &TuiApp) -> Vec<TranscriptItem> {
    transcript_operation_projections(app)
        .into_iter()
        .map(|projection| {
            let expanded = app.transcript_details_expanded
                || projection
                    .id()
                    .is_some_and(|id| app.expanded_operations.contains(id));
            projection.item(expanded)
        })
        .collect()
}

pub(crate) fn transcript_operation_projections(app: &TuiApp) -> Vec<OperationProjection> {
    if app.auth_dialog.is_some() {
        return Vec::new();
    }
    let mut items: Vec<OperationProjection> = Vec::new();
    let event_items = event_operation_projections(&app.events);
    let has_event_items = !event_items.is_empty();
    items.extend(event_items);
    items.extend(app.command_messages.iter().cloned().map(notice_projection));
    if let Some(projection) = &app.projection {
        if has_event_items {
            items.extend(
                projection_overlay_items(projection)
                    .into_iter()
                    .map(notice_projection),
            );
        } else {
            items.extend(
                projection_items(projection)
                    .into_iter()
                    .map(plain_projection),
            );
        }
    } else {
        items.push(notice_projection(TranscriptItem {
            role: TranscriptRole::System,
            title: "Connecting".to_owned(),
            body: vec!["loading runtime state".to_owned()],
        }));
    }
    items
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn event_transcript_items(events: &[RuntimeEvent]) -> Vec<TranscriptItem> {
    event_operation_projections(events)
        .into_iter()
        .map(|projection| projection.item(false))
        .collect()
}

pub(crate) fn event_operation_projections(events: &[RuntimeEvent]) -> Vec<OperationProjection> {
    let mut typed_events = events.iter().collect::<Vec<_>>();
    typed_events.sort_by_key(|event| event.sequence_no);

    let mut items: Vec<OperationProjection> = Vec::new();
    let mut visible_user_turns = HashSet::new();
    let mut streamed_assistant_items: HashMap<TurnId, usize> = HashMap::new();
    let mut active_tools = HashMap::<OperationId, usize>::new();
    for event in typed_events {
        match event.event_type {
            RuntimeEventType::TaskCreated | RuntimeEventType::TurnQueued => {
                let is_new_turn = event
                    .turn_id
                    .is_none_or(|turn_id| visible_user_turns.insert(turn_id));
                if is_new_turn && let Some(item) = user_event_transcript_item(event) {
                    items.push(message_projection(item));
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
                    if let Some(projection) = items.get_mut(index)
                        && let Some(body) = projection.item_mut().body.first_mut()
                    {
                        body.push_str(delta);
                    }
                } else {
                    let index = items.len();
                    items.push(message_projection(TranscriptItem {
                        role: TranscriptRole::Assistant,
                        title: "Golutra".to_owned(),
                        body: vec![delta.to_owned()],
                    }));
                    streamed_assistant_items.insert(turn_id, index);
                }
            }
            RuntimeEventType::AssistantMessage => {
                if let Some(item) = assistant_event_transcript_item(event) {
                    if let Some(index) = event
                        .turn_id
                        .and_then(|turn_id| streamed_assistant_items.remove(&turn_id))
                    {
                        items[index] = message_projection(item);
                    } else {
                        items.push(message_projection(item));
                    }
                }
            }
            RuntimeEventType::ToolStarted => {
                if let Some(projection) = tool_started_projection(event) {
                    let index = items.len();
                    if let Some(id) = projection.id().cloned() {
                        active_tools.insert(id, index);
                    }
                    items.push(projection);
                }
            }
            RuntimeEventType::ToolProgress => {
                if let Some(id) = operation_id_from_event(event)
                    && let Some(index) = active_tools.get(&id).copied()
                    && let Some(projection) = items.get_mut(index)
                {
                    update_tool_progress(projection, event);
                }
            }
            RuntimeEventType::ToolCompleted => {
                if let Some(projection) = tool_operation_projection(event) {
                    if let Some(id) = projection.id().cloned()
                        && let Some(index) = active_tools.remove(&id)
                    {
                        items[index] = projection;
                    } else {
                        items.push(projection);
                    }
                }
            }
            _ => {
                if let Some(item) = status_event_transcript_item(event) {
                    items.push(notice_projection(item));
                }
            }
        }
    }
    items
}

fn plain_projection(item: TranscriptItem) -> OperationProjection {
    match item.role {
        TranscriptRole::User | TranscriptRole::Assistant => message_projection(item),
        _ => notice_projection(item),
    }
}

fn message_projection(item: TranscriptItem) -> OperationProjection {
    OperationProjection::Message { item }
}

fn notice_projection(item: TranscriptItem) -> OperationProjection {
    OperationProjection::Notice { item }
}

fn operation_id_from_event(event: &RuntimeEvent) -> Option<OperationId> {
    event
        .payload
        .get("tool_call_id")
        .or_else(|| event.payload.pointer("/envelope/tool_call_id"))
        .and_then(Value::as_str)
        .map(|value| OperationId(value.to_owned()))
}

fn tool_started_projection(event: &RuntimeEvent) -> Option<OperationProjection> {
    let tool_name = event.payload.get("tool_name")?.as_str()?;
    let id = operation_id_from_event(event)?;
    let arguments = event.payload.get("arguments");
    let invocation = tool_invocation(tool_name, arguments);
    let mut details = Vec::new();
    if let Some(arguments) = arguments {
        details.push("Arguments".to_owned());
        details.extend(pretty_json_lines(arguments, 20));
    }
    Some(OperationProjection::ToolActivity {
        id,
        item: TranscriptItem {
            role: TranscriptRole::Activity,
            title: running_tool_title(tool_name),
            body: (!invocation.is_empty())
                .then_some(invocation)
                .into_iter()
                .collect(),
        },
        details,
    })
}

fn update_tool_progress(projection: &mut OperationProjection, event: &RuntimeEvent) {
    let Some(progress) = event.payload.get("progress") else {
        return;
    };
    if progress.get("phase").and_then(Value::as_str) != Some("output") {
        return;
    }
    let elapsed_ms = progress
        .get("elapsed_ms")
        .and_then(Value::as_u64)
        .unwrap_or_default();
    let output_bytes = progress
        .get("output_bytes")
        .and_then(Value::as_u64)
        .unwrap_or_default();
    let output_lines = progress
        .get("output_lines")
        .and_then(Value::as_u64)
        .unwrap_or_default();
    let item = projection.item_mut();
    item.body.truncate(1);
    item.body.push(format!(
        "{} · {} · {}",
        plural_count(output_lines, "line", "lines"),
        format_bytes(output_bytes),
        format_millis(elapsed_ms)
    ));
}

fn tool_operation_projection(event: &RuntimeEvent) -> Option<OperationProjection> {
    let id = operation_id_from_event(event)
        .unwrap_or_else(|| OperationId(format!("event:{}", event.sequence_no)));
    let Some(envelope) = event.payload.get("envelope") else {
        return tool_event_transcript_item(event).map(notice_projection);
    };
    let tool_name = envelope
        .get("tool_name")
        .and_then(Value::as_str)
        .unwrap_or("tool");
    let status = tool_result_status(event);
    let facts = envelope.get("structured_facts");
    let invocation = tool_invocation(tool_name, facts);
    let summary = envelope
        .get("summary")
        .and_then(Value::as_str)
        .or_else(|| event.payload.get("summary").and_then(Value::as_str))
        .unwrap_or("tool completed");
    let metrics = event.payload.get("metrics");
    let mut body = Vec::new();
    if !invocation.is_empty() {
        body.push(invocation);
    }
    if let Some(line) = tool_metrics_line(metrics, facts) {
        body.push(line);
    }
    if facts
        .and_then(|value| value.get("workspace_changes_known"))
        .and_then(Value::as_bool)
        == Some(false)
    {
        body.push("workspace changes unknown".to_owned());
    }
    if status != ToolResultStatus::Ok && !summary.trim().is_empty() {
        body.push(summary.to_owned());
    }

    let excerpt = envelope
        .get("model_visible_excerpt")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let output_lines = bounded_output_lines(excerpt, 40);
    let changes = operation_file_changes(event);
    if !changes.is_empty() {
        let mut item = file_change_item(&changes, status);
        body.append(&mut item.body);
        item.body = body;

        let mut details = Vec::new();
        if !output_lines.is_empty() {
            details.push("Output".to_owned());
            details.extend(output_lines);
        }
        details.extend(file_change_details(event, &changes));
        return Some(OperationProjection::FileChange { id, item, details });
    }

    let mut details = Vec::new();
    if !output_lines.is_empty() {
        details.push("Output".to_owned());
        details.extend(output_lines);
    }
    Some(OperationProjection::ToolActivity {
        id,
        item: TranscriptItem {
            role: tool_status_role(status),
            title: completed_tool_title(tool_name, status),
            body,
        },
        details,
    })
}

fn running_tool_title(tool_name: &str) -> String {
    match tool_name {
        "shell" => "Running".to_owned(),
        "read_file" | "list_dir" | "rg_search" | "symbol_search" | "find_references" => {
            "Exploring".to_owned()
        }
        "write_file" | "edit_file" => "Editing".to_owned(),
        other => format!("Calling {other}"),
    }
}

fn completed_tool_title(tool_name: &str, status: ToolResultStatus) -> String {
    match status {
        ToolResultStatus::Blocked => return "Blocked".to_owned(),
        ToolResultStatus::Cancelled => return "Cancelled".to_owned(),
        ToolResultStatus::Timeout => return "Timed out".to_owned(),
        ToolResultStatus::Error => return "Failed".to_owned(),
        ToolResultStatus::Ok => {}
    }
    match tool_name {
        "shell" => "Ran".to_owned(),
        "read_file" | "list_dir" | "rg_search" | "symbol_search" | "find_references" => {
            "Explored".to_owned()
        }
        "write_file" | "edit_file" => "Edited".to_owned(),
        _ => "Tool Completed".to_owned(),
    }
}

fn tool_status_role(status: ToolResultStatus) -> TranscriptRole {
    match status {
        ToolResultStatus::Ok => TranscriptRole::Success,
        ToolResultStatus::Blocked | ToolResultStatus::Timeout => TranscriptRole::Warning,
        ToolResultStatus::Cancelled => TranscriptRole::System,
        ToolResultStatus::Error => TranscriptRole::Error,
    }
}

fn tool_invocation(tool_name: &str, values: Option<&Value>) -> String {
    let Some(values) = values else {
        return String::new();
    };
    let string = |key: &str| values.get(key).and_then(Value::as_str);
    match tool_name {
        "shell" => string("command").unwrap_or_default().to_owned(),
        "read_file" | "write_file" | "edit_file" | "list_dir" => {
            string("path").unwrap_or_default().to_owned()
        }
        "rg_search" => match (string("pattern"), string("path")) {
            (Some(pattern), Some(path)) => format!("{pattern} in {path}"),
            (Some(pattern), None) => pattern.to_owned(),
            _ => String::new(),
        },
        "symbol_search" => string("query").unwrap_or_default().to_owned(),
        "find_references" => string("symbol").unwrap_or_default().to_owned(),
        _ => bounded_text(&values.to_string(), 240),
    }
}

fn tool_metrics_line(metrics: Option<&Value>, facts: Option<&Value>) -> Option<String> {
    let mut parts = Vec::new();
    let exit_code = metrics
        .and_then(|value| value.get("exit_code"))
        .and_then(Value::as_i64)
        .or_else(|| {
            facts
                .and_then(|value| value.get("exit_code"))
                .and_then(Value::as_i64)
        });
    if let Some(exit_code) = exit_code {
        parts.push(format!("exit {exit_code}"));
    }
    let match_count = metrics
        .and_then(|value| value.get("match_count"))
        .and_then(Value::as_u64);
    let item_count = metrics
        .and_then(|value| value.get("item_count"))
        .and_then(Value::as_u64);
    let output_lines = metrics
        .and_then(|value| value.get("output_lines"))
        .and_then(Value::as_u64)
        .unwrap_or_default();
    if let Some(matches) = match_count {
        parts.push(plural_count(matches, "match", "matches"));
    } else if let Some(items) = item_count {
        parts.push(plural_count(items, "item", "items"));
    } else if output_lines > 0 {
        parts.push(plural_count(output_lines, "line", "lines"));
    }
    if let Some(bytes) = metrics
        .and_then(|value| value.get("output_bytes"))
        .and_then(Value::as_u64)
        .filter(|bytes| *bytes > 0)
    {
        parts.push(format_bytes(bytes));
    }
    if let Some(duration) = metrics
        .and_then(|value| value.get("duration_ms"))
        .and_then(Value::as_u64)
    {
        parts.push(format_millis(duration));
    }
    if metrics
        .and_then(|value| value.get("output_truncated"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        parts.push("truncated".to_owned());
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
}

fn file_change_details(event: &RuntimeEvent, changes: &[FileChangeSummary]) -> Vec<String> {
    let mut details = changes
        .iter()
        .map(|change| {
            let kind = match change.kind {
                FileChangeKind::Added => "added",
                FileChangeKind::Modified => "modified",
                FileChangeKind::Deleted => "deleted",
            };
            match (change.added_lines, change.removed_lines) {
                (Some(added), Some(removed)) => {
                    format!("{kind} {}  +{added} -{removed}", change.path)
                }
                _ => format!("{kind} {}", change.path),
            }
        })
        .collect::<Vec<_>>();
    if let Some(hunks) = event.payload.get("diff_hunks").and_then(Value::as_array) {
        details.push("Diff".to_owned());
        details.extend(
            hunks
                .iter()
                .filter_map(Value::as_str)
                .take(80)
                .map(ToOwned::to_owned),
        );
    }
    if let Some(previews) = event.payload.get("diff_previews").and_then(Value::as_array) {
        details.push("Diff".to_owned());
        for preview in previews.iter().take(12) {
            let path = preview
                .get("path")
                .and_then(Value::as_str)
                .unwrap_or("file");
            details.push(format!("@@ {path}"));
            details.extend(
                preview
                    .get("lines")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .take(80)
                    .map(ToOwned::to_owned),
            );
            if preview
                .get("truncated")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                details.push("… diff preview truncated".to_owned());
            }
        }
    }
    details
}

fn pretty_json_lines(value: &Value, limit: usize) -> Vec<String> {
    serde_json::to_string_pretty(value)
        .unwrap_or_else(|_| value.to_string())
        .lines()
        .take(limit)
        .map(|line| bounded_text(line, 320))
        .collect()
}

fn bounded_output_lines(value: &str, limit: usize) -> Vec<String> {
    value
        .lines()
        .filter(|line| !line.trim().is_empty())
        .take(limit)
        .map(|line| bounded_text(line, 500))
        .collect()
}

fn plural_count(value: u64, singular: &str, plural: &str) -> String {
    format!("{value} {}", if value == 1 { singular } else { plural })
}

fn format_bytes(bytes: u64) -> String {
    if bytes < 1_024 {
        return format!("{bytes} B");
    }
    if bytes < 1_024 * 1_024 {
        return format!("{:.1} KiB", bytes as f64 / 1_024.0);
    }
    format!("{:.1} MiB", bytes as f64 / (1_024.0 * 1_024.0))
}

fn format_millis(milliseconds: u64) -> String {
    if milliseconds < 1_000 {
        format!("{milliseconds} ms")
    } else {
        format!("{:.1} s", milliseconds as f64 / 1_000.0)
    }
}

fn bounded_text(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let prefix = chars.by_ref().take(max_chars).collect::<String>();
    if chars.next().is_some() {
        format!("{prefix}…")
    } else {
        prefix
    }
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
    let status = tool_result_status(event);
    let file_changes = operation_file_changes(event);
    if !file_changes.is_empty() {
        return Some(file_change_item(&file_changes, status));
    }

    let envelope = event.payload.get("envelope");
    let tool_name = envelope
        .and_then(|value| value.get("tool_name"))
        .and_then(Value::as_str);
    let facts = envelope.and_then(|value| value.get("structured_facts"));
    match tool_name {
        Some("shell") => Some(TranscriptItem {
            role: tool_status_role(status),
            title: completed_tool_title("shell", status),
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
                role: tool_status_role(status),
                title: completed_tool_title(tool_name.unwrap_or("tool"), status),
                body: vec![tool_resource(facts).unwrap_or(summary)],
            })
        }
        _ => Some(TranscriptItem {
            role: tool_status_role(status),
            title: completed_tool_title(tool_name.unwrap_or("tool"), status),
            body: vec![summary],
        }),
    }
}

fn tool_result_status(event: &RuntimeEvent) -> ToolResultStatus {
    let Some(value) = event
        .payload
        .pointer("/envelope/status")
        .or_else(|| event.payload.get("status"))
    else {
        return ToolResultStatus::Ok;
    };
    serde_json::from_value(value.clone()).unwrap_or(ToolResultStatus::Error)
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

fn file_change_item(changes: &[FileChangeSummary], status: ToolResultStatus) -> TranscriptItem {
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
    let edit_summary = if stats_complete {
        format!("Edited {} {noun} (+{added} -{removed})", changes.len())
    } else {
        format!("Edited {} {noun}", changes.len())
    };
    let title = if status == ToolResultStatus::Ok {
        edit_summary
    } else {
        format!("{} · {edit_summary}", completed_tool_title("tool", status))
    };
    let visible = changes.iter().take(5);
    let mut body: Vec<String> = visible
        .map(|change| match (change.added_lines, change.removed_lines) {
            (Some(added), Some(removed)) => {
                format!("{}  +{added} -{removed}", change.path)
            }
            _ => change.path.clone(),
        })
        .collect();
    if changes.len() > 5 {
        body.push(format!("… {} more files", changes.len() - 5));
    }
    TranscriptItem {
        role: tool_status_role(status),
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
        RuntimeEventType::RetryScheduled => Some("Retrying"),
        RuntimeEventType::ProviderFallback => Some("Fallback"),
        RuntimeEventType::ProviderTransportFallback => Some("Transport Fallback"),
        RuntimeEventType::LoopGuardTriggered => Some("Stopped"),
        RuntimeEventType::TaskPaused => Some("Paused"),
        RuntimeEventType::TaskResumed => Some("Resumed"),
        RuntimeEventType::TaskAbortRequested => Some("Stopping"),
        RuntimeEventType::TaskAborted => Some("Aborted"),
        RuntimeEventType::ToolProgress => None,
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
    let role = if step.status.eq_ignore_ascii_case("failed")
        || step.summary.to_ascii_lowercase().contains("error")
    {
        TranscriptRole::Error
    } else {
        TranscriptRole::Status
    };
    TranscriptItem {
        role,
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
    use golutra_core::{EventId, SessionId, TaskId, ToolCallId};
    use golutra_protocol::RuntimeEventSource;
    use serde_json::json;

    use super::*;

    #[test]
    fn provider_recovery_events_have_distinct_user_facing_labels() {
        assert_eq!(
            event_status_title(RuntimeEventType::ProviderFallback),
            Some("Fallback")
        );
        assert_eq!(
            event_status_title(RuntimeEventType::ProviderTransportFallback),
            Some("Transport Fallback")
        );
    }

    fn tool_event(sequence_no: u64, event_type: RuntimeEventType, payload: Value) -> RuntimeEvent {
        RuntimeEvent {
            id: EventId::new(),
            sequence_no,
            session_id: SessionId::new(),
            turn_id: None,
            task_id: Some(TaskId::new()),
            parent_event_id: None,
            event_type,
            timestamp: Utc::now(),
            source: RuntimeEventSource::Tool,
            payload,
            payload_ref: None,
            durable: true,
        }
    }

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

    #[test]
    fn tool_lifecycle_is_projected_as_one_expandable_operation() {
        let tool_call_id = ToolCallId::new();
        let events = vec![
            tool_event(
                1,
                RuntimeEventType::ToolStarted,
                json!({
                    "tool_call_id": tool_call_id,
                    "tool_name": "shell",
                    "arguments": {"command": "cargo test"}
                }),
            ),
            tool_event(
                2,
                RuntimeEventType::ToolProgress,
                json!({
                    "tool_call_id": tool_call_id,
                    "tool_name": "shell",
                    "progress": {
                        "phase": "output",
                        "elapsed_ms": 120,
                        "output_bytes": 42,
                        "output_lines": 3
                    }
                }),
            ),
            tool_event(
                3,
                RuntimeEventType::ToolCompleted,
                json!({
                    "envelope": {
                        "tool_call_id": tool_call_id,
                        "tool_name": "shell",
                        "status": "ok",
                        "summary": "shell command completed",
                        "structured_facts": {"command": "cargo test"},
                        "model_visible_excerpt": "one\ntwo\nthree\nfour"
                    },
                    "metrics": {
                        "duration_ms": 240,
                        "output_bytes": 42,
                        "output_lines": 4,
                        "output_truncated": false,
                        "exit_code": 0
                    }
                }),
            ),
        ];

        let projections = event_operation_projections(&events);

        assert_eq!(projections.len(), 1);
        let OperationProjection::ToolActivity { item, details, .. } = &projections[0] else {
            panic!("tool lifecycle should remain a tool operation");
        };
        assert_eq!(item.title, "Ran");
        assert_eq!(item.role, TranscriptRole::Success);
        assert!(item.body.iter().any(|line| line == "cargo test"));
        assert!(item.body.iter().any(|line| line.contains("exit 0")));
        assert!(details.iter().all(|line| line != "Facts"));
        assert!(details.iter().any(|line| line == "four"));
    }

    #[test]
    fn terminal_tool_statuses_have_distinct_user_visible_roles() {
        let cases = [
            ("ok", "Ran", TranscriptRole::Success),
            ("error", "Failed", TranscriptRole::Error),
            ("timeout", "Timed out", TranscriptRole::Warning),
            ("blocked", "Blocked", TranscriptRole::Warning),
            ("cancelled", "Cancelled", TranscriptRole::System),
        ];

        for (status, title, role) in cases {
            let projections = event_operation_projections(&[tool_event(
                1,
                RuntimeEventType::ToolCompleted,
                json!({
                    "envelope": {
                        "tool_call_id": ToolCallId::new(),
                        "tool_name": "shell",
                        "status": status,
                        "summary": "terminal result",
                        "structured_facts": {}
                    }
                }),
            )]);
            let OperationProjection::ToolActivity { item, .. } = &projections[0] else {
                panic!("terminal tool result should be an activity");
            };
            assert_eq!(item.title, title);
            assert_eq!(item.role, role);
        }
    }

    #[test]
    fn file_diff_preview_is_hidden_until_operation_is_expanded() {
        let event = tool_event(
            1,
            RuntimeEventType::ToolCompleted,
            json!({
                "envelope": {
                    "tool_call_id": ToolCallId::new(),
                    "tool_name": "edit_file",
                    "status": "ok",
                    "summary": "file edited",
                    "structured_facts": {}
                },
                "file_changes": [{
                    "path": "src/lib.rs",
                    "kind": "modified",
                    "added_lines": 1,
                    "removed_lines": 1
                }],
                "diff_previews": [{
                    "path": "src/lib.rs",
                    "lines": ["-old", "+new"],
                    "truncated": false
                }]
            }),
        );
        let projection = event_operation_projections(&[event])[0].clone();
        let collapsed = projection.item(false);
        let expanded = projection.item(true);

        assert!(collapsed.body.iter().all(|line| !line.contains("-old")));
        assert!(expanded.body.iter().any(|line| line == "-old"));
        assert!(expanded.body.iter().any(|line| line == "+new"));
    }

    #[test]
    fn failed_shell_that_changed_files_keeps_failure_and_execution_context() {
        let event = tool_event(
            1,
            RuntimeEventType::ToolCompleted,
            json!({
                "envelope": {
                    "tool_call_id": ToolCallId::new(),
                    "tool_name": "shell",
                    "status": "error",
                    "summary": "shell command failed",
                    "structured_facts": {
                        "command": "printf new > src/lib.rs; false",
                        "exit_code": 1,
                        "workspace_changes_known": true
                    },
                    "model_visible_excerpt": "command failure output"
                },
                "metrics": {
                    "duration_ms": 125,
                    "output_bytes": 22,
                    "output_lines": 1,
                    "output_truncated": false,
                    "exit_code": 1
                },
                "file_changes": [{
                    "path": "src/lib.rs",
                    "kind": "modified",
                    "added_lines": 1,
                    "removed_lines": 0
                }],
                "diff_previews": [{
                    "path": "src/lib.rs",
                    "lines": ["+new"],
                    "truncated": false
                }]
            }),
        );

        let projection = event_operation_projections(&[event])[0].clone();
        let OperationProjection::FileChange { item, details, .. } = projection else {
            panic!("file-changing shell should remain a file change operation");
        };

        assert_eq!(item.role, TranscriptRole::Error);
        assert_eq!(item.title, "Failed · Edited 1 file (+1 -0)");
        assert!(
            item.body
                .iter()
                .any(|line| line == "printf new > src/lib.rs; false")
        );
        assert!(item.body.iter().any(|line| line.contains("exit 1")));
        assert!(item.body.iter().any(|line| line == "shell command failed"));
        assert!(item.body.iter().any(|line| line == "src/lib.rs  +1 -0"));
        assert!(details.iter().any(|line| line == "Output"));
        assert!(details.iter().any(|line| line == "command failure output"));
        assert!(details.iter().all(|line| line != "Facts"));
        assert!(details.iter().any(|line| line == "+new"));
    }

    #[test]
    fn legacy_file_change_payload_uses_its_terminal_status() {
        let event = tool_event(
            1,
            RuntimeEventType::ToolCompleted,
            json!({
                "status": "timeout",
                "summary": "command timed out",
                "file_changes": [{
                    "path": "partial.txt",
                    "kind": "added"
                }]
            }),
        );

        let item = status_event_transcript_item(&event).expect("legacy change item");

        assert_eq!(item.role, TranscriptRole::Warning);
        assert_eq!(item.title, "Timed out · Edited 1 file");
    }

    #[test]
    fn malformed_terminal_status_never_projects_as_success() {
        let projection = event_operation_projections(&[tool_event(
            1,
            RuntimeEventType::ToolCompleted,
            json!({
                "envelope": {
                    "tool_call_id": ToolCallId::new(),
                    "tool_name": "shell",
                    "status": "unexpected",
                    "summary": "malformed terminal result",
                    "structured_facts": {}
                }
            }),
        )])[0]
            .clone();
        let item = projection.item(false);

        assert_eq!(item.role, TranscriptRole::Error);
        assert_eq!(item.title, "Failed");
    }

    #[test]
    fn opaque_tool_side_effects_are_explicitly_labeled_unknown() {
        let projection = event_operation_projections(&[tool_event(
            1,
            RuntimeEventType::ToolCompleted,
            json!({
                "envelope": {
                    "tool_call_id": ToolCallId::new(),
                    "tool_name": "mcp__fixture__echo",
                    "status": "ok",
                    "summary": "external call completed",
                    "structured_facts": {"workspace_changes_known": false}
                }
            }),
        )])[0]
            .clone();
        let item = projection.item(false);

        assert!(
            item.body
                .iter()
                .any(|line| line == "workspace changes unknown")
        );
    }
}
