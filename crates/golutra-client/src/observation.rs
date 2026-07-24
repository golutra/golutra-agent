//! Canonical collection of complete runtime observations.
//!
//! This module owns no durable state. It reads the same session, event and
//! task-trace APIs used by interactive clients, then freezes a bounded
//! snapshot that exporters, benchmark harnesses and regression tooling can
//! serialize in their own formats.

use std::collections::{BTreeSet, HashSet};

use chrono::{DateTime, Utc};
use golutra_core::{SessionId, TaskId, TraceView};
use golutra_protocol::{
    EventPageDirection, EventPageRequest, RuntimeEvent, RuntimeEventType, SessionSummary,
    SessionWindowRequest, TaskTracePage, TaskTraceRequest,
};
use golutra_store::ThreadRecord;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{ClientError, RuntimeClient, RuntimeTransport, TaskTraceClient};

pub(crate) const MAX_EVENT_OBSERVATION_PAGES: usize = 65_536;

/// A stable, full-fidelity session snapshot assembled from canonical runtime
/// facts. Raw prompts and tool observations are intentionally retained here;
/// callers must keep the serialized form owner-only or redact it before
/// sharing.
#[derive(Debug, Clone)]
pub struct RuntimeObservationSnapshot {
    pub selection: SessionWindowRequest,
    pub sessions: Vec<ObservedSession>,
    pub complete: bool,
    pub missing_data: Vec<String>,
    pub retention_losses: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ObservedSession {
    pub summary: SessionSummary,
    pub thread: ThreadRecord,
    pub events: Vec<RuntimeEvent>,
    pub conversation: Vec<ConversationEntry>,
    pub tasks: Vec<ObservedTask>,
    pub events_complete: bool,
}

#[derive(Debug, Clone)]
pub struct ObservedTask {
    pub task_id: TaskId,
    pub trace: TaskTracePage,
}

/// Human conversation derived from the event stream. Tool and governance
/// observations remain in `events` and each task's complete trace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConversationEntry {
    pub sequence_no: u64,
    pub timestamp: DateTime<Utc>,
    pub turn_id: Option<String>,
    pub task_id: Option<TaskId>,
    pub role: ConversationRole,
    pub content: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConversationRole {
    User,
    Assistant,
}

/// Reads a full, bounded snapshot once. The collector deliberately does not
/// decide how data is persisted or disclosed, allowing a raw run bundle and a
/// redacted debug export to share one observation path.
#[derive(Debug, Clone)]
pub struct RuntimeObservationCollector<'a> {
    transport: &'a RuntimeTransport,
}

impl<'a> RuntimeObservationCollector<'a> {
    #[must_use]
    pub fn new(transport: &'a RuntimeTransport) -> Self {
        Self { transport }
    }

    pub async fn collect(
        &self,
        selection: SessionWindowRequest,
    ) -> Result<RuntimeObservationSnapshot, ClientError> {
        self.collect_with_evaluation_policy(selection, false).await
    }

    /// Collect after giving each discovered task's durable post-task job a
    /// bounded opportunity to reach a terminal state. This is intended for
    /// retained run bundles, where fidelity is more important than returning
    /// immediately after the agent turn ends.
    pub async fn collect_settled(
        &self,
        selection: SessionWindowRequest,
    ) -> Result<RuntimeObservationSnapshot, ClientError> {
        self.collect_with_evaluation_policy(selection, true).await
    }

    async fn collect_with_evaluation_policy(
        &self,
        selection: SessionWindowRequest,
        wait_for_evaluation: bool,
    ) -> Result<RuntimeObservationSnapshot, ClientError> {
        let window = self.transport.session_window(selection.clone()).await?;
        if window.sessions.is_empty() {
            return Err(ClientError::InvalidSession(
                "runtime observation selection contains no sessions".to_owned(),
            ));
        }

        let mut sessions = Vec::with_capacity(window.sessions.len());
        let mut missing_data = Vec::new();
        let mut retention_losses = Vec::new();
        for summary in window.sessions {
            let observed = self.collect_session(summary, wait_for_evaluation).await?;
            for task in &observed.tasks {
                missing_data.extend(
                    task.trace
                        .integrity
                        .unresolved_refs
                        .iter()
                        .map(|value| format!("task:{}:{value}", task.task_id)),
                );
                missing_data.extend(
                    task.trace
                        .integrity
                        .missing_sections
                        .iter()
                        .map(|value| format!("task:{}:{value}", task.task_id)),
                );
                retention_losses.extend(
                    task.trace
                        .integrity
                        .retention_losses
                        .iter()
                        .map(|value| format!("task:{}:{value}", task.task_id)),
                );
            }
            if !observed.events_complete {
                missing_data.push(format!(
                    "session:{}:events changed during observation",
                    observed.summary.session_id
                ));
            }
            sessions.push(observed);
        }
        missing_data.sort();
        missing_data.dedup();
        retention_losses.sort();
        retention_losses.dedup();
        let complete = missing_data.is_empty()
            && retention_losses.is_empty()
            && sessions.iter().all(|session| {
                session.events_complete
                    && session
                        .tasks
                        .iter()
                        .all(|task| task.trace.integrity.complete)
            });

        Ok(RuntimeObservationSnapshot {
            selection,
            sessions,
            complete,
            missing_data,
            retention_losses,
        })
    }

    async fn collect_session(
        &self,
        summary: SessionSummary,
        wait_for_evaluation: bool,
    ) -> Result<ObservedSession, ClientError> {
        let thread = self
            .transport
            .thread_for_session(summary.session_id)
            .await?
            .ok_or_else(|| {
                ClientError::InvalidSession(format!(
                    "thread for session `{}` disappeared during observation",
                    summary.session_id
                ))
            })?;

        if wait_for_evaluation {
            let initial_end_sequence =
                latest_event_sequence(self.transport, summary.session_id).await?;
            let initial_events =
                load_all_events(self.transport, summary.session_id, initial_end_sequence).await?;
            let task_ids = initial_events
                .iter()
                .filter_map(|event| event.task_id)
                .collect::<BTreeSet<_>>();
            for task_id in task_ids {
                self.transport
                    .task_trace(TaskTraceRequest {
                        session_id: summary.session_id,
                        task_id,
                        view: TraceView::Full,
                        cursor: None,
                        limit: 1,
                        wait_for_evaluation: true,
                    })
                    .await?;
            }
        }

        // Freeze the event boundary only after optional post-task waiting so
        // terminal evaluation events are included in the retained snapshot.
        let snapshot_end_sequence =
            latest_event_sequence(self.transport, summary.session_id).await?;
        let events =
            load_all_events(self.transport, summary.session_id, snapshot_end_sequence).await?;
        let conversation = conversation_entries(&events);
        let task_ids = events
            .iter()
            .filter_map(|event| event.task_id)
            .collect::<BTreeSet<_>>();
        let mut tasks = Vec::with_capacity(task_ids.len());
        for task_id in task_ids {
            let trace = self
                .transport
                .complete_task_trace(TaskTraceRequest {
                    session_id: summary.session_id,
                    task_id,
                    view: TraceView::Full,
                    cursor: None,
                    limit: 512,
                    wait_for_evaluation: false,
                })
                .await?;
            tasks.push(ObservedTask { task_id, trace });
        }
        let events_complete = event_snapshot_is_stable(
            snapshot_end_sequence,
            latest_event_sequence(self.transport, summary.session_id).await?,
        );
        Ok(ObservedSession {
            summary,
            thread,
            events,
            conversation,
            tasks,
            events_complete,
        })
    }
}

async fn latest_event_sequence(
    transport: &RuntimeTransport,
    session_id: SessionId,
) -> Result<Option<u64>, ClientError> {
    let page = transport
        .event_page(EventPageRequest {
            session_id,
            task_id: None,
            cursor: None,
            direction: EventPageDirection::Backward,
            limit: 1,
        })
        .await?;
    Ok(page.events.last().map(|event| event.sequence_no))
}

async fn load_all_events(
    transport: &RuntimeTransport,
    session_id: SessionId,
    upper_bound: Option<u64>,
) -> Result<Vec<RuntimeEvent>, ClientError> {
    let mut cursor = None;
    let mut events = Vec::new();
    for _ in 0..MAX_EVENT_OBSERVATION_PAGES {
        let page = transport
            .event_page(EventPageRequest {
                session_id,
                task_id: None,
                cursor,
                direction: EventPageDirection::Forward,
                limit: 512,
            })
            .await?;
        if page.events.is_empty() {
            if page.has_more {
                return Err(ClientError::TaskExecution(
                    "event observation page has_more without events".to_owned(),
                ));
            }
            return Ok(events);
        }
        let next = page.end_cursor.ok_or_else(|| {
            ClientError::TaskExecution("event observation page has no end cursor".to_owned())
        })?;
        if cursor == Some(next) {
            return Err(ClientError::TaskExecution(
                "event observation cursor did not advance".to_owned(),
            ));
        }
        cursor = Some(next);
        let reached_upper_bound = upper_bound.is_some_and(|upper_bound| {
            page.events
                .iter()
                .any(|event| event.sequence_no >= upper_bound)
        });
        events.extend(page.events.into_iter().filter(|event| {
            upper_bound.is_none_or(|upper_bound| event.sequence_no <= upper_bound)
        }));
        if reached_upper_bound || !page.has_more {
            return Ok(events);
        }
    }
    Err(ClientError::TaskExecution(format!(
        "session event observation exceeds {MAX_EVENT_OBSERVATION_PAGES} pages"
    )))
}

pub(crate) fn conversation_entries(events: &[RuntimeEvent]) -> Vec<ConversationEntry> {
    let mut entries = Vec::new();
    let mut user_turns = HashSet::new();
    for event in events {
        let (role, content) = match event.event_type {
            RuntimeEventType::TaskCreated | RuntimeEventType::TurnQueued => (
                ConversationRole::User,
                event
                    .payload
                    .pointer("/payload/prompt")
                    .and_then(Value::as_str),
            ),
            RuntimeEventType::TurnStarted => (
                ConversationRole::User,
                event.payload.get("prompt").and_then(Value::as_str),
            ),
            RuntimeEventType::AssistantMessage => (
                ConversationRole::Assistant,
                event.payload.get("content").and_then(Value::as_str),
            ),
            _ => continue,
        };
        let Some(content) = content.filter(|content| !content.trim().is_empty()) else {
            continue;
        };
        if role == ConversationRole::User
            && let Some(turn_id) = event.turn_id
            && !user_turns.insert(turn_id)
        {
            continue;
        }
        entries.push(ConversationEntry {
            sequence_no: event.sequence_no,
            timestamp: event.timestamp,
            turn_id: event.turn_id.map(|turn_id| turn_id.to_string()),
            task_id: event.task_id,
            role,
            content: content.to_owned(),
        });
    }
    entries
}

pub(crate) fn event_snapshot_is_stable(before: Option<u64>, after: Option<u64>) -> bool {
    before == after
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_snapshot_detects_concurrent_session_growth() {
        assert!(event_snapshot_is_stable(Some(10), Some(10)));
        assert!(event_snapshot_is_stable(None, None));
        assert!(!event_snapshot_is_stable(Some(10), Some(11)));
        assert!(!event_snapshot_is_stable(None, Some(1)));
    }
}
