//! Crash-recovery analysis for an orphaned RuntimeHost task.
//!
//! This module is intentionally pure: it inspects the durable event chain and
//! produces a typed reconciliation record. The host decides when to persist
//! that record, while this analyzer never starts a turn or replays a tool.

use std::collections::{BTreeMap, BTreeSet};

use golutra_core::{
    IncompleteToolCall, TaskId, TaskRecoveryDisposition, TaskRecoveryRecord, ToolCallId, TurnId,
};
use golutra_protocol::{RuntimeEvent, RuntimeEventType as ProtocolRuntimeEventType};
use serde_json::Value;

/// Analyze the durable facts for an active task after its owning host exits.
///
/// A started turn is never considered replay-safe. An incomplete tool is only
/// classified as uncertain when it could have changed the workspace, process
/// state, or an external system. Read-only work that stopped mid-call remains
/// visibly interrupted but does not trigger an unsafe side-effect recovery.
pub(crate) fn analyze_task(
    events: &[RuntimeEvent],
    task_id: TaskId,
    recovering_runtime_identity: &str,
) -> TaskRecoveryRecord {
    let mut started_tools = BTreeMap::<ToolCallId, IncompleteToolCall>::new();
    let mut completed_tools = BTreeSet::<ToolCallId>::new();
    let mut interrupted_turn_ids = BTreeSet::<TurnId>::new();
    let mut running_process_ids = BTreeSet::<String>::new();
    let mut checkpoint_event_refs = Vec::new();
    let mut previous_runtime_identity = None;
    let mut unparseable_incomplete_tools = 0_u32;

    for event in events {
        if previous_runtime_identity.is_none() {
            previous_runtime_identity = runtime_identity_from_event(event);
        }
        if matches!(
            event.event_type,
            ProtocolRuntimeEventType::TaskCreated | ProtocolRuntimeEventType::TurnStarted
        ) && let Some(turn_id) = event.turn_id
        {
            interrupted_turn_ids.insert(turn_id);
        }
        if event.event_type == ProtocolRuntimeEventType::CheckpointCreated {
            checkpoint_event_refs.push(event.id);
        }
        match event.event_type {
            ProtocolRuntimeEventType::ToolStarted => {
                let Some(tool_name) = event.payload.get("tool_name").and_then(Value::as_str) else {
                    unparseable_incomplete_tools = unparseable_incomplete_tools.saturating_add(1);
                    continue;
                };
                let Some(raw_tool_call_id) =
                    event.payload.get("tool_call_id").and_then(Value::as_str)
                else {
                    unparseable_incomplete_tools = unparseable_incomplete_tools.saturating_add(1);
                    continue;
                };
                let Ok(tool_call_id) = raw_tool_call_id.parse::<ToolCallId>() else {
                    unparseable_incomplete_tools = unparseable_incomplete_tools.saturating_add(1);
                    continue;
                };
                started_tools.insert(
                    tool_call_id,
                    IncompleteToolCall {
                        tool_call_id,
                        tool_name: tool_name.to_owned(),
                        side_effect_possible: side_effect_possible(tool_name),
                        started_event_ref: event.id,
                    },
                );
            }
            ProtocolRuntimeEventType::ToolCompleted => {
                if let Some(tool_call_id) = tool_call_id_from_completed(event) {
                    completed_tools.insert(tool_call_id);
                    started_tools.remove(&tool_call_id);
                }
                update_running_processes(&mut running_process_ids, event);
            }
            _ => {}
        }
    }

    // A completion can be loaded before its corresponding start in a damaged
    // or partially migrated event stream. Remove those IDs defensively.
    for tool_call_id in completed_tools {
        started_tools.remove(&tool_call_id);
    }
    let incomplete_tool_calls = started_tools.into_values().collect::<Vec<_>>();
    let uncertain = unparseable_incomplete_tools > 0
        || !running_process_ids.is_empty()
        || incomplete_tool_calls
            .iter()
            .any(|tool| tool.side_effect_possible);
    let disposition = if uncertain {
        TaskRecoveryDisposition::Uncertain
    } else {
        TaskRecoveryDisposition::Interrupted
    };
    let reconciliation_required = uncertain;
    let reason = if uncertain {
        "runtime host stopped while a side-effecting tool or process could not be reconciled"
    } else {
        "runtime host stopped before the active task reached a durable terminal event"
    };

    TaskRecoveryRecord {
        task_id,
        disposition,
        interrupted_turn_ids: interrupted_turn_ids.into_iter().collect(),
        incomplete_tool_calls,
        running_process_ids: running_process_ids.into_iter().collect(),
        checkpoint_event_refs,
        last_event_ref: events.last().map(|event| event.id),
        previous_runtime_identity,
        recovering_runtime_identity: recovering_runtime_identity.to_owned(),
        safe_to_replay: false,
        reconciliation_required,
        reason: reason.to_owned(),
        detected_at: chrono::Utc::now(),
    }
}

fn side_effect_possible(tool_name: &str) -> bool {
    !matches!(
        tool_name,
        "read_file"
            | "list_dir"
            | "rg_search"
            | "symbol_search"
            | "find_references"
            | "process_poll"
            | "process_reconnect"
    )
}

fn tool_call_id_from_completed(event: &RuntimeEvent) -> Option<ToolCallId> {
    event
        .payload
        .get("tool_call_id")
        .or_else(|| {
            event
                .payload
                .get("envelope")
                .and_then(|value| value.get("tool_call_id"))
        })
        .and_then(Value::as_str)
        .and_then(|value| value.parse().ok())
}

fn update_running_processes(running: &mut BTreeSet<String>, event: &RuntimeEvent) {
    let Some(facts) = event.payload.pointer("/envelope/structured_facts") else {
        return;
    };
    let Some(process_id) = facts.get("process_id").and_then(Value::as_str) else {
        return;
    };
    let state = facts
        .get("process_state")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    if matches!(state, "running" | "unknown") {
        running.insert(process_id.to_owned());
    } else {
        running.remove(process_id);
    }
}

fn runtime_identity_from_event(event: &RuntimeEvent) -> Option<String> {
    event
        .payload
        .get("runtime_identity")
        .or_else(|| event.payload.pointer("/runtime/runtime_identity"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use golutra_core::{EventId, Timestamp};
    use golutra_protocol::{RuntimeEventSource, RuntimeEventType};
    use serde_json::json;

    fn event(
        sequence_no: u64,
        event_type: RuntimeEventType,
        turn_id: Option<TurnId>,
        payload: Value,
    ) -> RuntimeEvent {
        RuntimeEvent {
            schema_version: golutra_core::RUNTIME_EVENT_SCHEMA_VERSION,
            causal_context: Default::default(),
            causal_links: Vec::new(),
            id: EventId::new(),
            sequence_no,
            session_id: golutra_core::SessionId::new(),
            turn_id,
            task_id: Some(TaskId::new()),
            parent_event_id: None,
            event_type,
            timestamp: Timestamp::default(),
            source: RuntimeEventSource::Runtime,
            payload,
            payload_ref: None,
            durable: true,
        }
    }

    #[test]
    fn read_only_interruption_is_not_marked_as_side_effect_uncertain() {
        let task_id = TaskId::new();
        let turn_id = TurnId::new();
        let tool_call_id = ToolCallId::new();
        let mut started = event(
            1,
            RuntimeEventType::TurnStarted,
            Some(turn_id),
            json!({"runtime_identity": "old"}),
        );
        started.task_id = Some(task_id);
        let mut tool = event(
            2,
            RuntimeEventType::ToolStarted,
            Some(turn_id),
            json!({"tool_call_id": tool_call_id, "tool_name": "read_file"}),
        );
        tool.task_id = Some(task_id);

        let record = analyze_task(&[started, tool], task_id, "new");

        assert_eq!(record.disposition, TaskRecoveryDisposition::Interrupted);
        assert!(!record.reconciliation_required);
        assert!(!record.safe_to_replay);
        assert_eq!(record.previous_runtime_identity.as_deref(), Some("old"));
    }

    #[test]
    fn side_effect_and_running_process_are_marked_uncertain() {
        let task_id = TaskId::new();
        let turn_id = TurnId::new();
        let tool_call_id = ToolCallId::new();
        let process_id = "proc-test";
        let mut shell = event(
            2,
            RuntimeEventType::ToolCompleted,
            Some(turn_id),
            json!({
                "envelope": {
                    "tool_call_id": ToolCallId::new(),
                    "structured_facts": {
                        "process_id": process_id,
                        "process_state": "running"
                    }
                }
            }),
        );
        shell.task_id = Some(task_id);
        let mut started = event(
            1,
            RuntimeEventType::ToolStarted,
            Some(turn_id),
            json!({"tool_call_id": tool_call_id, "tool_name": "write_file"}),
        );
        started.task_id = Some(task_id);

        let record = analyze_task(&[started, shell], task_id, "new");

        assert_eq!(record.disposition, TaskRecoveryDisposition::Uncertain);
        assert!(record.reconciliation_required);
        assert_eq!(record.running_process_ids, vec![process_id.to_owned()]);
        assert_eq!(record.incomplete_tool_calls.len(), 1);
    }

    #[test]
    fn completed_tool_closes_a_started_side_effect_call() {
        let task_id = TaskId::new();
        let turn_id = TurnId::new();
        let tool_call_id = ToolCallId::new();
        let mut started = event(
            1,
            RuntimeEventType::ToolStarted,
            Some(turn_id),
            json!({"tool_call_id": tool_call_id, "tool_name": "write_file"}),
        );
        started.task_id = Some(task_id);
        let mut completed = event(
            2,
            RuntimeEventType::ToolCompleted,
            Some(turn_id),
            json!({
                "tool_call_id": tool_call_id,
                "envelope": {"status": "ok"}
            }),
        );
        completed.task_id = Some(task_id);

        let record = analyze_task(&[started, completed], task_id, "new");

        assert_eq!(record.disposition, TaskRecoveryDisposition::Interrupted);
        assert!(record.incomplete_tool_calls.is_empty());
    }
}
