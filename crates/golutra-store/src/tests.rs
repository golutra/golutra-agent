use chrono::Utc;
use golutra_core::{
    Actor, ActorKind, ArtifactId, BusyPolicy, CommandId, LaneId, RedactionStatus, RuntimeLane,
    TaskStatus, ToolCallId, TurnId, WorkspaceId,
};
use golutra_protocol::{RuntimeEventSource, RuntimeEventType};
use serde_json::json;
use tempfile::tempdir;

use super::*;

#[tokio::test]
async fn command_ack_is_durable_by_idempotency_key() {
    let store = RuntimeStore::in_memory().await.expect("store");
    let ack = CommandAck {
        command_id: CommandId::new(),
        accepted: true,
        reason: Some("accepted".to_owned()),
    };

    store
        .store_command_ack("same-command", &ack)
        .await
        .expect("store ack");

    assert_eq!(
        store.command_ack("same-command").await.expect("load ack"),
        Some(ack)
    );
}

#[tokio::test]
async fn event_sequence_is_atomic_across_store_connections() {
    let directory = tempdir().expect("directory");
    let database_url = format!(
        "sqlite://{}",
        directory.path().join("runtime.sqlite").display()
    );
    let first = RuntimeStore::connect(&database_url)
        .await
        .expect("first store");
    let second = RuntimeStore::connect(&database_url)
        .await
        .expect("second store");
    let session_id = SessionId::new();
    let event = |summary: &str| RuntimeEvent {
        id: golutra_core::EventId::new(),
        sequence_no: 0,
        session_id,
        turn_id: Some(TurnId::new()),
        task_id: Some(TaskId::new()),
        parent_event_id: None,
        event_type: RuntimeEventType::CommandAccepted,
        timestamp: Utc::now(),
        source: RuntimeEventSource::Runtime,
        payload: json!({"summary": summary}),
        payload_ref: None,
        durable: true,
    };

    let (first_event, second_event) = tokio::join!(
        first.append_event_assigning_sequence(event("first")),
        second.append_event_assigning_sequence(event("second")),
    );
    let mut sequence_numbers = vec![
        first_event.expect("first event").sequence_no,
        second_event.expect("second event").sequence_no,
    ];
    sequence_numbers.sort_unstable();

    assert_eq!(sequence_numbers, vec![1, 2]);
    assert_eq!(
        first
            .load_events(session_id, None, None)
            .await
            .expect("events")
            .len(),
        2
    );
}

#[tokio::test]
async fn event_pages_advance_from_the_last_sequence_cursor() {
    let store = RuntimeStore::in_memory().await.expect("store");
    let session_id = SessionId::new();
    for index in 0..5 {
        store
            .append_event_assigning_sequence(RuntimeEvent {
                id: golutra_core::EventId::new(),
                sequence_no: 0,
                session_id,
                turn_id: None,
                task_id: None,
                parent_event_id: None,
                event_type: RuntimeEventType::CommandAccepted,
                timestamp: Utc::now(),
                source: RuntimeEventSource::Runtime,
                payload: json!({"summary": format!("event {index}")}),
                payload_ref: None,
                durable: true,
            })
            .await
            .expect("event");
    }

    let first = store
        .load_events_page(session_id, None, None, 2)
        .await
        .expect("first page");
    let second = store
        .load_events_page(session_id, None, Some(first[1].sequence_no), 2)
        .await
        .expect("second page");
    let third = store
        .load_events_page(session_id, None, Some(second[1].sequence_no), 2)
        .await
        .expect("third page");
    let recent = store
        .load_recent_events(session_id, None, None, 2)
        .await
        .expect("recent events");

    assert_eq!(first.len(), 2);
    assert_eq!(second.len(), 2);
    assert_eq!(third.len(), 1);
    assert_eq!(recent.len(), 2);
    assert_eq!(recent[0].payload["summary"], "event 3");
    assert_eq!(recent[1].payload["summary"], "event 4");
    assert!(first[1].sequence_no < second[0].sequence_no);
    assert!(second[1].sequence_no < third[0].sequence_no);
}

#[tokio::test]
async fn loads_the_latest_explicit_compaction_without_materializing_history() {
    let store = RuntimeStore::in_memory().await.expect("store");
    let session_id = SessionId::new();
    for (mode, content) in [
        ("explicit", "first summary"),
        ("automatic", "automatic summary"),
        ("explicit", "latest summary"),
    ] {
        store
            .append_event_assigning_sequence(RuntimeEvent {
                id: golutra_core::EventId::new(),
                sequence_no: 0,
                session_id,
                turn_id: None,
                task_id: None,
                parent_event_id: None,
                event_type: RuntimeEventType::CompactionCompleted,
                timestamp: Utc::now(),
                source: RuntimeEventSource::Runtime,
                payload: json!({"mode": mode, "content": content}),
                payload_ref: None,
                durable: true,
            })
            .await
            .expect("compaction");
    }

    let event = store
        .load_latest_explicit_compaction(session_id)
        .await
        .expect("query")
        .expect("explicit compaction");

    assert_eq!(event.payload["content"], "latest summary");
}

#[tokio::test]
async fn appends_events_and_reduces_state() {
    let store = RuntimeStore::in_memory().await.expect("store opens");
    let session_id = SessionId::new();
    let task_id = TaskId::new();
    let event = RuntimeEvent {
        id: golutra_core::EventId::new(),
        sequence_no: 1,
        session_id,
        turn_id: Some(TurnId::new()),
        task_id: Some(task_id),
        parent_event_id: None,
        event_type: RuntimeEventType::TaskCreated,
        timestamp: Utc::now(),
        source: RuntimeEventSource::Runtime,
        payload: json!({"summary": "task created"}),
        payload_ref: None,
        durable: true,
    };

    store.append_event(&event).await.expect("event appended");

    let events = store
        .load_events(session_id, Some(task_id), None)
        .await
        .expect("events load");
    let projection = RuntimeStore::reduce_state(session_id, &events);

    assert_eq!(projection.active_task_id, Some(task_id));
    assert_eq!(projection.task_status, TaskStatus::Running);
    assert_eq!(projection.last_sequence_no, 1);
}

#[tokio::test]
async fn assistant_message_becomes_user_projection_final_message() {
    let store = RuntimeStore::in_memory().await.expect("store opens");
    let session_id = SessionId::new();
    let task_id = TaskId::new();
    let event = RuntimeEvent {
        id: golutra_core::EventId::new(),
        sequence_no: 1,
        session_id,
        turn_id: Some(TurnId::new()),
        task_id: Some(task_id),
        parent_event_id: None,
        event_type: RuntimeEventType::AssistantMessage,
        timestamp: Utc::now(),
        source: RuntimeEventSource::Runtime,
        payload: json!({
            "summary": "Completed: file written",
            "content": "Completed: file written",
        }),
        payload_ref: None,
        durable: true,
    };

    store.append_event(&event).await.expect("event appended");

    let projection = store
        .user_projection(session_id, None)
        .await
        .expect("projection loads");

    assert_eq!(
        projection.final_message,
        Some("Completed: file written".to_owned())
    );
}

#[tokio::test]
async fn a_new_task_clears_terminal_fields_from_the_previous_task() {
    let store = RuntimeStore::in_memory().await.expect("store");
    let session_id = SessionId::new();
    let previous_task = TaskId::new();
    let mut event = RuntimeEvent {
        id: golutra_core::EventId::new(),
        sequence_no: 0,
        session_id,
        turn_id: Some(TurnId::new()),
        task_id: Some(previous_task),
        parent_event_id: None,
        event_type: RuntimeEventType::AssistantMessage,
        timestamp: Utc::now(),
        source: RuntimeEventSource::Runtime,
        payload: json!({"summary": "old", "content": "old response"}),
        payload_ref: None,
        durable: true,
    };
    store
        .append_event_assigning_sequence(event.clone())
        .await
        .expect("assistant");
    event.id = golutra_core::EventId::new();
    event.task_id = Some(TaskId::new());
    event.turn_id = Some(TurnId::new());
    event.event_type = RuntimeEventType::TaskCreated;
    event.payload = json!({"summary": "new task"});
    store
        .append_event_assigning_sequence(event)
        .await
        .expect("new task");

    let projection = store
        .query_state(session_id, None)
        .await
        .expect("projection");
    assert_eq!(projection.task_status, TaskStatus::Running);
    assert_eq!(projection.final_message, None);
    assert_eq!(projection.last_verification, None);
    assert_eq!(projection.last_loop_decision, None);
}

#[test]
fn projection_tracks_runtime_lane_and_keeps_pause_after_approval_resolution() {
    let session_id = SessionId::new();
    let task_id = TaskId::new();
    let turn_id = TurnId::new();
    let runtime_lane = RuntimeLane {
        lane_id: LaneId::new(),
        workspace_id: WorkspaceId::new(),
        session_id,
        task_id,
        active_turn_id: Some(turn_id),
        active_controller: Actor {
            kind: ActorKind::Cli,
            id: "test-controller".to_owned(),
        },
        status: TaskStatus::Running,
        pending_turns: Vec::new(),
        injected_inputs: Vec::new(),
        busy_policy_default: BusyPolicy::Append,
    };
    let event = |sequence_no, event_type, payload| RuntimeEvent {
        id: golutra_core::EventId::new(),
        sequence_no,
        session_id,
        turn_id: Some(turn_id),
        task_id: Some(task_id),
        parent_event_id: None,
        event_type,
        timestamp: Utc::now(),
        source: RuntimeEventSource::Runtime,
        payload,
        payload_ref: None,
        durable: true,
    };
    let events = vec![
        event(
            1,
            RuntimeEventType::TaskCreated,
            json!({"summary": "started", "runtime": {"runtime_lane": runtime_lane}}),
        ),
        event(
            2,
            RuntimeEventType::ApprovalRequested,
            json!({"summary": "approval", "approval_id": "approval-1"}),
        ),
        event(
            3,
            RuntimeEventType::TaskPaused,
            json!({"summary": "paused"}),
        ),
        event(
            4,
            RuntimeEventType::ApprovalResolved,
            json!({"summary": "approved", "approval_id": "approval-1"}),
        ),
    ];

    let projection = RuntimeStore::reduce_state(session_id, &events);

    assert_eq!(projection.task_status, TaskStatus::Paused);
    assert_eq!(projection.pending_approval, None);
    assert_eq!(
        projection.runtime_lane.as_ref().map(|lane| lane.status),
        Some(TaskStatus::Paused)
    );
    assert_eq!(
        projection
            .runtime_lane
            .as_ref()
            .map(|lane| lane.active_controller.id.as_str()),
        Some("test-controller")
    );
}

#[tokio::test]
async fn stores_artifact_metadata() {
    let store = RuntimeStore::in_memory().await.expect("store opens");
    let session_id = SessionId::new();
    let bytes = b"fixture";
    let artifact = ArtifactRecord {
        artifact_id: ArtifactId::new(),
        session_id,
        turn_id: Some(TurnId::new()),
        tool_call_id: Some(ToolCallId::new()),
        artifact_type: "stdout".to_owned(),
        uri: "artifact://fixture/stdout".to_owned(),
        checksum: artifact_checksum(bytes),
        size_bytes: bytes.len() as u64,
        created_at: Utc::now(),
        producer: "test".to_owned(),
        redaction_status: RedactionStatus::NotRequired,
        retention_policy: "test".to_owned(),
        provenance_refs: Vec::new(),
    };

    store
        .store_artifact(&artifact, bytes)
        .await
        .expect("artifact stored");
    let loaded = store
        .load_artifact(artifact.artifact_id)
        .await
        .expect("artifact loads");

    assert_eq!(loaded, Some(artifact.clone()));
    assert_eq!(
        store
            .load_artifact_bytes(loaded.expect("artifact").artifact_id)
            .await
            .expect("blob loads"),
        Some(bytes.to_vec())
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let directory_mode = std::fs::metadata(&store.artifact_root)
            .expect("artifact directory")
            .permissions()
            .mode()
            & 0o777;
        let file_mode = std::fs::metadata(store.artifact_blob_path(artifact.artifact_id))
            .expect("artifact blob")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(directory_mode, 0o700);
        assert_eq!(file_mode, 0o600);
    }
}

#[tokio::test]
async fn duplicate_artifact_id_cannot_overwrite_the_existing_blob() {
    let store = RuntimeStore::in_memory().await.expect("store opens");
    let original_bytes = b"original";
    let artifact = ArtifactRecord {
        artifact_id: ArtifactId::new(),
        session_id: SessionId::new(),
        turn_id: Some(TurnId::new()),
        tool_call_id: Some(ToolCallId::new()),
        artifact_type: "stdout".to_owned(),
        uri: "artifact://fixture/original".to_owned(),
        checksum: artifact_checksum(original_bytes),
        size_bytes: original_bytes.len() as u64,
        created_at: Utc::now(),
        producer: "test".to_owned(),
        redaction_status: RedactionStatus::NotRequired,
        retention_policy: "test".to_owned(),
        provenance_refs: Vec::new(),
    };
    store
        .store_artifact(&artifact, original_bytes)
        .await
        .expect("original artifact");
    let replacement_bytes = b"replacement";
    let mut replacement = artifact.clone();
    replacement.checksum = artifact_checksum(replacement_bytes);
    replacement.size_bytes = replacement_bytes.len() as u64;

    let error = store
        .store_artifact(&replacement, replacement_bytes)
        .await
        .expect_err("duplicate artifact id must be rejected");

    assert!(error.to_string().contains("different blob content"));
    assert_eq!(
        store
            .load_artifact_bytes(artifact.artifact_id)
            .await
            .expect("original blob remains"),
        Some(original_bytes.to_vec())
    );
}

#[tokio::test]
async fn debug_projection_includes_events_and_artifacts() {
    let store = RuntimeStore::in_memory().await.expect("store opens");
    let session_id = SessionId::new();
    let event = RuntimeEvent {
        id: golutra_core::EventId::new(),
        sequence_no: 1,
        session_id,
        turn_id: Some(TurnId::new()),
        task_id: Some(TaskId::new()),
        parent_event_id: None,
        event_type: RuntimeEventType::TaskCreated,
        timestamp: Utc::now(),
        source: RuntimeEventSource::Runtime,
        payload: json!({"summary": "task created"}),
        payload_ref: None,
        durable: true,
    };
    let bytes = b"x";
    let artifact = ArtifactRecord {
        artifact_id: ArtifactId::new(),
        session_id,
        turn_id: Some(TurnId::new()),
        tool_call_id: Some(ToolCallId::new()),
        artifact_type: "log".to_owned(),
        uri: "artifact://fixture/log".to_owned(),
        checksum: artifact_checksum(bytes),
        size_bytes: 1,
        created_at: Utc::now(),
        producer: "test".to_owned(),
        redaction_status: RedactionStatus::NotRequired,
        retention_policy: "test".to_owned(),
        provenance_refs: Vec::new(),
    };
    store.append_event(&event).await.expect("event");
    store
        .store_artifact(&artifact, bytes)
        .await
        .expect("artifact");

    let projection = store
        .debug_projection(session_id, None)
        .await
        .expect("debug projection");

    assert_eq!(projection.events.len(), 1);
    assert_eq!(projection.artifacts.len(), 1);
}

#[tokio::test]
async fn stores_and_lists_thread_metadata() {
    let store = RuntimeStore::in_memory().await.expect("store opens");
    let now = Utc::now();
    let thread = ThreadRecord {
        thread_id: ThreadId::new(),
        session_id: SessionId::new(),
        parent_thread_id: None,
        forked_from_turn_id: None,
        forked_from_sequence_no: None,
        workspace_root: Some("/workspace".to_owned()),
        rebound_from_workspace_root: None,
        rollout_path: Some("/state/rollouts/thread.jsonl".to_owned()),
        title: "Implement provider setup".to_owned(),
        preview: "Implement provider setup and persistence".to_owned(),
        created_at: now,
        updated_at: now,
        recency_at: now,
        archived: false,
    };

    store.upsert_thread(&thread).await.expect("thread stored");
    let loaded = store
        .thread_by_id(thread.thread_id)
        .await
        .expect("thread loads")
        .expect("thread exists");
    let listed = store
        .list_threads(Some("/workspace"), 10)
        .await
        .expect("threads list");

    assert_eq!(loaded.thread_id, thread.thread_id);
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].session_id, thread.session_id);
}

#[tokio::test]
async fn fork_transaction_copies_boundary_history_and_remaps_runtime_ids() {
    let store = RuntimeStore::in_memory().await.expect("store opens");
    let parent_session_id = SessionId::new();
    let parent_task_id = TaskId::new();
    let parent_turn_id = TurnId::new();
    let first = store
        .append_event_assigning_sequence(RuntimeEvent {
            id: EventId::new(),
            sequence_no: 0,
            session_id: parent_session_id,
            turn_id: Some(parent_turn_id),
            task_id: Some(parent_task_id),
            parent_event_id: None,
            event_type: RuntimeEventType::TaskCreated,
            timestamp: Utc::now(),
            source: RuntimeEventSource::Runtime,
            payload: json!({
                "summary": "parent task",
                "session_id": parent_session_id,
                "task_id": parent_task_id,
                "turn_id": parent_turn_id,
            }),
            payload_ref: None,
            durable: true,
        })
        .await
        .expect("first parent event");
    let second = store
        .append_event_assigning_sequence(RuntimeEvent {
            id: EventId::new(),
            sequence_no: 0,
            session_id: parent_session_id,
            turn_id: Some(parent_turn_id),
            task_id: Some(parent_task_id),
            parent_event_id: Some(first.id),
            event_type: RuntimeEventType::AssistantMessage,
            timestamp: Utc::now(),
            source: RuntimeEventSource::Runtime,
            payload: json!({"summary": "parent answer"}),
            payload_ref: None,
            durable: true,
        })
        .await
        .expect("second parent event");
    store
        .append_event_assigning_sequence(RuntimeEvent {
            id: EventId::new(),
            sequence_no: 0,
            session_id: parent_session_id,
            turn_id: Some(TurnId::new()),
            task_id: Some(TaskId::new()),
            parent_event_id: None,
            event_type: RuntimeEventType::TaskCreated,
            timestamp: Utc::now(),
            source: RuntimeEventSource::Runtime,
            payload: json!({"summary": "outside fork boundary"}),
            payload_ref: None,
            durable: true,
        })
        .await
        .expect("event outside boundary");
    let now = Utc::now();
    let child = ThreadRecord {
        thread_id: ThreadId::new(),
        session_id: SessionId::new(),
        parent_thread_id: Some(ThreadId::new()),
        forked_from_turn_id: Some(parent_turn_id),
        forked_from_sequence_no: Some(second.sequence_no),
        workspace_root: Some("/workspace".to_owned()),
        rebound_from_workspace_root: None,
        rollout_path: Some("/state/rollouts/child.jsonl".to_owned()),
        title: "Fork".to_owned(),
        preview: "Fork preview".to_owned(),
        created_at: now,
        updated_at: now,
        recency_at: now,
        archived: false,
    };

    let forked = store
        .create_forked_thread(&child, parent_session_id, second.sequence_no)
        .await
        .expect("fork transaction");

    assert_eq!(forked.len(), 2);
    assert!(
        forked
            .iter()
            .all(|event| event.session_id == child.session_id)
    );
    assert!(
        forked
            .iter()
            .all(|event| event.id != first.id && event.id != second.id)
    );
    assert_eq!(forked[1].parent_event_id, Some(forked[0].id));
    assert_ne!(forked[0].task_id, Some(parent_task_id));
    assert_eq!(forked[0].task_id, forked[1].task_id);
    assert_ne!(forked[0].turn_id, Some(parent_turn_id));
    assert_eq!(forked[0].turn_id, forked[1].turn_id);
    assert_eq!(
        forked[0].payload["session_id"],
        child.session_id.to_string()
    );
    assert_eq!(
        forked[0].payload["task_id"],
        forked[0].task_id.expect("child task id").to_string()
    );
    assert_eq!(
        store
            .load_events(child.session_id, None, None)
            .await
            .expect("child events"),
        forked
    );
    assert_eq!(
        store
            .thread_by_id(child.thread_id)
            .await
            .expect("child thread query"),
        Some(child)
    );
}

#[tokio::test]
async fn different_threads_cannot_bind_the_same_session() {
    let store = RuntimeStore::in_memory().await.expect("store opens");
    let now = Utc::now();
    let session_id = SessionId::new();
    let thread = |thread_id| ThreadRecord {
        thread_id,
        session_id,
        parent_thread_id: None,
        forked_from_turn_id: None,
        forked_from_sequence_no: None,
        workspace_root: Some("/workspace".to_owned()),
        rebound_from_workspace_root: None,
        rollout_path: None,
        title: "Thread".to_owned(),
        preview: "Thread preview".to_owned(),
        created_at: now,
        updated_at: now,
        recency_at: now,
        archived: false,
    };
    store
        .upsert_thread(&thread(ThreadId::new()))
        .await
        .expect("first thread");

    let error = store
        .upsert_thread(&thread(ThreadId::new()))
        .await
        .expect_err("session must be unique");

    assert!(matches!(error, StoreError::Sqlx(_)));
    assert_eq!(
        store
            .thread_by_session(session_id)
            .await
            .expect("thread query")
            .map(|thread| thread.session_id),
        Some(session_id)
    );
}

#[tokio::test]
async fn migration_deduplicates_legacy_threads_before_adding_session_uniqueness() {
    let directory = tempdir().expect("directory");
    let database_url = format!(
        "sqlite://{}",
        directory.path().join("runtime.sqlite").display()
    );
    let options = SqliteConnectOptions::from_str(&database_url)
        .expect("sqlite options")
        .create_if_missing(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .expect("legacy database");
    sqlx::query(
        r#"
            CREATE TABLE threads (
                thread_id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                parent_thread_id TEXT,
                workspace_root TEXT,
                title TEXT NOT NULL,
                preview TEXT NOT NULL,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                recency_at TEXT NOT NULL,
                archived INTEGER NOT NULL DEFAULT 0
            )
            "#,
    )
    .execute(&pool)
    .await
    .expect("legacy threads table");
    let session_id = SessionId::new();
    let older_thread_id = ThreadId::new();
    let newer_thread_id = ThreadId::new();
    for (thread_id, timestamp) in [
        (older_thread_id, "2026-07-10T00:00:00Z"),
        (newer_thread_id, "2026-07-11T00:00:00Z"),
    ] {
        sqlx::query(
            r#"
                INSERT INTO threads (
                    thread_id, session_id, parent_thread_id, workspace_root, title, preview,
                    created_at, updated_at, recency_at, archived
                ) VALUES (?, ?, NULL, '/workspace', 'Thread', 'Preview', ?, ?, ?, 0)
                "#,
        )
        .bind(thread_id.to_string())
        .bind(session_id.to_string())
        .bind(timestamp)
        .bind(timestamp)
        .bind(timestamp)
        .execute(&pool)
        .await
        .expect("legacy thread");
    }
    drop(pool);

    let store = RuntimeStore::connect(&database_url)
        .await
        .expect("migrated store");
    let thread = store
        .thread_by_session(session_id)
        .await
        .expect("thread query")
        .expect("deduplicated thread");

    assert_eq!(thread.thread_id, newer_thread_id);
    assert!(thread.forked_from_turn_id.is_none());
    assert!(thread.forked_from_sequence_no.is_none());
    assert!(thread.rebound_from_workspace_root.is_none());
    assert!(thread.rollout_path.is_none());
    assert_eq!(
        store
            .list_threads(Some("/workspace"), 10)
            .await
            .expect("thread list")
            .len(),
        1
    );
}

#[tokio::test]
async fn durable_projection_and_runtime_indexes_survive_reopen() {
    let directory = tempdir().expect("directory");
    let database_url = format!(
        "sqlite://{}",
        directory.path().join("runtime.sqlite").display()
    );
    let session_id = SessionId::new();
    let task_id = TaskId::new();
    let event = RuntimeEvent {
        id: golutra_core::EventId::new(),
        sequence_no: 1,
        session_id,
        turn_id: Some(TurnId::new()),
        task_id: Some(task_id),
        parent_event_id: None,
        event_type: RuntimeEventType::TaskCreated,
        timestamp: Utc::now(),
        source: RuntimeEventSource::Runtime,
        payload: json!({"summary": "durable task"}),
        payload_ref: None,
        durable: true,
    };
    let store = RuntimeStore::connect(&database_url).await.expect("store");
    store.append_event(&event).await.expect("event");
    drop(store);

    let reopened = RuntimeStore::connect(&database_url)
        .await
        .expect("reopened");
    let state = reopened
        .query_state(session_id, None)
        .await
        .expect("projection");
    let states = reopened.list_session_states().await.expect("states");

    assert_eq!(state.active_task_id, Some(task_id));
    assert_eq!(state.task_status, TaskStatus::Running);
    assert_eq!(states, vec![state]);
}
