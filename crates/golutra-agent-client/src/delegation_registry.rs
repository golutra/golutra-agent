use super::*;

pub(crate) async fn list(
    host: &RuntimeHost,
    parent: SessionId,
    cursor: u64,
    limit: u32,
) -> Result<Value, ClientError> {
    host.ensure_session_in_workspace(parent).await?;
    let limit = limit.clamp(1, 64);
    let mut records = host
        .storage
        .store
        .subagents(parent, cursor, limit + 1)
        .await?;
    let has_more = records.len() > limit as usize;
    records.truncate(limit as usize);
    let next = has_more
        .then(|| records.last().map(|record| record.cursor))
        .flatten();
    let mut children = Vec::with_capacity(records.len());
    for record in records {
        host.ensure_thread_in_workspace(&record.thread)?;
        let state = host
            .storage
            .repositories
            .projections
            .state(record.thread.session_id, None)
            .await?;
        let question = if state.task_status == TaskStatus::Blocked {
            match state.active_task_id {
                Some(task) => {
                    host.storage
                        .store
                        .subagent_question(record.thread.session_id, task)
                        .await?
                }
                None => None,
            }
        } else {
            None
        };
        let mut child = json!({"name":record.name,"child_session_id":record.thread.session_id,
            "child_thread_id":record.thread.thread_id,"task_id":state.active_task_id,"status":state.task_status,
            "task":record.thread.preview,"archived":record.thread.archived,
            "pending_messages":host.storage.store.pending_subagent_message_count(record.thread.session_id).await?});
        if let Some(question) = question {
            child["waiting_for_user"] = json!(true);
            child["questions"] = question["questions"].clone();
            child["question_id"] = question["question_id"].clone();
        }
        children.push(child);
    }
    Ok(
        json!({"available":true,"scope":"parent_session","children":children,"truncated":has_more,"next_cursor":next}),
    )
}

pub(super) async fn queue(
    host: &RuntimeHost,
    request: &ToolRequest,
) -> Result<TaskDelegationOutput, ClientError> {
    let child = control::child_id(request)?;
    let thread = control::owned_thread(host, request.session_id, child).await?;
    if request.arguments.get("reply_to_question_id").is_some()
        || request.arguments.get("expected_child_task_id").is_some()
    {
        return Err(ClientError::TaskExecution(
            "execution-bound replies require resume or delivery_mode continue".into(),
        ));
    }
    // Legacy children acquire the same registry identity on their first new message.
    let name = host
        .storage
        .store
        .subagent_name(request.session_id, child)
        .await?
        .unwrap_or_else(|| format!("agent-{child}"));
    host.storage
        .store
        .register_subagent(request.session_id, child, &name)
        .await?;
    let content = request.arguments["task"]
        .as_str()
        .ok_or_else(|| ClientError::TaskExecution("message requires task text".into()))?;
    let inserted = host
        .storage
        .store
        .enqueue_subagent_message(
            request.session_id,
            child,
            &request.tool_call_id.to_string(),
            content,
        )
        .await?;
    Ok(TaskDelegationOutput {
        status: golutra_agent_core::ToolResultStatus::Ok,
        summary: "message queued without starting another execution".into(),
        content: String::new(),
        structured_facts: json!({"child_session_id":child,"child_thread_id":thread.thread_id,"message_id":request.tool_call_id,
            "message_queued":true,"duplicate":!inserted,"delivery_mode":"queue_only"}),
    })
}
