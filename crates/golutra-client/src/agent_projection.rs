//! Shared projection from durable runtime facts to the Agent stream contract.
//!
//! Every adapter (embedded client, app-server transports and external SDK
//! gateways) uses this projector.  The command/turn correlation is important:
//! a queued turn must not finish when the task that was active before it is
//! completed.

use golutra_core::{
    CommandId, TaskId, TaskOutcome, TaskStatus, ToolResultStatus, TurnId, VerificationRecord,
    VerificationResult,
};
use golutra_protocol::{
    AgentItem, AgentItemKind, AgentItemStatus, AgentStreamEvent, AgentThreadRef, AgentTurnResult,
    RuntimeEvent, RuntimeEventType,
};
use serde_json::Value;

#[derive(Debug, Clone)]
pub struct AgentEventProjector {
    thread: AgentThreadRef,
    command_id: Option<CommandId>,
    task_id: Option<TaskId>,
    turn_id: Option<TurnId>,
    final_message: Option<String>,
    verification: Option<VerificationRecord>,
    outcome: Option<TaskOutcome>,
    last_sequence_no: Option<u64>,
    terminal_status: Option<TaskStatus>,
    turn_started: bool,
    finished: bool,
}

impl AgentEventProjector {
    #[must_use]
    pub fn new(thread: AgentThreadRef, command_id: Option<CommandId>) -> Self {
        Self {
            thread,
            command_id,
            task_id: None,
            turn_id: None,
            final_message: None,
            verification: None,
            outcome: None,
            last_sequence_no: None,
            terminal_status: None,
            turn_started: false,
            finished: false,
        }
    }

    #[must_use]
    pub fn thread_started(&self) -> AgentStreamEvent {
        AgentStreamEvent::ThreadStarted {
            thread_id: self.thread.thread_id,
            session_id: self.thread.session_id,
            workspace_root: self.thread.workspace_root.clone(),
            timestamp: chrono::Utc::now(),
        }
    }

    #[must_use]
    pub fn task_id(&self) -> Option<TaskId> {
        self.task_id
    }

    #[must_use]
    pub fn turn_id(&self) -> Option<TurnId> {
        self.turn_id
    }

    #[must_use]
    pub fn final_message(&self) -> Option<&str> {
        self.final_message.as_deref()
    }

    #[must_use]
    pub fn verification(&self) -> Option<&VerificationRecord> {
        self.verification.as_ref()
    }

    #[must_use]
    pub fn last_sequence_no(&self) -> Option<u64> {
        self.last_sequence_no
    }

    #[must_use]
    pub fn terminal_status(&self) -> Option<TaskStatus> {
        self.terminal_status
    }

    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.finished
    }

    /// Project one fact. `None` means the fact belongs to another turn.
    pub fn project(&mut self, event: RuntimeEvent) -> Option<AgentStreamEvent> {
        let late_evaluation_update = self.finished
            && event.source == golutra_protocol::RuntimeEventSource::Evaluator
            && event.event_type == RuntimeEventType::TaskCompleted
            && event.task_id == self.task_id
            && event.payload.get("outcome").is_some();
        if (!late_evaluation_update && self.finished) || !self.accepts(&event) {
            return None;
        }
        self.last_sequence_no = Some(event.sequence_no);
        if self.task_id.is_none() {
            self.task_id = event.task_id;
        }
        if self.turn_id.is_none() && self.command_matches(&event) {
            self.turn_id = event.turn_id;
        }
        if event.event_type == RuntimeEventType::AssistantMessage
            && let Some(content) = event.payload.get("content").and_then(Value::as_str)
        {
            self.final_message = Some(content.to_owned());
        }
        if event.event_type == RuntimeEventType::VerificationCompleted {
            self.verification = event
                .payload
                .get("record")
                .cloned()
                .or_else(|| Some(event.payload.clone()))
                .and_then(|value| serde_json::from_value(value).ok());
        }
        if let Some(outcome) = event
            .payload
            .get("outcome")
            .cloned()
            .and_then(|value| serde_json::from_value(value).ok())
        {
            self.outcome = Some(outcome);
        }

        let timestamp = event.timestamp;
        let task_id = event.task_id.or(self.task_id);
        let turn_id = if event.event_type.is_task_terminal() {
            self.turn_id.or(event.turn_id)
        } else {
            event.turn_id.or(self.turn_id)
        };
        let projected = match event.event_type {
            RuntimeEventType::TaskCreated | RuntimeEventType::TurnStarted => {
                self.task_id = task_id;
                self.turn_id = turn_id;
                if self.turn_started {
                    AgentStreamEvent::RuntimeEvent { event }
                } else {
                    self.turn_started = true;
                    AgentStreamEvent::TurnStarted {
                        thread_id: self.thread.thread_id,
                        session_id: self.thread.session_id,
                        task_id,
                        turn_id,
                        timestamp,
                    }
                }
            }
            RuntimeEventType::TaskCompleted
            | RuntimeEventType::TaskAborted
            | RuntimeEventType::TaskInterrupted
            | RuntimeEventType::TaskUncertain => {
                let status = event
                    .payload
                    .get("status")
                    .cloned()
                    .and_then(|value| serde_json::from_value(value).ok())
                    .unwrap_or(TaskStatus::Failed);
                self.terminal_status = Some(status);
                if self.outcome.is_none() {
                    self.outcome = Some(TaskOutcome::from_status(
                        status,
                        self.verification
                            .as_ref()
                            .map(|record| record.result)
                            .unwrap_or(VerificationResult::Unknown),
                    ));
                }
                self.finished = true;
                if late_evaluation_update {
                    AgentStreamEvent::RuntimeEvent { event }
                } else if status == TaskStatus::Completed {
                    AgentStreamEvent::TurnCompleted {
                        thread_id: self.thread.thread_id,
                        session_id: self.thread.session_id,
                        task_id,
                        turn_id,
                        status,
                        final_message: self.final_message.clone(),
                        verification: self.verification.clone(),
                        last_sequence_no: Some(event.sequence_no),
                        timestamp,
                    }
                } else {
                    AgentStreamEvent::TurnFailed {
                        thread_id: self.thread.thread_id,
                        session_id: self.thread.session_id,
                        task_id,
                        turn_id,
                        status,
                        error: event
                            .payload
                            .get("summary")
                            .and_then(Value::as_str)
                            .unwrap_or("runtime task did not complete successfully")
                            .to_owned(),
                        final_message: self.final_message.clone(),
                        verification: self.verification.clone(),
                        last_sequence_no: Some(event.sequence_no),
                        timestamp,
                    }
                }
            }
            RuntimeEventType::ProviderStarted
            | RuntimeEventType::ToolStarted
            | RuntimeEventType::ApprovalRequested => AgentStreamEvent::ItemStarted {
                item: item_from_event(&event),
            },
            RuntimeEventType::ProviderStreamed | RuntimeEventType::ToolProgress => {
                AgentStreamEvent::ItemUpdated {
                    item: item_from_event(&event),
                }
            }
            RuntimeEventType::ProviderCompleted
            | RuntimeEventType::ProviderFailed
            | RuntimeEventType::AssistantMessage
            | RuntimeEventType::ToolCompleted
            | RuntimeEventType::ApprovalResolved
            | RuntimeEventType::VerificationCompleted => AgentStreamEvent::ItemCompleted {
                item: item_from_event(&event),
            },
            _ => AgentStreamEvent::RuntimeEvent { event },
        };
        Some(projected)
    }

    #[must_use]
    pub fn result(&self) -> Option<AgentTurnResult> {
        self.terminal_status.map(|status| AgentTurnResult {
            thread_id: self.thread.thread_id,
            session_id: self.thread.session_id,
            task_id: self.task_id,
            turn_id: self.turn_id,
            status,
            final_message: self.final_message.clone(),
            verification: self.verification.clone(),
            outcome: self.outcome.clone(),
            last_sequence_no: self.last_sequence_no,
        })
    }

    fn accepts(&self, event: &RuntimeEvent) -> bool {
        if let Some(turn_id) = self.turn_id {
            if event.turn_id == Some(turn_id) {
                return true;
            }
            if event.task_id == self.task_id && event.event_type.is_task_terminal() {
                return true;
            }
            if event.task_id == self.task_id
                && event.event_type == RuntimeEventType::VerificationCompleted
            {
                return true;
            }
            // A steer is intentionally projected as a continuation of the
            // active turn. Independent queued prompts do not carry this flag,
            // so each handle remains scoped to its own command.
            return event.task_id == self.task_id
                && event.event_type == RuntimeEventType::TurnStarted
                && event
                    .payload
                    .get("steer")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
        }
        if self.command_id.is_some() {
            return self.command_matches(event);
        }
        true
    }

    fn command_matches(&self, event: &RuntimeEvent) -> bool {
        let Some(command_id) = self.command_id else {
            return true;
        };
        event
            .payload
            .get("command_id")
            .and_then(Value::as_str)
            .and_then(|value| value.parse::<CommandId>().ok())
            == Some(command_id)
    }
}

fn item_from_event(event: &RuntimeEvent) -> AgentItem {
    let (kind, status, title, content) = match event.event_type {
        RuntimeEventType::ProviderStarted => (
            AgentItemKind::Model,
            AgentItemStatus::InProgress,
            "model request".to_owned(),
            None,
        ),
        RuntimeEventType::ProviderStreamed => (
            AgentItemKind::Model,
            AgentItemStatus::InProgress,
            "model response".to_owned(),
            event
                .payload
                .pointer("/delta/text")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
        ),
        RuntimeEventType::ProviderCompleted => (
            AgentItemKind::Model,
            AgentItemStatus::Completed,
            "model request".to_owned(),
            None,
        ),
        RuntimeEventType::ProviderFailed => (
            AgentItemKind::Model,
            AgentItemStatus::Failed,
            "model request".to_owned(),
            event
                .payload
                .get("error")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
        ),
        RuntimeEventType::AssistantMessage => (
            AgentItemKind::AssistantMessage,
            AgentItemStatus::Completed,
            "assistant message".to_owned(),
            event
                .payload
                .get("content")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
        ),
        RuntimeEventType::ToolStarted | RuntimeEventType::ToolProgress => (
            AgentItemKind::Tool,
            AgentItemStatus::InProgress,
            event
                .payload
                .get("tool_name")
                .and_then(Value::as_str)
                .unwrap_or("tool")
                .to_owned(),
            None,
        ),
        RuntimeEventType::ToolCompleted => (
            AgentItemKind::Tool,
            completed_tool_status(event),
            event
                .payload
                .get("tool_name")
                .and_then(Value::as_str)
                .unwrap_or("tool")
                .to_owned(),
            None,
        ),
        RuntimeEventType::ApprovalRequested => (
            AgentItemKind::Approval,
            AgentItemStatus::InProgress,
            "approval".to_owned(),
            None,
        ),
        RuntimeEventType::ApprovalResolved => (
            AgentItemKind::Approval,
            AgentItemStatus::Completed,
            "approval".to_owned(),
            None,
        ),
        RuntimeEventType::VerificationCompleted => (
            AgentItemKind::Verification,
            AgentItemStatus::Completed,
            "verification".to_owned(),
            None,
        ),
        _ => (
            AgentItemKind::Runtime,
            AgentItemStatus::Completed,
            format!("runtime {:?}", event.event_type),
            event
                .payload
                .get("summary")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
        ),
    };
    AgentItem {
        id: stable_item_id(event),
        kind,
        status,
        title,
        content,
        data: serde_json::to_value(event).unwrap_or_else(|_| event.payload.clone()),
        runtime_event_id: Some(event.id.to_string()),
        sequence_no: Some(event.sequence_no),
    }
}

fn stable_item_id(event: &RuntimeEvent) -> String {
    if matches!(
        event.event_type,
        RuntimeEventType::ToolStarted
            | RuntimeEventType::ToolProgress
            | RuntimeEventType::ToolCompleted
    ) && let Some(tool_call_id) = [
        "/tool_call_id",
        "/envelope/tool_call_id",
        "/progress/tool_call_id",
    ]
    .into_iter()
    .find_map(|pointer| event.payload.pointer(pointer).and_then(Value::as_str))
    {
        return tool_call_id.to_owned();
    }
    event.id.to_string()
}

fn completed_tool_status(event: &RuntimeEvent) -> AgentItemStatus {
    let status = event
        .payload
        .pointer("/envelope/status")
        .cloned()
        .and_then(|value| serde_json::from_value::<ToolResultStatus>(value).ok());
    match status {
        None | Some(ToolResultStatus::Ok) => AgentItemStatus::Completed,
        Some(ToolResultStatus::Cancelled) => AgentItemStatus::Cancelled,
        Some(ToolResultStatus::Error | ToolResultStatus::Blocked | ToolResultStatus::Timeout) => {
            AgentItemStatus::Failed
        }
    }
}

#[cfg(test)]
mod tests {
    use golutra_core::{
        CommandId, EventId, SessionId, TaskId, TaskStatus, ThreadId, TurnId, VerificationId,
        VerificationRecord, VerificationResult,
    };
    use golutra_protocol::{
        AgentItemStatus, AgentStreamEvent, AgentThreadRef, RuntimeEvent, RuntimeEventSource,
        RuntimeEventType,
    };
    use serde_json::{Value, json};

    use super::AgentEventProjector;

    #[test]
    fn ignores_terminal_event_from_task_active_before_the_command() {
        let thread = thread_ref();
        let command_id = CommandId::new();
        let mut projector = AgentEventProjector::new(thread.clone(), Some(command_id));

        let old_terminal = event(
            &thread,
            1,
            Some(TaskId::new()),
            Some(TurnId::new()),
            RuntimeEventType::TaskCompleted,
            json!({"status": "completed"}),
        );

        assert!(projector.project(old_terminal).is_none());
        assert!(!projector.is_finished());
        assert!(projector.result().is_none());
    }

    #[test]
    fn queued_turn_binds_only_to_its_command_id() {
        let thread = thread_ref();
        let expected_command = CommandId::new();
        let expected_task = TaskId::new();
        let expected_turn = TurnId::new();
        let mut projector = AgentEventProjector::new(thread.clone(), Some(expected_command));

        let unrelated = event(
            &thread,
            2,
            Some(TaskId::new()),
            Some(TurnId::new()),
            RuntimeEventType::TurnStarted,
            json!({"command_id": CommandId::new()}),
        );
        assert!(projector.project(unrelated).is_none());

        let matching = event(
            &thread,
            3,
            Some(expected_task),
            Some(expected_turn),
            RuntimeEventType::TurnStarted,
            json!({"command_id": expected_command}),
        );
        assert!(matches!(
            projector.project(matching),
            Some(AgentStreamEvent::TurnStarted {
                task_id: Some(task_id),
                turn_id: Some(turn_id),
                ..
            }) if task_id == expected_task && turn_id == expected_turn
        ));
        assert_eq!(projector.task_id(), Some(expected_task));
        assert_eq!(projector.turn_id(), Some(expected_turn));
    }

    #[test]
    fn terminal_result_uses_correlated_turn_facts() {
        let thread = thread_ref();
        let command_id = CommandId::new();
        let task_id = TaskId::new();
        let turn_id = TurnId::new();
        let mut projector = AgentEventProjector::new(thread.clone(), Some(command_id));

        projector.project(event(
            &thread,
            4,
            Some(task_id),
            Some(turn_id),
            RuntimeEventType::TaskCreated,
            json!({"command_id": command_id}),
        ));
        projector.project(event(
            &thread,
            5,
            Some(task_id),
            Some(turn_id),
            RuntimeEventType::AssistantMessage,
            json!({"content": "done"}),
        ));
        let verification = VerificationRecord {
            verification_id: VerificationId::new(),
            task_id,
            objective: "inspect the workspace".to_owned(),
            completion_criteria: vec!["assistant response produced".to_owned()],
            checks: Vec::new(),
            evidence_refs: Vec::new(),
            result: VerificationResult::Pass,
            policy_status: "pass".to_owned(),
            residual_risks: Vec::new(),
            plan_id: None,
            assertions: Vec::new(),
            source: Default::default(),
            independence: Default::default(),
            environment_digest: None,
        };
        projector.project(event(
            &thread,
            6,
            Some(task_id),
            Some(turn_id),
            RuntimeEventType::VerificationCompleted,
            json!({"record": &verification}),
        ));
        let terminal = projector.project(event(
            &thread,
            7,
            Some(task_id),
            None,
            RuntimeEventType::TaskCompleted,
            json!({"status": "completed"}),
        ));

        assert!(matches!(
            terminal,
            Some(AgentStreamEvent::TurnCompleted {
                task_id: Some(completed_task),
                turn_id: Some(completed_turn),
                status: TaskStatus::Completed,
                ref final_message,
                verification: Some(ref completed_verification),
                ..
            }) if completed_task == task_id
                && completed_turn == turn_id
                && final_message.as_deref() == Some("done")
                && completed_verification == &verification
        ));
        let result = projector.result().expect("terminal result");
        assert_eq!(result.task_id, Some(task_id));
        assert_eq!(result.turn_id, Some(turn_id));
        assert_eq!(result.status, TaskStatus::Completed);
        assert_eq!(result.final_message.as_deref(), Some("done"));
        assert_eq!(result.verification.as_ref(), Some(&verification));
        assert_eq!(result.last_sequence_no, Some(7));
    }

    #[test]
    fn uncertain_recovery_is_a_failed_terminal_agent_event() {
        let thread = thread_ref();
        let command_id = CommandId::new();
        let task_id = TaskId::new();
        let turn_id = TurnId::new();
        let mut projector = AgentEventProjector::new(thread.clone(), Some(command_id));
        projector.project(event(
            &thread,
            1,
            Some(task_id),
            Some(turn_id),
            RuntimeEventType::TurnStarted,
            json!({"command_id": command_id}),
        ));

        let terminal = projector.project(event(
            &thread,
            2,
            Some(task_id),
            Some(turn_id),
            RuntimeEventType::TaskUncertain,
            json!({
                "status": "uncertain",
                "summary": "side effect requires reconciliation",
            }),
        ));

        assert!(matches!(
            terminal,
            Some(AgentStreamEvent::TurnFailed {
                status: TaskStatus::Uncertain,
                ref error,
                ..
            }) if error.contains("reconciliation")
        ));
        assert!(projector.is_finished());
    }

    #[test]
    fn task_verification_is_retained_when_the_final_turn_differs() {
        let thread = thread_ref();
        let command_id = CommandId::new();
        let task_id = TaskId::new();
        let initial_turn_id = TurnId::new();
        let final_turn_id = TurnId::new();
        let verification = VerificationRecord {
            verification_id: VerificationId::new(),
            task_id,
            objective: "final pending turn".to_owned(),
            completion_criteria: Vec::new(),
            checks: Vec::new(),
            evidence_refs: Vec::new(),
            result: VerificationResult::Pass,
            policy_status: "pass".to_owned(),
            residual_risks: Vec::new(),
            plan_id: None,
            assertions: Vec::new(),
            source: Default::default(),
            independence: Default::default(),
            environment_digest: None,
        };
        let mut projector = AgentEventProjector::new(thread.clone(), Some(command_id));
        projector.project(event(
            &thread,
            1,
            Some(task_id),
            Some(initial_turn_id),
            RuntimeEventType::TaskCreated,
            json!({"command_id": command_id}),
        ));

        assert!(matches!(
            projector.project(event(
                &thread,
                2,
                Some(task_id),
                Some(final_turn_id),
                RuntimeEventType::VerificationCompleted,
                json!({"record": &verification}),
            )),
            Some(AgentStreamEvent::ItemCompleted { .. })
        ));
        assert!(matches!(
            projector.project(event(
                &thread,
                3,
                Some(task_id),
                Some(final_turn_id),
                RuntimeEventType::TaskCompleted,
                json!({"status": "completed"}),
            )),
            Some(AgentStreamEvent::TurnCompleted {
                verification: Some(ref actual),
                ..
            }) if actual == &verification
        ));
        assert_eq!(
            projector.result().and_then(|result| result.verification),
            Some(verification)
        );
    }

    #[test]
    fn late_external_evaluation_replaces_pending_outcome_in_replayed_projection() {
        let thread = thread_ref();
        let command_id = CommandId::new();
        let task_id = TaskId::new();
        let turn_id = TurnId::new();
        let mut projector = AgentEventProjector::new(thread.clone(), Some(command_id));
        projector.project(event(
            &thread,
            1,
            Some(task_id),
            Some(turn_id),
            RuntimeEventType::TurnStarted,
            json!({"command_id": command_id}),
        ));
        projector.project(event(
            &thread,
            2,
            Some(task_id),
            None,
            RuntimeEventType::TaskCompleted,
            json!({
                "status": "completed",
                "outcome": {
                    "execution": "completed",
                    "verification": "pass",
                    "evidence_refs": [],
                    "external_verification": "pending",
                    "failure_class": null,
                    "scorable": false,
                    "confidence": 50,
                    "next_action": "await the external evaluator result"
                }
            }),
        ));

        let mut update = event(
            &thread,
            3,
            Some(task_id),
            Some(turn_id),
            RuntimeEventType::TaskCompleted,
            json!({
                "status": "completed",
                "outcome": {
                    "execution": "completed",
                    "verification": "pass",
                    "evidence_refs": [],
                    "external_verification": "pass",
                    "failure_class": null,
                    "scorable": true,
                    "confidence": 100,
                    "next_action": null
                }
            }),
        );
        update.source = RuntimeEventSource::Evaluator;
        assert!(matches!(
            projector.project(update),
            Some(AgentStreamEvent::RuntimeEvent { .. })
        ));
        assert_eq!(
            projector
                .result()
                .and_then(|result| result.outcome)
                .map(|outcome| outcome.external_verification),
            Some(golutra_core::ExternalVerificationStatus::Pass)
        );
        assert_eq!(projector.last_sequence_no(), Some(3));
    }

    #[test]
    fn completed_tool_item_preserves_failure_and_cancellation_status() {
        let thread = thread_ref();
        for (sequence_no, status, expected) in [
            (1, "ok", AgentItemStatus::Completed),
            (2, "error", AgentItemStatus::Failed),
            (3, "blocked", AgentItemStatus::Failed),
            (4, "timeout", AgentItemStatus::Failed),
            (5, "cancelled", AgentItemStatus::Cancelled),
        ] {
            let mut projector = AgentEventProjector::new(thread.clone(), None);
            let projected = projector.project(event(
                &thread,
                sequence_no,
                Some(TaskId::new()),
                Some(TurnId::new()),
                RuntimeEventType::ToolCompleted,
                json!({
                    "tool_name": "shell",
                    "envelope": {"status": status},
                }),
            ));
            assert!(matches!(
                projected,
                Some(AgentStreamEvent::ItemCompleted { item }) if item.status == expected
            ));
        }
    }

    #[test]
    fn tool_lifecycle_uses_the_tool_call_id_as_its_stable_item_id() {
        let thread = thread_ref();
        let task_id = TaskId::new();
        let turn_id = TurnId::new();
        let tool_call_id = "tool-call-1";
        let mut projector = AgentEventProjector::new(thread.clone(), None);

        let started = projector
            .project(event(
                &thread,
                1,
                Some(task_id),
                Some(turn_id),
                RuntimeEventType::ToolStarted,
                json!({"tool_call_id": tool_call_id, "tool_name": "shell"}),
            ))
            .expect("tool started event");
        let progress = projector
            .project(event(
                &thread,
                2,
                Some(task_id),
                Some(turn_id),
                RuntimeEventType::ToolProgress,
                json!({
                    "tool_name": "shell",
                    "progress": {"tool_call_id": tool_call_id},
                }),
            ))
            .expect("tool progress event");
        let completed = projector
            .project(event(
                &thread,
                3,
                Some(task_id),
                Some(turn_id),
                RuntimeEventType::ToolCompleted,
                json!({
                    "tool_name": "shell",
                    "envelope": {"tool_call_id": tool_call_id, "status": "ok"},
                }),
            ))
            .expect("tool completed event");

        let item_id = |event: AgentStreamEvent| match event {
            AgentStreamEvent::ItemStarted { item }
            | AgentStreamEvent::ItemUpdated { item }
            | AgentStreamEvent::ItemCompleted { item } => item.id,
            other => panic!("expected tool item event, got {other:?}"),
        };
        assert_eq!(item_id(started), tool_call_id);
        assert_eq!(item_id(progress), tool_call_id);
        assert_eq!(item_id(completed), tool_call_id);
    }

    #[test]
    fn queued_turn_ignores_other_turn_events_on_the_same_task() {
        let thread = thread_ref();
        let expected_command = CommandId::new();
        let task_id = TaskId::new();
        let old_turn_id = TurnId::new();
        let expected_turn_id = TurnId::new();
        let mut projector = AgentEventProjector::new(thread.clone(), Some(expected_command));

        let old_turn_started = event(
            &thread,
            1,
            Some(task_id),
            Some(old_turn_id),
            RuntimeEventType::TurnStarted,
            json!({"command_id": CommandId::new()}),
        );
        assert!(projector.project(old_turn_started).is_none());

        let old_terminal = event(
            &thread,
            2,
            Some(task_id),
            None,
            RuntimeEventType::TaskCompleted,
            json!({"status": "completed"}),
        );
        assert!(projector.project(old_terminal).is_none());
        assert!(!projector.is_finished());

        let expected_turn_started = event(
            &thread,
            3,
            Some(task_id),
            Some(expected_turn_id),
            RuntimeEventType::TurnStarted,
            json!({"command_id": expected_command}),
        );
        assert!(matches!(
            projector.project(expected_turn_started),
            Some(AgentStreamEvent::TurnStarted {
                task_id: Some(actual_task_id),
                turn_id: Some(actual_turn_id),
                ..
            }) if actual_task_id == task_id && actual_turn_id == expected_turn_id
        ));

        let task_terminal = event(
            &thread,
            4,
            Some(task_id),
            Some(TurnId::new()),
            RuntimeEventType::TaskCompleted,
            json!({"status": "completed"}),
        );
        assert!(matches!(
            projector.project(task_terminal),
            Some(AgentStreamEvent::TurnCompleted {
                task_id: Some(actual_task_id),
                turn_id: Some(actual_turn_id),
                ..
            }) if actual_task_id == task_id && actual_turn_id == expected_turn_id
        ));
        assert!(projector.is_finished());
    }

    #[test]
    fn steer_turn_with_a_new_command_id_stays_on_the_original_projection() {
        let thread = thread_ref();
        let original_command = CommandId::new();
        let steer_command = CommandId::new();
        let task_id = TaskId::new();
        let original_turn = TurnId::new();
        let steered_turn = TurnId::new();
        let mut projector = AgentEventProjector::new(thread.clone(), Some(original_command));

        assert!(matches!(
            projector.project(event(
                &thread,
                1,
                Some(task_id),
                Some(original_turn),
                RuntimeEventType::TurnStarted,
                json!({"command_id": original_command}),
            )),
            Some(AgentStreamEvent::TurnStarted { .. })
        ));
        assert!(matches!(
            projector.project(event(
                &thread,
                2,
                Some(task_id),
                Some(steered_turn),
                RuntimeEventType::TurnStarted,
                json!({"command_id": steer_command, "steer": true}),
            )),
            Some(AgentStreamEvent::RuntimeEvent { .. })
        ));
        assert_eq!(projector.turn_id(), Some(steered_turn));

        assert!(matches!(
            projector.project(event(
                &thread,
                3,
                Some(task_id),
                Some(steered_turn),
                RuntimeEventType::AssistantMessage,
                json!({"content": "steered response"}),
            )),
            Some(AgentStreamEvent::ItemCompleted { .. })
        ));
        assert!(matches!(
            projector.project(event(
                &thread,
                4,
                Some(task_id),
                None,
                RuntimeEventType::TaskCompleted,
                json!({"status": "completed"}),
            )),
            Some(AgentStreamEvent::TurnCompleted { ref final_message, .. })
                if final_message.as_deref() == Some("steered response")
        ));
    }

    fn thread_ref() -> AgentThreadRef {
        AgentThreadRef {
            thread_id: ThreadId::new(),
            session_id: SessionId::new(),
            workspace_root: Some("/workspace".to_owned()),
        }
    }

    fn event(
        thread: &AgentThreadRef,
        sequence_no: u64,
        task_id: Option<TaskId>,
        turn_id: Option<TurnId>,
        event_type: RuntimeEventType,
        payload: Value,
    ) -> RuntimeEvent {
        RuntimeEvent {
            schema_version: golutra_core::RUNTIME_EVENT_SCHEMA_VERSION,
            causal_context: Default::default(),
            causal_links: Vec::new(),
            id: EventId::new(),
            sequence_no,
            session_id: thread.session_id,
            turn_id,
            task_id,
            parent_event_id: None,
            event_type,
            timestamp: chrono::Utc::now(),
            source: RuntimeEventSource::Runtime,
            payload,
            payload_ref: None,
            durable: true,
        }
    }
}
