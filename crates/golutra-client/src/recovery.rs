//! Crash-recovery analysis for an orphaned RuntimeHost task.
//!
//! This module is intentionally pure: it inspects the durable event chain and
//! produces a typed reconciliation record. The host decides when to persist
//! that record, while this analyzer never starts a turn or replays a tool.

use std::collections::{BTreeMap, BTreeSet};

use golutra_core::{
    IncompleteToolCall, InterruptedToolAction, ProviderRequestId, SideEffectType, TaskId,
    TaskRecoveryDisposition, TaskRecoveryRecord, ToolCallId, ToolRecoveryPolicy, TurnId,
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
    let journal = DurableTurnJournal::reduce(events);
    let incomplete_tool_calls = journal.incomplete_tool_calls();
    let incomplete_provider_request_ids = journal.incomplete_provider_requests();
    let uncertain = journal.unparseable_incomplete_tools > 0
        || !journal.running_process_ids.is_empty()
        || incomplete_tool_calls.iter().any(|tool| {
            tool.recovery_policy.interrupted_action != InterruptedToolAction::ReplaySafe
        });
    let disposition = if uncertain {
        TaskRecoveryDisposition::Uncertain
    } else {
        TaskRecoveryDisposition::Interrupted
    };
    let reconciliation_required = uncertain;
    let reason = if uncertain {
        "runtime host stopped while an interrupted tool or process requires reconciliation"
    } else if !incomplete_provider_request_ids.is_empty() {
        "runtime host stopped with an incomplete provider request and no uncertain side effect"
    } else {
        "runtime host stopped before the active task reached a durable terminal event"
    };

    TaskRecoveryRecord {
        task_id,
        disposition,
        interrupted_turn_ids: journal.interrupted_turn_ids.into_iter().collect(),
        incomplete_tool_calls,
        incomplete_provider_request_ids,
        running_process_ids: journal.running_process_ids.into_iter().collect(),
        checkpoint_event_refs: journal.checkpoint_event_refs,
        last_event_ref: events.last().map(|event| event.id),
        previous_runtime_identity: journal.previous_runtime_identity,
        recovering_runtime_identity: recovering_runtime_identity.to_owned(),
        safe_to_replay: false,
        reconciliation_required,
        reason: reason.to_owned(),
        detected_at: chrono::Utc::now(),
    }
}

#[derive(Debug, Default)]
struct DurableTurnJournal {
    started_tools: BTreeMap<ToolCallId, IncompleteToolCall>,
    completed_tools: BTreeSet<ToolCallId>,
    active_provider_requests: BTreeMap<ProviderRequestId, ()>,
    completed_provider_requests: BTreeSet<ProviderRequestId>,
    interrupted_turn_ids: BTreeSet<TurnId>,
    running_process_ids: BTreeSet<String>,
    checkpoint_event_refs: Vec<golutra_core::EventId>,
    previous_runtime_identity: Option<String>,
    unparseable_incomplete_tools: u32,
}

impl DurableTurnJournal {
    fn reduce(events: &[RuntimeEvent]) -> Self {
        let mut journal = Self::default();
        for event in events {
            journal.apply(event);
        }
        journal
    }

    fn apply(&mut self, event: &RuntimeEvent) {
        if self.previous_runtime_identity.is_none() {
            self.previous_runtime_identity = runtime_identity_from_event(event);
        }
        if matches!(
            event.event_type,
            ProtocolRuntimeEventType::TaskCreated | ProtocolRuntimeEventType::TurnStarted
        ) && let Some(turn_id) = event.turn_id
        {
            self.interrupted_turn_ids.insert(turn_id);
        }
        if event.event_type == ProtocolRuntimeEventType::CheckpointCreated {
            self.checkpoint_event_refs.push(event.id);
        }
        match event.event_type {
            ProtocolRuntimeEventType::ProviderStarted => {
                if let Some(request_id) = provider_request_id(event) {
                    self.active_provider_requests.insert(request_id, ());
                }
            }
            ProtocolRuntimeEventType::ProviderCompleted
            | ProtocolRuntimeEventType::ProviderFailed => {
                if let Some(request_id) = provider_request_id(event) {
                    self.completed_provider_requests.insert(request_id);
                    self.active_provider_requests.remove(&request_id);
                }
            }
            ProtocolRuntimeEventType::ToolStarted => {
                let Some(tool_name) = event.payload.get("tool_name").and_then(Value::as_str) else {
                    self.unparseable_incomplete_tools =
                        self.unparseable_incomplete_tools.saturating_add(1);
                    return;
                };
                let Some(raw_tool_call_id) =
                    event.payload.get("tool_call_id").and_then(Value::as_str)
                else {
                    self.unparseable_incomplete_tools =
                        self.unparseable_incomplete_tools.saturating_add(1);
                    return;
                };
                let Ok(tool_call_id) = raw_tool_call_id.parse::<ToolCallId>() else {
                    self.unparseable_incomplete_tools =
                        self.unparseable_incomplete_tools.saturating_add(1);
                    return;
                };
                let recovery_policy = event
                    .payload
                    .get("recovery_policy")
                    .cloned()
                    .and_then(|value| serde_json::from_value(value).ok())
                    .unwrap_or_else(|| legacy_recovery_policy(tool_name));
                self.started_tools.insert(
                    tool_call_id,
                    IncompleteToolCall {
                        tool_call_id,
                        tool_name: tool_name.to_owned(),
                        side_effect_possible: recovery_policy.side_effect_possible(),
                        recovery_policy,
                        started_event_ref: event.id,
                    },
                );
            }
            ProtocolRuntimeEventType::ToolCompleted => {
                if let Some(tool_call_id) = tool_call_id_from_completed(event) {
                    self.completed_tools.insert(tool_call_id);
                    self.started_tools.remove(&tool_call_id);
                }
                update_running_processes(&mut self.running_process_ids, event);
            }
            _ => {}
        }
    }

    fn incomplete_tool_calls(&self) -> Vec<IncompleteToolCall> {
        self.started_tools
            .iter()
            .filter(|(tool_call_id, _)| !self.completed_tools.contains(tool_call_id))
            .map(|(_, tool)| tool.clone())
            .collect()
    }

    fn incomplete_provider_requests(&self) -> Vec<ProviderRequestId> {
        self.active_provider_requests
            .keys()
            .filter(|request_id| !self.completed_provider_requests.contains(request_id))
            .copied()
            .collect()
    }
}

fn legacy_recovery_policy(tool_name: &str) -> ToolRecoveryPolicy {
    let side_effect_type = if matches!(
        tool_name,
        "read_file"
            | "list_dir"
            | "rg_search"
            | "symbol_search"
            | "find_references"
            | "process_poll"
            | "process_reconnect"
    ) {
        SideEffectType::None
    } else {
        SideEffectType::ExternalSystem
    };
    ToolRecoveryPolicy::for_side_effect(side_effect_type)
}

fn provider_request_id(event: &RuntimeEvent) -> Option<ProviderRequestId> {
    event
        .payload
        .get("provider_request_id")
        .and_then(Value::as_str)
        .and_then(|value| value.parse().ok())
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
        assert_eq!(
            record.incomplete_tool_calls[0]
                .recovery_policy
                .interrupted_action,
            InterruptedToolAction::ReplaySafe
        );
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

    #[test]
    fn durable_recovery_policy_is_authoritative_over_legacy_tool_names() {
        let task_id = TaskId::new();
        let tool_call_id = ToolCallId::new();
        let recovery_policy = ToolRecoveryPolicy::for_side_effect(SideEffectType::Process);
        let mut started = event(
            1,
            RuntimeEventType::ToolStarted,
            Some(TurnId::new()),
            json!({
                "tool_call_id": tool_call_id,
                "tool_name": "read_file",
                "recovery_policy": recovery_policy,
            }),
        );
        started.task_id = Some(task_id);

        let record = analyze_task(&[started], task_id, "new");

        assert_eq!(record.disposition, TaskRecoveryDisposition::Uncertain);
        assert!(record.reconciliation_required);
        assert_eq!(
            record.incomplete_tool_calls[0]
                .recovery_policy
                .interrupted_action,
            InterruptedToolAction::ReconcileBeforeRetry
        );
    }

    #[test]
    fn provider_lifecycle_reduces_to_incomplete_requests_deterministically() {
        let task_id = TaskId::new();
        let incomplete_id = ProviderRequestId::new();
        let completed_id = ProviderRequestId::new();
        let mut events = vec![
            event(
                1,
                RuntimeEventType::ProviderStarted,
                Some(TurnId::new()),
                json!({"provider_request_id": incomplete_id}),
            ),
            event(
                2,
                RuntimeEventType::ProviderStarted,
                Some(TurnId::new()),
                json!({"provider_request_id": completed_id}),
            ),
            event(
                3,
                RuntimeEventType::ProviderCompleted,
                Some(TurnId::new()),
                json!({"provider_request_id": completed_id}),
            ),
        ];
        for event in &mut events {
            event.task_id = Some(task_id);
        }

        let record = analyze_task(&events, task_id, "new");

        assert_eq!(record.incomplete_provider_request_ids, vec![incomplete_id]);
        assert_eq!(record.disposition, TaskRecoveryDisposition::Interrupted);
        assert!(!record.reconciliation_required);
    }
}
