//! Synchronous, host-owned delegation to one isolated child agent.
//!
//! The model-facing surface is deliberately small. The child gets a fresh
//! session and the explicit task only; the host keeps the parent capability
//! boundary, waits for a terminal projection, and returns bounded structured
//! facts through the ordinary tool result contract.

use std::collections::hash_map::Entry;
use std::sync::{Arc, Mutex as StdMutex, Weak};
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use golutra_core::{
    Actor, ActorKind, ApprovalDecision, ApprovalId, ApprovalResolution, ApprovalScope, CommandId,
    SessionId, TaskStatus, ThreadId, TokenUsageRecord, ToolCallId,
};
use golutra_llm::{ProviderGenerationConfig, ProviderReasoningEffort};
use golutra_protocol::{
    AgentToolProfile, RuntimeEvent, RuntimeEventType, SessionCommand, SessionCommandKind,
    StateProjection,
};
use golutra_tools::{TaskDelegationBackend, TaskDelegationOutput, ToolError, ToolRequest};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::{
    sync::watch,
    task::AbortHandle,
    time::{sleep, timeout},
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::{ClientError, RuntimeHost, delegation_policy};

pub(crate) const DELEGATED_TASK_MARKER: &str = "_delegated_task";
const DELEGATED_PARENT_THREAD_KEY: &str = "_parent_thread_id";
pub(crate) const DELEGATED_ADMISSION_TOKEN_KEY: &str = "_delegation_admission_token";
const DELEGATED_THREAD_TITLE: &str = "Delegated task";
const DELEGATED_COMPLETION_GRACE_MS: u64 = 5_000;
const DELEGATED_COMPLETION_GRACE_DIVISOR: u64 = 20;
const DELEGATED_WAIT_POLL_MS: u64 = 50;

type SharedDelegationResult = Result<TaskDelegationOutput, String>;

/// One in-process operation per deterministic delegated tool-call identity.
///
/// Concurrent retries subscribe to the same result instead of reserving a
/// second budget lease or gaining cancellation ownership over the child.
#[derive(Debug)]
pub(crate) struct DelegationOperation {
    parent_session_id: SessionId,
    result: watch::Receiver<Option<SharedDelegationResult>>,
    result_sender: watch::Sender<Option<SharedDelegationResult>>,
    cancellation: CancellationToken,
    lifecycle: StdMutex<DelegationOperationLifecycle>,
}

#[derive(Debug, Default)]
struct DelegationOperationLifecycle {
    completed: bool,
    force_stopped: bool,
    owner_abort: Option<AbortHandle>,
}

impl DelegationOperation {
    fn new(parent_session_id: SessionId, cancellation: CancellationToken) -> Self {
        let (sender, result) = watch::channel(None);
        Self {
            parent_session_id,
            result,
            result_sender: sender,
            cancellation,
            lifecycle: StdMutex::new(DelegationOperationLifecycle::default()),
        }
    }

    pub(crate) fn belongs_to(&self, session_id: SessionId) -> bool {
        self.parent_session_id == session_id
    }

    fn complete(&self, result: &Result<TaskDelegationOutput, ClientError>) {
        let mut lifecycle = self
            .lifecycle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if lifecycle.completed {
            return;
        }
        self.result_sender.send_replace(Some(
            result
                .as_ref()
                .cloned()
                .map_err(std::string::ToString::to_string),
        ));
        lifecycle.completed = true;
        lifecycle.owner_abort.take();
    }

    pub(crate) fn is_complete(&self) -> bool {
        self.lifecycle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .completed
    }

    fn cancellation(&self) -> CancellationToken {
        self.cancellation.clone()
    }

    pub(crate) fn cancel(&self) {
        self.cancellation.cancel();
    }

    fn set_owner_abort(&self, abort: AbortHandle) {
        let mut lifecycle = self
            .lifecycle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if lifecycle.force_stopped {
            drop(lifecycle);
            abort.abort();
            return;
        }
        if !lifecycle.completed {
            lifecycle.owner_abort = Some(abort);
        }
    }

    pub(crate) fn force_stop(&self) {
        let stopped = ClientError::TaskExecution(
            "delegation owner stopped during runtime shutdown".to_owned(),
        )
        .to_string();
        let abort = {
            let mut lifecycle = self
                .lifecycle
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if lifecycle.completed {
                return;
            }
            lifecycle.force_stopped = true;
            self.result_sender.send_replace(Some(Err(stopped)));
            lifecycle.completed = true;
            lifecycle.owner_abort.take()
        };
        self.cancel();
        if let Some(abort) = abort {
            abort.abort();
        }
    }

    async fn wait(
        &self,
        cancellation: CancellationToken,
    ) -> Result<TaskDelegationOutput, ClientError> {
        let mut receiver = self.result.clone();
        loop {
            if let Some(result) = receiver.borrow().clone() {
                return result.map_err(ClientError::TaskExecution);
            }
            tokio::select! {
                _ = cancellation.cancelled() => {
                    return Ok(cancelled_delegation_output(
                        "delegation retry cancelled while the child remains active",
                    ));
                }
                changed = receiver.changed() => {
                    if changed.is_err() {
                        return Err(ClientError::TaskExecution(
                            "delegation owner stopped before publishing its result".to_owned(),
                        ));
                    }
                }
            }
        }
    }
}

/// Ephemeral capability authorizing exactly one host-created child command.
#[derive(Debug, Clone)]
pub(crate) struct DelegationAdmission {
    context: delegation_policy::DelegationContext,
    parent_session_id: SessionId,
    parent_tool_call_id: ToolCallId,
    child_thread_id: ThreadId,
    actor_id: String,
    task_sha256: String,
    token: String,
}

impl DelegationAdmission {
    pub(crate) fn new(
        context: delegation_policy::DelegationContext,
        parent_session_id: SessionId,
        parent_tool_call_id: ToolCallId,
        child_thread_id: ThreadId,
        actor_id: String,
        task: &str,
    ) -> Self {
        Self {
            context,
            parent_session_id,
            parent_tool_call_id,
            child_thread_id,
            actor_id,
            task_sha256: text_sha256(task),
            token: Uuid::now_v7().to_string(),
        }
    }

    pub(crate) fn token(&self) -> &str {
        &self.token
    }

    pub(crate) fn authorizes(&self, command: &SessionCommand) -> bool {
        if command.actor.kind != ActorKind::Runtime
            || command.actor.id != self.actor_id
            || !command
                .payload
                .get(DELEGATED_TASK_MARKER)
                .and_then(Value::as_bool)
                .unwrap_or(false)
            || command
                .payload
                .get(DELEGATED_ADMISSION_TOKEN_KEY)
                .and_then(Value::as_str)
                != Some(self.token.as_str())
            || command
                .payload
                .get("_delegation_parent_session_id")
                .and_then(Value::as_str)
                != Some(self.parent_session_id.to_string().as_str())
            || command
                .payload
                .get("_delegation_parent_tool_call_id")
                .and_then(Value::as_str)
                != Some(self.parent_tool_call_id.to_string().as_str())
        {
            return false;
        }

        match command.kind {
            SessionCommandKind::Create => {
                command.payload.get("_thread_id").and_then(Value::as_str)
                    == Some(self.child_thread_id.to_string().as_str())
            }
            SessionCommandKind::Prompt => command
                .payload
                .get("prompt")
                .and_then(Value::as_str)
                .is_some_and(|prompt| text_sha256(prompt) == self.task_sha256),
            _ => false,
        }
    }

    pub(crate) fn into_context(self) -> delegation_policy::DelegationContext {
        self.context
    }
}

pub(crate) fn contains_delegation_metadata(payload: &Value) -> bool {
    [
        DELEGATED_TASK_MARKER,
        DELEGATED_ADMISSION_TOKEN_KEY,
        "_delegation_parent_session_id",
        "_delegation_parent_tool_call_id",
        "_delegation",
    ]
    .into_iter()
    .any(|key| payload.get(key).is_some())
}

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
    if host.execution.shutdown.is_cancelled() {
        return Err(ClientError::TaskExecution(
            "runtime host is shutting down".to_owned(),
        ));
    }
    let parent_thread = host
        .storage
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
    let (parent_control, parent_context) = {
        let mut controls = host.execution.task_controls.lock().await;
        let control = controls.get_mut(&request.session_id).ok_or_else(|| {
            ClientError::TaskExecution(
                "delegation requires an active parent agent task control".to_owned(),
            )
        })?;
        let context = control.delegation.clone().ok_or_else(|| {
            ClientError::TaskExecution(
                "delegation parent task is missing its admission context".to_owned(),
            )
        })?;
        (control.clone(), context)
    };

    let active_surface = parent_control.execution.active_execution_surface();
    let overrides = delegation_overrides(
        &parent_control.provider_settings,
        super::NormalizedExecutionMode::from_explicit(active_surface.execution_mode),
        active_surface.tool_profile,
        &request.arguments,
    )?;
    let identity = delegation_identity(
        request,
        task,
        &overrides,
        parent_control.allow_network,
        parent_control.yolo,
    )?;
    let (operation, result_sender) = {
        let mut operations = host.execution.delegation_operations.lock().await;
        match operations.entry(identity.clone()) {
            Entry::Occupied(entry) => (entry.get().clone(), None),
            Entry::Vacant(entry) => {
                if host.execution.shutdown.is_cancelled() {
                    return Err(ClientError::TaskExecution(
                        "runtime host is shutting down".to_owned(),
                    ));
                }
                let operation = Arc::new(DelegationOperation::new(
                    request.session_id,
                    host.execution.shutdown.child_token(),
                ));
                entry.insert(operation.clone());
                (operation, Some(()))
            }
        }
    };
    let Some(()) = result_sender else {
        return operation.wait(cancellation).await;
    };

    // The operation belongs to the host, not to whichever tool future created it. A parent
    // worker can be aborted while the child is between durable creation and task startup; the
    // detached owner must remain alive to observe parent cancellation and finish cleanup.
    let operation_host = host.clone();
    let operation_request = request.clone();
    let operation_cancellation = operation.cancellation();
    let operation_task = task.to_owned();
    let operation_for_cleanup = operation.clone();
    let operation_identity = identity.clone();
    let operation_parent_session_id = operation.parent_session_id;
    let owner = tokio::spawn(async move {
        let result = run_delegated_child(
            &operation_host,
            &operation_request,
            operation_cancellation,
            &operation_task,
            parent_thread.thread_id,
            parent_control,
            parent_context,
            overrides,
            identity,
        )
        .await;
        operation_for_cleanup.complete(&result);
        operation_host
            .cleanup_delegation_operation(
                operation_parent_session_id,
                &operation_identity,
                &operation_for_cleanup,
            )
            .await;
    });
    operation.set_owner_abort(owner.abort_handle());
    operation.wait(cancellation).await
}

#[allow(clippy::too_many_arguments)]
async fn run_delegated_child(
    host: &Arc<RuntimeHost>,
    request: &ToolRequest,
    cancellation: CancellationToken,
    task: &str,
    parent_thread_id: ThreadId,
    parent_control: super::HostedTaskControl,
    parent_context: delegation_policy::DelegationContext,
    overrides: DelegationOverrides,
    identity: String,
) -> Result<TaskDelegationOutput, ClientError> {
    let (requested_tokens, child_generation_config) = child_generation_config(&overrides)?;
    let checkpoint_lock = parent_context.checkpoint_lock();
    let checkpoint_guard = checkpoint_lock.lock().await;
    let child_context = match parent_context.child(
        request.session_id,
        parent_control.task_id,
        parent_thread_id,
        requested_tokens,
        None,
        &cancellation,
    ) {
        Ok(context) => context,
        Err(limit) => {
            return Ok(delegation_limit_output(limit, &parent_context));
        }
    };
    let reservation_recovery = parent_context.recovery_state(Utc::now());
    record_delegation_recovery_checkpoint(
        host,
        request,
        parent_control.task_id,
        &parent_context,
        reservation_recovery,
        "delegation child reservation persisted",
    )
    .await?;
    drop(checkpoint_guard);
    let child_session_id = SessionId(deterministic_uuid(&identity, "session"));
    let child_thread_id = ThreadId(deterministic_uuid(&identity, "thread"));
    let actor = Actor {
        kind: ActorKind::Runtime,
        id: format!("delegate:parent:{}", request.session_id),
    };
    let admission = DelegationAdmission::new(
        child_context.clone(),
        request.session_id,
        request.tool_call_id,
        child_thread_id,
        actor.id.clone(),
        task,
    );
    let admission_token = admission.token().to_owned();
    host.execution
        .delegation_admissions
        .lock()
        .await
        .insert(child_session_id, admission);

    let mut create_payload = json!({
        "_thread_id": child_thread_id,
        DELEGATED_PARENT_THREAD_KEY: parent_thread_id,
        "title": DELEGATED_THREAD_TITLE,
        "prompt": task,
        DELEGATED_TASK_MARKER: true,
        DELEGATED_ADMISSION_TOKEN_KEY: admission_token,
    });
    apply_inherited_execution_surface(
        &mut create_payload,
        overrides.execution_mode,
        overrides.tool_profile,
    )?;
    create_payload["_delegation_parent_session_id"] = json!(request.session_id);
    create_payload["_delegation_parent_tool_call_id"] = json!(request.tool_call_id);
    create_payload["_delegation"] = child_context.metadata();
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
        "max_elapsed_ms": delegated_child_runtime_elapsed_ms(
            child_context.remaining_elapsed_ms(),
        ),
        "_delegation_parent_session_id": request.session_id,
        "_delegation_parent_tool_call_id": request.tool_call_id,
        DELEGATED_ADMISSION_TOKEN_KEY: admission_token,
        "_delegation": child_context.metadata(),
    });
    apply_inherited_execution_surface(
        &mut prompt_payload,
        overrides.execution_mode,
        overrides.tool_profile,
    )?;
    if let Some(profile) = overrides.profile.clone() {
        prompt_payload["provider_profile"] = profile;
    }
    if let Some(model) = overrides.model.clone() {
        prompt_payload["provider_model"] = model;
    }
    prompt_payload["provider_generation_config"] = child_generation_config;
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
    if let Err(error) = host
        .reconcile_replayed_delegated_prompt(child_session_id)
        .await
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
    host.execution
        .delegation_admissions
        .lock()
        .await
        .remove(&child_session_id);

    let child_state = match timeout(
        Duration::from_millis(child_context.remaining_elapsed_ms().max(1)),
        wait_for_child(host, child_session_id, cancellation, &child_context),
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
        .execution
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
    let (actual_tokens, actual_cost_microusd) = child_usage(host, child_session_id).await?;
    persist_delegation_usage_settlement(
        host,
        request,
        parent_control.task_id,
        &parent_context,
        &child_context,
        actual_tokens.unwrap_or(requested_tokens),
        actual_cost_microusd,
    )
    .await?;
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
            "delegation": child_context.metadata(),
            "usage": {
                "total_tokens": actual_tokens,
                "estimated_cost_microusd": actual_cost_microusd,
            },
        }),
    })
}

#[allow(clippy::too_many_arguments)]
async fn persist_delegation_usage_settlement(
    host: &Arc<RuntimeHost>,
    request: &ToolRequest,
    task_id: golutra_core::TaskId,
    canonical_context: &delegation_policy::DelegationContext,
    child_context: &delegation_policy::DelegationContext,
    actual_tokens: u64,
    actual_cost_microusd: Option<u64>,
) -> Result<(), ClientError> {
    let checkpoint_lock = canonical_context.checkpoint_lock();
    let _checkpoint_guard = checkpoint_lock.lock().await;
    let recovery =
        child_context.settlement_recovery_state(Utc::now(), actual_tokens, actual_cost_microusd);
    let persisted = record_delegation_recovery_checkpoint(
        host,
        request,
        task_id,
        canonical_context,
        recovery,
        "delegation child usage settled",
    )
    .await;
    child_context.finish(actual_tokens, actual_cost_microusd);
    persisted
}

async fn record_delegation_recovery_checkpoint(
    host: &Arc<RuntimeHost>,
    request: &ToolRequest,
    current_task_id: golutra_core::TaskId,
    canonical_context: &delegation_policy::DelegationContext,
    recovery: delegation_policy::TimedDelegationRecoveryState,
    summary: &str,
) -> Result<(), ClientError> {
    let session_id = canonical_context.root_session_id;
    let task_id = canonical_context.canonical_task_id(current_task_id);
    let mut event = super::host_event(
        host.next_sequence_no(),
        session_id,
        Some(task_id),
        RuntimeEventType::CheckpointCreated,
        golutra_protocol::RuntimeEventSource::Runtime,
        json!({
            "summary": summary,
            "recovery_kind": "delegation_budget",
            "delegation_recovery": recovery,
        }),
    );
    if session_id == request.session_id {
        event.turn_id = request.turn_id;
    }
    host.record_event(event).await
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
    host.execution
        .delegation_admissions
        .lock()
        .await
        .remove(&child_session_id);
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
        .execution
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
    execution_mode: super::NormalizedExecutionMode,
    tool_profile: AgentToolProfile,
}

fn delegation_token_reservation(overrides: &DelegationOverrides) -> u64 {
    let max_tokens = overrides
        .generation_config
        .as_ref()
        .and_then(|value| serde_json::from_value::<ProviderGenerationConfig>(value.clone()).ok())
        .and_then(|config| config.max_tokens);
    delegation_policy::requested_token_reservation(max_tokens)
}

fn child_generation_config(overrides: &DelegationOverrides) -> Result<(u64, Value), ClientError> {
    let mut config = overrides
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
    config.validate().map_err(ClientError::TaskExecution)?;
    let requested_tokens = delegation_token_reservation(overrides);
    let context_limit = config
        .context_window_size
        .map(|context_window| context_window.saturating_sub(1));
    let max_tokens = context_limit.map_or(requested_tokens, |limit| requested_tokens.min(limit));
    if max_tokens == 0 {
        return Err(ClientError::TaskExecution(
            "provider context window cannot reserve a positive child output budget".to_owned(),
        ));
    }
    config.max_tokens = Some(max_tokens);
    config.validate().map_err(ClientError::TaskExecution)?;
    Ok((max_tokens, serde_json::to_value(config)?))
}

fn delegated_child_runtime_elapsed_ms(local_remaining_elapsed_ms: u64) -> u64 {
    if local_remaining_elapsed_ms <= 1 {
        return 1;
    }
    let completion_grace_ms = local_remaining_elapsed_ms
        .div_ceil(DELEGATED_COMPLETION_GRACE_DIVISOR)
        .min(DELEGATED_COMPLETION_GRACE_MS)
        .min(local_remaining_elapsed_ms - 1);
    local_remaining_elapsed_ms
        .saturating_sub(completion_grace_ms)
        .max(1)
}

fn blocked_delegation_output(
    limit: delegation_policy::DelegationLimit,
    context: &delegation_policy::DelegationContext,
) -> TaskDelegationOutput {
    TaskDelegationOutput {
        status: golutra_core::ToolResultStatus::Blocked,
        summary: limit.message().to_owned(),
        content: String::new(),
        structured_facts: json!({
            "blocked": true,
            "limit_code": limit.code(),
            "reason": limit.message(),
            "delegation": context.metadata(),
        }),
    }
}

fn delegation_limit_output(
    limit: delegation_policy::DelegationLimit,
    context: &delegation_policy::DelegationContext,
) -> TaskDelegationOutput {
    match limit {
        delegation_policy::DelegationLimit::Cancelled => {
            cancelled_delegation_output(limit.message())
        }
        limit => blocked_delegation_output(limit, context),
    }
}

async fn child_usage(
    host: &Arc<RuntimeHost>,
    session_id: SessionId,
) -> Result<(Option<u64>, Option<u64>), ClientError> {
    let events = host
        .storage
        .repositories
        .events
        .load(session_id, None, None)
        .await?;
    Ok(summarize_child_usage(&events))
}

fn summarize_child_usage(events: &[RuntimeEvent]) -> (Option<u64>, Option<u64>) {
    let usage_events = events
        .iter()
        .filter(|event| event.event_type == RuntimeEventType::TokenUsageRecorded)
        .collect::<Vec<_>>();
    if usage_events.is_empty() {
        return (None, None);
    }

    let mut total_tokens = 0_u64;
    let mut tokens_complete = true;
    let mut estimated_cost_microusd = 0_u64;
    let mut cost_complete = true;
    for event in usage_events {
        let Some(record_value) = event.payload.get("record") else {
            tokens_complete = false;
            cost_complete = false;
            continue;
        };
        let Ok(record) = serde_json::from_value::<TokenUsageRecord>(record_value.clone()) else {
            tokens_complete = false;
            cost_complete = false;
            continue;
        };
        let tokens = record.provider_total_tokens;
        if let Some(tokens) = tokens {
            total_tokens = total_tokens.saturating_add(tokens);
        } else {
            tokens_complete = false;
        }

        match record.estimated_cost {
            Some(cost) if cost.is_finite() && !cost.is_sign_negative() => {
                let micros = (cost * 1_000_000.0).round();
                let micros = if micros >= u64::MAX as f64 {
                    u64::MAX
                } else {
                    micros as u64
                };
                estimated_cost_microusd = estimated_cost_microusd.saturating_add(micros);
            }
            _ => cost_complete = false,
        }
    }
    (
        tokens_complete.then_some(total_tokens),
        cost_complete.then_some(estimated_cost_microusd),
    )
}

fn delegation_overrides(
    parent: &super::ProviderTurnSettings,
    execution_mode: super::NormalizedExecutionMode,
    tool_profile: AgentToolProfile,
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
        execution_mode,
        tool_profile,
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
    context: &delegation_policy::DelegationContext,
) -> Result<StateProjection, ClientError> {
    let mut completion = host
        .execution
        .task_controls
        .lock()
        .await
        .get(&session_id)
        .map(|control| control.completion.clone());
    let mut denied_approval = None;
    let mut cancelled_child = false;
    let context_cancellation = context.cancellation();
    loop {
        host.reconcile_replayed_delegated_prompt(session_id).await?;
        let state = host
            .storage
            .repositories
            .projections
            .state(session_id, None)
            .await?;
        if state.task_status.is_terminal() {
            return Ok(state);
        }
        if (cancellation.is_cancelled()
            || context.cancellation().is_cancelled()
            || context.is_expired())
            && !cancelled_child
        {
            cancel_child(host, session_id).await;
            cancelled_child = true;
        }
        if state.task_status == TaskStatus::WaitingApproval
            && denied_approval.as_deref() != state.pending_approval.as_deref()
            && let Some(approval_id) = state
                .pending_approval
                .as_deref()
                .and_then(|value| value.parse::<ApprovalId>().ok())
            && let Some(control) = host
                .execution
                .task_controls
                .lock()
                .await
                .get(&session_id)
                .cloned()
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
                _ = context_cancellation.cancelled(), if !cancelled_child => {}
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
                _ = context_cancellation.cancelled(), if !cancelled_child => {}
                _ = sleep(Duration::from_millis(DELEGATED_WAIT_POLL_MS)) => {}
            }
        }
    }
}

async fn cancel_child(host: &Arc<RuntimeHost>, session_id: SessionId) {
    if let Some(control) = host
        .execution
        .task_controls
        .lock()
        .await
        .get(&session_id)
        .cloned()
    {
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
    let Some(thread) = host.storage.repositories.threads.by_id(thread_id).await? else {
        return Ok(());
    };
    if thread.archived {
        return Ok(());
    }
    archive_child(host, attached_session_id, thread_id, actor).await
}

pub(super) async fn cleanup_cancelled_delegated_task(
    host: &Arc<RuntimeHost>,
    task: &super::HostedAgentTask,
) -> Result<(), ClientError> {
    host.execution
        .process_supervisor
        .terminate_session(task.session_id)
        .await
        .map_err(|error| ClientError::TaskExecution(error.to_string()))?;
    let Some(parent_session_id) = task
        .payload
        .get("_delegation_parent_session_id")
        .and_then(Value::as_str)
        .and_then(|value| value.parse::<SessionId>().ok())
    else {
        return Ok(());
    };
    let Some(thread) = host
        .storage
        .repositories
        .threads
        .by_session(task.session_id)
        .await?
    else {
        return Ok(());
    };
    let actor = Actor {
        kind: ActorKind::Runtime,
        id: format!("delegate:parent:{parent_session_id}"),
    };
    host.archive_thread_after_delegated_parent_cancel(parent_session_id, thread.thread_id, &actor)
        .await?;
    Ok(())
}

async fn child_provider_model(
    host: &Arc<RuntimeHost>,
    session_id: SessionId,
) -> Result<Option<String>, ClientError> {
    let events = host
        .storage
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

fn apply_inherited_execution_surface(
    payload: &mut Value,
    execution_mode: super::NormalizedExecutionMode,
    tool_profile: AgentToolProfile,
) -> Result<(), ClientError> {
    if matches!(execution_mode, super::NormalizedExecutionMode::Legacy) {
        // Omitted mode is the compatibility marker for tasks written before
        // the open/coding surface existed. Do not silently convert those
        // children to the new contract semantics.
        if let Some(object) = payload.as_object_mut() {
            object.remove(super::task_mode::EXECUTION_MODE_KEY);
        }
        payload[super::task_mode::TOOL_PROFILE_KEY] = serde_json::to_value(tool_profile)?;
    } else {
        payload[super::task_mode::EXECUTION_MODE_KEY] =
            Value::String(execution_mode.wire_name().to_owned());
        payload[super::task_mode::TOOL_PROFILE_KEY] = serde_json::to_value(tool_profile)?;
    }
    Ok(())
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
        "execution_mode": overrides.execution_mode.wire_name(),
        "tool_profile": overrides.tool_profile,
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

fn text_sha256(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))
}

#[cfg(test)]
mod tests {
    use golutra_core::{ProviderRequestId, ProviderResponseId, TokenBudgetSnapshotId, TurnId};
    use golutra_protocol::{CommandAck, RuntimeEventSource};
    use golutra_runtime::agent_execution_channel;

    use super::*;
    use crate::CommandClaim;
    use crate::event_codec::host_event;

    #[tokio::test]
    async fn runtime_close_cancels_and_reaps_a_delegation_operation() {
        let host = RuntimeHost::in_memory().await.expect("host");
        let operation = Arc::new(DelegationOperation::new(
            host.default_session_id(),
            host.execution.shutdown.child_token(),
        ));
        let completing_operation = operation.clone();
        let owner = tokio::spawn(async move {
            completing_operation.cancellation().cancelled().await;
            completing_operation
                .complete(&Err(ClientError::TaskExecution("test shutdown".to_owned())));
        });
        operation.set_owner_abort(owner.abort_handle());
        host.execution
            .delegation_operations
            .lock()
            .await
            .insert("shutdown-test".to_owned(), operation.clone());

        host.close().await.expect("host close");
        owner.await.expect("operation owner");
        assert!(operation.is_complete());
        assert!(host.execution.delegation_operations.lock().await.is_empty());
    }

    #[tokio::test]
    async fn normal_completion_before_owner_install_does_not_abort_owner() {
        let operation = DelegationOperation::new(SessionId::new(), CancellationToken::new());
        operation.complete(&Ok(cancelled_delegation_output("test completion")));
        let owner = tokio::spawn(std::future::pending::<()>());

        operation.set_owner_abort(owner.abort_handle());
        tokio::task::yield_now().await;

        assert!(!owner.is_finished());
        owner.abort();
        assert!(owner.await.expect_err("owner abort").is_cancelled());
    }

    #[tokio::test]
    async fn force_stop_before_owner_install_still_aborts_owner() {
        let operation = DelegationOperation::new(SessionId::new(), CancellationToken::new());
        operation.force_stop();
        let owner = tokio::spawn(std::future::pending::<()>());

        operation.set_owner_abort(owner.abort_handle());

        assert!(owner.await.expect_err("forced owner abort").is_cancelled());
        let error = operation
            .wait(CancellationToken::new())
            .await
            .expect_err("force stop result");
        assert!(error.to_string().contains("runtime shutdown"));
    }

    #[tokio::test]
    async fn parent_cleanup_between_delegation_completion_and_map_cleanup_leaves_no_entry() {
        let host = RuntimeHost::in_memory().await.expect("host");
        let parent_session_id = host.default_session_id();
        let parent_task_id = golutra_core::TaskId::new();
        let operation = Arc::new(DelegationOperation::new(
            parent_session_id,
            host.execution.shutdown.child_token(),
        ));
        let identity = "completion-cleanup-race".to_owned();
        host.execution
            .delegation_operations
            .lock()
            .await
            .insert(identity.clone(), operation.clone());

        let (parent_execution, _parent_execution_control) = agent_execution_channel(1);
        let parent_worker = tokio::spawn(std::future::pending::<()>());
        let (_completion_sender, completion) = watch::channel(false);
        host.execution.task_controls.lock().await.insert(
            parent_session_id,
            crate::HostedTaskControl {
                task_id: parent_task_id,
                allow_network: false,
                yolo: false,
                provider_settings: crate::ProviderTurnSettings::default(),
                execution: parent_execution,
                abort_handle: parent_worker.abort_handle(),
                completion,
                delegation: None,
                _session_lease: None,
            },
        );

        let mut controls = host.execution.task_controls.lock().await;
        let finishing_host = host.clone();
        let finishing_operation = operation.clone();
        let finish = tokio::spawn(async move {
            finishing_operation.complete(&Ok(cancelled_delegation_output("test completion")));
            finishing_host
                .cleanup_delegation_operation(parent_session_id, &identity, &finishing_operation)
                .await;
        });
        timeout(Duration::from_secs(1), async {
            while !operation.is_complete() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("delegation completion");
        controls.remove(&parent_session_id);
        drop(controls);

        finish.await.expect("operation cleanup");
        assert!(host.execution.delegation_operations.lock().await.is_empty());
        parent_worker.abort();
        assert!(
            parent_worker
                .await
                .expect_err("parent worker abort")
                .is_cancelled()
        );
        host.close().await.expect("host close");
    }

    #[tokio::test]
    async fn replayed_delegated_prompt_reconciles_orphan_and_archives_child() {
        let host = RuntimeHost::in_memory().await.expect("host");
        let parent_session_id = host.default_session_id();
        host.upsert_current_thread(parent_session_id, &json!({"prompt": "parent"}))
            .await
            .expect("parent thread");
        let parent_thread_id = host
            .storage
            .repositories
            .threads
            .by_session(parent_session_id)
            .await
            .expect("parent lookup")
            .expect("parent thread")
            .thread_id;

        let parent_task_id = golutra_core::TaskId::new();
        let (parent_execution, _parent_execution_control) = agent_execution_channel(1);
        let parent_cancellation = parent_execution.cancellation_token();
        let parent_context = delegation_policy::DelegationContext::root(
            parent_session_id,
            Some(30_000),
            Some(4_096),
            None,
            parent_cancellation,
        );
        let parent_worker = tokio::spawn(std::future::pending::<()>());
        let (parent_completion_sender, parent_completion) = watch::channel(false);
        host.execution.task_controls.lock().await.insert(
            parent_session_id,
            crate::HostedTaskControl {
                task_id: parent_task_id,
                allow_network: false,
                yolo: false,
                provider_settings: crate::ProviderTurnSettings::default(),
                execution: parent_execution,
                abort_handle: parent_worker.abort_handle(),
                completion: parent_completion,
                delegation: Some(parent_context),
                _session_lease: None,
            },
        );

        let task = "recover this delegated child";
        let request = ToolRequest {
            tool_call_id: ToolCallId::new(),
            provider_tool_call_id: Some("replayed-delegated-child".to_owned()),
            session_id: parent_session_id,
            turn_id: Some(TurnId::new()),
            tool_name: "subagent".to_owned(),
            arguments: json!({"task": task}),
        };
        let overrides = delegation_overrides(
            &crate::ProviderTurnSettings::default(),
            crate::NormalizedExecutionMode::Legacy,
            AgentToolProfile::Coding,
            &request.arguments,
        )
        .expect("delegation overrides");
        let identity = delegation_identity(&request, task, &overrides, false, false)
            .expect("delegation identity");
        let child_session_id = SessionId(deterministic_uuid(&identity, "session"));
        let child_thread_id = ThreadId(deterministic_uuid(&identity, "thread"));
        let actor = Actor {
            kind: ActorKind::Runtime,
            id: format!("delegate:parent:{parent_session_id}"),
        };

        host.upsert_current_thread(
            child_session_id,
            &json!({
                "_thread_id": child_thread_id,
                DELEGATED_PARENT_THREAD_KEY: parent_thread_id,
                "prompt": task,
                DELEGATED_TASK_MARKER: true,
            }),
        )
        .await
        .expect("child thread");
        let task_id = golutra_core::TaskId::new();
        let turn_id = TurnId::new();
        let prompt_payload = json!({
            "prompt": task,
            DELEGATED_TASK_MARKER: true,
            "_delegation_parent_session_id": parent_session_id,
            "_delegation_parent_tool_call_id": request.tool_call_id,
        });
        host.record_event({
            let mut event = host_event(
                host.next_sequence_no(),
                child_session_id,
                Some(task_id),
                RuntimeEventType::TaskCreated,
                RuntimeEventSource::Runtime,
                json!({
                    "summary": "delegated child task started before runtime exit",
                    "payload": prompt_payload,
                }),
            );
            event.turn_id = Some(turn_id);
            event
        })
        .await
        .expect("child task event");

        for kind in [SessionCommandKind::Create, SessionCommandKind::Prompt] {
            let idempotency_key = format!(
                "{identity}:{}",
                match kind {
                    SessionCommandKind::Create => "create",
                    SessionCommandKind::Prompt => "prompt",
                    _ => unreachable!(),
                }
            );
            let payload = if kind == SessionCommandKind::Create {
                json!({
                    "_thread_id": child_thread_id,
                    DELEGATED_PARENT_THREAD_KEY: parent_thread_id,
                    "prompt": task,
                    DELEGATED_TASK_MARKER: true,
                })
            } else {
                json!({
                    "prompt": task,
                    DELEGATED_TASK_MARKER: true,
                    "_delegation_parent_session_id": parent_session_id,
                    "_delegation_parent_tool_call_id": request.tool_call_id,
                })
            };
            let command = internal_command(
                child_session_id,
                kind,
                idempotency_key.clone(),
                actor.clone(),
                payload,
            );
            let provisional = CommandAck {
                command_id: command.command_id,
                accepted: true,
                reason: Some("command accepted for processing".to_owned()),
            };
            let scoped_key = host.scoped_idempotency_key(&idempotency_key);
            let claim = host
                .claim_command_journal(
                    &scoped_key,
                    command.command_id,
                    &provisional,
                    host_event(
                        0,
                        child_session_id,
                        None,
                        RuntimeEventType::CommandReceived,
                        RuntimeEventSource::Runtime,
                        json!({"command_id": command.command_id}),
                    ),
                )
                .await
                .expect("command claim");
            assert!(matches!(claim, CommandClaim::Claimed { .. }));
            let ack = CommandAck {
                command_id: command.command_id,
                accepted: true,
                reason: Some(format!("recovered {kind:?}")),
            };
            host.complete_command_journal(
                &scoped_key,
                command.command_id,
                &ack,
                host_event(
                    0,
                    child_session_id,
                    None,
                    RuntimeEventType::CommandCompleted,
                    RuntimeEventSource::Runtime,
                    json!({"command_id": command.command_id, "accepted": true}),
                ),
            )
            .await
            .expect("command completion");
        }

        let output = timeout(
            Duration::from_secs(1),
            delegate_task(&host, &request, CancellationToken::new()),
        )
        .await
        .expect("orphan recovery must not wait for delegation timeout")
        .expect("delegation output");
        assert_eq!(output.status, golutra_core::ToolResultStatus::Cancelled);
        assert_eq!(
            output.structured_facts["child_status"],
            json!(TaskStatus::Interrupted)
        );

        let state = host
            .storage
            .repositories
            .projections
            .state(child_session_id, None)
            .await
            .expect("child state");
        assert_eq!(state.task_status, TaskStatus::Interrupted);
        let events = host
            .storage
            .repositories
            .events
            .load(child_session_id, Some(task_id), None)
            .await
            .expect("child events");
        assert_eq!(
            events
                .iter()
                .filter(|event| {
                    matches!(
                        event.event_type,
                        RuntimeEventType::TaskInterrupted | RuntimeEventType::TaskUncertain
                    )
                })
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .find(|event| event.event_type == RuntimeEventType::TaskInterrupted)
                .and_then(|event| event.payload.get("recovery"))
                .and_then(Value::as_str),
            Some("delegated_prompt_replay")
        );
        assert!(
            host.storage
                .repositories
                .threads
                .by_id(child_thread_id)
                .await
                .expect("child lookup")
                .expect("child thread")
                .archived
        );

        host.execution
            .task_controls
            .lock()
            .await
            .remove(&parent_session_id);
        drop(parent_completion_sender);
        parent_worker.abort();
    }

    fn usage_record(
        provider_total_tokens: Option<u64>,
        input_tokens: Option<u64>,
        output_tokens: Option<u64>,
        estimated_cost: Option<f64>,
    ) -> Value {
        serde_json::to_value(TokenUsageRecord {
            task_id: golutra_core::TaskId::new(),
            turn_id: TurnId::new(),
            provider_id: "provider".to_owned(),
            model_id: "model".to_owned(),
            request_event_id: ProviderRequestId::new(),
            response_event_id: ProviderResponseId::new(),
            input_tokens,
            output_tokens,
            reasoning_tokens: None,
            estimated_cost,
            budget_snapshot_ref: TokenBudgetSnapshotId::new(),
            attribution_ref: None,
            usage_source: "provider".to_owned(),
            cache_read_tokens: None,
            cache_write_tokens: None,
            non_cached_input_tokens: None,
            tool_schema_tokens_estimated: None,
            tool_result_tokens_estimated: None,
            tool_estimated_tokens: None,
            provider_total_tokens,
            usage_complete: false,
            session_id: None,
            cache_identity: None,
        })
        .expect("usage record")
    }

    fn usage_event(session_id: SessionId, record: Option<Value>) -> RuntimeEvent {
        host_event(
            0,
            session_id,
            None,
            RuntimeEventType::TokenUsageRecorded,
            RuntimeEventSource::Provider,
            record.map_or_else(
                || json!({"summary": "missing usage record"}),
                |record| json!({"record": record}),
            ),
        )
    }

    #[test]
    fn child_usage_sums_only_complete_observations() {
        let session_id = SessionId::new();
        let events = [
            usage_event(
                session_id,
                Some(usage_record(Some(10), None, None, Some(0.000_003))),
            ),
            usage_event(
                session_id,
                Some(usage_record(Some(10), Some(4), Some(6), Some(0.000_007))),
            ),
        ];

        assert_eq!(summarize_child_usage(&events), (Some(20), Some(10)));
    }

    #[test]
    fn child_usage_keeps_provider_total_unknown_when_only_components_exist() {
        let session_id = SessionId::new();
        let events = [usage_event(
            session_id,
            Some(usage_record(None, Some(4), Some(6), Some(0.000_007))),
        )];

        assert_eq!(summarize_child_usage(&events), (None, Some(7)));
    }

    #[test]
    fn child_usage_tracks_token_and_cost_completeness_independently() {
        let session_id = SessionId::new();
        let missing_cost = [usage_event(
            session_id,
            Some(usage_record(Some(10), None, None, None)),
        )];
        let missing_tokens = [usage_event(
            session_id,
            Some(usage_record(None, None, None, Some(0.000_004))),
        )];

        assert_eq!(summarize_child_usage(&missing_cost), (Some(10), None));
        assert_eq!(summarize_child_usage(&missing_tokens), (None, Some(4)));
    }

    #[test]
    fn malformed_or_missing_usage_makes_the_corresponding_totals_unknown() {
        let session_id = SessionId::new();
        let events = [
            usage_event(
                session_id,
                Some(usage_record(Some(10), None, None, Some(0.000_003))),
            ),
            usage_event(session_id, Some(json!({"total_tokens": 5}))),
            usage_event(session_id, None),
        ];

        assert_eq!(summarize_child_usage(&events), (None, None));
    }

    #[test]
    fn negative_usage_cost_is_unknown_instead_of_releasing_the_reservation() {
        let session_id = SessionId::new();
        let events = [usage_event(
            session_id,
            Some(usage_record(Some(10), None, None, Some(-0.5))),
        )];

        assert_eq!(summarize_child_usage(&events), (Some(10), None));
    }

    #[test]
    fn child_generation_config_always_matches_the_reserved_output_budget() {
        let missing_limit = DelegationOverrides {
            profile: None,
            model: None,
            generation_config: None,
            reasoning_effort: None,
            execution_mode: crate::NormalizedExecutionMode::Legacy,
            tool_profile: AgentToolProfile::Full,
        };
        let (default_budget, default_config) =
            child_generation_config(&missing_limit).expect("default child config");
        assert_eq!(
            default_budget,
            delegation_policy::DEFAULT_DELEGATED_CHILD_TOKEN_RESERVATION
        );
        assert_eq!(
            default_config["max_tokens"],
            json!(delegation_policy::DEFAULT_DELEGATED_CHILD_TOKEN_RESERVATION)
        );

        let bounded = DelegationOverrides {
            generation_config: Some(json!({"context_window_size": 2_048})),
            ..missing_limit
        };
        let (bounded_budget, bounded_config) =
            child_generation_config(&bounded).expect("bounded child config");
        assert_eq!(bounded_budget, 2_047);
        assert_eq!(bounded_config["max_tokens"], json!(2_047));
    }

    #[test]
    fn child_runtime_budget_reserves_bounded_completion_grace() {
        assert_eq!(delegated_child_runtime_elapsed_ms(100_000), 95_000);
        assert_eq!(delegated_child_runtime_elapsed_ms(10_000), 9_500);
        assert_eq!(delegated_child_runtime_elapsed_ms(20), 19);
        assert_eq!(delegated_child_runtime_elapsed_ms(2), 1);
        assert_eq!(delegated_child_runtime_elapsed_ms(1), 1);
        assert_eq!(delegated_child_runtime_elapsed_ms(0), 1);
    }

    #[test]
    fn legacy_delegation_omits_only_the_mode_and_preserves_the_selected_profile() {
        let mut payload = json!({"execution_mode": "open", "tool_profile": "full"});

        apply_inherited_execution_surface(
            &mut payload,
            crate::NormalizedExecutionMode::Legacy,
            AgentToolProfile::Coding,
        )
        .expect("legacy execution surface");

        assert!(payload.get("execution_mode").is_none());
        assert_eq!(payload["tool_profile"], "coding");
    }

    #[tokio::test]
    async fn nested_delegation_checkpoints_use_the_root_recovery_stream() {
        let host = RuntimeHost::in_memory().await.expect("host");
        let root_session_id = host.default_session_id();
        let root_task_id = golutra_core::TaskId::new();
        let child_session_id = SessionId::new();
        let child_task_id = golutra_core::TaskId::new();
        let cancellation = CancellationToken::new();
        let root = delegation_policy::DelegationContext::root(
            root_session_id,
            Some(10_000),
            Some(1_024),
            None,
            cancellation.clone(),
        );
        let child = root
            .child(
                root_session_id,
                root_task_id,
                ThreadId::new(),
                1_024,
                None,
                &cancellation,
            )
            .expect("child context");
        let request = ToolRequest {
            tool_call_id: ToolCallId::new(),
            provider_tool_call_id: None,
            session_id: child_session_id,
            turn_id: Some(TurnId::new()),
            tool_name: "subagent".to_owned(),
            arguments: json!({"task": "nested"}),
        };

        let checkpoint_lock = child.checkpoint_lock();
        let checkpoint_guard = checkpoint_lock.lock().await;
        let recovery = child.recovery_state(Utc::now());
        record_delegation_recovery_checkpoint(
            &host,
            &request,
            child_task_id,
            &child,
            recovery,
            "nested reservation",
        )
        .await
        .expect("root checkpoint");
        drop(checkpoint_guard);
        persist_delegation_usage_settlement(
            &host,
            &request,
            root_task_id,
            &root,
            &child,
            2_000,
            None,
        )
        .await
        .expect("parent settlement");

        let root_events = host
            .storage
            .store
            .load_events(root_session_id, Some(root_task_id), None)
            .await
            .expect("root events");
        let child_events = host
            .storage
            .store
            .load_events(child_session_id, Some(child_task_id), None)
            .await
            .expect("child events");
        let checkpoints = root_events
            .iter()
            .filter(|event| event.event_type == RuntimeEventType::CheckpointCreated)
            .collect::<Vec<_>>();

        assert_eq!(checkpoints.len(), 2);
        assert!(child_events.is_empty());
        assert!(checkpoints.iter().all(|event| event.turn_id.is_none()));
        assert_eq!(
            checkpoints[0].payload["delegation_recovery"]["state"]["started_children"],
            1
        );
        assert_eq!(
            checkpoints[1].payload["delegation_recovery"]["state"]["spent_tokens"],
            2_000
        );
    }

    #[test]
    fn cancelled_admission_is_reported_as_cancelled_instead_of_blocked() {
        let cancellation = CancellationToken::new();
        let context = delegation_policy::DelegationContext::root(
            SessionId::new(),
            Some(10_000),
            Some(1_024),
            None,
            cancellation.clone(),
        );
        cancellation.cancel();

        let limit = context
            .child(
                SessionId::new(),
                golutra_core::TaskId::new(),
                ThreadId::new(),
                1_024,
                None,
                &cancellation,
            )
            .expect_err("cancelled admission");
        let output = delegation_limit_output(limit, &context);

        assert_eq!(
            output.status,
            golutra_core::ToolResultStatus::Cancelled,
            "cancellation must remain distinguishable from a policy block"
        );
        assert_eq!(output.structured_facts["cancelled"], true);
    }
}
