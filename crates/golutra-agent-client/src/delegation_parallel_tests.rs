use super::*;
use golutra_agent_core::{TaskId, ToolResultStatus};

#[tokio::test]
async fn child_wait_ignores_foreign_control_and_own_execution_noise() {
    let child = SessionId::new();
    let (sender, mut events) = tokio::sync::broadcast::channel(16);
    let event = |session, kind| {
        crate::event_codec::host_event(
            1,
            session,
            None,
            kind,
            golutra_agent_protocol::RuntimeEventSource::Runtime,
            json!({}),
        )
    };
    sender
        .send(event(SessionId::new(), RuntimeEventType::TaskCompleted))
        .unwrap();
    sender
        .send(event(child, RuntimeEventType::ToolStarted))
        .unwrap();
    assert!(
        timeout(
            Duration::from_millis(20),
            wait_for_child_control_event(&mut events, child)
        )
        .await
        .is_err()
    );
    sender
        .send(event(child, RuntimeEventType::TaskCompleted))
        .unwrap();
    timeout(
        Duration::from_millis(100),
        wait_for_child_control_event(&mut events, child),
    )
    .await
    .unwrap()
    .unwrap();
}

fn request(parent: SessionId, arguments: Value) -> ToolRequest {
    ToolRequest {
        tool_call_id: ToolCallId::new(),
        provider_tool_call_id: None,
        session_id: parent,
        turn_id: None,
        tool_name: "subagent".to_owned(),
        arguments,
    }
}

fn result(child: SessionId, status: &str) -> TaskDelegationOutput {
    TaskDelegationOutput {
        status: ToolResultStatus::Ok,
        summary: status.to_owned(),
        content: format!("findings {child}"),
        structured_facts: json!({"child_session_id":child,"child_task_id":TaskId::new(),"child_status":status,"child_terminal":true,"completed":status == "completed"}),
    }
}

async fn operation(host: &RuntimeHost, parent: SessionId) -> (SessionId, Arc<DelegationOperation>) {
    let child = SessionId::new();
    let operation = Arc::new(DelegationOperation::new(parent, CancellationToken::new()));
    operation.lifecycle.lock().unwrap().child_session_id = Some(child);
    operation.ready_sender.send_replace(true);
    host.execution
        .delegation_operations
        .lock()
        .await
        .insert(child.to_string(), operation.clone());
    (child, operation)
}

#[tokio::test]
async fn multi_wait_returns_any_then_all_without_consuming_results() {
    let host = RuntimeHost::in_memory().await.unwrap();
    let parent = host.default_session_id();
    let (first, first_op) = operation(&host, parent).await;
    let (second, second_op) = operation(&host, parent).await;
    let call = request(
        parent,
        json!({"action":"wait","child_session_ids":[first,second,first],"wait_ms":1000}),
    );
    let worker = tokio::spawn(async move {
        tokio::task::yield_now().await;
        first_op.complete(&Ok(result(first, "partial")));
    });
    let output = control::dispatch(&host, &call, CancellationToken::new(), "wait")
        .await
        .unwrap();
    worker.await.unwrap();
    assert_eq!(
        output.structured_facts["child_results"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        output.structured_facts["child_pending_ids"],
        json!([second])
    );
    assert_eq!(output.structured_facts["wait_expired"], false);
    assert!(!second_op.cancellation().is_cancelled());
    second_op.complete(&Ok(result(second, "completed")));
    let mut call = call;
    call.arguments["wait_mode"] = json!("all");
    for _ in 0..2 {
        let output = control::dispatch(&host, &call, CancellationToken::new(), "wait")
            .await
            .unwrap();
        assert_eq!(output.structured_facts["completed"], false);
        assert_eq!(output.structured_facts["child_terminal"], true);
        assert_eq!(output.structured_facts["child_pending_ids"], json!([]));
    }
    host.close().await.unwrap();
}

#[tokio::test]
async fn multi_wait_keeps_observed_execution_when_another_turn_resumes_child() {
    let host = RuntimeHost::in_memory().await.unwrap();
    let parent = host.default_session_id();
    let (first, first_op) = operation(&host, parent).await;
    let (second, second_op) = operation(&host, parent).await;
    let original = result(first, "completed");
    let original_task = original.structured_facts["child_task_id"].clone();
    first_op.complete(&Ok(original));
    let call = request(
        parent,
        json!({"action":"wait","child_session_ids":[first,second],"wait_mode":"all","wait_ms":1000}),
    );
    let waiting = control::dispatch(&host, &call, CancellationToken::new(), "wait");
    tokio::pin!(waiting);
    // 先让 wait 观察第一项终态、停在第二项；模拟另一个父轮次此时续跑第一项。
    assert!(
        timeout(Duration::from_millis(10), &mut waiting)
            .await
            .is_err()
    );
    let resumed = Arc::new(DelegationOperation::new(parent, CancellationToken::new()));
    resumed.lifecycle.lock().unwrap().child_session_id = Some(first);
    host.execution
        .delegation_operations
        .lock()
        .await
        .insert("resumed-first".to_owned(), resumed.clone());
    second_op.complete(&Ok(result(second, "completed")));
    let output = waiting.await.unwrap();
    let observed = output.structured_facts["child_results"]
        .as_array()
        .unwrap()
        .iter()
        .find(|value| value["child_session_id"] == json!(first))
        .unwrap();
    assert_eq!(observed["facts"]["child_task_id"], original_task);
    assert_eq!(output.structured_facts["completed"], true);
    assert_eq!(output.structured_facts["child_pending_ids"], json!([]));
    assert!(!resumed.cancellation().is_cancelled());
    resumed.complete(&Ok(result(first, "completed")));
    host.close().await.unwrap();
}

#[tokio::test]
async fn published_result_is_available_before_notification_cleanup_and_survives_shutdown() {
    let host = RuntimeHost::in_memory().await.unwrap();
    let parent = host.default_session_id();
    let (child, operation) = operation(&host, parent).await;
    let notice_lock = host.execution.delegation_notification_lock.lock().await;
    let owner = tokio::spawn(std::future::pending::<()>());
    operation.set_owner_abort(owner.abort_handle());
    operation.publish_result(&Ok(result(child, "partial")));
    let output = timeout(
        Duration::from_millis(100),
        operation.wait(CancellationToken::new()),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(output.structured_facts["child_status"], "partial");
    assert!(
        !operation.is_complete(),
        "notification owner must remain supervised"
    );
    assert!(operation.execution_finished());
    assert!(
        !control::has_active_operation_for(
            &*host.execution.delegation_operations.lock().await,
            child
        ),
        "notification cleanup must not block another execution in the child session"
    );
    operation.force_stop();
    assert!(owner.await.unwrap_err().is_cancelled());
    assert_eq!(
        operation
            .wait(CancellationToken::new())
            .await
            .unwrap()
            .content,
        output.content
    );
    drop(notice_lock);
    host.close().await.unwrap();
}

#[tokio::test]
async fn background_handle_contains_the_execution_identity() {
    let host = RuntimeHost::in_memory().await.unwrap();
    let (child, operation) = operation(&host, host.default_session_id()).await;
    let task = TaskId::new();
    operation.lifecycle.lock().unwrap().child_task_id = Some(task);
    let output = control::wait_after_start(
        &operation,
        &json!({"run_in_background":true}),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(output.structured_facts["child_task_id"], json!(task));
    operation.complete(&Ok(result(child, "completed")));
    host.close().await.unwrap();
}

#[tokio::test]
async fn a_new_parent_turn_cannot_exceed_slots_held_by_earlier_background_children() {
    let host = RuntimeHost::in_memory().await.unwrap();
    let parent = host.default_session_id();
    host.upsert_current_thread(parent, &json!({"prompt":"parent"}))
        .await
        .unwrap();
    let task = TaskId::new();
    let (execution, _) = golutra_agent_runtime::agent_execution_channel(1);
    let worker = tokio::spawn(std::future::pending::<()>());
    let (_, completion) = watch::channel(false);
    host.execution.task_controls.lock().await.insert(
        parent,
        crate::HostedTaskControl {
            task_id: task,
            allow_network: false,
            yolo: false,
            provider_settings: crate::ProviderTurnSettings::default(),
            execution,
            abort_handle: worker.abort_handle(),
            completion,
            delegation: Some(delegation_policy::DelegationContext::root(
                parent,
                None,
                None,
                host.execution.shutdown.child_token(),
            )),
            _session_lease: None,
        },
    );
    let mut children = Vec::new();
    for _ in 0..10 {
        children.push(operation(&host, parent).await);
    }
    let call = request(parent, json!({"task":"one more","run_in_background":true}));
    let output = delegate_task(&host, &call, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(output.status, ToolResultStatus::Blocked);
    assert_eq!(output.structured_facts["child_active_count"], 10);
    for (child, operation) in children {
        operation.complete(&Ok(result(child, "completed")));
    }
    host.clear_task_control(parent, task).await;
    worker.abort();
    host.close().await.unwrap();
}

#[tokio::test]
async fn multi_wait_timeout_cancellation_and_foreign_handles_do_not_cancel_children() {
    let host = RuntimeHost::in_memory().await.unwrap();
    let parent = host.default_session_id();
    let (first, first_op) = operation(&host, parent).await;
    let (second, second_op) = operation(&host, parent).await;
    let mut call = request(
        parent,
        json!({"action":"wait","child_session_ids":[first,second],"wait_mode":"all","wait_ms":1}),
    );
    let output = control::dispatch(&host, &call, CancellationToken::new(), "wait")
        .await
        .unwrap();
    assert_eq!(output.structured_facts["wait_expired"], true);
    assert_eq!(
        output.structured_facts["child_pending_ids"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    assert!(
        control::dispatch(&host, &call, cancellation, "wait")
            .await
            .is_err()
    );
    call.session_id = SessionId::new();
    assert!(
        control::dispatch(&host, &call, CancellationToken::new(), "wait")
            .await
            .is_err()
    );
    assert!(!first_op.cancellation().is_cancelled());
    assert!(!second_op.cancellation().is_cancelled());
    first_op.complete(&Ok(result(first, "cancelled")));
    second_op.complete(&Ok(result(second, "cancelled")));
    host.close().await.unwrap();
}

#[tokio::test]
async fn completion_notifications_are_durable_deduplicated_and_replayed_in_context() {
    let home = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let host = RuntimeHost::from_home_and_cwd(home.path(), root.path())
        .await
        .unwrap();
    let parent = host.default_session_id();
    let call = request(parent, json!({"task":"inspect","run_in_background":true}));
    let output = Ok(result(SessionId::new(), "completed"));
    notifications::publish(&host, &call, &output).await.unwrap();
    notifications::publish(&host, &call, &output).await.unwrap();
    assert_eq!(notifications::load(&host, parent).await.unwrap().len(), 1);
    host.close().await.unwrap();
    drop(host);
    let reopened = RuntimeHost::from_home_and_cwd(home.path(), root.path())
        .await
        .unwrap();
    let notifications = notifications::load(&reopened, parent).await.unwrap();
    assert_eq!(notifications.len(), 1);
    assert!(notifications[0].content.contains("findings"));
    let events = reopened
        .storage
        .repositories
        .events
        .load(parent, None, None)
        .await
        .unwrap();
    let event = events
        .iter()
        .find(|event| event.event_type == RuntimeEventType::SubagentUpdated)
        .unwrap();
    let contributor = crate::context::conversation_history_contributor(event).unwrap();
    assert!(crate::context::is_history_cache_event(event));
    assert_eq!(contributor.source_refs, vec![notifications[0].id.clone()]);
    reopened.close().await.unwrap();
}
