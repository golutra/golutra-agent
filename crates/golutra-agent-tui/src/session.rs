//! TUI 会话选择、命令构造与 ID 解析。

use golutra_agent_client::{DebugExportReceipt, RuntimeClient, RuntimeTransport};
use golutra_agent_core::{
    Actor, ActorKind, CommandId, SessionId, TaskId, TaskStatus, ThreadId, TurnId,
};
use golutra_agent_protocol::{
    RuntimeEvent, RuntimeEventType, RuntimeQuery, RuntimeQueryKind, SessionCommand,
    SessionCommandKind,
};
use serde_json::{Value, json};
use std::collections::HashMap;
use tokio::task::JoinHandle;
use uuid::Uuid;

use super::{ComposerInput, TUI_ACTOR_ID};

#[derive(Debug, Clone)]
pub(crate) struct ResumePickerState {
    pub(crate) items: Vec<ResumeThreadItem>,
    all_items: Vec<ResumeThreadItem>,
    pub(crate) selected: usize,
    pub(crate) search: ComposerInput,
    pub(crate) show_details: bool,
    pub(crate) action: Option<SessionPickerAction>,
    pub(crate) action_input: ComposerInput,
    /// 默认隐藏已被历史编辑分支替代的父项；原始条目仍保留，便于搜索、删除和审计。
    pub(crate) show_all_branches: bool,
    pub(crate) current_thread_id: Option<ThreadId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SessionPickerAction {
    Rename,
    Archive,
    Delete,
}

#[derive(Debug, Clone)]
pub(crate) struct ResumeThreadItem {
    pub(crate) thread_id: ThreadId,
    pub(crate) session_id: SessionId,
    pub(crate) parent_thread_id: Option<ThreadId>,
    pub(crate) forked_from_turn_id: Option<TurnId>,
    pub(crate) title: String,
    pub(crate) preview: String,
    pub(crate) metadata: String,
}

#[derive(Debug, Clone)]
pub(crate) struct HistoricalTurnItem {
    pub(crate) turn_id: TurnId,
    pub(crate) prompt: String,
    pub(crate) attachment_paths: Vec<String>,
    pub(crate) preview: Vec<HistoricalTurnPreview>,
}

#[derive(Debug, Clone)]
pub(crate) struct HistoricalTurnPreview {
    pub(crate) kind: HistoricalTurnPreviewKind,
    pub(crate) text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HistoricalTurnPreviewKind {
    Assistant,
    Tool,
}

#[derive(Debug, Clone)]
pub(crate) struct TurnPickerState {
    pub(crate) items: std::sync::Arc<Vec<HistoricalTurnItem>>,
    pub(crate) layout_cache:
        std::cell::RefCell<Option<std::sync::Arc<super::render::TurnPickerVisualLayout>>>,
    pub(crate) selected: usize,
    pub(crate) scroll_offset: usize,
    pub(crate) focus_pending: bool,
}

impl TurnPickerState {
    pub(crate) fn new(items: Vec<HistoricalTurnItem>) -> Self {
        let selected = items.len().saturating_sub(1);
        Self {
            items: std::sync::Arc::new(items),
            layout_cache: Default::default(),
            selected,
            scroll_offset: 0,
            focus_pending: true,
        }
    }

    pub(crate) fn selected_turn_id(&self) -> Option<TurnId> {
        self.items.get(self.selected).map(|item| item.turn_id)
    }

    pub(crate) fn move_selection(&mut self, direction: ResumeSelectionDirection) {
        if self.items.is_empty() {
            self.selected = 0;
            return;
        }
        self.selected = match direction {
            ResumeSelectionDirection::Previous => self.selected.saturating_sub(1),
            ResumeSelectionDirection::Next => {
                (self.selected + 1).min(self.items.len().saturating_sub(1))
            }
        };
        self.focus_pending = true;
    }
}

/// 按首次出现的时间排列用户 turn；更新正文不改变其历史位置。
/// 同一个 turn 可能写入多个更新事件，最后一次非空 prompt 才是用户看到的内容。
pub(crate) fn historical_turn_items(events: &[RuntimeEvent]) -> Vec<HistoricalTurnItem> {
    let mut items = Vec::<(u64, HistoricalTurnItem)>::new();
    let mut ordered = events.iter().collect::<Vec<_>>();
    ordered.sort_by_key(|event| event.sequence_no);
    let mut previews = HashMap::<TurnId, Vec<HistoricalTurnPreview>>::new();
    for event in &ordered {
        let Some(turn_id) = event_turn_id(event) else {
            continue;
        };
        let (kind, text) = match event.event_type {
            RuntimeEventType::AssistantMessage => (
                HistoricalTurnPreviewKind::Assistant,
                event.payload.get("content").and_then(Value::as_str),
            ),
            RuntimeEventType::ToolCompleted => (
                HistoricalTurnPreviewKind::Tool,
                event
                    .payload
                    .pointer("/envelope/summary")
                    .and_then(Value::as_str)
                    .or_else(|| event.payload.get("summary").and_then(Value::as_str)),
            ),
            _ => (HistoricalTurnPreviewKind::Assistant, None),
        };
        let Some(text) = text.map(str::trim).filter(|text| !text.is_empty()) else {
            continue;
        };
        previews
            .entry(turn_id)
            .or_default()
            .push(HistoricalTurnPreview {
                kind,
                text: text.to_owned(),
            });
    }
    for event in ordered {
        if !matches!(
            event.event_type,
            RuntimeEventType::TaskCreated
                | RuntimeEventType::TurnQueued
                | RuntimeEventType::TurnUpdated
        ) {
            continue;
        }
        if event_is_steer(event) {
            // Codex cannot branch independently from a steering prompt. Keep it out of
            // the picker instead of allowing Enter to create an invalid branch.
            continue;
        }
        let Some(turn_id) = event_turn_id(event) else {
            continue;
        };
        let Some(prompt) = event
            .payload
            .get("payload")
            .and_then(|payload| payload.get("prompt"))
            .and_then(Value::as_str)
            .filter(|prompt| !prompt.trim().is_empty())
        else {
            continue;
        };
        let item = HistoricalTurnItem {
            turn_id,
            prompt: prompt.to_owned(),
            attachment_paths: event
                .payload
                .pointer("/payload/attachments")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|attachment| attachment.get("path").and_then(Value::as_str))
                .map(str::to_owned)
                .collect(),
            preview: previews.get(&turn_id).cloned().unwrap_or_default(),
        };
        if let Some((_, existing)) = items
            .iter_mut()
            .find(|(_, existing)| existing.turn_id == turn_id)
        {
            let mut item = item;
            if event.payload.pointer("/payload/attachments").is_none() {
                item.attachment_paths.clone_from(&existing.attachment_paths);
            }
            *existing = item;
        } else {
            items.push((event.sequence_no, item));
        }
    }
    items.sort_by_key(|(sequence, _)| *sequence);
    items.into_iter().map(|(_, item)| item).collect()
}

fn event_turn_id(event: &RuntimeEvent) -> Option<TurnId> {
    event.turn_id.or(event.causal_context.turn_id)
}

fn event_is_steer(event: &RuntimeEvent) -> bool {
    event
        .payload
        .get("steer")
        .and_then(Value::as_bool)
        .or_else(|| {
            event
                .payload
                .pointer("/payload/steer")
                .and_then(Value::as_bool)
        })
        .unwrap_or(false)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ContinuationHint {
    pub(crate) thread_id: ThreadId,
    pub(crate) session_id: SessionId,
    pub(crate) status: TaskStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExportFlowStep {
    SelectSession,
    Range,
    Destination,
    Review,
    Running,
    Completed,
    Error,
}

#[derive(Debug, Clone)]
pub(crate) struct ExportFlowState {
    pub(crate) picker: ResumePickerState,
    pub(crate) step: ExportFlowStep,
    pub(crate) range_input: ComposerInput,
    pub(crate) destination_input: ComposerInput,
    pub(crate) error: Option<String>,
    pub(crate) receipt: Option<DebugExportReceipt>,
}

#[derive(Debug)]
pub(crate) struct PendingExportOperation {
    pub(crate) task: JoinHandle<Result<DebugExportReceipt, String>>,
}

impl ExportFlowState {
    pub(crate) fn selected_thread_id(&self) -> Option<ThreadId> {
        self.picker.selected_thread_id()
    }

    pub(crate) fn selected_item(&self) -> Option<&ResumeThreadItem> {
        self.picker.items.get(self.picker.selected)
    }

    pub(crate) fn input_bytes(&self) -> usize {
        self.picker
            .input_bytes()
            .saturating_add(self.range_input.text().len())
            .saturating_add(self.destination_input.text().len())
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum ResumeSelectionDirection {
    Previous,
    Next,
}

impl ResumePickerState {
    /// 分页到达时保留搜索词、选择和用户已改过的目录条目。
    pub(crate) fn append_items(&mut self, items: Vec<ResumeThreadItem>) {
        let mut known = self
            .all_items
            .iter()
            .map(|item| item.thread_id)
            .collect::<std::collections::HashSet<_>>();
        self.all_items.extend(
            items
                .into_iter()
                .map(normalize_resume_item)
                .filter(|item| known.insert(item.thread_id)),
        );
        self.refresh_search();
    }

    pub(crate) fn new(items: Vec<ResumeThreadItem>) -> Self {
        let items = items
            .into_iter()
            .map(normalize_resume_item)
            .collect::<Vec<_>>();
        let mut state = Self {
            all_items: items.clone(),
            items,
            selected: 0,
            search: ComposerInput::default(),
            show_details: false,
            action: None,
            action_input: ComposerInput::default(),
            show_all_branches: false,
            current_thread_id: None,
        };
        state.refresh_search();
        state
    }

    pub(crate) fn selected_thread_id(&self) -> Option<ThreadId> {
        self.items.get(self.selected).map(|item| item.thread_id)
    }

    pub(crate) fn input_bytes(&self) -> usize {
        self.search
            .text()
            .len()
            .saturating_add(self.action_input.text().len())
    }

    pub(crate) fn redact_text_with(&mut self, redact: fn(&str) -> String) {
        for item in self.items.iter_mut().chain(self.all_items.iter_mut()) {
            item.title = redact(&item.title);
            item.preview = redact(&item.preview);
            item.metadata = redact(&item.metadata);
        }
        self.search.set_text(redact(self.search.text()));
        self.action_input.set_text(redact(self.action_input.text()));
    }

    pub(crate) fn move_selection(&mut self, direction: ResumeSelectionDirection) {
        if self.items.is_empty() {
            self.selected = 0;
            return;
        }
        self.selected = match direction {
            ResumeSelectionDirection::Previous => self.selected.saturating_sub(1),
            ResumeSelectionDirection::Next => {
                (self.selected + 1).min(self.items.len().saturating_sub(1))
            }
        };
    }

    pub(crate) fn move_selection_by_page(
        &mut self,
        direction: ResumeSelectionDirection,
        page_size: usize,
    ) {
        if self.items.is_empty() {
            self.selected = 0;
            return;
        }
        let page_size = page_size.max(1);
        self.selected = match direction {
            ResumeSelectionDirection::Previous => self.selected.saturating_sub(page_size),
            ResumeSelectionDirection::Next => self
                .selected
                .saturating_add(page_size)
                .min(self.items.len().saturating_sub(1)),
        };
    }

    pub(crate) fn select_first(&mut self) {
        self.selected = 0;
    }

    pub(crate) fn select_last(&mut self) {
        self.selected = self.items.len().saturating_sub(1);
    }

    pub(crate) fn refresh_search(&mut self) {
        let selected_thread = self.selected_thread_id();
        let query = self.search.text().trim().to_lowercase();
        let superseded = if self.show_all_branches {
            std::collections::HashSet::new()
        } else {
            self.all_items
                .iter()
                .filter(|item| item.forked_from_turn_id.is_some())
                .filter_map(|item| item.parent_thread_id)
                .collect::<std::collections::HashSet<_>>()
        };
        self.items = self
            .all_items
            .iter()
            .filter(|item| {
                !superseded.contains(&item.thread_id)
                    || self.current_thread_id == Some(item.thread_id)
            })
            .filter(|item| {
                query.is_empty()
                    || item.title.to_lowercase().contains(&query)
                    || item.preview.to_lowercase().contains(&query)
                    || item.metadata.to_lowercase().contains(&query)
                    || item.thread_id.to_string().contains(&query)
                    || item.session_id.to_string().contains(&query)
            })
            .cloned()
            .collect();
        self.selected = selected_thread
            .and_then(|thread_id| {
                self.items
                    .iter()
                    .position(|item| item.thread_id == thread_id)
            })
            .unwrap_or_default()
            .min(self.items.len().saturating_sub(1));
    }

    pub(crate) fn toggle_all_branches(&mut self) {
        self.show_all_branches = !self.show_all_branches;
        self.refresh_search();
    }

    pub(crate) fn set_current_thread_id(&mut self, thread_id: ThreadId) {
        self.current_thread_id = Some(thread_id);
        self.refresh_search();
    }

    pub(crate) fn begin_action(&mut self, action: SessionPickerAction) -> bool {
        let Some(item) = self.items.get(self.selected) else {
            return false;
        };
        self.action = Some(action);
        self.action_input.reset();
        if action == SessionPickerAction::Rename {
            self.action_input.set_text(item.title.clone());
        }
        true
    }

    pub(crate) fn finish_action(&mut self) {
        self.action = None;
        self.action_input.reset();
    }

    pub(crate) fn remove_selected(&mut self) {
        let Some(thread_id) = self.selected_thread_id() else {
            return;
        };
        self.all_items.retain(|item| item.thread_id != thread_id);
        self.refresh_search();
    }

    pub(crate) fn rename_selected(&mut self, title: &str) {
        let Some(thread_id) = self.selected_thread_id() else {
            return;
        };
        for item in &mut self.all_items {
            if item.thread_id == thread_id {
                item.title = title.to_owned();
            }
        }
        self.refresh_search();
    }
}

fn normalize_resume_item(mut item: ResumeThreadItem) -> ResumeThreadItem {
    if item.forked_from_turn_id.is_some() {
        let mut base = item.title.trim();
        while let Some(rest) = base.strip_prefix("Fork of ") {
            base = rest.trim_start();
        }
        item.title = base.to_owned();
    }
    item
}

pub(crate) fn session_command(
    session_id: SessionId,
    kind: SessionCommandKind,
    payload: Value,
) -> SessionCommand {
    SessionCommand {
        command_id: CommandId::new(),
        session_id: Some(session_id),
        kind,
        idempotency_key: CommandId::new().to_string(),
        actor: Actor {
            kind: ActorKind::Tui,
            id: TUI_ACTOR_ID.as_str().to_owned(),
        },
        payload,
        timestamp: chrono::Utc::now(),
    }
}

pub(crate) fn initial_session() -> (ThreadId, SessionId) {
    (ThreadId::new(), SessionId::new())
}

pub(crate) async fn resume_session(
    value: &str,
    transport: &RuntimeTransport,
) -> miette::Result<(ThreadId, SessionId)> {
    let value = value.trim();
    if value.is_empty() {
        return Err(miette::miette!("resume key cannot be empty"));
    }
    if value.chars().any(char::is_whitespace) {
        return Err(miette::miette!(
            "resume key cannot contain whitespace: {value}"
        ));
    }

    let session_id = resume_alias_session_id(value, transport);
    if let Some(thread) = transport
        .thread_for_session(session_id)
        .await
        .map_err(|error| miette::miette!("{error}"))?
    {
        return Ok((thread.thread_id, thread.session_id));
    }

    let thread_id = ThreadId::new();
    let acknowledgement = transport
        .send_command(session_command(
            session_id,
            SessionCommandKind::Create,
            json!({"_thread_id": thread_id.to_string()}),
        ))
        .await
        .map_err(|error| miette::miette!("{error}"))?;
    let thread = transport
        .thread_for_session(session_id)
        .await
        .map_err(|error| miette::miette!("{error}"))?;
    if let Some(thread) = thread {
        return Ok((thread.thread_id, thread.session_id));
    }
    let reason = acknowledgement
        .reason
        .unwrap_or_else(|| "session creation did not persist a thread".to_owned());
    Err(miette::miette!("failed to create resume session: {reason}"))
}

fn resume_alias_session_id(value: &str, transport: &RuntimeTransport) -> SessionId {
    const RESUME_ALIAS_NAMESPACE: Uuid = Uuid::from_u128(0x5ae3_10d2_9488_59ed_a50c_b6b7_fcaf_ee16);
    let workspace = transport.cwd().map_or_else(
        || transport.workspace_id().to_string(),
        |cwd| cwd.display().to_string(),
    );
    let identity = format!("{workspace}\0{value}");
    SessionId(Uuid::new_v5(&RESUME_ALIAS_NAMESPACE, identity.as_bytes()))
}

pub(crate) async fn recent_continuation_hint(
    transport: &RuntimeTransport,
) -> Result<Option<ContinuationHint>, String> {
    let threads = transport
        .list_threads(20)
        .await
        .map_err(|error| error.to_string())?;
    for thread in threads {
        let value = transport
            .query(RuntimeQuery {
                query_id: golutra_agent_core::QueryId::new(),
                session_id: thread.session_id,
                task_id: None,
                kind: RuntimeQueryKind::UserProjection,
                requester: ActorKind::Tui,
                cursor: None,
                timestamp: chrono::Utc::now(),
            })
            .await
            .map_err(|error| error.to_string())?;
        let Ok(projection) =
            serde_json::from_value::<golutra_agent_protocol::UserProjection>(value)
        else {
            continue;
        };
        if matches!(
            projection.status,
            TaskStatus::Interrupted
                | TaskStatus::Uncertain
                | TaskStatus::Partial
                | TaskStatus::WaitingApproval
                | TaskStatus::WaitingAuthentication
                | TaskStatus::Paused
        ) {
            return Ok(Some(ContinuationHint {
                thread_id: thread.thread_id,
                session_id: thread.session_id,
                status: projection.status,
            }));
        }
    }
    Ok(None)
}

pub(crate) fn parse_task_id(value: Option<&str>) -> miette::Result<Option<TaskId>> {
    value
        .map(|value| {
            Uuid::parse_str(value)
                .map(TaskId)
                .map_err(|error| miette::miette!("invalid task id: {error}"))
        })
        .transpose()
}

pub(crate) fn parse_thread_id(value: &str) -> miette::Result<ThreadId> {
    value
        .parse()
        .map_err(|error: uuid::Error| miette::miette!("invalid thread id: {error}"))
}

pub(crate) fn parse_turn_id(value: &str) -> miette::Result<TurnId> {
    value
        .parse()
        .map_err(|error: uuid::Error| miette::miette!("invalid turn id: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(
        thread_id: ThreadId,
        parent_thread_id: Option<ThreadId>,
        forked_from_turn_id: Option<TurnId>,
        title: &str,
    ) -> ResumeThreadItem {
        ResumeThreadItem {
            thread_id,
            session_id: SessionId::new(),
            parent_thread_id,
            forked_from_turn_id,
            title: title.to_owned(),
            preview: String::new(),
            metadata: String::new(),
        }
    }

    #[test]
    fn resume_picker_hides_superseded_history_parents_and_can_reveal_them() {
        let parent = ThreadId::new();
        let child = ThreadId::new();
        let sibling = ThreadId::new();
        let mut picker = ResumePickerState::new(vec![
            item(parent, None, None, "workspace"),
            item(
                child,
                Some(parent),
                Some(TurnId::new()),
                "Fork of Fork of workspace",
            ),
            item(
                sibling,
                Some(parent),
                Some(TurnId::new()),
                "Fork of workspace",
            ),
        ]);

        assert_eq!(picker.items.len(), 2);
        assert!(!picker.items.iter().any(|item| item.thread_id == parent));
        assert!(picker.items.iter().all(|item| item.title == "workspace"));

        picker.set_current_thread_id(parent);
        assert!(picker.items.iter().any(|item| item.thread_id == parent));

        picker.toggle_all_branches();
        assert_eq!(picker.items.len(), 3);
        assert!(picker.items.iter().any(|item| item.thread_id == parent));
    }
}
