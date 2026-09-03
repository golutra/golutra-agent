//! Runtime 领域记录与持久化协议事件之间的转换。

use golutra_context::{
    ContextCompactionRecord, parse_compaction_summary_envelope, stable_prefix_message_count,
    stable_prefix_token_estimate,
};
use golutra_core::{
    Actor, ActorKind, ArtifactId, ArtifactRecord, CommandId, ContextSnapshot, EventId, LoopAction,
    RedactionStatus, SessionId, TaskContract, TaskId, TaskStatus, ThreadId, TurnId,
};
use golutra_llm::{ProviderRequest, ProviderResponse, ProviderStreamEvent};
use golutra_protocol::{EventFilter, RuntimeEvent, RuntimeEventSource, RuntimeEventType};
use golutra_runtime::{
    AgentLoopTraceEvent, PendingAgentTurn, PendingTurnExecutionOptions, RuntimeObservation,
};
use golutra_store::ThreadRecord;
use golutra_tools::redact_sensitive_text;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::{
    ClientError, EXTERNAL_VERIFIERS_REQUIRE_OS_SANDBOX_KEY, HostedAgentTask, LegacyTaskAdapter,
    NormalizedExecutionMode, RecoveredPendingTurn, compact_event_summary,
    execution_mode_from_payload, model_prompt_from_payload, should_apply_legacy_adapter,
    task_contract_from_payload, title_from_payload, tool_profile_from_payload,
};

pub(crate) fn thread_id_from_payload(payload: &Value) -> Option<ThreadId> {
    payload
        .get("_thread_id")
        .and_then(Value::as_str)
        .and_then(|value| value.parse().ok())
}

pub(crate) fn parent_thread_id_from_payload(payload: &Value) -> Option<ThreadId> {
    payload
        .get("_parent_thread_id")
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
) -> Result<Option<RecoveredPendingTurn>, ClientError> {
    let turn_id = event.turn_id.ok_or_else(|| {
        ClientError::TaskExecution(format!(
            "durable queued turn event {} is missing turn_id",
            event.id
        ))
    })?;
    let payload = event.payload.get("payload").cloned().ok_or_else(|| {
        ClientError::TaskExecution(format!(
            "durable queued turn event {} is missing payload",
            event.id
        ))
    })?;
    let content = model_prompt_from_payload(&payload);
    if content.trim().is_empty() {
        return Err(ClientError::TaskExecution(format!(
            "durable queued turn event {} has an empty prompt",
            event.id
        )));
    }
    let steer = payload
        .get("steer")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let execution_mode = (!steer)
        .then(|| execution_mode_from_payload(&payload))
        .transpose()
        .map_err(|error| {
            ClientError::TaskExecution(format!(
                "durable queued turn event {} has an invalid execution mode: {error}",
                event.id
            ))
        })?;
    let effective_execution_mode = execution_mode.unwrap_or(NormalizedExecutionMode::Legacy);
    let tool_profile = if steer && payload.get("tool_profile").is_none_or(Value::is_null) {
        None
    } else {
        Some(tool_profile_from_payload(&payload).map_err(|error| {
            ClientError::TaskExecution(format!(
                "durable queued turn event {} has an invalid tool profile: {error}",
                event.id
            ))
        })?)
    };
    let task_contract = if steer {
        None
    } else if payload
        .get("task_contract")
        .is_some_and(|value| !value.is_null())
    {
        let contract: TaskContract = serde_json::from_value(payload["task_contract"].clone())
            .map_err(|error| {
                ClientError::TaskExecution(format!(
                    "durable queued turn event {} has an invalid task contract: {error}",
                    event.id
                ))
            })?;
        Some(contract)
    } else if should_apply_legacy_adapter(&payload, effective_execution_mode) {
        let mut contract = task_contract_from_payload(&payload)?;
        LegacyTaskAdapter::new(&payload, &content).apply_to(&mut contract);
        Some(contract)
    } else {
        Some(task_contract_from_payload(&payload)?)
    };
    if let Some(contract) = &task_contract {
        contract.validate().map_err(|error| {
            ClientError::TaskExecution(format!(
                "durable queued turn event {} has an invalid task contract: {error}",
                event.id
            ))
        })?;
    }
    let external_verifiers = match payload.get("external_verifiers") {
        Some(value) => serde_json::from_value(value.clone()).map_err(|error| {
            ClientError::TaskExecution(format!(
                "durable queued turn event {} has invalid external verifiers: {error}",
                event.id
            ))
        })?,
        None => Vec::new(),
    };
    let max_elapsed_ms = match payload.get("max_elapsed_ms") {
        None | Some(Value::Null) => None,
        Some(value) if value.as_u64().is_some_and(|value| value > 0) => value.as_u64(),
        Some(_) => {
            return Err(ClientError::TaskExecution(format!(
                "durable queued turn event {} has an invalid max_elapsed_ms",
                event.id
            )));
        }
    };
    let defer_external_verification = match payload.get("defer_external_verification") {
        None => false,
        Some(Value::Bool(deferred)) => *deferred,
        Some(_) => {
            return Err(ClientError::TaskExecution(format!(
                "durable queued turn event {} has non-boolean defer_external_verification",
                event.id
            )));
        }
    };
    let allow_network = match payload.get("allow_network") {
        None => false,
        Some(Value::Bool(allow_network)) => *allow_network,
        Some(_) => {
            return Err(ClientError::TaskExecution(format!(
                "durable queued turn event {} has non-boolean allow_network",
                event.id
            )));
        }
    };
    let yolo = match payload.get("yolo") {
        None => false,
        Some(Value::Bool(yolo)) => *yolo,
        Some(_) => {
            return Err(ClientError::TaskExecution(format!(
                "durable queued turn event {} has non-boolean yolo",
                event.id
            )));
        }
    };
    let external_verifiers_require_os_sandbox =
        match payload.get(EXTERNAL_VERIFIERS_REQUIRE_OS_SANDBOX_KEY) {
            None => false,
            Some(Value::Bool(required)) => *required,
            Some(_) => {
                return Err(ClientError::TaskExecution(format!(
                    "durable queued turn event {} has an invalid verifier sandbox requirement",
                    event.id
                )));
            }
        };
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
    let output_schema = payload.get("output_schema").cloned();
    Ok(Some(RecoveredPendingTurn {
        sequence_no: event.sequence_no,
        actor,
        payload,
        pending: PendingAgentTurn {
            command_id,
            turn_id,
            content,
            task_contract,
            output_schema,
            external_verifiers,
            max_elapsed_ms,
            defer_external_verification,
            external_verifiers_require_os_sandbox,
            allow_network,
            yolo,
            steer,
        },
        execution: PendingTurnExecutionOptions {
            execution_mode: execution_mode.and_then(NormalizedExecutionMode::explicit),
            tool_profile,
        },
        continuation: None,
    }))
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

#[cfg(test)]
pub(crate) fn context_request_artifact(
    task: &HostedAgentTask,
    snapshot: &ContextSnapshot,
    request: &ProviderRequest,
) -> Result<(ArtifactRecord, Vec<u8>), ClientError> {
    let raw_bytes = serde_json::to_vec(request)?;
    let (redacted_bytes, redaction_status) = redacted_request_bytes(&raw_bytes)?;
    let artifact = context_request_artifact_record(
        task,
        snapshot,
        "context_request_redacted",
        "artifact://context",
        redaction_status,
        "debug_default",
        &redacted_bytes,
    );
    Ok((artifact, redacted_bytes))
}

/// 从一次原始序列化同时生成脱敏和回放 artifact，避免热路径重复遍历消息。
pub(crate) struct ContextRequestArtifacts {
    pub(crate) redacted: (ArtifactRecord, Vec<u8>),
    pub(crate) replay: (ArtifactRecord, Vec<u8>),
}

pub(crate) fn context_request_artifacts(
    task: &HostedAgentTask,
    snapshot: &ContextSnapshot,
    request: &ProviderRequest,
) -> Result<ContextRequestArtifacts, ClientError> {
    let raw_bytes = serde_json::to_vec(request)?;
    let (redacted_bytes, redaction_status) = redacted_request_bytes(&raw_bytes)?;
    let redacted = context_request_artifact_record(
        task,
        snapshot,
        "context_request_redacted",
        "artifact://context",
        redaction_status,
        "debug_default",
        &redacted_bytes,
    );
    let replay = context_request_artifact_record(
        task,
        snapshot,
        "provider_request_replay",
        "artifact://replay/provider-request",
        RedactionStatus::Raw,
        "replay_owner_access",
        &raw_bytes,
    );
    Ok(ContextRequestArtifacts {
        redacted: (redacted, redacted_bytes),
        replay: (replay, raw_bytes),
    })
}

fn redacted_request_bytes(raw_bytes: &[u8]) -> Result<(Vec<u8>, RedactionStatus), ClientError> {
    let raw: Value = serde_json::from_slice(raw_bytes)?;
    let mut redacted = raw.clone();
    redact_provider_json(&mut redacted);
    let redaction_status = if redacted == raw {
        RedactionStatus::NotRequired
    } else {
        RedactionStatus::Redacted
    };
    Ok((serde_json::to_vec(&redacted)?, redaction_status))
}

fn context_request_artifact_record(
    task: &HostedAgentTask,
    snapshot: &ContextSnapshot,
    artifact_type: &str,
    uri_prefix: &str,
    redaction_status: RedactionStatus,
    retention_policy: &str,
    bytes: &[u8],
) -> ArtifactRecord {
    let artifact_id = ArtifactId::new();
    let checksum = Sha256::digest(bytes);
    ArtifactRecord {
        artifact_id,
        session_id: task.session_id,
        turn_id: Some(snapshot.turn_id),
        tool_call_id: None,
        artifact_type: artifact_type.to_owned(),
        uri: format!("{uri_prefix}/{artifact_id}"),
        checksum: format!("sha256:{checksum:x}"),
        size_bytes: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
        created_at: chrono::Utc::now(),
        producer: "context-builder".to_owned(),
        redaction_status,
        retention_policy: retention_policy.to_owned(),
        provenance_refs: Vec::new(),
    }
}

pub(crate) fn provider_response_replay_artifact(
    task: &HostedAgentTask,
    turn_id: TurnId,
    response: &ProviderResponse,
) -> Result<(ArtifactRecord, Vec<u8>), ClientError> {
    let bytes = serde_json::to_vec(response)?;
    let checksum = Sha256::digest(&bytes);
    let artifact_id = ArtifactId::new();
    Ok((
        ArtifactRecord {
            artifact_id,
            session_id: task.session_id,
            turn_id: Some(turn_id),
            tool_call_id: None,
            artifact_type: "provider_response_replay".to_owned(),
            uri: format!("artifact://replay/provider-response/{artifact_id}"),
            checksum: format!("sha256:{checksum:x}"),
            size_bytes: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
            created_at: chrono::Utc::now(),
            producer: "provider".to_owned(),
            redaction_status: RedactionStatus::Raw,
            retention_policy: "replay_owner_access".to_owned(),
            provenance_refs: Vec::new(),
        },
        bytes,
    ))
}

pub(crate) fn context_compaction_artifact(
    task: &HostedAgentTask,
    record: &ContextCompactionRecord,
) -> Result<(ArtifactRecord, Vec<u8>), ClientError> {
    let raw = serde_json::to_value(record)?;
    let mut redacted = raw.clone();
    redact_provider_json(&mut redacted);
    let redaction_status = if redacted == raw {
        RedactionStatus::NotRequired
    } else {
        RedactionStatus::Redacted
    };
    if let Some(object) = redacted.as_object_mut() {
        let source_checksum = parse_compaction_summary_envelope(&record.summary)
            .ok_or_else(|| {
                ClientError::TaskExecution("compaction summary envelope is invalid".to_owned())
            })?
            .checksum;
        object.insert("source_checksum".to_owned(), Value::String(source_checksum));
        let replacement = object
            .get("replacement_messages")
            .cloned()
            .unwrap_or(Value::Null);
        let replacement_bytes = serde_json::to_vec(&replacement)?;
        object.insert(
            "checksum".to_owned(),
            Value::String(format!("sha256:{:x}", Sha256::digest(&replacement_bytes))),
        );
    }
    let bytes = serde_json::to_vec(&redacted)?;
    let checksum = Sha256::digest(&bytes);
    let artifact_id = ArtifactId::new();
    Ok((
        ArtifactRecord {
            artifact_id,
            session_id: task.session_id,
            turn_id: Some(record.turn_id),
            tool_call_id: None,
            artifact_type: "context_compaction_baseline".to_owned(),
            uri: format!("artifact://context/compaction/{artifact_id}"),
            checksum: format!("sha256:{checksum:x}"),
            size_bytes: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
            created_at: chrono::Utc::now(),
            producer: "context-window-manager".to_owned(),
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ObservationIntegrityClass {
    Required,
    Supporting,
    Diagnostic,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ObservationDescriptor {
    pub(crate) event_type: RuntimeEventType,
    pub(crate) source: RuntimeEventSource,
    pub(crate) integrity: ObservationIntegrityClass,
}

/// Exhaustive catalog for execution observations. Adding a new observation
/// variant requires an explicit disclosure and integrity decision here.
pub(crate) fn observation_descriptor(observation: &RuntimeObservation) -> ObservationDescriptor {
    let (event_type, source, integrity) = match observation {
        RuntimeObservation::StepStarted(_) => (
            RuntimeEventType::StepStarted,
            RuntimeEventSource::Runtime,
            ObservationIntegrityClass::Required,
        ),
        RuntimeObservation::StepCompleted(_) => (
            RuntimeEventType::StepCompleted,
            RuntimeEventSource::Runtime,
            ObservationIntegrityClass::Required,
        ),
        RuntimeObservation::StepCheckpointed(_) => (
            RuntimeEventType::StepCheckpointed,
            RuntimeEventSource::Runtime,
            ObservationIntegrityClass::Required,
        ),
        RuntimeObservation::ContextBuilt { .. } => (
            RuntimeEventType::ContextBuilt,
            RuntimeEventSource::Runtime,
            ObservationIntegrityClass::Supporting,
        ),
        RuntimeObservation::ContextCompacted { .. } => (
            RuntimeEventType::CompactionCompleted,
            RuntimeEventSource::Runtime,
            ObservationIntegrityClass::Supporting,
        ),
        RuntimeObservation::ContextCompactionStarted { .. } => (
            RuntimeEventType::CompactionStarted,
            RuntimeEventSource::Runtime,
            ObservationIntegrityClass::Supporting,
        ),
        RuntimeObservation::ContextAutoCompacted(_) => (
            RuntimeEventType::CompactionCompleted,
            RuntimeEventSource::Runtime,
            ObservationIntegrityClass::Required,
        ),
        RuntimeObservation::ContextCompactionFailed { .. } => (
            RuntimeEventType::CompactionFailed,
            RuntimeEventSource::Runtime,
            ObservationIntegrityClass::Required,
        ),
        RuntimeObservation::ContextSnapshot(_)
        | RuntimeObservation::ContextSnapshotCaptured { .. } => (
            RuntimeEventType::ContextSnapshotCreated,
            RuntimeEventSource::Runtime,
            ObservationIntegrityClass::Required,
        ),
        RuntimeObservation::CandidateReady { .. } => (
            RuntimeEventType::CandidateReady,
            RuntimeEventSource::Runtime,
            ObservationIntegrityClass::Required,
        ),
        RuntimeObservation::VerificationReady { .. } => (
            RuntimeEventType::VerificationReady,
            RuntimeEventSource::Verifier,
            ObservationIntegrityClass::Required,
        ),
        RuntimeObservation::VerificationPlanned(_) => (
            RuntimeEventType::VerificationPlanned,
            RuntimeEventSource::Verifier,
            ObservationIntegrityClass::Required,
        ),
        RuntimeObservation::VerificationAssertionCompleted(_) => (
            RuntimeEventType::VerificationAssertionCompleted,
            RuntimeEventSource::Verifier,
            ObservationIntegrityClass::Required,
        ),
        RuntimeObservation::VerificationCompleted { .. } => (
            RuntimeEventType::VerificationCompleted,
            RuntimeEventSource::Verifier,
            ObservationIntegrityClass::Required,
        ),
        RuntimeObservation::CorrectionIssued(_) => (
            RuntimeEventType::ContinuationDecided,
            RuntimeEventSource::Runtime,
            ObservationIntegrityClass::Required,
        ),
        RuntimeObservation::ProviderStarted { .. } => (
            RuntimeEventType::ProviderStarted,
            RuntimeEventSource::Provider,
            ObservationIntegrityClass::Supporting,
        ),
        RuntimeObservation::ProviderStreamed { .. } => (
            RuntimeEventType::ProviderStreamed,
            RuntimeEventSource::Provider,
            ObservationIntegrityClass::Diagnostic,
        ),
        RuntimeObservation::ProviderCompleted { .. } => (
            RuntimeEventType::ProviderCompleted,
            RuntimeEventSource::Provider,
            ObservationIntegrityClass::Supporting,
        ),
        RuntimeObservation::ProviderFailed { .. } => (
            RuntimeEventType::ProviderFailed,
            RuntimeEventSource::Provider,
            ObservationIntegrityClass::Required,
        ),
        RuntimeObservation::TokenUsageRecorded(_) => (
            RuntimeEventType::TokenUsageRecorded,
            RuntimeEventSource::Provider,
            ObservationIntegrityClass::Supporting,
        ),
        RuntimeObservation::ToolStarted { .. } => (
            RuntimeEventType::ToolStarted,
            RuntimeEventSource::Tool,
            ObservationIntegrityClass::Supporting,
        ),
        RuntimeObservation::ToolProgress(_) => (
            RuntimeEventType::ToolProgress,
            RuntimeEventSource::Tool,
            ObservationIntegrityClass::Diagnostic,
        ),
        RuntimeObservation::ToolCompleted(_) => (
            RuntimeEventType::ToolCompleted,
            RuntimeEventSource::Tool,
            ObservationIntegrityClass::Required,
        ),
        RuntimeObservation::PolicyEvaluated(_) => (
            RuntimeEventType::PolicyEvaluated,
            RuntimeEventSource::Policy,
            ObservationIntegrityClass::Required,
        ),
        RuntimeObservation::ApprovalRequested(_) => (
            RuntimeEventType::ApprovalRequested,
            RuntimeEventSource::Policy,
            ObservationIntegrityClass::Required,
        ),
        RuntimeObservation::ApprovalResolved(_) => (
            RuntimeEventType::ApprovalResolved,
            RuntimeEventSource::User,
            ObservationIntegrityClass::Required,
        ),
        RuntimeObservation::UserQuestionRequested(_) => (
            RuntimeEventType::UserQuestionRequested,
            RuntimeEventSource::Runtime,
            ObservationIntegrityClass::Required,
        ),
        RuntimeObservation::UserQuestionResolved(_) => (
            RuntimeEventType::UserQuestionResolved,
            RuntimeEventSource::User,
            ObservationIntegrityClass::Required,
        ),
        RuntimeObservation::RetryScheduled { .. } => (
            RuntimeEventType::RetryScheduled,
            RuntimeEventSource::Runtime,
            ObservationIntegrityClass::Supporting,
        ),
        RuntimeObservation::ProviderFallback { .. } => (
            RuntimeEventType::ProviderFallback,
            RuntimeEventSource::Runtime,
            ObservationIntegrityClass::Supporting,
        ),
        RuntimeObservation::ProviderTransportFallback { .. } => (
            RuntimeEventType::ProviderTransportFallback,
            RuntimeEventSource::Runtime,
            ObservationIntegrityClass::Supporting,
        ),
        RuntimeObservation::LoopGuardTriggered { .. } => (
            RuntimeEventType::LoopGuardTriggered,
            RuntimeEventSource::Runtime,
            ObservationIntegrityClass::Required,
        ),
        RuntimeObservation::GovernorDecided(_) => (
            RuntimeEventType::GovernorDecided,
            RuntimeEventSource::Governor,
            ObservationIntegrityClass::Required,
        ),
        RuntimeObservation::PendingTurnStarted(_)
        | RuntimeObservation::PendingTurnStartedWithExecution(_) => (
            RuntimeEventType::TurnStarted,
            RuntimeEventSource::User,
            ObservationIntegrityClass::Required,
        ),
        RuntimeObservation::AssistantMessage { .. } => (
            RuntimeEventType::AssistantMessage,
            RuntimeEventSource::Runtime,
            ObservationIntegrityClass::Supporting,
        ),
    };
    ObservationDescriptor {
        event_type,
        source,
        integrity,
    }
}

pub(crate) fn trace_event_payload(
    trace_event: AgentLoopTraceEvent,
) -> Option<(RuntimeEventType, RuntimeEventSource, Value)> {
    let descriptor = observation_descriptor(&trace_event);
    let mapped = match trace_event {
        AgentLoopTraceEvent::StepStarted(step) => Some((
            RuntimeEventType::StepStarted,
            RuntimeEventSource::Runtime,
            json!({
                "summary": format!("runtime step {} started", step.step_no),
                "step": step,
            }),
        )),
        AgentLoopTraceEvent::StepCompleted(completion) => Some((
            RuntimeEventType::StepCompleted,
            RuntimeEventSource::Runtime,
            json!({
                "summary": format!("runtime step {} completed", completion.snapshot.step_no),
                "step": completion.snapshot,
                "fingerprint": completion.fingerprint,
                "made_progress": completion.made_progress,
                "made_material_progress": completion.made_material_progress,
                "repeated_no_progress": completion.repeated_no_progress,
                "correction_no_progress_steps": completion.correction_no_progress_steps,
                "correction_no_progress_elapsed_ms": completion.correction_no_progress_elapsed_ms,
                "advisory": completion.advisory,
                "should_stop": completion.should_stop,
                "stop_reason": completion.stop_reason,
            }),
        )),
        AgentLoopTraceEvent::StepCheckpointed(checkpoint) => Some((
            RuntimeEventType::StepCheckpointed,
            RuntimeEventSource::Runtime,
            json!({
                "summary": "runtime step checkpoint persisted",
                "checkpoint": checkpoint,
            }),
        )),
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
        AgentLoopTraceEvent::ContextCompactionStarted {
            original_input_tokens,
            budget_limit,
        } => Some((
            RuntimeEventType::CompactionStarted,
            RuntimeEventSource::Runtime,
            json!({
                "summary": "automatic context compaction started",
                "mode": "automatic",
                "original_input_tokens": original_input_tokens,
                "budget_limit": budget_limit,
            }),
        )),
        AgentLoopTraceEvent::ContextAutoCompacted(record) => Some((
            RuntimeEventType::CompactionCompleted,
            RuntimeEventSource::Runtime,
            json!({
                "summary": "automatic context compaction completed",
                "mode": record.mode,
                "strategy": record.strategy,
                "content": record.summary,
                "original_message_count": record.original_message_count,
                "replacement_message_count": record.replacement_message_count,
                "dropped_message_count": record.dropped_message_count,
                "protected_prefix_len": record.protected_prefix_len,
                "original_estimated_tokens": record.original_estimated_tokens,
                "replacement_estimated_tokens": record.replacement_estimated_tokens,
                "planned_tool_tokens": record.planned_tool_tokens,
                "budget_limit": record.budget_limit,
                "checksum": record.checksum,
            }),
        )),
        AgentLoopTraceEvent::ContextCompactionFailed {
            planned_input_tokens,
            budget_limit,
            reason,
        } => Some((
            RuntimeEventType::CompactionFailed,
            RuntimeEventSource::Runtime,
            json!({
                "summary": "automatic context compaction failed",
                "mode": "automatic",
                "planned_input_tokens": planned_input_tokens,
                "budget_limit": budget_limit,
                "reason": reason,
            }),
        )),
        AgentLoopTraceEvent::ContextSnapshot(snapshot) => {
            let cache_diagnostics = context_cache_diagnostics(&snapshot, None);
            Some((
                RuntimeEventType::ContextSnapshotCreated,
                RuntimeEventSource::Runtime,
                json!({
                    "summary": "provider request context snapshot created",
                    "snapshot": snapshot,
                    "cache_diagnostics": cache_diagnostics,
                }),
            ))
        }
        AgentLoopTraceEvent::ContextSnapshotCaptured { snapshot, request } => {
            let cache_diagnostics = context_cache_diagnostics(&snapshot, Some(&request));
            Some((
                RuntimeEventType::ContextSnapshotCreated,
                RuntimeEventSource::Runtime,
                json!({
                    "summary": "provider request context snapshot created",
                    "snapshot": snapshot,
                    "cache_diagnostics": cache_diagnostics,
                }),
            ))
        }
        AgentLoopTraceEvent::CandidateReady {
            turn_id,
            tool_count,
            has_assistant_message,
        } => Some((
            RuntimeEventType::CandidateReady,
            RuntimeEventSource::Runtime,
            json!({
                "summary": "agent produced a candidate ready for verification",
                "turn_id": turn_id,
                "tool_count": tool_count,
                "has_assistant_message": has_assistant_message,
            }),
        )),
        AgentLoopTraceEvent::VerificationReady { plan_id } => Some((
            RuntimeEventType::VerificationReady,
            RuntimeEventSource::Verifier,
            json!({
                "summary": "verification plan is ready to execute",
                "plan_id": plan_id,
            }),
        )),
        AgentLoopTraceEvent::VerificationPlanned(plan) => Some((
            RuntimeEventType::VerificationPlanned,
            RuntimeEventSource::Verifier,
            json!({
                "summary": format!("verification plan created for {:?}", plan.task_class),
                "plan": plan,
            }),
        )),
        AgentLoopTraceEvent::VerificationAssertionCompleted(assertion) => Some((
            RuntimeEventType::VerificationAssertionCompleted,
            RuntimeEventSource::Verifier,
            json!({
                "summary": format!(
                    "verification assertion {} completed as {:?}",
                    assertion.criterion_id,
                    assertion.status
                ),
                "assertion": assertion,
            }),
        )),
        AgentLoopTraceEvent::VerificationCompleted { record, terminal } => Some((
            RuntimeEventType::VerificationCompleted,
            RuntimeEventSource::Verifier,
            json!({
                "summary": format!("verification result: {:?}", record.result),
                "terminal": terminal,
                "record": record,
            }),
        )),
        AgentLoopTraceEvent::CorrectionIssued(correction) => Some((
            RuntimeEventType::ContinuationDecided,
            RuntimeEventSource::Runtime,
            json!({
                "summary": "runtime requested a bounded correction after verification",
                "reason": "verification_failed",
                "correction": correction,
            }),
        )),
        AgentLoopTraceEvent::ProviderStarted {
            request_id,
            provider_id,
            model_id,
        } => Some((
            RuntimeEventType::ProviderStarted,
            RuntimeEventSource::Provider,
            json!({
                "summary": "provider request started",
                "provider_request_id": request_id,
                "provider_id": provider_id,
                "model_id": model_id,
            }),
        )),
        AgentLoopTraceEvent::ProviderStreamed {
            request_id,
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
                    "provider_request_id": request_id,
                    "provider_id": provider_id,
                    "model_id": model_id,
                    "delta": delta,
                }),
            ))
        }
        AgentLoopTraceEvent::ProviderCompleted {
            request_id,
            provider_id,
            model_id,
            response,
        } => {
            let provider_tool_calls = response
                .tool_calls
                .into_iter()
                .map(|tool_call| {
                    json!({
                        "provider_tool_call_id": tool_call.tool_call_id,
                        "tool_name": tool_call.tool_name,
                    })
                })
                .collect::<Vec<_>>();
            Some((
                RuntimeEventType::ProviderCompleted,
                RuntimeEventSource::Provider,
                json!({
                    "summary": "provider request completed",
                    "provider_request_id": request_id,
                    "provider_response_id": response.response_id,
                    "provider_id": provider_id,
                    "model_id": model_id,
                    "finish_reason": response.finish_reason,
                    "tool_call_count": provider_tool_calls.len(),
                    "provider_tool_calls": provider_tool_calls,
                    "usage": response.usage,
                }),
            ))
        }
        AgentLoopTraceEvent::ProviderFailed {
            request_id,
            provider_id,
            model_id,
            error,
        } => Some((
            RuntimeEventType::ProviderFailed,
            RuntimeEventSource::Provider,
            json!({
                "summary": "provider request failed",
                "provider_request_id": request_id,
                "provider_id": provider_id,
                "model_id": model_id,
                "error": error,
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
        AgentLoopTraceEvent::ToolStarted {
            tool_call_id,
            provider_tool_call_id,
            tool_name,
            display_arguments,
            recovery_policy,
        } => Some((
            RuntimeEventType::ToolStarted,
            RuntimeEventSource::Tool,
            json!({
                "summary": format!("tool {tool_name} started"),
                "tool_call_id": tool_call_id,
                "provider_tool_call_id": provider_tool_call_id,
                "tool_name": tool_name,
                "arguments": display_arguments,
                "recovery_policy": recovery_policy,
            }),
        )),
        AgentLoopTraceEvent::ToolProgress(progress) => Some((
            RuntimeEventType::ToolProgress,
            RuntimeEventSource::Tool,
            json!({
                "summary": format!("tool {} {:?}", progress.tool_name, progress.phase),
                "tool_call_id": progress.tool_call_id,
                "tool_name": progress.tool_name.clone(),
                "progress": progress,
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
        AgentLoopTraceEvent::UserQuestionRequested(request) => Some((
            RuntimeEventType::UserQuestionRequested,
            RuntimeEventSource::Runtime,
            json!({
                "summary": "agent requested structured user input",
                "question_id": request.question_id,
                "request": request,
            }),
        )),
        AgentLoopTraceEvent::UserQuestionResolved(resolution) => Some((
            RuntimeEventType::UserQuestionResolved,
            RuntimeEventSource::User,
            json!({
                "summary": "structured user input received",
                "question_id": resolution.question_id,
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
        AgentLoopTraceEvent::ProviderTransportFallback {
            provider_id,
            from_transport,
            to_transport,
            reason,
        } => Some((
            RuntimeEventType::ProviderTransportFallback,
            RuntimeEventSource::Runtime,
            json!({
                "summary": format!(
                    "provider transport fallback for {provider_id}: {from_transport} -> {to_transport}"
                ),
                "provider_id": provider_id,
                "from_transport": from_transport,
                "to_transport": to_transport,
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
                "steer": turn.steer,
            }),
        )),
        AgentLoopTraceEvent::PendingTurnStartedWithExecution(configured) => Some((
            RuntimeEventType::TurnStarted,
            RuntimeEventSource::User,
            json!({
                "summary": "queued user turn started",
                "command_id": configured.turn.command_id,
                "turn_id": configured.turn.turn_id,
                "prompt": configured.turn.content,
                "steer": configured.turn.steer,
                "execution_mode": configured.execution.execution_mode,
                "tool_profile": configured.execution.tool_profile,
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
    };
    if let Some((event_type, source, _)) = &mapped {
        debug_assert_eq!(*event_type, descriptor.event_type);
        debug_assert_eq!(*source, descriptor.source);
    }
    mapped
}

/// 从已脱敏的上下文清单构造非敏感缓存诊断。只哈希消息 wire 摘要，避免
/// 序列化提示词正文；工具摘要直接复用快照中已经计算的 provider wire digest。
fn context_cache_diagnostics(
    snapshot: &ContextSnapshot,
    request: Option<&ProviderRequest>,
) -> Value {
    let stable_prefix_length = stable_prefix_message_count(snapshot);
    let manifest = snapshot
        .message_manifest
        .iter()
        .take(stable_prefix_length)
        .map(|message| {
            json!({
                "index": message.index,
                "role": message.role,
                "content_digest": message.content_digest,
                "wire_digest": message.wire_digest,
                "tool_call_ids": message.tool_call_ids,
            })
        })
        .collect::<Vec<_>>();
    let prefix_bytes = serde_json::to_vec(&manifest).unwrap_or_default();
    let prefix_digest =
        context_message_wire_prefix_digest_for_count(snapshot, stable_prefix_length)
            .expect("stable prefix length is bounded by the message manifest");
    let tool_digest = digest_json_value(&snapshot.tool_schema_digests);
    let provider_id = request
        .map(|request| request.provider_id.as_str())
        .unwrap_or(snapshot.provider_id.as_str());
    let model_id = request
        .map(|request| request.model_id.as_str())
        .unwrap_or(snapshot.model_id.as_str());
    let route_digest = format!(
        "sha256:{:x}",
        Sha256::digest(format!("{provider_id}\0{model_id}").as_bytes())
    );
    let cache_policy = request
        .map(|request| json!(request.cache_policy))
        .unwrap_or_else(|| Value::String("unknown".to_owned()));
    let message_prefix_token_estimate = stable_prefix_token_estimate(snapshot);
    json!({
        "scope_key": snapshot.cache_scope_key,
        "route": {
            "provider_id": diagnostic_route_label(provider_id),
            "model_id": diagnostic_route_label(model_id),
            "digest": route_digest,
        },
        "cache_policy": cache_policy,
        "message_count": snapshot.message_manifest.len(),
        "message_prefix_length": stable_prefix_length,
        "dynamic_message_count": snapshot
            .message_manifest
            .len()
            .saturating_sub(stable_prefix_length),
        "message_prefix_bytes": prefix_bytes.len(),
        "message_prefix_token_estimate": message_prefix_token_estimate,
        "message_prefix_digest": prefix_digest,
        "tool_schema_digests": snapshot.tool_schema_digests,
        "tool_digest": tool_digest,
        "planned_input_tokens": snapshot.budget_snapshot.planned_input_tokens,
        "planned_tool_tokens": snapshot.budget_snapshot.planned_tool_tokens,
        "canonical_request_digest": snapshot.canonical_request_digest,
    })
}

#[cfg(test)]
fn context_message_wire_prefix_digest(snapshot: &ContextSnapshot) -> String {
    context_message_wire_prefix_digest_for_count(snapshot, snapshot.message_manifest.len())
        .expect("full context snapshot prefix is always in range")
}

fn context_message_wire_prefix_digest_for_count(
    snapshot: &ContextSnapshot,
    message_count: usize,
) -> Option<String> {
    if message_count > snapshot.message_manifest.len() {
        return None;
    }
    let mut digest = Sha256::new();
    digest.update(b"golutra-context-prefix-v1\0");
    digest.update((message_count as u64).to_le_bytes());
    for message in &snapshot.message_manifest[..message_count] {
        let wire_digest = if message.wire_digest.is_empty() {
            &message.content_digest
        } else {
            &message.wire_digest
        };
        digest.update((wire_digest.len() as u64).to_le_bytes());
        digest.update(wire_digest.as_bytes());
    }
    Some(format!("sha256:{:x}", digest.finalize()))
}

fn digest_json_value<T: serde::Serialize>(value: &T) -> String {
    let bytes = serde_json::to_vec(value).unwrap_or_default();
    format!("sha256:{:x}", Sha256::digest(bytes))
}

fn diagnostic_route_label(value: &str) -> String {
    let (redacted, _) = redact_sensitive_text(value);
    let label = redacted
        .chars()
        .filter(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.' | ':')
        })
        .take(96)
        .collect::<String>();
    if label.is_empty() {
        "unknown".to_owned()
    } else {
        label
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
        schema_version: golutra_core::RUNTIME_EVENT_SCHEMA_VERSION,
        causal_context: Default::default(),
        causal_links: Vec::new(),
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
        schema_version: golutra_core::RUNTIME_EVENT_SCHEMA_VERSION,
        causal_context: Default::default(),
        causal_links: Vec::new(),
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
        "runtime_identity": super::runtime_identity(),
        "payload": payload,
        "runtime": event.payload,
    });
    event
}

#[cfg(test)]
mod tests {
    use golutra_context::{
        ContextBuilder, context_snapshot_from_request, provider_request_from_plan,
    };
    use golutra_core::WorkspaceChangeRequirement;

    use super::*;

    #[test]
    fn legacy_pending_turn_recovery_rebuilds_its_task_contract() {
        let turn_id = TurnId::new();
        let mut event = host_event(
            7,
            SessionId::new(),
            Some(TaskId::new()),
            RuntimeEventType::TurnQueued,
            RuntimeEventSource::Runtime,
            json!({
                "payload": {
                    "prompt": "write the requested content to result.txt",
                    "path": "result.txt",
                    "content": "recovered\n",
                    "output_schema": {"type": "object"},
                    "allow_network": true,
                    "yolo": true,
                    "_external_verifiers_require_os_sandbox": true
                }
            }),
        );
        event.turn_id = Some(turn_id);

        let recovered = recovered_pending_turn_from_event(&event)
            .expect("valid durable turn")
            .expect("pending turn");
        let contract = recovered.pending.task_contract.expect("adapted contract");

        assert_eq!(
            contract.workspace_change,
            WorkspaceChangeRequirement::Required
        );
        assert_eq!(contract.required_paths, ["result.txt"]);
        assert_eq!(contract.required_file_contents.len(), 1);
        assert_eq!(contract.required_file_contents[0].path, "result.txt");
        assert_eq!(contract.required_file_contents[0].content, "recovered\n");
        assert_eq!(
            recovered.pending.output_schema,
            Some(json!({"type": "object"}))
        );
        assert!(recovered.pending.allow_network);
        assert!(recovered.pending.yolo);
        assert!(recovered.pending.external_verifiers_require_os_sandbox);
        assert_eq!(
            recovered.execution.tool_profile,
            Some(golutra_protocol::AgentToolProfile::Coding)
        );
    }

    #[test]
    fn steering_recovery_inherits_the_active_tool_profile_unless_explicit() {
        for (payload, expected) in [
            (
                json!({
                    "prompt": "focus on the public API",
                    "execution_mode": "open",
                    "steer": true
                }),
                None,
            ),
            (
                json!({
                    "prompt": "use the managed tools",
                    "execution_mode": "open",
                    "tool_profile": "full",
                    "steer": true
                }),
                Some(golutra_protocol::AgentToolProfile::Full),
            ),
        ] {
            let mut event = host_event(
                7,
                SessionId::new(),
                Some(TaskId::new()),
                RuntimeEventType::TurnQueued,
                RuntimeEventSource::Runtime,
                json!({"payload": payload}),
            );
            event.turn_id = Some(TurnId::new());

            let recovered = recovered_pending_turn_from_event(&event)
                .expect("valid steering event")
                .expect("pending steering turn");
            assert_eq!(recovered.execution.tool_profile, expected);
            assert_eq!(recovered.pending.task_contract, None);
        }
    }

    #[test]
    fn context_cache_diagnostics_are_wire_complete_without_sensitive_content() {
        let session_id = SessionId::new();
        let task_id = TaskId::new();
        let turn_id = TurnId::new();
        let messages = vec![
            golutra_llm::ProviderMessage {
                role: golutra_llm::ProviderRole::System,
                content: "stable system prompt".to_owned(),
                tool_call_id: None,
                tool_name: None,
                tool_calls: Vec::new(),
                metadata: Default::default(),
            },
            golutra_llm::ProviderMessage {
                role: golutra_llm::ProviderRole::Assistant,
                content: "calling a tool".to_owned(),
                tool_call_id: None,
                tool_name: None,
                tool_calls: vec![golutra_llm::ProviderToolCall {
                    tool_call_id: "call-1".to_owned(),
                    tool_name: "read_file".to_owned(),
                    arguments: serde_json::json!({
                        "path": "private.txt",
                        "token": "do-not-log"
                    }),
                }],
                metadata: Default::default(),
            },
        ];
        let plan = ContextBuilder::default()
            .build_from_messages(task_id, turn_id, messages)
            .expect("context plan");
        let mut request =
            provider_request_from_plan(&plan, task_id, turn_id, "mock", "mock-model", Vec::new());
        request.cache_policy = golutra_core::PromptCachePolicy::Long;
        request.cache_scope = Some(golutra_llm::PromptCacheScope::session(session_id, None));
        let snapshot = context_snapshot_from_request(session_id, &plan, &request);
        let (_, _, payload) =
            trace_event_payload(AgentLoopTraceEvent::ContextSnapshotCaptured { snapshot, request })
                .expect("context snapshot event");
        let diagnostics = &payload["cache_diagnostics"];
        assert_eq!(diagnostics["message_count"], 2);
        assert!(diagnostics["scope_key"].is_string());
        assert_eq!(diagnostics["cache_policy"], "long");
        assert_eq!(diagnostics["route"]["provider_id"], "mock");
        assert_eq!(diagnostics["route"]["model_id"], "mock-model");
        assert!(diagnostics["route"]["digest"].is_string());
        assert_eq!(diagnostics["message_prefix_length"], 1);
        assert_eq!(diagnostics["dynamic_message_count"], 1);
        assert!(diagnostics["message_prefix_bytes"].as_u64().is_some());
        assert_eq!(
            diagnostics["message_prefix_token_estimate"],
            payload["snapshot"]["message_manifest"][0]["estimated_tokens"]
        );
        assert!(diagnostics["message_prefix_digest"].is_string());
        assert!(diagnostics["canonical_request_digest"].is_string());
        assert!(diagnostics["tool_schema_digests"].is_array());
        let encoded = diagnostics.to_string();
        assert!(!encoded.contains("stable system prompt"));
        assert!(!encoded.contains("private.txt"));
        assert!(!encoded.contains("do-not-log"));
        assert!(payload["snapshot"]["message_manifest"][1]["wire_digest"].is_string());
    }

    #[test]
    fn cache_diagnostics_prove_append_only_message_prefix_and_tool_invalidation() {
        let task_id = TaskId::new();
        let first_turn = TurnId::new();
        let second_turn = TurnId::new();
        let session_id = SessionId::new();
        let first_messages = vec![
            golutra_llm::ProviderMessage {
                role: golutra_llm::ProviderRole::System,
                content: "stable instructions".to_owned(),
                tool_call_id: None,
                tool_name: None,
                tool_calls: Vec::new(),
                metadata: Default::default(),
            },
            golutra_llm::ProviderMessage {
                role: golutra_llm::ProviderRole::User,
                content: "inspect the workspace".to_owned(),
                tool_call_id: None,
                tool_name: None,
                tool_calls: Vec::new(),
                metadata: Default::default(),
            },
        ];
        let mut second_messages = first_messages.clone();
        second_messages.push(golutra_llm::ProviderMessage {
            role: golutra_llm::ProviderRole::Assistant,
            content: "I will inspect it".to_owned(),
            tool_call_id: None,
            tool_name: None,
            tool_calls: Vec::new(),
            metadata: Default::default(),
        });
        let tool = golutra_core::ToolContract {
            tool_name: "read_file".to_owned(),
            input_schema: json!({
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "required": ["path"]
            }),
            output_schema: json!({}),
            error_schema: json!({}),
            side_effect_type: golutra_core::SideEffectType::None,
            idempotency_key_policy: "none".to_owned(),
            timeout_policy: "bounded".to_owned(),
            cancellation_policy: "supported".to_owned(),
            retry_policy: "none".to_owned(),
            artifact_policy: "none".to_owned(),
            permission_policy_ref: None,
        };
        let first_plan = ContextBuilder::default()
            .build_from_messages(task_id, first_turn, first_messages)
            .expect("first plan");
        let second_plan = ContextBuilder::default()
            .build_from_messages(task_id, second_turn, second_messages)
            .expect("second plan");
        let mut first_request = provider_request_from_plan(
            &first_plan,
            task_id,
            first_turn,
            "mock",
            "mock-model",
            vec![tool.clone()],
        );
        let mut second_request = provider_request_from_plan(
            &second_plan,
            task_id,
            second_turn,
            "mock",
            "mock-model",
            vec![tool.clone()],
        );
        first_request.cache_scope = Some(golutra_llm::PromptCacheScope::session(session_id, None));
        second_request.cache_scope = first_request.cache_scope.clone();
        let first_snapshot = context_snapshot_from_request(session_id, &first_plan, &first_request);
        let second_snapshot =
            context_snapshot_from_request(session_id, &second_plan, &second_request);
        let first_digest = context_message_wire_prefix_digest(&first_snapshot);
        assert_eq!(
            first_digest,
            context_message_wire_prefix_digest_for_count(
                &second_snapshot,
                first_snapshot.message_manifest.len(),
            )
            .expect("previous request prefix")
        );
        let first_diagnostics = context_cache_diagnostics(&first_snapshot, Some(&first_request));
        let second_diagnostics = context_cache_diagnostics(&second_snapshot, Some(&second_request));
        assert_eq!(
            first_diagnostics["tool_digest"],
            second_diagnostics["tool_digest"]
        );

        let mut changed_tool = tool;
        changed_tool.input_schema["properties"]["path"]["type"] = json!("integer");
        second_request.tools = vec![changed_tool];
        let changed_snapshot =
            context_snapshot_from_request(session_id, &second_plan, &second_request);
        let changed_diagnostics =
            context_cache_diagnostics(&changed_snapshot, Some(&second_request));
        assert_ne!(
            second_diagnostics["tool_digest"],
            changed_diagnostics["tool_digest"]
        );
    }

    #[test]
    fn pending_turn_recovery_rejects_malformed_or_invalid_contracts() {
        for payload in [
            json!({
                "prompt": "run a recovered turn",
                "task_contract": {"verification": "not-a-verification-mode"}
            }),
            json!({
                "prompt": "run a recovered turn",
                "external_verifiers": "cargo test"
            }),
            json!({
                "prompt": "run a recovered turn",
                "task_contract": {"schema_version": 999}
            }),
            json!({
                "prompt": "run a recovered turn",
                "yolo": "true"
            }),
            json!({
                "prompt": "run a recovered turn",
                "execution_mode": "adaptive"
            }),
            json!({
                "prompt": "run a recovered turn",
                "execution_mode": "open",
                "tool_profile": "everything"
            }),
        ] {
            let mut event = host_event(
                7,
                SessionId::new(),
                Some(TaskId::new()),
                RuntimeEventType::TurnQueued,
                RuntimeEventSource::Runtime,
                json!({"payload": payload}),
            );
            event.turn_id = Some(TurnId::new());

            let error = recovered_pending_turn_from_event(&event)
                .expect_err("malformed durable verification data must fail recovery");
            assert!(error.to_string().contains("durable queued turn event"));
        }
    }
}
