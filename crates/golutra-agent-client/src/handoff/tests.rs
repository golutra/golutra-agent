//! 交接的持久化、幂等、取消与会话隔离回归。
use super::*;

async fn host() -> Arc<RuntimeHost> {
    let host = RuntimeHost::in_memory().await.unwrap();
    host.upsert_current_thread(
        host.default_session_id,
        &json!({"_thread_id": host.default_thread_id}),
    )
    .await
    .unwrap();
    host
}

#[tokio::test]
async fn handoff_creation_is_atomic_idempotent_and_not_a_delegated_task() {
    let host = host().await;
    host.record_event(host_event(
        1,
        host.default_session_id,
        None,
        RuntimeEventType::AssistantMessage,
        RuntimeEventSource::Provider,
        json!({"content": "original source message"}),
    ))
    .await
    .unwrap();
    let before = host
        .storage
        .repositories
        .events
        .load(host.default_session_id, None, None)
        .await
        .unwrap();
    let request = HandoffRequest::Create {
        thread_id: ThreadId::new(),
        session_id: SessionId::new(),
        draft: "Goal: verify parser\nTests: pending".into(),
    };
    let first = host
        .handoff_thread(host.default_thread_id, request.clone())
        .await
        .unwrap();
    let HandoffResult::Created { thread } = first else {
        panic!("created");
    };
    assert_eq!(thread.parent_thread_id, Some(host.default_thread_id));
    assert_eq!(thread.forked_from_sequence_no, Some(0));
    assert!(
        !host
            .session_is_delegated_child(thread.session_id)
            .await
            .unwrap()
    );
    assert!(
        matches!(host.handoff_thread(host.default_thread_id, request).await.unwrap(), HandoffResult::Created { thread: again } if again.thread_id == thread.thread_id)
    );
    let events = host
        .storage
        .repositories
        .events
        .load(thread.session_id, None, None)
        .await
        .unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event_type, RuntimeEventType::SessionCreated);
    assert_eq!(
        events[0].payload["handoff_draft"],
        "Goal: verify parser\nTests: pending"
    );
    assert_eq!(
        host.storage
            .repositories
            .events
            .load(host.default_session_id, None, None)
            .await
            .unwrap(),
        before
    );
    assert_eq!(
        host.storage
            .repositories
            .projections
            .state(thread.session_id, None)
            .await
            .unwrap()
            .task_status,
        TaskStatus::Idle
    );
    assert!(
        host.handoff_thread(
            host.default_thread_id,
            HandoffRequest::Create {
                thread_id: thread.thread_id,
                session_id: thread.session_id,
                draft: "different draft".into()
            }
        )
        .await
        .is_err()
    );
}

#[tokio::test]
async fn handoff_cancel_before_prepare_leaves_no_work_or_history() {
    let host = host().await;
    let operation_id = Uuid::now_v7();
    assert!(matches!(
        host.handoff_thread(
            host.default_thread_id,
            HandoffRequest::Cancel { operation_id }
        )
        .await
        .unwrap(),
        HandoffResult::Cancelled
    ));
    assert!(matches!(
        host.handoff_thread(
            host.default_thread_id,
            HandoffRequest::Prepare {
                operation_id,
                goal: None,
                provider: Value::Null
            }
        )
        .await
        .unwrap(),
        HandoffResult::Cancelled
    ));
    assert!(host.execution.handoff_operations.lock().unwrap().is_empty());
    assert!(
        host.storage
            .repositories
            .events
            .load(host.default_session_id, None, None)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn handoff_rejects_empty_draft_missing_source_and_destination_collision() {
    let host = host().await;
    let request = |draft: &str| HandoffRequest::Create {
        thread_id: ThreadId::new(),
        session_id: SessionId::new(),
        draft: draft.into(),
    };
    assert!(
        host.handoff_thread(host.default_thread_id, request("  "))
            .await
            .is_err()
    );
    assert!(
        host.handoff_thread(ThreadId::new(), request("valid"))
            .await
            .is_err()
    );
    assert!(
        host.handoff_thread(
            host.default_thread_id,
            HandoffRequest::Create {
                thread_id: ThreadId::new(),
                session_id: host.default_session_id,
                draft: "valid".into()
            }
        )
        .await
        .is_err()
    );
    assert_eq!(host.list_threads(20).await.unwrap().len(), 1);
}

#[tokio::test]
async fn handoff_rejects_active_source_and_shutdown_without_creating_a_thread() {
    let host = host().await;
    host.record_event(host_event(
        1,
        host.default_session_id,
        Some(TaskId::new()),
        RuntimeEventType::TaskCreated,
        RuntimeEventSource::Runtime,
        json!({"payload": {"prompt": "running"}}),
    ))
    .await
    .unwrap();
    for request in [
        HandoffRequest::Prepare {
            operation_id: Uuid::now_v7(),
            goal: None,
            provider: Value::Null,
        },
        HandoffRequest::Create {
            thread_id: ThreadId::new(),
            session_id: SessionId::new(),
            draft: "pending".into(),
        },
    ] {
        let error = host
            .handoff_thread(host.default_thread_id, request)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("active task"));
    }
    host.execution.shutdown.cancel();
    let error = host
        .handoff_thread(
            host.default_thread_id,
            HandoffRequest::Create {
                thread_id: ThreadId::new(),
                session_id: SessionId::new(),
                draft: "pending".into(),
            },
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("shutting down"));
    assert_eq!(host.list_threads(20).await.unwrap().len(), 1);
}
