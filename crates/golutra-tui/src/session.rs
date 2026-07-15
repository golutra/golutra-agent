//! TUI 会话选择、命令构造与 ID 解析。

use golutra_client::RuntimeTransport;
use golutra_core::{Actor, ActorKind, CommandId, SessionId, TaskId, ThreadId, TurnId};
use golutra_protocol::{SessionCommand, SessionCommandKind};
use serde_json::Value;
use uuid::Uuid;

use super::TUI_ACTOR_ID;

#[derive(Debug, Clone)]
pub(crate) struct ResumePickerState {
    pub(crate) items: Vec<ResumeThreadItem>,
    pub(crate) selected: usize,
}

#[derive(Debug, Clone)]
pub(crate) struct ResumeThreadItem {
    pub(crate) thread_id: ThreadId,
    pub(crate) session_id: SessionId,
    pub(crate) title: String,
    pub(crate) preview: String,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum ResumeSelectionDirection {
    Previous,
    Next,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TranscriptScrollAction {
    LineUp,
    LineDown,
    PageUp,
    PageDown,
    Top,
    Bottom,
}

impl ResumePickerState {
    pub(crate) fn selected_thread_id(&self) -> Option<ThreadId> {
        self.items.get(self.selected).map(|item| item.thread_id)
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
