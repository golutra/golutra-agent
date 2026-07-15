//! Runtime 领域记录与持久化协议事件之间的转换。

use golutra_core::{
    Actor, ActorKind, ArtifactId, ArtifactRecord, CommandId, EventId, LoopAction, RedactionStatus,
    SessionId, TaskId, TaskStatus, ThreadId, TurnId,
};
use golutra_llm::ProviderStreamEvent;
use golutra_protocol::{EventFilter, RuntimeEvent, RuntimeEventSource, RuntimeEventType};
use golutra_runtime::{AgentLoopTraceEvent, PendingAgentTurn};
use golutra_store::ThreadRecord;
use golutra_tools::redact_sensitive_text;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::{
    ClientError, HostedAgentTask, RecoveredPendingTurn, compact_event_summary, prompt_from_payload,
    title_from_payload,
};

pub(crate) fn thread_id_from_payload(payload: &Value) -> Option<ThreadId> {
    payload
        .get("_thread_id")
        .and_then(Value::as_str)
        .and_then(|value| value.parse().ok())
}

pub(crate) fn thread_title_for_prompt(
    source_thread: Option<&ThreadRecord>,
    payload: &Value,
) -> String {
    let current_title = source_thread
        .map(|thread| thread.title.trim())
        .unwrap_or_default();
    if current_title.is_empty() {
        title_from_payload(payload)
    } else {
        current_title.to_owned()
    }
}

#[must_use]
pub fn projection_status(value: &Value) -> Option<TaskStatus> {
    value
        .get("task_status")
        .or_else(|| value.get("status"))
        .and_then(|status| serde_json::from_value(status.clone()).ok())
}

#[must_use]
pub fn event_sequence_no(value: &Value) -> Option<u64> {
    value.get("sequence_no").and_then(Value::as_u64)
}

pub(crate) fn event_matches_filter(
    event: &RuntimeEvent,
    filter: &EventFilter,
    cursor: Option<u64>,
) -> bool {
    event.session_id == filter.session_id
        && filter
            .task_id
            .is_none_or(|task_id| event.task_id == Some(task_id))
        && cursor.is_none_or(|sequence_no| event.sequence_no > sequence_no)
}

pub(crate) fn recovered_pending_turn_from_event(
    event: &RuntimeEvent,
) -> Option<RecoveredPendingTurn> {
    let turn_id = event.turn_id?;
    let payload = event.payload.get("payload")?.clone();
    let content = prompt_from_payload(&payload);
    if content.trim().is_empty() {
        return None;
    }
    let command_id = event
        .payload
        .get("command_id")
        .and_then(Value::as_str)
        .and_then(|value| value.parse::<CommandId>().ok())
        .unwrap_or_default();
    let actor = event
        .payload
        .pointer("/runtime/runtime_lane/active_controller")
        .or_else(|| event.payload.pointer("/runtime_lane/active_controller"))
        .cloned()
        .and_then(|value| serde_json::from_value(value).ok())
        .unwrap_or_else(|| Actor {
            kind: ActorKind::Runtime,
            id: "runtime-pending-turn-recovery".to_owned(),
        });
    Some(RecoveredPendingTurn {
        sequence_no: event.sequence_no,
        actor,
        payload,
        pending: PendingAgentTurn {
            command_id,
            turn_id,
            content,
        },
    })
}

pub(crate) fn provider_raw_artifact(
    task: &HostedAgentTask,
    turn_id: TurnId,
    raw_metadata: &Value,
) -> Result<(ArtifactRecord, Vec<u8>), ClientError> {
    let mut redacted = raw_metadata.clone();
    redact_provider_json(&mut redacted);
    let redaction_status = if redacted == *raw_metadata {
        RedactionStatus::NotRequired
    } else {
        RedactionStatus::Redacted
    };
    let bytes = serde_json::to_vec(&redacted)?;
    let artifact_id = ArtifactId::new();
    let checksum = Sha256::digest(&bytes);
    Ok((
        ArtifactRecord {
            artifact_id,
            session_id: task.session_id,
            turn_id: Some(turn_id),
            tool_call_id: None,
            artifact_type: "provider_raw_metadata".to_owned(),
            uri: format!("artifact://provider/{artifact_id}"),
            checksum: format!("sha256:{checksum:x}"),
            size_bytes: bytes.len() as u64,
            created_at: chrono::Utc::now(),
            producer: "provider".to_owned(),
            redaction_status,
            retention_policy: "debug_default".to_owned(),
            provenance_refs: Vec::new(),
        },
        bytes,
    ))
}

pub(crate) fn redact_provider_json(value: &mut Value) {
    match value {
        Value::Object(object) => {
            for (key, value) in object {
                if provider_json_key_is_sensitive(key) {
                    *value = Value::String("<redacted-secret>".to_owned());
                } else {
                    redact_provider_json(value);
                }
            }
        }
        Value::Array(values) => {
            for value in values {
                redact_provider_json(value);
            }
        }
        Value::String(text) => {
            let (redacted, _) = redact_sensitive_text(text);
            *text = redacted;
        }
        _ => {}
    }
}

pub(crate) fn provider_json_key_is_sensitive(key: &str) -> bool {
    let normalized = key.to_ascii_lowercase().replace('-', "_");
    let collapsed = normalized.replace('_', "");
    matches!(
        normalized.as_str(),
        "api_key" | "authorization" | "token" | "secret" | "password"
    ) || ["_api_key", "_token", "_secret", "_password"]
        .iter()
        .any(|suffix| normalized.ends_with(suffix))
        || ["apikey", "token", "secret", "password"]
            .iter()
            .any(|suffix| collapsed.ends_with(suffix))
}

pub(crate) fn candidate_id_from_payload(payload: &Value) -> Result<&str, ClientError> {
    payload
        .get("candidate_id")
        .and_then(Value::as_str)
        .filter(|candidate_id| !candidate_id.trim().is_empty())
        .ok_or_else(|| ClientError::InvalidSession("candidate_id is required".to_owned()))
}

pub(crate) fn task_status_from_loop_action(action: LoopAction) -> TaskStatus {
    match action {
        LoopAction::StopSuccess => TaskStatus::Completed,
        LoopAction::StopPartial => TaskStatus::Partial,
        LoopAction::StopFailed => TaskStatus::Failed,
        LoopAction::Blocked => TaskStatus::Blocked,
        LoopAction::AskUser => TaskStatus::Blocked,
        LoopAction::Continue
        | LoopAction::Compact
        | LoopAction::Retry
        | LoopAction::Fallback
        | LoopAction::Verify => TaskStatus::Partial,
    }
}

pub(crate) fn trace_event_payload(
    trace_event: AgentLoopTraceEvent,
) -> Option<(RuntimeEventType, RuntimeEventSource, Value)> {
    match trace_event {
        AgentLoopTraceEvent::ContextBuilt {
            contributors,
            planned_input_tokens,
        } => Some((
            RuntimeEventType::ContextBuilt,
            RuntimeEventSource::Runtime,
            json!({
                "summary": "context built for provider request",
                "contributors": contributors,
                "planned_input_tokens": planned_input_tokens,
            }),
        )),
        AgentLoopTraceEvent::ContextCompacted {
            original_input_tokens,
            planned_input_tokens,
            trimmed_contributors,
        } => Some((
            RuntimeEventType::CompactionCompleted,
            RuntimeEventSource::Runtime,
            json!({
                "summary": "context compacted to fit provider budget",
                "original_input_tokens": original_input_tokens,
                "planned_input_tokens": planned_input_tokens,
                "trimmed_contributors": trimmed_contributors,
            }),
        )),
        AgentLoopTraceEvent::ProviderStarted {
            provider_id,
            model_id,
        } => Some((
            RuntimeEventType::ProviderStarted,
            RuntimeEventSource::Provider,
            json!({
                "summary": "provider request started",
                "provider_id": provider_id,
                "model_id": model_id,
            }),
        )),
        AgentLoopTraceEvent::ProviderStreamed {
            provider_id,
            model_id,
            event,
        } => {
            let delta = match event {
                ProviderStreamEvent::ReasoningDelta { text } => json!({
                    "kind": "reasoning_delta",
                    "redacted": true,
                    "byte_count": text.len(),
                }),
                event => json!(event),
            };
            Some((
                RuntimeEventType::ProviderStreamed,
                RuntimeEventSource::Provider,
                json!({
                    "summary": "provider response delta received",
                    "provider_id": provider_id,
                    "model_id": model_id,
                    "delta": delta,
                }),
            ))
        }
        AgentLoopTraceEvent::ProviderCompleted {
            provider_id,
            model_id,
            finish_reason,
            tool_call_count,
            usage,
            raw_metadata: _,
        } => Some((
            RuntimeEventType::ProviderCompleted,
            RuntimeEventSource::Provider,
            json!({
                "summary": "provider request completed",
                "provider_id": provider_id,
                "model_id": model_id,
                "finish_reason": finish_reason,
                "tool_call_count": tool_call_count,
                "usage": usage,
            }),
        )),
        AgentLoopTraceEvent::TokenUsageRecorded(record) => Some((
            RuntimeEventType::TokenUsageRecorded,
            RuntimeEventSource::Provider,
            json!({
                "summary": "provider token usage recorded",
                "record": record,
            }),
        )),
        AgentLoopTraceEvent::ToolStarted { tool_name } => Some((
            RuntimeEventType::ToolStarted,
            RuntimeEventSource::Tool,
            json!({
                "summary": format!("tool {tool_name} started"),
                "tool_name": tool_name,
            }),
        )),
        AgentLoopTraceEvent::ToolCompleted(_) => None,
        AgentLoopTraceEvent::PolicyEvaluated(evaluation) => Some((
            RuntimeEventType::PolicyEvaluated,
            RuntimeEventSource::Policy,
            json!({
                "summary": format!("policy decision: {:?}", evaluation.decision),
                "record": evaluation,
            }),
        )),
        AgentLoopTraceEvent::ApprovalRequested(approval) => Some((
            RuntimeEventType::ApprovalRequested,
            RuntimeEventSource::Policy,
            json!({
                "summary": format!("approval required for {}", approval.tool_name),
                "approval_id": approval.approval_id,
                "request": approval,
            }),
        )),
        AgentLoopTraceEvent::ApprovalResolved(resolution) => Some((
            RuntimeEventType::ApprovalResolved,
            RuntimeEventSource::User,
            json!({
                "summary": format!("approval resolved as {:?}", resolution.decision),
                "approval_id": resolution.approval_id,
                "resolution": resolution,
            }),
        )),
        AgentLoopTraceEvent::RetryScheduled { attempt, reason } => Some((
            RuntimeEventType::RetryScheduled,
            RuntimeEventSource::Runtime,
            json!({
                "summary": format!("provider retry attempt {attempt}"),
                "attempt": attempt,
                "reason": reason,
            }),
        )),
        AgentLoopTraceEvent::ProviderFallback {
            from_provider,
            to_provider,
            reason,
        } => Some((
            RuntimeEventType::ProviderFallback,
            RuntimeEventSource::Runtime,
            json!({
                "summary": format!("provider fallback from {from_provider} to {to_provider}"),
                "from_provider": from_provider,
                "to_provider": to_provider,
                "reason": reason,
            }),
        )),
        AgentLoopTraceEvent::LoopGuardTriggered { trigger, reason } => Some((
            RuntimeEventType::LoopGuardTriggered,
            RuntimeEventSource::Runtime,
            json!({
                "summary": reason,
                "trigger": trigger,
            }),
        )),
        AgentLoopTraceEvent::GovernorDecided(decision) => Some((
            RuntimeEventType::GovernorDecided,
            RuntimeEventSource::Governor,
            json!({
                "summary": format!("runtime governor decision: {:?}", decision.action),
                "record": decision,
            }),
        )),
        AgentLoopTraceEvent::PendingTurnStarted(turn) => Some((
            RuntimeEventType::TurnStarted,
            RuntimeEventSource::User,
            json!({
                "summary": "queued user turn started",
                "command_id": turn.command_id,
                "turn_id": turn.turn_id,
                "prompt": turn.content,
            }),
        )),
        AgentLoopTraceEvent::AssistantMessage { content, .. } => Some((
            RuntimeEventType::AssistantMessage,
            RuntimeEventSource::Runtime,
            json!({
                "summary": compact_event_summary(&content),
                "content": content,
            }),
        )),
    }
}

pub(crate) fn host_event(
    sequence_no: u64,
    session_id: SessionId,
    task_id: Option<TaskId>,
    event_type: RuntimeEventType,
    source: RuntimeEventSource,
    payload: Value,
) -> RuntimeEvent {
    RuntimeEvent {
        id: EventId::new(),
        sequence_no,
        session_id,
        turn_id: None,
        task_id,
        parent_event_id: None,
        event_type,
        timestamp: chrono::Utc::now(),
        source,
        payload,
        payload_ref: None,
        durable: true,
    }
}

pub(crate) fn agent_event(
    sequence_no: u64,
    task: &HostedAgentTask,
    event_type: RuntimeEventType,
    source: RuntimeEventSource,
    payload: Value,
) -> RuntimeEvent {
    RuntimeEvent {
        id: EventId::new(),
        sequence_no,
        session_id: task.session_id,
        turn_id: Some(task.turn_id),
        task_id: Some(task.task_id),
        parent_event_id: None,
        event_type,
        timestamp: chrono::Utc::now(),
        source,
        payload,
        payload_ref: None,
        durable: true,
    }
}

pub(crate) fn agent_event_for_turn(
    sequence_no: u64,
    task: &HostedAgentTask,
    turn_id: TurnId,
    event_type: RuntimeEventType,
    source: RuntimeEventSource,
    payload: Value,
) -> RuntimeEvent {
    let mut event = agent_event(sequence_no, task, event_type, source, payload);
    event.turn_id = Some(turn_id);
    event
}

pub(crate) fn with_command_payload(
    mut event: RuntimeEvent,
    command_id: golutra_core::CommandId,
    payload: Value,
) -> RuntimeEvent {
    event.payload = json!({
        "summary": event
            .payload
            .get("summary")
            .and_then(Value::as_str)
            .unwrap_or("runtime host accepted command"),
        "command_id": command_id.to_string(),
        "payload": payload,
        "runtime": event.payload,
    });
    event
}
