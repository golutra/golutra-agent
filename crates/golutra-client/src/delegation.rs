//! Synchronous, host-owned delegation to one isolated child agent.
//!
//! The model-facing surface is deliberately small. The child gets a fresh
//! session and the explicit task only; the host keeps the parent capability
//! boundary, waits for a terminal projection, and returns bounded structured
//! facts through the ordinary tool result contract.

use std::sync::{Arc, Weak};
use std::time::Duration;

use async_trait::async_trait;
use golutra_core::{
    Actor, ActorKind, ApprovalDecision, ApprovalId, ApprovalResolution, ApprovalScope, CommandId,
    SessionId, TaskStatus, ThreadId,
};
use golutra_llm::{ProviderGenerationConfig, ProviderReasoningEffort};
use golutra_protocol::{
    RuntimeEvent, RuntimeEventType, SessionCommand, SessionCommandKind, StateProjection,
};
use golutra_tools::{TaskDelegationBackend, TaskDelegationOutput, ToolError, ToolRequest};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::time::{sleep, timeout};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::{ClientError, RuntimeHost};

pub(crate) const DELEGATED_TASK_MARKER: &str = "_delegated_task";
const DELEGATED_PARENT_THREAD_KEY: &str = "_parent_thread_id";
const DELEGATED_THREAD_TITLE: &str = "Delegated task";
const DELEGATED_MAX_ELAPSED_MS: u64 = 30 * 60 * 1_000;
const DELEGATED_WAIT_GRACE_MS: u64 = 5_000;
const DELEGATED_WAIT_POLL_MS: u64 = 50;

#[derive(Debug)]
pub(crate) struct RuntimeTaskDelegationBackend {
    host: Weak<RuntimeHost>,
}

impl RuntimeTaskDelegationBackend {
    pub(crate) fn new(host: Weak<RuntimeHost>) -> Self {
        Self { host }
    }
}

#[async_trait]
impl TaskDelegationBackend for RuntimeTaskDelegationBackend {
    async fn delegate(
        &self,
        request: &ToolRequest,
        cancellation: CancellationToken,
    ) -> Result<TaskDelegationOutput, ToolError> {
        let host = self
            .host
            .upgrade()
            .ok_or_else(|| ToolError::Execution("runtime host is shutting down".to_owned()))?;
        delegate_task(&host, request, cancellation)
            .await
            .map_err(|error| ToolError::Execution(error.to_string()))
    }
}

async fn delegate_task(
    host: &Arc<RuntimeHost>,
    request: &ToolRequest,
    cancellation: CancellationToken,
) -> Result<TaskDelegationOutput, ClientError> {
    let task = request
        .arguments
        .get("task")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|task| !task.is_empty())
        .ok_or_else(|| ClientError::TaskExecution("delegate task cannot be empty".to_owned()))?;
    if cancellation.is_cancelled() {
        return Ok(cancelled_delegation_output(
            "delegation cancelled before child creation",
        ));
    }
    let parent_thread = host
        .repositories
        .threads
        .by_session(request.session_id)
        .await?
        .ok_or_else(|| {
            ClientError::InvalidSession(format!(
                "parent session `{}` has no thread record",
                request.session_id
            ))
        })?;
    host.ensure_thread_in_workspace(&parent_thread)?;
    let parent_control = host
        .task_controls
        .lock()
        .await
        .get(&request.session_id)
        .cloned()
        .ok_or_else(|| {
            ClientError::TaskExecution(
                "delegation requires an active parent agent task control".to_owned(),
            )
        })?;

    let overrides = delegation_overrides(&parent_control.provider_settings, &request.arguments)?;
    let identity = delegation_identity(
        request,
        task,
        &overrides,
        parent_control.allow_network,
        parent_control.yolo,
    )?;
    let child_session_id = SessionId(deterministic_uuid(&identity, "session"));
    let child_thread_id = ThreadId(deterministic_uuid(&identity, "thread"));
    let actor = Actor {
        kind: ActorKind::Runtime,
        id: format!("delegate:parent:{}", request.session_id),
    };

    let mut create_payload = json!({
        "_thread_id": child_thread_id,
        DELEGATED_PARENT_THREAD_KEY: parent_thread.thread_id,
        "title": DELEGATED_THREAD_TITLE,
        "prompt": task,
        DELEGATED_TASK_MARKER: true,
    });
    create_payload["_delegation_parent_session_id"] = json!(request.session_id);
    create_payload["_delegation_parent_tool_call_id"] = json!(request.tool_call_id);
    let create_ack = match host
        .clone()
        .handle_command(internal_command(
            child_session_id,
            SessionCommandKind::Create,
            format!("{identity}:create"),
            actor.clone(),
            create_payload,
        ))
        .await
    {
        Ok(ack) => ack,
        Err(error) => {
            return fail_delegation(
                host,
                request.session_id,
                child_session_id,
                child_thread_id,
                &actor,
                error,
            )
            .await;
        }
    };
    if !create_ack.accepted {
        return fail_delegation(
            host,
            request.session_id,
            child_session_id,
            child_thread_id,
            &actor,
            ClientError::TaskExecution(
                create_ack
                    .reason
                    .unwrap_or_else(|| "delegated child session creation was rejected".to_owned()),
            ),
        )
        .await;
    }
    if cancellation.is_cancelled() {
        cleanup_child_after_failure(
            host,
            request.session_id,
            child_session_id,
            child_thread_id,
            &actor,
        )
        .await?;
        return Ok(cancelled_delegation_output(
            "delegation cancelled after child creation",
        ));
    }

    let mut prompt_payload = json!({
        "prompt": task,
        DELEGATED_TASK_MARKER: true,
        "allow_network": parent_control.allow_network,
        "yolo": parent_control.yolo,
        "max_elapsed_ms": DELEGATED_MAX_ELAPSED_MS,
        "_delegation_parent_session_id": request.session_id,
        "_delegation_parent_tool_call_id": request.tool_call_id,
    });
    if let Some(profile) = overrides.profile.clone() {
        prompt_payload["provider_profile"] = profile;
    }
    if let Some(model) = overrides.model.clone() {
        prompt_payload["provider_model"] = model;
    }
    if let Some(generation_config) = overrides.generation_config.clone() {
        prompt_payload["provider_generation_config"] = generation_config;
    }
    let prompt_ack = match host
        .clone()
        .handle_command(internal_command(
            child_session_id,
            SessionCommandKind::Prompt,
            format!("{identity}:prompt"),
            actor.clone(),
            prompt_payload,
        ))
        .await
    {
        Ok(ack) => ack,
        Err(error) => {
            return fail_delegation(
                host,
                request.session_id,
                child_session_id,
                child_thread_id,
                &actor,
                error,
            )
            .await;
        }
    };
    if !prompt_ack.accepted {
        return fail_delegation(
            host,
            request.session_id,
            child_session_id,
            child_thread_id,
            &actor,
            ClientError::TaskExecution(
                prompt_ack
                    .reason
                    .unwrap_or_else(|| "delegated child prompt was rejected".to_owned()),
            ),
        )
        .await;
    }

    let child_state = match timeout(
        Duration::from_millis(DELEGATED_MAX_ELAPSED_MS.saturating_add(DELEGATED_WAIT_GRACE_MS)),
        wait_for_child(host, child_session_id, cancellation),
    )
    .await
    {
        Ok(Ok(state)) => state,
        Ok(Err(error)) => {
            return fail_delegation(
                host,
                request.session_id,
                child_session_id,
                child_thread_id,
                &actor,
                error,
            )
            .await;
        }
        Err(_) => {
            return fail_delegation(
                host,
                request.session_id,
                child_session_id,
                child_thread_id,
                &actor,
                ClientError::TaskExecution(
                    "delegated child exceeded its maximum elapsed time".to_owned(),
                ),
            )
            .await;
        }
    };
    if let Err(error) = host.wait_for_finishing_task_control(child_session_id).await {
        return fail_delegation(
            host,
            request.session_id,
            child_session_id,
            child_thread_id,
            &actor,
            error,
        )
        .await;
    }
    let actual_model = match child_provider_model(host, child_session_id).await {
        Ok(model) => model,
        Err(error) => {
            return fail_delegation(
                host,
                request.session_id,
                child_session_id,
                child_thread_id,
                &actor,
                error,
            )
            .await;
        }
    };
    if let Err(error) = host
        .process_supervisor
        .terminate_session(child_session_id)
        .await
    {
        return fail_delegation(
            host,
            request.session_id,
            child_session_id,
            child_thread_id,
            &actor,
            ClientError::TaskExecution(error.to_string()),
        )
        .await;
    }
    if let Err(error) =
        archive_child_if_present(host, request.session_id, child_thread_id, &actor).await
    {
        return fail_delegation(
            host,
            request.session_id,
            child_session_id,
            child_thread_id,
            &actor,
            error,
        )
        .await;
    }
    let effective_reasoning_effort = overrides
        .reasoning_effort
        .or_else(|| parent_reasoning_effort(&parent_control.provider_settings))
        .map(|effort| Value::String(effort.as_wire_value().to_owned()))
        .unwrap_or(Value::String("default".to_owned()));
    let requested_model = overrides.model.unwrap_or(Value::Null);
    let effective_model = actual_model
        .map(Value::String)
        .unwrap_or_else(|| requested_model.clone());
    let content = child_state
        .final_message
        .clone()
        .or_else(|| {
            child_state
                .last_loop_decision
                .as_ref()
                .map(|decision| decision.reason.clone())
        })
        .unwrap_or_else(|| "delegated child produced no final response".to_owned());
    let status = delegated_result_status(child_state.task_status);
    let summary = match child_state.task_status {
        TaskStatus::Completed => "delegated task completed",
        TaskStatus::Partial => "delegated task produced a partial result",
        TaskStatus::Cancelled | TaskStatus::Interrupted => "delegated task was cancelled",
        TaskStatus::Blocked => "delegated task was blocked",
        TaskStatus::Uncertain => "delegated task needs reconciliation",
        _ => "delegated task failed",
    };
    Ok(TaskDelegationOutput {
        status,
        summary: summary.to_owned(),
        content,
        structured_facts: json!({
            "child_session_id": child_session_id,
            "child_thread_id": child_thread_id,
            "child_status": child_state.task_status,
            "requested_model": requested_model,
            "effective_model": effective_model,
            "effective_reasoning_effort": effective_reasoning_effort,
            "verification": child_state.last_verification,
        }),
    })
}

fn cancelled_delegation_output(reason: &str) -> TaskDelegationOutput {
    TaskDelegationOutput {
        status: golutra_core::ToolResultStatus::Cancelled,
        summary: reason.to_owned(),
        content: String::new(),
        structured_facts: json!({"cancelled": true}),
    }
}

async fn cleanup_child_after_failure(
    host: &Arc<RuntimeHost>,
    attached_session_id: SessionId,
    child_session_id: SessionId,
    child_thread_id: ThreadId,
    actor: &Actor,
) -> Result<(), ClientError> {
    cancel_child(host, child_session_id).await;
    let mut failures = Vec::new();
    let task_stopped = match host.wait_for_finishing_task_control(child_session_id).await {
        Ok(()) => true,
        Err(error) => {
            failures.push(format!("task supervisor cleanup failed: {error}"));
            false
        }
    };
    let processes_stopped = match host
        .process_supervisor
        .terminate_session(child_session_id)
        .await
    {
        Ok(_) => true,
        Err(error) => {
            failures.push(format!("managed process cleanup failed: {error}"));
            false
        }
    };
    if task_stopped
        && processes_stopped
        && let Err(error) =
            archive_child_if_present(host, attached_session_id, child_thread_id, actor).await
    {
        failures.push(format!("thread archival failed: {error}"));
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(ClientError::TaskExecution(format!(
            "delegated child cleanup failed: {}",
            failures.join("; ")
        )))
    }
}

async fn fail_delegation<T>(
    host: &Arc<RuntimeHost>,
    attached_session_id: SessionId,
    child_session_id: SessionId,
    child_thread_id: ThreadId,
    actor: &Actor,
    primary: ClientError,
) -> Result<T, ClientError> {
    match cleanup_child_after_failure(
        host,
        attached_session_id,
        child_session_id,
        child_thread_id,
        actor,
    )
    .await
    {
        Ok(()) => Err(primary),
        Err(cleanup) => Err(ClientError::TaskExecution(format!(
            "{primary}; additionally, {cleanup}"
        ))),
    }
}

#[derive(Debug, Clone)]
struct DelegationOverrides {
    profile: Option<Value>,
    model: Option<Value>,
    generation_config: Option<Value>,
    reasoning_effort: Option<ProviderReasoningEffort>,
}

fn delegation_overrides(
    parent: &super::ProviderTurnSettings,
    arguments: &Value,
) -> Result<DelegationOverrides, ClientError> {
    let profile = parent.profile.clone();
    let model = arguments
        .get("model")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| Value::String(value.to_owned()))
        .or_else(|| parent.model.clone());
    let reasoning_effort = arguments
        .get("reasoning_effort")
        .map(parse_reasoning_effort)
        .transpose()?;
    let generation_config = if let Some(reasoning_effort) = reasoning_effort {
        let mut config = parent
            .generation_config
            .clone()
            .map(serde_json::from_value::<ProviderGenerationConfig>)
            .transpose()
            .map_err(|error| {
                ClientError::TaskExecution(format!(
                    "parent provider generation config is invalid: {error}"
                ))
            })?
            .unwrap_or_default();
        config.reasoning_effort = Some(reasoning_effort);
        config.validate().map_err(ClientError::TaskExecution)?;
        Some(serde_json::to_value(config)?)
    } else {
        parent.generation_config.clone()
    };
    Ok(DelegationOverrides {
        profile,
        model,
        generation_config,
        reasoning_effort,
    })
}

fn parse_reasoning_effort(value: &Value) -> Result<ProviderReasoningEffort, ClientError> {
    let value = value.as_str().ok_or_else(|| {
        ClientError::TaskExecution("reasoning_effort must be a string".to_owned())
    })?;
    match value.trim().to_ascii_lowercase().replace('-', "_").as_str() {
        "low" => Ok(ProviderReasoningEffort::Low),
        "medium" => Ok(ProviderReasoningEffort::Medium),
        "high" => Ok(ProviderReasoningEffort::High),
        "xhigh" | "x_high" => Ok(ProviderReasoningEffort::Xhigh),
        _ => Err(ClientError::TaskExecution(
            "reasoning_effort must be one of: low, medium, high, xhigh".to_owned(),
        )),
    }
}

fn parent_reasoning_effort(
    settings: &super::ProviderTurnSettings,
) -> Option<ProviderReasoningEffort> {
    settings
        .generation_config
        .as_ref()
        .and_then(|value| serde_json::from_value::<ProviderGenerationConfig>(value.clone()).ok())
        .and_then(|config| config.reasoning_effort)
}

fn delegated_result_status(status: TaskStatus) -> golutra_core::ToolResultStatus {
    match status {
        TaskStatus::Completed => golutra_core::ToolResultStatus::Ok,
        TaskStatus::Cancelled | TaskStatus::Interrupted => {
            golutra_core::ToolResultStatus::Cancelled
        }
        TaskStatus::Blocked => golutra_core::ToolResultStatus::Blocked,
        _ => golutra_core::ToolResultStatus::Error,
    }
}

async fn wait_for_child(
    host: &Arc<RuntimeHost>,
    session_id: SessionId,
    cancellation: CancellationToken,
) -> Result<StateProjection, ClientError> {
    let mut completion = host
        .task_controls
        .lock()
        .await
        .get(&session_id)
        .map(|control| control.completion.clone());
    let mut denied_approval = None;
    let mut cancelled_child = false;
    loop {
        let state = host
            .repositories
            .projections
            .state(session_id, None)
            .await?;
        if state.task_status.is_terminal() {
            return Ok(state);
        }
        if cancellation.is_cancelled() && !cancelled_child {
            cancel_child(host, session_id).await;
            cancelled_child = true;
        }
        if state.task_status == TaskStatus::WaitingApproval
            && denied_approval.as_deref() != state.pending_approval.as_deref()
            && let Some(approval_id) = state
                .pending_approval
                .as_deref()
                .and_then(|value| value.parse::<ApprovalId>().ok())
            && let Some(control) = host.task_controls.lock().await.get(&session_id).cloned()
        {
            control
                .execution
                .resolve_approval(ApprovalResolution {
                    approval_id,
                    decision: ApprovalDecision::Denied,
                    scope: ApprovalScope::Once,
                    resource_prefix: None,
                    reason: "delegated child agents cannot pause for interactive approval"
                        .to_owned(),
                })
                .await
                .map_err(|error| ClientError::TaskExecution(error.to_string()))?;
            denied_approval = Some(approval_id.to_string());
        }
        if state.task_status == TaskStatus::WaitingAuthentication {
            cancel_child(host, session_id).await;
            cancelled_child = true;
        }
        if let Some(receiver) = completion.as_mut() {
            tokio::select! {
                _ = cancellation.cancelled(), if !cancelled_child => {}
                changed = receiver.changed() => {
                    if changed.is_err() {
                        completion = None;
                    }
                }
                _ = sleep(Duration::from_millis(DELEGATED_WAIT_POLL_MS)) => {}
            }
        } else {
            tokio::select! {
                _ = cancellation.cancelled(), if !cancelled_child => {}
                _ = sleep(Duration::from_millis(DELEGATED_WAIT_POLL_MS)) => {}
            }
        }
    }
}

async fn cancel_child(host: &Arc<RuntimeHost>, session_id: SessionId) {
    if let Some(control) = host.task_controls.lock().await.get(&session_id).cloned() {
        control.execution.cancel();
    }
}

async fn archive_child(
    host: &Arc<RuntimeHost>,
    attached_session_id: SessionId,
    thread_id: ThreadId,
    actor: &Actor,
) -> Result<(), ClientError> {
    let ack = host
        .clone()
        .handle_command(internal_command(
            attached_session_id,
            SessionCommandKind::ArchiveThread,
            // A rejected metadata command is journaled as rejected. Use a fresh
            // attempt key so a transient active-lane/lease rejection can be
            // retried after cleanup has completed.
            format!("delegate-archive:{thread_id}:{}", Uuid::now_v7()),
            actor.clone(),
            json!({"thread_id": thread_id}),
        ))
        .await?;
    if ack.accepted {
        Ok(())
    } else {
        Err(ClientError::TaskExecution(ack.reason.unwrap_or_else(
            || format!("delegated child thread `{thread_id}` could not be archived"),
        )))
    }
}

async fn archive_child_if_present(
    host: &Arc<RuntimeHost>,
    attached_session_id: SessionId,
    thread_id: ThreadId,
    actor: &Actor,
) -> Result<(), ClientError> {
    let Some(thread) = host.repositories.threads.by_id(thread_id).await? else {
        return Ok(());
    };
    if thread.archived {
        return Ok(());
    }
    archive_child(host, attached_session_id, thread_id, actor).await
}

async fn child_provider_model(
    host: &Arc<RuntimeHost>,
    session_id: SessionId,
) -> Result<Option<String>, ClientError> {
    let events = host
        .repositories
        .events
        .load(session_id, None, None)
        .await?;
    Ok(latest_provider_model(&events))
}

fn latest_provider_model(events: &[RuntimeEvent]) -> Option<String> {
    events
        .iter()
        .rev()
        .filter(|event| {
            matches!(
                event.event_type,
                RuntimeEventType::ProviderStarted
                    | RuntimeEventType::ProviderCompleted
                    | RuntimeEventType::ProviderFailed
            )
        })
        .find_map(|event| {
            event
                .payload
                .get("model_id")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|model| !model.is_empty())
                .map(ToOwned::to_owned)
        })
}

fn internal_command(
    session_id: SessionId,
    kind: SessionCommandKind,
    idempotency_key: String,
    actor: Actor,
    payload: Value,
) -> SessionCommand {
    let command_id = CommandId(deterministic_uuid(&idempotency_key, "command"));
    SessionCommand {
        command_id,
        session_id: Some(session_id),
        kind,
        idempotency_key,
        actor,
        payload,
        timestamp: chrono::Utc::now(),
    }
}

fn delegation_identity(
    request: &ToolRequest,
    task: &str,
    overrides: &DelegationOverrides,
    allow_network: bool,
    yolo: bool,
) -> Result<String, ClientError> {
    let tool_call_identity = request
        .provider_tool_call_id
        .clone()
        .unwrap_or_else(|| request.tool_call_id.to_string());
    let parameters = json!({
        "task": task,
        "provider_profile": overrides.profile,
        "provider_model": overrides.model,
        "provider_generation_config": overrides.generation_config,
        "allow_network": allow_network,
        "yolo": yolo,
    });
    let parameter_digest = Sha256::digest(serde_json::to_vec(&parameters)?);
    Ok(format!(
        "{}:{}:{}:{parameter_digest:x}",
        request.session_id,
        request
            .turn_id
            .map(|turn_id| turn_id.to_string())
            .unwrap_or_else(|| "no-turn".to_owned()),
        tool_call_identity,
    ))
}

fn deterministic_uuid(identity: &str, kind: &str) -> Uuid {
    let digest = Sha256::digest(format!("golutra-delegate:{kind}:{identity}").as_bytes());
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x50;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Uuid::from_bytes(bytes)
}
