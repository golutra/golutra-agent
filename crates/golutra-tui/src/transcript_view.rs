//! Pure mapping from runtime/user projections to transcript view models.

use std::collections::{HashMap, HashSet};

use golutra_core::{
    EventId, FileChangeKind, FileChangeSummary, TaskId, TaskStatus, ToolResultStatus, TurnId,
    VerificationIndependence, VerificationRecord, VerificationResult, VerificationSource,
};
use golutra_protocol::{RuntimeEvent, RuntimeEventType, UserProjection, VisibleStep};
use serde_json::Value;

use super::{
    BodyViewMode, PaneScrollState, TranscriptLayoutCache, TranscriptPresentation,
    TranscriptSearchState, TuiApp, operation_file_changes,
};

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

#[derive(Debug, Clone)]
pub(crate) struct TranscriptHistoryState {
    pub(crate) enabled: bool,
    pub(crate) committed_event_ids: HashSet<EventId>,
    pub(crate) replay_generation: u64,
    pub(crate) replay_ready: bool,
}

impl Default for TranscriptHistoryState {
    fn default() -> Self {
        Self {
            enabled: false,
            committed_event_ids: HashSet::new(),
            replay_generation: 0,
            replay_ready: true,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct TranscriptState {
    pub(crate) expanded_operations: HashSet<OperationId>,
    pub(crate) details_expanded: bool,
    pub(crate) scroll: PaneScrollState,
    pub(crate) top_row_override: Option<usize>,
    pub(crate) revision: u64,
    pub(crate) layout_cache: Option<TranscriptLayoutCache>,
    pub(crate) history: TranscriptHistoryState,
    pub(crate) presentation: TranscriptPresentation,
    pub(crate) search: Option<TranscriptSearchState>,
    pub(crate) search_restore_body_view: Option<BodyViewMode>,
}

impl Default for TranscriptState {
    fn default() -> Self {
        Self {
            expanded_operations: HashSet::new(),
            details_expanded: false,
            scroll: PaneScrollState {
                follow_tail: true,
                ..PaneScrollState::default()
            },
            top_row_override: None,
            revision: 0,
            layout_cache: None,
            history: TranscriptHistoryState::default(),
            presentation: TranscriptPresentation::Rich,
            search: None,
            search_restore_body_view: None,
        }
    }
}

impl TranscriptState {
    pub(crate) fn invalidate_layout(&mut self) {
        self.revision = self.revision.wrapping_add(1);
        self.layout_cache = None;
    }

    pub(crate) fn reset_view(&mut self) {
        self.expanded_operations.clear();
        self.details_expanded = false;
        self.top_row_override = None;
        self.invalidate_layout();
    }

    pub(crate) fn toggle_operation(&mut self, id: OperationId) {
        if !self.expanded_operations.insert(id.clone()) {
            self.expanded_operations.remove(&id);
        }
        self.invalidate_layout();
    }

    pub(crate) fn toggle_details(&mut self) -> bool {
        self.details_expanded = !self.details_expanded;
        self.invalidate_layout();
        self.details_expanded
    }

    pub(crate) fn enable_inline_history(&mut self) {
        if !self.history.enabled {
            self.history.enabled = true;
            self.invalidate_layout();
        }
    }

    pub(crate) fn begin_history_replay(&mut self) {
        self.history.replay_generation = self.history.replay_generation.wrapping_add(1);
        self.history.replay_ready = false;
        self.set_committed_event_ids(HashSet::new());
    }

    pub(crate) fn request_history_rebuild(&mut self) {
        self.history.replay_generation = self.history.replay_generation.wrapping_add(1);
        self.set_committed_event_ids(HashSet::new());
    }

    pub(crate) fn set_committed_event_ids(&mut self, ids: HashSet<EventId>) {
        if self.history.committed_event_ids != ids {
            self.history.committed_event_ids = ids;
            self.invalidate_layout();
        }
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
            let expanded = app.transcript.details_expanded
                || projection
                    .id()
                    .is_some_and(|id| app.transcript.expanded_operations.contains(id));
            projection.item(expanded)
        })
        .collect()
}

pub(crate) fn transcript_operation_projections(app: &TuiApp) -> Vec<OperationProjection> {
    transcript_operation_projections_after(app, None, false)
}

pub(crate) fn rendered_transcript_operation_projections(app: &TuiApp) -> Vec<OperationProjection> {
    let committed = (app.transcript.history.enabled && app.transcript.search.is_none())
        .then_some(&app.transcript.history.committed_event_ids);
    transcript_operation_projections_after(app, committed, true)
}

fn transcript_operation_projections_after(
    app: &TuiApp,
    committed_event_ids: Option<&HashSet<EventId>>,
    include_result_card: bool,
) -> Vec<OperationProjection> {
    if app.auth_dialog.is_some() {
        return Vec::new();
    }
    let mut items: Vec<OperationProjection> = Vec::new();
    let event_items = event_operation_entries(&app.events);
    let has_event_items = !event_items.is_empty();
    items.extend(
        event_items
            .into_iter()
            .filter(|entry| {
                committed_event_ids.is_none_or(|committed| !committed.contains(&entry.id))
            })
            .map(|entry| entry.projection),
    );
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
        if include_result_card && let Some(result_card) = result_card_projection(app) {
            items.push(result_card);
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

fn result_card_projection(app: &TuiApp) -> Option<OperationProjection> {
    let projection = app.projection.as_ref()?;
    if !projection.status.is_terminal() {
        return None;
    }
    let verification = app
        .developer_projection
        .as_ref()
        .filter(|debug| {
            debug
                .task_id
                .is_none_or(|task_id| Some(task_id) == projection.task_id)
        })
        .and_then(|debug| debug.verification.clone())
        .or_else(|| latest_verification(&app.events, projection.task_id));
    let independently_verified = verification.as_ref().is_some_and(is_independently_verified);
    let state = if projection.status == TaskStatus::Failed
        || projection.status == TaskStatus::Cancelled
        || projection.status == TaskStatus::Blocked
        || verification
            .as_ref()
            .is_some_and(|record| record.result == VerificationResult::Fail)
    {
        "Failed"
    } else if projection.status == TaskStatus::Partial
        || verification
            .as_ref()
            .is_some_and(|record| record.result == VerificationResult::Partial)
    {
        "Partial"
    } else if independently_verified {
        "Verified"
    } else {
        "Completed · Unverified"
    };
    let role = match state {
        "Verified" => TranscriptRole::Success,
        "Failed" => TranscriptRole::Error,
        _ => TranscriptRole::Warning,
    };
    let mut detail_body = Vec::new();
    let files_summary = if let Some(changes) = app.change_projection.summary() {
        let stats = match (changes.added_lines, changes.removed_lines) {
            (Some(added), Some(removed)) => format!(" (+{added} -{removed})"),
            _ => String::new(),
        };
        detail_body.extend(
            changes
                .files
                .iter()
                .take(3)
                .map(|change| format!("  {}", change.path)),
        );
        if changes.files.len() > 3 {
            detail_body.push(format!("  … {} more files", changes.files.len() - 3));
        }
        format!("files changed: {}{stats}", changes.file_count)
    } else {
        "files changed: 0".to_owned()
    };
    let checks_summary = if let Some(record) = &verification {
        let passed = record.checks.iter().filter(|check| check.passed).count();
        detail_body.extend(
            record
                .residual_risks
                .iter()
                .take(3)
                .map(|risk| format!("risk: {risk}")),
        );
        format!("checks: {passed}/{} passed", record.checks.len())
    } else {
        "checks: no independent verification record".to_owned()
    };
    let next_action = match state {
        "Verified" => "next: review the verified diff or continue with a follow-up".to_owned(),
        "Failed" => "next: inspect the failure, then use /retry [model]".to_owned(),
        "Partial" => "next: resolve residual risks, then retry or verify again".to_owned(),
        _ => "next: inspect the diff or use /retry [model] for an independent run".to_owned(),
    };
    let mut body = vec![format!("{files_summary} · {checks_summary}"), next_action];
    if app.transcript.details_expanded {
        body.splice(1..1, detail_body);
    }
    Some(notice_projection(TranscriptItem {
        role,
        title: format!("Result · {state}"),
        body,
    }))
}

fn latest_verification(
    events: &[RuntimeEvent],
    task_id: Option<TaskId>,
) -> Option<VerificationRecord> {
    events
        .iter()
        .rev()
        .filter(|event| {
            event.event_type == RuntimeEventType::VerificationCompleted
                && task_id.is_none_or(|task_id| event.task_id == Some(task_id))
        })
        .find_map(|event| {
            event
                .payload
                .get("record")
                .cloned()
                .or_else(|| Some(event.payload.clone()))
                .and_then(|value| serde_json::from_value::<VerificationRecord>(value).ok())
                .filter(|record| task_id.is_none_or(|task_id| record.task_id == task_id))
        })
}

fn is_independently_verified(record: &VerificationRecord) -> bool {
    record.result == VerificationResult::Pass
        && matches!(
            record.source,
            VerificationSource::ExternalVerifier | VerificationSource::Mixed
        )
        && record.independence == VerificationIndependence::Independent
        && !record.checks.is_empty()
        && record.checks.iter().all(|check| check.passed)
        && !record.evidence_refs.is_empty()
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn event_transcript_items(events: &[RuntimeEvent]) -> Vec<TranscriptItem> {
    event_operation_projections(events)
        .into_iter()
        .map(|projection| projection.item(false))
        .collect()
}

pub(crate) fn event_operation_projections(events: &[RuntimeEvent]) -> Vec<OperationProjection> {
    event_operation_entries(events)
        .into_iter()
        .map(|entry| entry.projection)
        .collect()
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn stable_event_operation_projection_count(events: &[RuntimeEvent]) -> usize {
    event_operation_entries(events)
        .into_iter()
        .take_while(|entry| entry.stable)
        .count()
}

#[derive(Debug, Clone)]
pub(crate) struct EventOperationEntry {
    pub(crate) id: EventId,
    pub(crate) projection: OperationProjection,
    pub(crate) stable: bool,
    task_id: Option<TaskId>,
    turn_id: Option<TurnId>,
}

impl EventOperationEntry {
    fn new(event: &RuntimeEvent, projection: OperationProjection, stable: bool) -> Self {
        Self {
            id: event.id,
            projection,
            task_id: event.task_id,
            turn_id: event.turn_id,
            stable,
        }
    }
}

pub(crate) fn event_operation_entries(events: &[RuntimeEvent]) -> Vec<EventOperationEntry> {
    let mut typed_events = events.iter().collect::<Vec<_>>();
    typed_events.sort_by_key(|event| event.sequence_no);

    let mut items: Vec<EventOperationEntry> = Vec::new();
    let mut visible_user_turns = HashMap::<TurnId, usize>::new();
    let mut streamed_assistant_items: HashMap<TurnId, usize> = HashMap::new();
    let mut active_tools = HashMap::<OperationId, usize>::new();
    for event in typed_events {
        if event.event_type.is_task_terminal() {
            for record in &mut items {
                if event.task_id.is_some() && record.task_id == event.task_id
                    || event.task_id.is_none()
                        && event.turn_id.is_some()
                        && record.turn_id == event.turn_id
                {
                    record.stable = true;
                }
            }
        }
        match event.event_type {
            RuntimeEventType::TaskCreated => {
                let is_new_turn = event
                    .turn_id
                    .is_none_or(|turn_id| !visible_user_turns.contains_key(&turn_id));
                if is_new_turn && let Some(item) = user_event_transcript_item(event) {
                    if let Some(turn_id) = event.turn_id {
                        visible_user_turns.insert(turn_id, items.len());
                    }
                    items.push(EventOperationEntry::new(
                        event,
                        message_projection(item),
                        true,
                    ));
                }
            }
            RuntimeEventType::TurnQueued => {
                let is_new_turn = event
                    .turn_id
                    .is_none_or(|turn_id| !visible_user_turns.contains_key(&turn_id));
                if is_new_turn && let Some(item) = user_event_transcript_item(event) {
                    if let Some(turn_id) = event.turn_id {
                        visible_user_turns.insert(turn_id, items.len());
                    }
                    items.push(EventOperationEntry::new(
                        event,
                        message_projection(item),
                        false,
                    ));
                }
            }
            RuntimeEventType::TurnStarted => {
                if let Some(index) = event
                    .turn_id
                    .and_then(|turn_id| visible_user_turns.get(&turn_id).copied())
                    && let Some(record) = items.get_mut(index)
                {
                    record.task_id = event.task_id.or(record.task_id);
                    record.stable = true;
                }
            }
            RuntimeEventType::TurnUpdated => {
                if let Some(index) = event
                    .turn_id
                    .and_then(|turn_id| visible_user_turns.get(&turn_id).copied())
                    && let Some(item) = user_event_transcript_item(event)
                {
                    items[index].projection = message_projection(item);
                }
            }
            RuntimeEventType::TurnCancelled => {
                let Some(index) = event
                    .turn_id
                    .and_then(|turn_id| visible_user_turns.remove(&turn_id))
                else {
                    continue;
                };
                items.remove(index);
                for position in visible_user_turns.values_mut() {
                    if *position > index {
                        *position = position.saturating_sub(1);
                    }
                }
                for position in streamed_assistant_items.values_mut() {
                    if *position > index {
                        *position = position.saturating_sub(1);
                    }
                }
                for position in active_tools.values_mut() {
                    if *position > index {
                        *position = position.saturating_sub(1);
                    }
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
                    if let Some(record) = items.get_mut(index)
                        && let Some(body) = record.projection.item_mut().body.first_mut()
                    {
                        body.push_str(delta);
                    }
                } else {
                    let index = items.len();
                    items.push(EventOperationEntry::new(
                        event,
                        message_projection(TranscriptItem {
                            role: TranscriptRole::Assistant,
                            title: "Golutra".to_owned(),
                            body: vec![delta.to_owned()],
                        }),
                        false,
                    ));
                    streamed_assistant_items.insert(turn_id, index);
                }
            }
            RuntimeEventType::AssistantMessage => {
                let streamed = event
                    .turn_id
                    .and_then(|turn_id| streamed_assistant_items.remove(&turn_id));
                if let Some(index) = streamed {
                    if let Some(record) = items.get_mut(index) {
                        if let Some(item) = assistant_event_transcript_item(event) {
                            record.projection = message_projection(item);
                        }
                        record.stable = true;
                    }
                } else if let Some(item) = assistant_event_transcript_item(event) {
                    items.push(EventOperationEntry::new(
                        event,
                        message_projection(item),
                        true,
                    ));
                }
            }
            RuntimeEventType::ToolStarted => {
                if let Some(projection) = tool_started_projection(event) {
                    let index = items.len();
                    if let Some(id) = projection.id().cloned() {
                        active_tools.insert(id, index);
                    }
                    items.push(EventOperationEntry::new(event, projection, false));
                }
            }
            RuntimeEventType::ToolProgress => {
                if let Some(id) = operation_id_from_event(event)
                    && let Some(index) = active_tools.get(&id).copied()
                    && let Some(record) = items.get_mut(index)
                {
                    update_tool_progress(&mut record.projection, event);
                }
            }
            RuntimeEventType::ToolCompleted => {
                if let Some(projection) = tool_operation_projection(event) {
                    if let Some(id) = projection.id().cloned()
                        && let Some(index) = active_tools.remove(&id)
                    {
                        items[index].projection = projection;
                        items[index].stable = true;
                    } else {
                        items.push(EventOperationEntry::new(event, projection, true));
                    }
                }
            }
            _ => {
                if let Some(item) = status_event_transcript_item(event) {
                    items.push(EventOperationEntry::new(
                        event,
                        notice_projection(item),
                        true,
                    ));
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
    let stream = progress
        .get("detail")
        .and_then(Value::as_str)
        .unwrap_or("output");
    let item = projection.item_mut();
    item.body.truncate(1);
    item.body.push(format!(
        "{stream} · {} · {} · {}",
        plural_count(output_lines, "line", "lines"),
        format_bytes(output_bytes),
        format_millis(elapsed_ms)
    ));
    if let Some(excerpt) = progress.get("output_excerpt").and_then(Value::as_str) {
        let lines = excerpt.lines().filter(|line| !line.trim().is_empty());
        let mut lines = lines.rev().take(6).collect::<Vec<_>>();
        lines.reverse();
        item.body.extend(
            lines
                .into_iter()
                .map(|line| format!("│ {}", bounded_text(line, 320))),
        );
    }
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
        RuntimeEventType::TaskInterrupted => Some("Task Interrupted"),
        RuntimeEventType::TaskUncertain => Some("Task Uncertain / reconciliation required"),
        RuntimeEventType::TaskReconciled => Some("Task Recovery Reconciled"),
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
    use golutra_core::{
        EventId, EvidenceId, SessionId, TaskId, ToolCallId, VerificationCheck,
        VerificationCheckKind, VerificationId, VerificationIndependence, VerificationSource,
    };
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
        assert_eq!(
            event_status_title(RuntimeEventType::TaskInterrupted),
            Some("Task Interrupted")
        );
        assert_eq!(
            event_status_title(RuntimeEventType::TaskUncertain),
            Some("Task Uncertain / reconciliation required")
        );
    }

    fn verification_for(task_id: TaskId) -> VerificationRecord {
        VerificationRecord {
            verification_id: VerificationId::new(),
            task_id,
            objective: "verify the change".to_owned(),
            completion_criteria: vec!["the check passes".to_owned()],
            checks: vec![VerificationCheck {
                kind: VerificationCheckKind::ObjectiveValidation,
                name: "objective:check".to_owned(),
                command: Some("cargo test".to_owned()),
                passed: true,
                evidence_refs: vec![EvidenceId::new()],
                message: "passed".to_owned(),
            }],
            evidence_refs: vec![EvidenceId::new()],
            result: VerificationResult::Pass,
            policy_status: "allowed".to_owned(),
            residual_risks: Vec::new(),
            plan_id: None,
            assertions: Vec::new(),
            source: VerificationSource::ExternalVerifier,
            independence: VerificationIndependence::Independent,
            environment_digest: None,
        }
    }

    #[test]
    fn result_card_requires_verification_for_the_current_task() {
        let current_task = TaskId::new();
        let old_task = TaskId::new();
        let mut app = TuiApp::new(
            golutra_core::ThreadId::new(),
            SessionId::new(),
            Some(current_task),
            false,
            "ready (mock)".to_owned(),
            None,
        );
        app.projection = Some(UserProjection {
            session_id: app.session_id,
            task_id: Some(current_task),
            status: TaskStatus::Completed,
            visible_steps: Vec::new(),
            pending_approval: None,
            final_message: Some("done".to_owned()),
            residual_risks: Vec::new(),
        });
        let mut event = RuntimeEvent {
            schema_version: golutra_core::RUNTIME_EVENT_SCHEMA_VERSION,
            causal_context: Default::default(),
            causal_links: Vec::new(),
            id: EventId::new(),
            sequence_no: 1,
            session_id: app.session_id,
            turn_id: None,
            task_id: Some(old_task),
            parent_event_id: None,
            event_type: RuntimeEventType::VerificationCompleted,
            timestamp: Utc::now(),
            source: RuntimeEventSource::Verifier,
            payload: json!({"record": verification_for(old_task)}),
            payload_ref: None,
            durable: true,
        };
        app.events.push(event.clone());
        let OperationProjection::Notice { item } =
            result_card_projection(&app).expect("terminal result card")
        else {
            panic!("result card must be a notice");
        };
        assert_eq!(item.title, "Result · Completed · Unverified");

        event.task_id = Some(current_task);
        event.payload = json!({"record": verification_for(current_task)});
        app.events = vec![event];
        let OperationProjection::Notice { item } =
            result_card_projection(&app).expect("verified result card")
        else {
            panic!("result card must be a notice");
        };
        assert_eq!(item.title, "Result · Verified");
    }

    fn tool_event(sequence_no: u64, event_type: RuntimeEventType, payload: Value) -> RuntimeEvent {
        RuntimeEvent {
            schema_version: golutra_core::RUNTIME_EVENT_SCHEMA_VERSION,
            causal_context: Default::default(),
            causal_links: Vec::new(),
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
            schema_version: golutra_core::RUNTIME_EVENT_SCHEMA_VERSION,
            causal_context: Default::default(),
            causal_links: Vec::new(),
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
            schema_version: golutra_core::RUNTIME_EVENT_SCHEMA_VERSION,
            causal_context: Default::default(),
            causal_links: Vec::new(),
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
                        "output_lines": 3,
                        "detail": "stdout",
                        "output_excerpt": "running test one\nrunning test two"
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

        let running = event_operation_projections(&events[..2]);
        let OperationProjection::ToolActivity { item, .. } = &running[0] else {
            panic!("running tool projection");
        };
        assert!(
            item.body
                .iter()
                .any(|line| line.contains("stdout · 3 lines"))
        );
        assert!(item.body.iter().any(|line| line == "│ running test two"));

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
