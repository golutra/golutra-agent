use super::*;
use futures_util::{StreamExt, stream::FuturesUnordered};
use golutra_core::{TaskContract, ToolResultStatus, WorkspaceChangeRequirement};
use golutra_store::ThreadRecord;
use std::collections::HashMap;

const MAX_WAIT_MS: u64 = 60_000;

pub(super) fn page_output(
    mut output: TaskDelegationOutput,
    arguments: &Value,
) -> TaskDelegationOutput {
    if output.content.is_empty() {
        return output;
    }
    let total = output.content.chars().count();
    let offset = arguments
        .get("offset")
        .and_then(Value::as_u64)
        .unwrap_or(0)
        .min(total as u64) as usize;
    let limit = arguments
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(1024)
        .clamp(1, 2048) as usize;
    let end = offset.saturating_add(limit).min(total);
    output.content = output.content.chars().skip(offset).take(limit).collect();
    output.structured_facts["child_result_offset"] = json!(offset);
    output.structured_facts["child_result_total_chars"] = json!(total);
    output.structured_facts["child_result_has_more"] = json!(end < total);
    if end < total {
        output.structured_facts["child_result_next_offset"] = json!(end);
    }
    output
}

pub(super) fn action(arguments: &Value) -> Result<&str, ClientError> {
    match arguments
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or("spawn")
    {
        action @ ("spawn" | "status" | "wait" | "send_input" | "resume" | "cancel") => Ok(action),
        _ => Err(ClientError::TaskExecution(
            "invalid subagent action".to_owned(),
        )),
    }
}

pub(super) fn validate_start(arguments: &Value) -> Result<(), ClientError> {
    if !matches!(
        arguments.get("context").and_then(Value::as_str),
        None | Some("independent" | "fork")
    ) {
        return Err(ClientError::TaskExecution(
            "context must be independent or fork".to_owned(),
        ));
    }
    if action(arguments)? == "resume"
        && arguments
            .get("context")
            .is_some_and(|value| !value.is_null())
    {
        return Err(ClientError::TaskExecution(
            "resume keeps the child's own history; context is spawn-only".to_owned(),
        ));
    }
    if !matches!(
        arguments.get("agent_type").and_then(Value::as_str),
        None | Some("general" | "explore")
    ) {
        return Err(ClientError::TaskExecution(
            "agent_type must be general or explore".to_owned(),
        ));
    }
    if action(arguments)? == "spawn"
        && arguments
            .get("child_session_id")
            .is_some_and(|value| !value.is_null())
    {
        return Err(ClientError::TaskExecution(
            "use resume to continue an existing child".to_owned(),
        ));
    }
    Ok(())
}

pub(super) fn operation_session(operation: &DelegationOperation) -> Option<SessionId> {
    operation
        .lifecycle
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .child_session_id
}

pub(super) fn has_active_operation_for(
    operations: &HashMap<String, Arc<DelegationOperation>>,
    session: SessionId,
) -> bool {
    operations.values().any(|operation| {
        operation_session(operation) == Some(session) && !operation.execution_finished()
    })
}

async fn owned_operation(
    host: &RuntimeHost,
    parent: SessionId,
    child: SessionId,
) -> Option<Arc<DelegationOperation>> {
    let operations = host.execution.delegation_operations.lock().await;
    operations
        .values()
        .filter(|operation| {
            operation.belongs_to(parent) && operation_session(operation) == Some(child)
        })
        .max_by_key(|operation| (!operation.execution_finished(), operation.created_at))
        .cloned()
}

fn child_id(request: &ToolRequest) -> Result<SessionId, ClientError> {
    request
        .arguments
        .get("child_session_id")
        .and_then(Value::as_str)
        .and_then(|id| id.parse().ok())
        .ok_or_else(|| {
            ClientError::TaskExecution(
                "child_session_id must be a returned child handle".to_owned(),
            )
        })
}

async fn owned_thread(
    host: &RuntimeHost,
    parent: SessionId,
    child: SessionId,
) -> Result<ThreadRecord, ClientError> {
    let repositories = &host.storage.repositories;
    let parent = repositories
        .threads
        .by_session(parent)
        .await?
        .ok_or_else(|| ClientError::InvalidSession("parent thread is missing".to_owned()))?;
    let thread = repositories
        .threads
        .by_session(child)
        .await?
        .ok_or_else(|| ClientError::InvalidSession("child thread is missing".to_owned()))?;
    host.ensure_thread_in_workspace(&parent)?;
    host.ensure_thread_in_workspace(&thread)?;
    host.ensure_thread_not_removed(&thread)?;
    if thread.parent_thread_id != Some(parent.thread_id) {
        return Err(ClientError::InvalidSession(
            "child does not belong to this parent".to_owned(),
        ));
    }
    Ok(thread)
}

pub(super) async fn resume_target(
    host: &RuntimeHost,
    request: &ToolRequest,
) -> Result<ThreadRecord, ClientError> {
    let child = child_id(request)?;
    let thread = owned_thread(host, request.session_id, child).await?;
    Ok(thread)
}

pub(super) async fn ensure_resumable(
    host: &RuntimeHost,
    child: SessionId,
) -> Result<(), ClientError> {
    let state = host
        .storage
        .repositories
        .projections
        .state(child, None)
        .await?;
    if state.task_status.requires_reconciliation() {
        return Err(ClientError::TaskExecution(
            "child requires reconciliation; do not resubmit uncertain work".to_owned(),
        ));
    }
    if !state.task_status.is_terminal() {
        return Err(ClientError::TaskExecution(
            "child is active; use send_input or wait".to_owned(),
        ));
    }
    Ok(())
}

pub(super) async fn apply_agent_type(
    payload: &mut Value,
    request: &ToolRequest,
    resumed: Option<&ThreadRecord>,
    host: &RuntimeHost,
) -> Result<(), ClientError> {
    let mut explore =
        request.arguments.get("agent_type").and_then(Value::as_str) == Some("explore");
    if let Some(thread) = resumed {
        let events = host
            .storage
            .repositories
            .events
            .load(thread.session_id, None, None)
            .await?;
        if let Some(previous) = events
            .iter()
            .rev()
            .find(|event| event.event_type == RuntimeEventType::TaskCreated)
            .and_then(|event| event.payload.pointer("/payload/task_contract"))
        {
            let contract: TaskContract = serde_json::from_value(previous.clone())?;
            explore |= contract.workspace_change == WorkspaceChangeRequirement::Forbidden;
        }
    }
    if explore {
        payload["task_contract"] = serde_json::to_value(TaskContract {
            workspace_change: WorkspaceChangeRequirement::Forbidden,
            max_correction_rounds: 0,
            ..TaskContract::default()
        })?;
        payload["verify_on_change"] = json!("off");
        payload["tool_profile"] = json!("coding");
    }
    Ok(())
}

fn running_output(child: SessionId, expired: bool) -> TaskDelegationOutput {
    TaskDelegationOutput {
        status: ToolResultStatus::Ok,
        summary: "subagent is running; use wait for its result".to_owned(),
        content: String::new(),
        structured_facts: json!({"child_session_id": child, "child_status": "running", "child_terminal": false, "completed": false, "wait_expired": expired}),
    }
}

fn running_operation_output(
    operation: &DelegationOperation,
    child: SessionId,
    expired: bool,
) -> TaskDelegationOutput {
    let mut output = running_output(child, expired);
    output.structured_facts["child_task_id"] = json!(
        operation
            .lifecycle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .child_task_id
    );
    output
}

pub(super) async fn wait_after_start(
    operation: &DelegationOperation,
    arguments: &Value,
    cancellation: CancellationToken,
) -> Result<TaskDelegationOutput, ClientError> {
    let wait_ms = if arguments.get("run_in_background").and_then(Value::as_bool) == Some(true) {
        Some(0)
    } else {
        arguments.get("wait_ms").and_then(Value::as_u64)
    };
    match wait_ms {
        Some(ms) => {
            operation.wait_until_ready(cancellation.clone()).await?;
            bounded_wait(operation, ms, cancellation).await
        }
        None => operation.wait(cancellation).await,
    }
}

async fn bounded_wait(
    operation: &DelegationOperation,
    ms: u64,
    cancellation: CancellationToken,
) -> Result<TaskDelegationOutput, ClientError> {
    if let Some(result) = operation.result.borrow().clone() {
        return result.map_err(ClientError::TaskExecution);
    }
    let child = operation_session(operation)
        .ok_or_else(|| ClientError::TaskExecution("child handle is unavailable".to_owned()))?;
    if ms == 0 {
        return Ok(running_operation_output(operation, child, false));
    }
    match timeout(
        Duration::from_millis(ms.min(MAX_WAIT_MS)),
        operation.wait(cancellation),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => Ok(running_operation_output(operation, child, true)),
    }
}

pub(super) async fn dispatch(
    host: &Arc<RuntimeHost>,
    request: &ToolRequest,
    cancellation: CancellationToken,
    action: &str,
) -> Result<TaskDelegationOutput, ClientError> {
    if request.arguments.get("child_session_ids").is_some() {
        if action != "wait" || request.arguments.get("child_session_id").is_some() {
            return Err(ClientError::TaskExecution("child_session_ids is only valid for wait and cannot be combined with child_session_id".to_owned()));
        }
        return wait_many(host, request, cancellation).await;
    }
    dispatch_single(host, request, cancellation, action).await
}

async fn wait_many(
    host: &Arc<RuntimeHost>,
    request: &ToolRequest,
    cancellation: CancellationToken,
) -> Result<TaskDelegationOutput, ClientError> {
    let values = request.arguments["child_session_ids"]
        .as_array()
        .filter(|values| !values.is_empty())
        .ok_or_else(|| {
            ClientError::TaskExecution("child_session_ids must be a nonempty array".to_owned())
        })?;
    let wait_all = match request.arguments.get("wait_mode").and_then(Value::as_str) {
        None | Some("any") => false,
        Some("all") => true,
        _ => {
            return Err(ClientError::TaskExecution(
                "wait_mode must be any or all".to_owned(),
            ));
        }
    };
    let mut targets = std::collections::BTreeSet::new();
    for value in values {
        let child = value
            .as_str()
            .and_then(|id| id.parse::<SessionId>().ok())
            .ok_or_else(|| ClientError::TaskExecution("invalid child session handle".to_owned()))?;
        if owned_operation(host, request.session_id, child)
            .await
            .is_none()
        {
            owned_thread(host, request.session_id, child).await?;
        }
        targets.insert(child);
    }
    let mut waits = FuturesUnordered::new();
    for child in &targets {
        let mut single = request.clone();
        single
            .arguments
            .as_object_mut()
            .unwrap()
            .remove("child_session_ids");
        single.arguments["child_session_id"] = json!(child);
        let cancellation = cancellation.clone();
        waits.push(async move { dispatch_single(host, &single, cancellation, "wait").await });
    }
    let mut wait_expired = false;
    while let Some(result) = waits.next().await {
        if cancellation.is_cancelled() {
            return Err(ClientError::TaskCancelled);
        }
        wait_expired |= result.as_ref().is_ok_and(|output| {
            output
                .structured_facts
                .get("wait_expired")
                .and_then(Value::as_bool)
                == Some(true)
        });
        if !wait_all {
            break;
        }
    }
    drop(waits);
    let mut results = Vec::new();
    let mut pending = Vec::new();
    for child in targets {
        let mut single = request.clone();
        single.arguments["child_session_id"] = json!(child);
        match dispatch_single(host, &single, cancellation.clone(), "status").await {
            Ok(output) => {
                let output = page_output(output, &request.arguments);
                if output
                    .structured_facts
                    .get("child_terminal")
                    .or_else(|| output.structured_facts.get("completed"))
                    .and_then(Value::as_bool)
                    != Some(true)
                {
                    pending.push(child);
                }
                results.push(json!({"child_session_id": child, "summary": output.summary, "content": output.content, "facts": output.structured_facts}));
            }
            Err(error) => results.push(json!({"child_session_id":child,"error":error.to_string()})),
        }
    }
    let completed = !results.is_empty()
        && results
            .iter()
            .all(|result| result["facts"]["completed"] == true);
    let terminal = !results.is_empty()
        && results
            .iter()
            .all(|result| result["facts"]["child_terminal"] == true);
    Ok(TaskDelegationOutput {
        status: ToolResultStatus::Ok,
        summary: format!(
            "{} subagent results; {} still running",
            results.len(),
            pending.len()
        ),
        content: String::new(),
        structured_facts: json!({"child_results":results,"child_pending_ids":pending,"completed":completed,"child_terminal":terminal,"wait_expired":!pending.is_empty() && wait_expired}),
    })
}

async fn dispatch_single(
    host: &Arc<RuntimeHost>,
    request: &ToolRequest,
    cancellation: CancellationToken,
    action: &str,
) -> Result<TaskDelegationOutput, ClientError> {
    let child = child_id(request)?;
    let operation = owned_operation(host, request.session_id, child).await;
    if operation.is_none() {
        owned_thread(host, request.session_id, child).await?;
    }
    match action {
        "send_input" => {
            let operation = operation.ok_or_else(|| {
                ClientError::TaskExecution(
                    "child has no active owner; use status before resume".to_owned(),
                )
            })?;
            let _guard = operation.input_lock.lock().await;
            return send_input_once(host, request, child, &operation).await;
        }
        "cancel" => {
            if let Some(operation) = &operation {
                operation.cancel();
            }
            cancel_child(host, child).await;
        }
        "wait" => {
            if let Some(operation) = &operation {
                return bounded_wait(
                    operation,
                    request
                        .arguments
                        .get("wait_ms")
                        .and_then(Value::as_u64)
                        .unwrap_or(MAX_WAIT_MS),
                    cancellation,
                )
                .await;
            }
        }
        _ => {}
    }
    if let Some(operation) = &operation {
        if let Some(result) = operation.result.borrow().clone() {
            return result.map_err(ClientError::TaskExecution);
        }
        let mut output = running_output(child, false);
        if action == "cancel" {
            output.summary = "subagent cancellation requested".to_owned();
            output.structured_facts["child_status"] = json!("aborting");
        } else if host
            .storage
            .repositories
            .threads
            .by_session(child)
            .await?
            .is_some()
        {
            let state = host
                .storage
                .repositories
                .projections
                .state(child, None)
                .await?;
            if state.active_task_id.is_some() && !state.task_status.is_terminal() {
                return state_output(host, &state).await;
            }
        }
        return Ok(output);
    }
    host.reconcile_replayed_delegated_prompt(child).await?;
    let state = host
        .storage
        .repositories
        .projections
        .state(child, None)
        .await?;
    state_output(host, &state).await
}

async fn send_input_once(
    host: &Arc<RuntimeHost>,
    request: &ToolRequest,
    child: SessionId,
    operation: &DelegationOperation,
) -> Result<TaskDelegationOutput, ClientError> {
    let key = format!(
        "{:?}:{}",
        request.turn_id,
        request
            .provider_tool_call_id
            .as_deref()
            .map(str::to_owned)
            .unwrap_or_else(|| request.tool_call_id.to_string())
    );
    let digest = text_sha256(&request.arguments.to_string());
    if let Some((previous_digest, result)) = operation
        .accepted_inputs
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&key)
    {
        return if previous_digest == &digest {
            Ok(result.clone())
        } else {
            Err(ClientError::TaskExecution(
                "repeated input identity has different arguments".to_owned(),
            ))
        };
    }
    let result = send_input(host, request, child).await?;
    operation
        .accepted_inputs
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(key, (digest, result.clone()));
    Ok(result)
}

async fn send_input(
    host: &Arc<RuntimeHost>,
    request: &ToolRequest,
    child: SessionId,
) -> Result<TaskDelegationOutput, ClientError> {
    let thread = owned_thread(host, request.session_id, child).await?;
    let task = request
        .arguments
        .get("task")
        .and_then(Value::as_str)
        .filter(|task| !task.trim().is_empty())
        .ok_or_else(|| ClientError::TaskExecution("send_input requires task text".to_owned()))?;
    let control = host
        .execution
        .task_controls
        .lock()
        .await
        .get(&child)
        .cloned()
        .ok_or_else(|| {
            ClientError::TaskExecution("child is no longer active; use resume".to_owned())
        })?;
    let context = control.delegation.ok_or_else(|| {
        ClientError::TaskExecution("child admission context is missing".to_owned())
    })?;
    let actor = Actor {
        kind: ActorKind::Runtime,
        id: format!("delegate:parent:{}", request.session_id),
    };
    let admission = DelegationAdmission::new(
        context,
        request.session_id,
        request.tool_call_id,
        thread.thread_id,
        actor.id.clone(),
        task,
    );
    let payload = json!({"prompt": task, "steer": true, DELEGATED_TASK_MARKER: true,
        DELEGATED_ADMISSION_TOKEN_KEY: admission.token(), "_delegation_parent_session_id": request.session_id,
        "_delegation_parent_tool_call_id": request.tool_call_id});
    host.execution
        .delegation_admissions
        .lock()
        .await
        .insert(child, admission);
    let ack = host
        .clone()
        .handle_command(internal_command(
            child,
            SessionCommandKind::Prompt,
            format!("delegate-input:{}:{}", child, request.tool_call_id),
            actor,
            payload,
        ))
        .await;
    host.execution
        .delegation_admissions
        .lock()
        .await
        .remove(&child);
    let ack = ack?;
    if !ack.accepted {
        return Err(ClientError::TaskExecution(
            ack.reason
                .unwrap_or_else(|| "child rejected input".to_owned()),
        ));
    }
    let mut output = running_output(child, false);
    output.summary = "input accepted by the active subagent".to_owned();
    output.structured_facts["input_accepted"] = json!(true);
    Ok(output)
}

pub(super) async fn state_output(
    host: &RuntimeHost,
    state: &StateProjection,
) -> Result<TaskDelegationOutput, ClientError> {
    let events = host
        .storage
        .repositories
        .events
        .load(state.session_id, state.active_task_id, None)
        .await?;
    let findings = findings(&events, state.active_task_id);
    let task_text = events
        .iter()
        .rev()
        .find(|event| event.event_type == RuntimeEventType::TaskCreated)
        .and_then(|event| event.payload.pointer("/payload/prompt"))
        .and_then(Value::as_str)
        .map(|text| text.chars().take(240).collect::<String>());
    let terminal = state.task_status.is_terminal();
    let verification = state.last_verification.as_ref();
    let issues = verification.map(verification_issues).unwrap_or_default();
    Ok(TaskDelegationOutput {
        status: if terminal {
            delegated_result_status(state.task_status)
        } else {
            ToolResultStatus::Ok
        },
        summary: match state.task_status {
            TaskStatus::Completed => "subagent completed",
            TaskStatus::Partial => "subagent returned findings with unresolved verification",
            TaskStatus::Failed => "subagent failed; inspect available findings and diagnostics",
            TaskStatus::Cancelled | TaskStatus::Interrupted => {
                "subagent interrupted; inspect available findings"
            }
            TaskStatus::Blocked => "subagent blocked",
            TaskStatus::Uncertain => "subagent needs reconciliation before continuation",
            _ => "subagent is running",
        }
        .to_owned(),
        content: findings
            .clone()
            .unwrap_or_else(|| state.final_message.clone().unwrap_or_default()),
        structured_facts: json!({
            "child_session_id": state.session_id, "child_status": state.task_status, "completed": state.task_status == TaskStatus::Completed,
            "child_task_id": state.active_task_id.or_else(|| events.iter().rev().find(|event| event.event_type == RuntimeEventType::TaskCreated).and_then(|event| event.task_id)),
            "child_terminal": terminal,
            "child_task": task_text,
            "child_findings_available": findings.is_some(), "child_verification_status": verification.map(|record| record.result),
            "child_verification_issues": issues, "child_diagnostic": verification.map(|record| &record.residual_risks),
            "child_execution_status": if verification.is_some() && findings.is_some() { "response_returned" } else if terminal { "stopped" } else { "running" },
            "child_next_action": if terminal { "use available findings; resume only for additional work or unresolved issues" } else { "wait or send_input; do not spawn a duplicate" },
        }),
    })
}

fn verification_issues(record: &golutra_core::VerificationRecord) -> Vec<Value> {
    record
        .assertions
        .iter()
        .filter(|assertion| {
            assertion.blocking
                && !matches!(
                    assertion.status,
                    golutra_core::VerificationAssertionStatus::Pass
                        | golutra_core::VerificationAssertionStatus::NotApplicable
                )
        })
        .map(|assertion| {
            json!({
                "criterion": assertion.criterion_id,
                "expected": assertion.expected,
                "status": assertion.status,
                "reason": assertion.message,
            })
        })
        .collect()
}

fn findings(events: &[RuntimeEvent], task: Option<golutra_core::TaskId>) -> Option<String> {
    let events = events
        .iter()
        .filter(|event| event.task_id == task)
        .collect::<Vec<_>>();
    let end = events
        .iter()
        .rposition(|event| event.event_type == RuntimeEventType::VerificationCompleted)
        .unwrap_or(events.len());
    events[..end]
        .iter()
        .rev()
        .find(|event| event.event_type == RuntimeEventType::AssistantMessage)
        .and_then(|event| event.payload.get("content"))
        .and_then(Value::as_str)
        .filter(|text| !text.trim().is_empty())
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unicode_findings_pages_are_lossless_and_repeatable() {
        let content = "项目😀\n".repeat(700);
        let output = TaskDelegationOutput {
            status: ToolResultStatus::Ok,
            summary: "done".to_owned(),
            content: content.clone(),
            structured_facts: json!({}),
        };
        let mut offset = 0;
        let mut collected = String::new();
        loop {
            let args = json!({"offset": offset, "limit": 257});
            let page = page_output(output.clone(), &args);
            assert_eq!(page_output(output.clone(), &args), page);
            collected.push_str(&page.content);
            if page.structured_facts["child_result_has_more"] == false {
                break;
            }
            offset = page.structured_facts["child_result_next_offset"]
                .as_u64()
                .unwrap();
        }
        assert_eq!(collected, content);
        let eof = page_output(output, &json!({"offset": u64::MAX}));
        assert!(eof.content.is_empty());
        assert_eq!(eof.structured_facts["child_result_has_more"], false);
    }

    #[tokio::test]
    async fn bounded_wait_keeps_child_alive_and_result_can_be_read_repeatedly() {
        let child = SessionId::new();
        let operation = DelegationOperation::new(SessionId::new(), CancellationToken::new());
        operation.lifecycle.lock().unwrap().child_session_id = Some(child);
        let expired = bounded_wait(&operation, 1, CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(expired.structured_facts["wait_expired"], true);
        assert_eq!(expired.structured_facts["completed"], false);
        assert!(!operation.cancellation().is_cancelled());
        let result = TaskDelegationOutput {
            status: ToolResultStatus::Ok,
            summary: "done".to_owned(),
            content: "result".to_owned(),
            structured_facts: json!({"child_session_id": child}),
        };
        operation.complete(&Ok(result.clone()));
        for _ in 0..2 {
            assert_eq!(
                bounded_wait(&operation, 0, CancellationToken::new())
                    .await
                    .unwrap(),
                result
            );
        }
    }

    #[test]
    fn original_findings_survive_synthetic_failure_and_prior_tasks() {
        let session = SessionId::new();
        let task = golutra_core::TaskId::new();
        let event = |sequence, kind, content| {
            super::super::super::host_event(
                sequence,
                session,
                Some(task),
                kind,
                golutra_protocol::RuntimeEventSource::Runtime,
                json!({"content": content}),
            )
        };
        let events = vec![
            event(1, RuntimeEventType::AssistantMessage, "项目是 Golutra"),
            event(2, RuntimeEventType::VerificationCompleted, "partial"),
            event(3, RuntimeEventType::AssistantMessage, "Could not complete"),
        ];
        assert_eq!(
            findings(&events, Some(task)).as_deref(),
            Some("项目是 Golutra")
        );
        assert_eq!(findings(&events, Some(golutra_core::TaskId::new())), None);
    }

    #[tokio::test]
    async fn pending_old_notification_cannot_replace_the_latest_execution_result() {
        let host = RuntimeHost::in_memory().await.unwrap();
        let parent = host.default_session_id();
        let child = SessionId::new();
        let mut old = DelegationOperation::new(parent, CancellationToken::new());
        old.created_at -= Duration::from_secs(1);
        old.lifecycle.lock().unwrap().child_session_id = Some(child);
        let old = Arc::new(old);
        let latest = Arc::new(DelegationOperation::new(parent, CancellationToken::new()));
        latest.lifecycle.lock().unwrap().child_session_id = Some(child);
        let output = TaskDelegationOutput {
            status: ToolResultStatus::Ok,
            summary: "done".to_owned(),
            content: "findings".to_owned(),
            structured_facts: json!({"child_session_id": child}),
        };
        old.publish_result(&Ok(output.clone()));
        latest.complete(&Ok(output));
        {
            let mut operations = host.execution.delegation_operations.lock().await;
            operations.insert("old".to_owned(), old.clone());
            operations.insert("latest".to_owned(), latest.clone());
            assert!(!has_active_operation_for(&operations, child));
        }
        assert!(!old.is_complete());
        assert!(Arc::ptr_eq(
            &owned_operation(&host, parent, child).await.unwrap(),
            &latest
        ));
        old.force_stop();
        host.close().await.unwrap();
    }
}
