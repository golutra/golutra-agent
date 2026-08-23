//! TUI 会话选择、命令构造与 ID 解析。

use golutra_client::{DebugExportReceipt, RuntimeClient, RuntimeTransport};
use golutra_core::{Actor, ActorKind, CommandId, SessionId, TaskId, TaskStatus, ThreadId, TurnId};
use golutra_protocol::{RuntimeQuery, RuntimeQueryKind, SessionCommand, SessionCommandKind};
use serde_json::Value;
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
    pub(crate) title: String,
    pub(crate) preview: String,
    pub(crate) metadata: String,
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
    pub(crate) fn new(items: Vec<ResumeThreadItem>) -> Self {
        Self {
            all_items: items.clone(),
            items,
            selected: 0,
            search: ComposerInput::default(),
            show_details: false,
            action: None,
            action_input: ComposerInput::default(),
        }
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
        self.items = self
            .all_items
            .iter()
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

pub(crate) async fn initial_session(
    value: Option<&str>,
    transport: &RuntimeTransport,
) -> miette::Result<(ThreadId, SessionId)> {
    if let Some(value) = value {
        let session_id = Uuid::parse_str(value)
            .map(SessionId)
            .map_err(|error| miette::miette!("invalid session id: {error}"))?;
        let thread_id = transport
            .thread_for_session(session_id)
            .await
            .map_err(|error| miette::miette!("{error}"))?
            .map_or_else(ThreadId::new, |thread| thread.thread_id);
        return Ok((thread_id, session_id));
    }
    Ok((ThreadId::new(), SessionId::new()))
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
                query_id: golutra_core::QueryId::new(),
                session_id: thread.session_id,
                task_id: None,
                kind: RuntimeQueryKind::UserProjection,
                requester: ActorKind::Tui,
                cursor: None,
                timestamp: chrono::Utc::now(),
            })
            .await
            .map_err(|error| error.to_string())?;
        let Ok(projection) = serde_json::from_value::<golutra_protocol::UserProjection>(value)
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
