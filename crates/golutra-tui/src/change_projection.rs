//! Replayable projection of the current turn's workspace changes.

use golutra_core::{FileChangeKind, FileChangeSummary, TaskId, TurnChangeSummary, TurnId};
use golutra_protocol::{RuntimeEvent, RuntimeEventType};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ChangeProjection {
    task_id: Option<TaskId>,
    turn_id: Option<TurnId>,
    summary: Option<TurnChangeSummary>,
}

impl ChangeProjection {
    pub(crate) fn rebuild(&mut self, events: &[RuntimeEvent]) {
        *self = Self::default();
        let mut events = events.to_vec();
        events.sort_by_key(|event| event.sequence_no);
        for event in &events {
            self.apply(event);
        }
    }

    pub(crate) fn apply(&mut self, event: &RuntimeEvent) {
        let Some(task_id) = event.task_id else {
            return;
        };
        if self.task_id.is_none() {
            self.reset(task_id, event.turn_id);
        } else if self.task_id != Some(task_id) {
            if !matches!(
                event.event_type,
                RuntimeEventType::TaskCreated | RuntimeEventType::TurnStarted
            ) {
                return;
            }
            self.reset(task_id, event.turn_id);
        }
        if event.event_type == RuntimeEventType::TurnStarted && self.turn_id != event.turn_id {
            self.reset(task_id, event.turn_id);
        }
        if event.event_type != RuntimeEventType::ToolCompleted {
            return;
        }
        if self.turn_id.is_some() && event.turn_id.is_some() && self.turn_id != event.turn_id {
            return;
        }
        if self.turn_id.is_none() {
            self.turn_id = event.turn_id;
        }

        if let Some(summary) = event_turn_change_summary(event) {
            self.summary = Some(summary);
        }
    }

    pub(crate) fn summary(&self) -> Option<&TurnChangeSummary> {
        self.summary.as_ref()
    }

    fn reset(&mut self, task_id: TaskId, turn_id: Option<TurnId>) {
        self.task_id = Some(task_id);
        self.turn_id = turn_id;
        self.summary = None;
    }
}

pub(crate) fn operation_file_changes(event: &RuntimeEvent) -> Vec<FileChangeSummary> {
    event
        .payload
        .get("file_changes")
        .cloned()
        .and_then(|value| serde_json::from_value(value).ok())
        .unwrap_or_else(|| legacy_file_changes(event))
}

fn event_turn_change_summary(event: &RuntimeEvent) -> Option<TurnChangeSummary> {
    event
        .payload
        .get("turn_change_summary")
        .cloned()
        .and_then(|value| serde_json::from_value(value).ok())
        .or_else(|| {
            let files = legacy_file_changes(event);
            let file_count = u64::try_from(files.len()).unwrap_or(u64::MAX);
            (!files.is_empty()).then_some(TurnChangeSummary {
                files,
                added_lines: None,
                removed_lines: None,
                stats_complete: false,
                file_count,
                files_truncated: false,
            })
        })
}

fn legacy_file_changes(event: &RuntimeEvent) -> Vec<FileChangeSummary> {
    event
        .payload
        .get("changed_files")
        .and_then(|value| value.as_array())
        .into_iter()
        .flatten()
        .filter_map(|path| path.as_str())
        .map(|path| FileChangeSummary {
            path: path.to_owned(),
            kind: FileChangeKind::Modified,
            added_lines: None,
            removed_lines: None,
            before: None,
            after: None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use golutra_core::{EventId, SessionId};
    use golutra_protocol::RuntimeEventSource;
    use serde_json::json;

    use super::*;

    fn event(
        sequence_no: u64,
        task_id: TaskId,
        turn_id: TurnId,
        event_type: RuntimeEventType,
        payload: serde_json::Value,
    ) -> RuntimeEvent {
        RuntimeEvent {
            schema_version: golutra_core::RUNTIME_EVENT_SCHEMA_VERSION,
            causal_context: Default::default(),
            causal_links: Vec::new(),
            id: EventId::new(),
            sequence_no,
            session_id: SessionId::new(),
            turn_id: Some(turn_id),
            task_id: Some(task_id),
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
    fn replay_uses_the_latest_durable_turn_snapshot() {
        let task = TaskId::new();
        let turn = TurnId::new();
        let events = vec![
            event(1, task, turn, RuntimeEventType::TaskCreated, json!({})),
            event(
                2,
                task,
                turn,
                RuntimeEventType::ToolCompleted,
                json!({
                    "turn_change_summary": {
                        "files": [{
                            "path": "src/lib.rs",
                            "kind": "modified",
                            "added_lines": 3,
                            "removed_lines": 1
                        }],
                        "added_lines": 3,
                        "removed_lines": 1,
                        "stats_complete": true
                    }
                }),
            ),
        ];
        let mut projection = ChangeProjection::default();

        projection.rebuild(&events);

        let summary = projection.summary().expect("change summary");
        assert_eq!(summary.added_lines, Some(3));
        assert_eq!(summary.removed_lines, Some(1));
    }

    #[test]
    fn late_events_from_an_old_task_do_not_replace_current_changes() {
        let old_task = TaskId::new();
        let active_task = TaskId::new();
        let turn = TurnId::new();
        let mut projection = ChangeProjection::default();
        projection.apply(&event(
            1,
            active_task,
            turn,
            RuntimeEventType::TaskCreated,
            json!({}),
        ));
        projection.apply(&event(
            2,
            old_task,
            turn,
            RuntimeEventType::ToolCompleted,
            json!({"changed_files": ["old.rs"]}),
        ));

        assert!(projection.summary().is_none());
    }

    #[test]
    fn legacy_non_file_tool_events_do_not_clear_known_turn_changes() {
        let task = TaskId::new();
        let turn = TurnId::new();
        let mut projection = ChangeProjection::default();
        projection.apply(&event(
            1,
            task,
            turn,
            RuntimeEventType::ToolCompleted,
            json!({"changed_files": ["src/lib.rs"]}),
        ));
        projection.apply(&event(
            2,
            task,
            turn,
            RuntimeEventType::ToolCompleted,
            json!({"summary": "shell completed"}),
        ));

        let summary = projection.summary().expect("legacy change summary");
        assert_eq!(summary.files[0].path, "src/lib.rs");
        assert!(!summary.stats_complete);
    }
}
