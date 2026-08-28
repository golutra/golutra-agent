use chrono::Utc;
use golutra_core::{
    Actor, ActorKind, ArtifactId, BusyPolicy, CommandId, EventId, EvidenceId, EvidenceStrength,
    LaneId, PostTaskJob, PostTaskJobId, PostTaskJobKind, PostTaskJobStatus,
    RUNTIME_EVENT_SCHEMA_VERSION, RedactionStatus, RuntimeLane, TaskId, TaskStatus, ToolCallId,
    ToolResultStatus, TurnId, WorkspaceId,
};
use golutra_protocol::{ArtifactReadRequest, RuntimeEventSource, RuntimeEventType};
use serde_json::json;
use tempfile::tempdir;

use super::*;

#[tokio::test]
async fn command_journal_atomically_records_receipt_and_completion() {
    let store = RuntimeStore::in_memory().await.expect("store");
    let session_id = SessionId::new();
    let command_id = CommandId::new();
    let provisional = CommandAck {
        command_id,
        accepted: true,
        reason: Some("processing".to_owned()),
    };
    let ack = CommandAck {
        command_id,
        accepted: true,
        reason: Some("accepted".to_owned()),
    };
    let event = |event_type| RuntimeEvent {
        schema_version: golutra_core::RUNTIME_EVENT_SCHEMA_VERSION,
        causal_context: Default::default(),
        causal_links: Vec::new(),
        id: EventId::new(),
        sequence_no: 0,
        session_id,
        turn_id: None,
        task_id: None,
        parent_event_id: None,
        event_type,
        timestamp: Utc::now(),
        source: RuntimeEventSource::Runtime,
        payload: json!({"command_id": command_id}),
        payload_ref: None,
        durable: true,
    };

    let claim = store
        .claim_command(
            "same-command",
            command_id,
            &provisional,
            event(RuntimeEventType::CommandReceived),
        )
        .await
        .expect("claim command");
    assert!(matches!(
        claim,
        CommandClaim::Claimed {
            receipt_event: Some(_)
        }
    ));
    assert_eq!(
        store.command_ack("same-command").await.expect("load ack"),
        Some(provisional)
    );

    let completed = store
        .complete_command(
            "same-command",
            command_id,
            &ack,
            event(RuntimeEventType::CommandCompleted),
        )
        .await
        .expect("complete command");

    assert_eq!(completed.sequence_no, 2);
    assert_eq!(
        store.command_ack("same-command").await.expect("load ack"),
        Some(ack)
    );
    let events = store
        .load_events(session_id, None, None)
        .await
        .expect("command journal events");
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].event_type, RuntimeEventType::CommandReceived);
    assert_eq!(events[1].event_type, RuntimeEventType::CommandCompleted);
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
        schema_version: golutra_core::RUNTIME_EVENT_SCHEMA_VERSION,
        causal_context: Default::default(),
        causal_links: Vec::new(),
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
async fn single_writer_store_uses_a_self_contained_rollback_journal() {
    let directory = tempdir().expect("directory");
    let database_url = format!(
        "sqlite://{}",
        directory.path().join("runtime.sqlite").display()
    );
    let store = RuntimeStore::connect_single_writer_with_artifact_root(
        &database_url,
        directory.path().join("artifacts"),
    )
    .await
    .expect("single-writer store");

    let journal_mode: String = sqlx::query_scalar("PRAGMA journal_mode")
        .fetch_one(&store.pool)
        .await
        .expect("journal mode");
    assert_eq!(journal_mode, "delete");
    let integrity: String = sqlx::query_scalar("PRAGMA integrity_check")
        .fetch_one(&store.pool)
        .await
        .expect("integrity check");
    assert_eq!(integrity, "ok");
}

#[tokio::test]
async fn event_pages_advance_from_the_last_sequence_cursor() {
    let store = RuntimeStore::in_memory().await.expect("store");
    let session_id = SessionId::new();
    for index in 0..5 {
        store
            .append_event_assigning_sequence(RuntimeEvent {
                schema_version: golutra_core::RUNTIME_EVENT_SCHEMA_VERSION,
                causal_context: Default::default(),
                causal_links: Vec::new(),
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
async fn model_history_pages_filter_telemetry_and_honor_snapshot_boundary() {
    let store = RuntimeStore::in_memory().await.expect("store");
    let session_id = SessionId::new();
    let append = |event_type| RuntimeEvent {
        schema_version: golutra_core::RUNTIME_EVENT_SCHEMA_VERSION,
        causal_context: Default::default(),
        causal_links: Vec::new(),
        id: golutra_core::EventId::new(),
        sequence_no: 0,
        session_id,
        turn_id: Some(TurnId::new()),
        task_id: None,
        parent_event_id: None,
        event_type,
        timestamp: Utc::now(),
        source: RuntimeEventSource::Runtime,
        payload: json!({"content": format!("{event_type:?}")}),
        payload_ref: None,
        durable: true,
    };

    for event_type in [
        RuntimeEventType::TaskCreated,
        RuntimeEventType::ProviderStreamed,
        RuntimeEventType::AssistantMessage,
        RuntimeEventType::ToolProgress,
        RuntimeEventType::TurnUpdated,
        RuntimeEventType::CompactionCompleted,
    ] {
        store
            .append_event_assigning_sequence(append(event_type))
            .await
            .expect("event");
    }
    let through_sequence_no = store
        .max_sequence_no_for_session(session_id)
        .await
        .expect("snapshot sequence");
    store
        .append_event_assigning_sequence(append(RuntimeEventType::AssistantMessage))
        .await
        .expect("post-snapshot event");

    let first = store
        .load_model_history_page(session_id, None, through_sequence_no, 2)
        .await
        .expect("first history page");
    let second = store
        .load_model_history_page(
            session_id,
            Some(first[1].sequence_no),
            through_sequence_no,
            2,
        )
        .await
        .expect("second history page");
    let recent = store
        .load_recent_model_history(session_id, through_sequence_no, 2)
        .await
        .expect("recent history");

    assert_eq!(
        first
            .iter()
            .map(|event| event.event_type)
            .collect::<Vec<_>>(),
        vec![
            RuntimeEventType::TaskCreated,
            RuntimeEventType::AssistantMessage
        ]
    );
    assert_eq!(
        second
            .iter()
            .map(|event| event.event_type)
            .collect::<Vec<_>>(),
        vec![
            RuntimeEventType::TurnUpdated,
            RuntimeEventType::CompactionCompleted
        ]
    );
    assert_eq!(recent.len(), 2);
    assert_eq!(recent[0].event_type, RuntimeEventType::TurnUpdated);
    assert_eq!(recent[1].event_type, RuntimeEventType::CompactionCompleted);
    assert!(
        recent
            .iter()
            .all(|event| event.sequence_no <= through_sequence_no)
    );
}

#[tokio::test]
async fn model_history_window_is_not_shrunk_by_high_volume_telemetry() {
    let store = RuntimeStore::in_memory().await.expect("store");
    let session_id = SessionId::new();
    let turn_id = TurnId::new();
    let mut transaction = store.pool.begin().await.expect("transaction");
    for sequence_no in 1..=16_400_u64 {
        let event_type = if sequence_no % 2 == 1 {
            RuntimeEventType::AssistantMessage
        } else {
            RuntimeEventType::ProviderStreamed
        };
        let event = RuntimeEvent {
            schema_version: RUNTIME_EVENT_SCHEMA_VERSION,
            causal_context: Default::default(),
            causal_links: Vec::new(),
            id: EventId::new(),
            sequence_no,
            session_id,
            turn_id: Some(turn_id),
            task_id: None,
            parent_event_id: None,
            event_type,
            timestamp: Utc::now(),
            source: RuntimeEventSource::Runtime,
            payload: json!({"content": format!("message-{sequence_no}")}),
            payload_ref: None,
            durable: true,
        };
        sqlx::query(
            "INSERT INTO runtime_events (
                event_id, sequence_no, session_id, task_id, turn_id, event_type, source,
                durable, payload_json, event_json
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(event.id.to_string())
        .bind(i64::try_from(sequence_no).expect("sequence fits sqlite"))
        .bind(session_id.to_string())
        .bind(Option::<String>::None)
        .bind(turn_id.to_string())
        .bind(format!("{event_type:?}"))
        .bind(format!("{:?}", event.source))
        .bind(event.durable)
        .bind(serde_json::to_string(&event.payload).expect("payload"))
        .bind(serde_json::to_string(&event).expect("event"))
        .execute(&mut *transaction)
        .await
        .expect("event row");
    }
    transaction.commit().await.expect("commit");

    let through_sequence_no = store
        .max_sequence_no_for_session(session_id)
        .await
        .expect("snapshot sequence");
    let recent = store
        .load_recent_model_history(session_id, through_sequence_no, 8_192)
        .await
        .expect("recent model history");
    let mut cursor = None;
    let mut paged = Vec::new();
    loop {
        let page = store
            .load_model_history_page(session_id, cursor, through_sequence_no, 257)
            .await
            .expect("history page");
        let Some(last) = page.last() else {
            break;
        };
        cursor = Some(last.sequence_no);
        paged.extend(page);
    }

    assert_eq!(recent.len(), 8_192);
    assert_eq!(recent.first().map(|event| event.sequence_no), Some(17));
    assert_eq!(recent.last().map(|event| event.sequence_no), Some(16_399));
    assert_eq!(paged.len(), 8_200);
    assert_eq!(paged.first().map(|event| event.sequence_no), Some(1));
    assert_eq!(paged.last().map(|event| event.sequence_no), Some(16_399));
    assert!(paged.windows(2).all(|events| {
        events[0].sequence_no < events[1].sequence_no
            && events[0].event_type == RuntimeEventType::AssistantMessage
            && events[1].event_type == RuntimeEventType::AssistantMessage
    }));
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
                schema_version: golutra_core::RUNTIME_EVENT_SCHEMA_VERSION,
                causal_context: Default::default(),
                causal_links: Vec::new(),
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

    store
        .append_event_assigning_sequence(RuntimeEvent {
            schema_version: golutra_core::RUNTIME_EVENT_SCHEMA_VERSION,
            causal_context: Default::default(),
            causal_links: Vec::new(),
            id: golutra_core::EventId::new(),
            sequence_no: 0,
            session_id,
            turn_id: None,
            task_id: None,
            parent_event_id: None,
            event_type: RuntimeEventType::CompactionCompleted,
            timestamp: Utc::now(),
            source: RuntimeEventSource::Runtime,
            payload: json!({"mode": "automatic", "content": "latest automatic summary"}),
            payload_ref: Some(ArtifactId::new()),
            durable: true,
        })
        .await
        .expect("automatic compaction");

    let latest = store
        .load_latest_context_compaction(session_id)
        .await
        .expect("query")
        .expect("context compaction");
    let explicit = store
        .load_latest_explicit_compaction(session_id)
        .await
        .expect("query")
        .expect("explicit compaction");

    assert_eq!(latest.payload["content"], "latest automatic summary");
    assert_eq!(explicit.payload["content"], "latest summary");
}

#[tokio::test]
async fn appends_events_and_reduces_state() {
    let store = RuntimeStore::in_memory().await.expect("store opens");
    let session_id = SessionId::new();
    let task_id = TaskId::new();
    let event = RuntimeEvent {
        schema_version: golutra_core::RUNTIME_EVENT_SCHEMA_VERSION,
        causal_context: Default::default(),
        causal_links: Vec::new(),
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
        schema_version: golutra_core::RUNTIME_EVENT_SCHEMA_VERSION,
        causal_context: Default::default(),
        causal_links: Vec::new(),
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
        schema_version: golutra_core::RUNTIME_EVENT_SCHEMA_VERSION,
        causal_context: Default::default(),
        causal_links: Vec::new(),
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
        schema_version: golutra_core::RUNTIME_EVENT_SCHEMA_VERSION,
        causal_context: Default::default(),
        causal_links: Vec::new(),
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

#[test]
fn projection_turns_reconciled_uncertain_task_into_interrupted_state() {
    let session_id = SessionId::new();
    let task_id = TaskId::new();
    let event = RuntimeEvent {
        schema_version: golutra_core::RUNTIME_EVENT_SCHEMA_VERSION,
        causal_context: Default::default(),
        causal_links: Vec::new(),
        id: golutra_core::EventId::new(),
        sequence_no: 1,
        session_id,
        turn_id: Some(TurnId::new()),
        task_id: Some(task_id),
        parent_event_id: None,
        event_type: RuntimeEventType::TaskReconciled,
        timestamp: Utc::now(),
        source: RuntimeEventSource::Runtime,
        payload: json!({"status": "interrupted"}),
        payload_ref: None,
        durable: true,
    };

    let projection = RuntimeStore::reduce_state(session_id, &[event]);

    assert_eq!(projection.active_task_id, Some(task_id));
    assert_eq!(projection.task_status, TaskStatus::Interrupted);
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

#[cfg(unix)]
#[tokio::test]
async fn identical_artifact_blobs_share_content_storage() {
    use std::os::unix::fs::MetadataExt;

    let store = RuntimeStore::in_memory().await.expect("store opens");
    let bytes = b"same immutable artifact payload";
    let first = ArtifactRecord {
        artifact_id: ArtifactId::new(),
        session_id: SessionId::new(),
        turn_id: Some(TurnId::new()),
        tool_call_id: None,
        artifact_type: "workspace_change_manifest".to_owned(),
        uri: "artifact://fixture/first".to_owned(),
        checksum: artifact_checksum(bytes),
        size_bytes: bytes.len() as u64,
        created_at: Utc::now(),
        producer: "test".to_owned(),
        redaction_status: RedactionStatus::Redacted,
        retention_policy: "test".to_owned(),
        provenance_refs: Vec::new(),
    };
    let mut second = first.clone();
    second.artifact_id = ArtifactId::new();
    second.uri = "artifact://fixture/second".to_owned();

    store
        .store_artifact(&first, bytes)
        .await
        .expect("first artifact");
    store
        .store_artifact(&second, bytes)
        .await
        .expect("second artifact");
    let canonical = store
        .find_artifact_by_content(
            first.session_id,
            &first.artifact_type,
            &first.checksum,
            first.size_bytes,
        )
        .await
        .expect("content lookup")
        .expect("canonical artifact");

    let first_metadata = std::fs::metadata(store.artifact_blob_path(first.artifact_id))
        .expect("first blob metadata");
    let second_metadata = std::fs::metadata(store.artifact_blob_path(second.artifact_id))
        .expect("second blob metadata");
    assert_eq!(first_metadata.ino(), second_metadata.ino());
    assert!(first_metadata.nlink() >= 2);
    assert_eq!(canonical.artifact_id, first.artifact_id);
    assert_eq!(
        store
            .load_artifact_bytes(second.artifact_id)
            .await
            .expect("second blob"),
        Some(bytes.to_vec())
    );
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
async fn storage_maintenance_expires_debug_blobs_but_keeps_metadata() {
    let store = RuntimeStore::in_memory().await.expect("store opens");
    let bytes = b"expired debug payload";
    let artifact = ArtifactRecord {
        artifact_id: ArtifactId::new(),
        session_id: SessionId::new(),
        turn_id: Some(TurnId::new()),
        tool_call_id: None,
        artifact_type: "provider_raw_metadata".to_owned(),
        uri: "artifact://fixture/expired".to_owned(),
        checksum: artifact_checksum(bytes),
        size_bytes: bytes.len() as u64,
        created_at: Utc::now() - chrono::Duration::days(31),
        producer: "test".to_owned(),
        redaction_status: RedactionStatus::Redacted,
        retention_policy: "debug_default".to_owned(),
        provenance_refs: Vec::new(),
    };
    store
        .store_artifact(&artifact, bytes)
        .await
        .expect("artifact");

    let report = store
        .run_artifact_maintenance(Utc::now())
        .await
        .expect("maintenance");

    assert_eq!(report.artifact_blobs_removed, 1);
    assert_eq!(
        store
            .load_artifact_bytes(artifact.artifact_id)
            .await
            .expect("expired blob"),
        None
    );
    assert_eq!(
        store
            .load_artifact(artifact.artifact_id)
            .await
            .expect("metadata"),
        Some(artifact)
    );
    let stats = store.storage_stats().await.expect("stats");
    assert_eq!(stats.artifact_records, 1);
    assert_eq!(stats.expired_artifact_blobs, 1);
    assert_eq!(stats.live_artifact_bytes, 0);
}

#[tokio::test]
async fn storage_maintenance_preserves_evidence_backed_artifacts() {
    let store = RuntimeStore::in_memory().await.expect("store opens");
    let bytes = b"durable evidence";
    let artifact = ArtifactRecord {
        artifact_id: ArtifactId::new(),
        session_id: SessionId::new(),
        turn_id: Some(TurnId::new()),
        tool_call_id: None,
        artifact_type: "verification".to_owned(),
        uri: "artifact://fixture/evidence".to_owned(),
        checksum: artifact_checksum(bytes),
        size_bytes: bytes.len() as u64,
        created_at: Utc::now() - chrono::Duration::days(31),
        producer: "test".to_owned(),
        redaction_status: RedactionStatus::NotRequired,
        retention_policy: "debug_default".to_owned(),
        provenance_refs: Vec::new(),
    };
    store
        .store_artifact(&artifact, bytes)
        .await
        .expect("artifact");
    store
        .store_evidence(&EvidenceRecord {
            evidence_id: EvidenceId::new(),
            claim: "verification output exists".to_owned(),
            artifact_refs: vec![artifact.artifact_id],
            source_event_refs: Vec::new(),
            evidence_strength: EvidenceStrength::Strong,
            verifier: "test".to_owned(),
            confidence: 1.0,
            limitations: "fixture".to_owned(),
        })
        .await
        .expect("evidence");

    let report = store
        .run_artifact_maintenance(Utc::now())
        .await
        .expect("maintenance");

    assert_eq!(report.artifact_blobs_removed, 0);
    assert_eq!(report.protected_artifacts_skipped, 1);
    assert_eq!(
        store
            .load_artifact_bytes(artifact.artifact_id)
            .await
            .expect("protected blob"),
        Some(bytes.to_vec())
    );
}

#[tokio::test]
async fn temporary_artifact_pruning_tolerates_writer_cleanup_after_directory_scan() {
    let root = tempdir().expect("artifact root");
    let path = root.path().join("artifact.tmp-writer");
    tokio::fs::write(&path, b"in flight")
        .await
        .expect("temporary artifact");
    let mut entries = tokio::fs::read_dir(root.path())
        .await
        .expect("artifact directory");
    let entry = entries
        .next_entry()
        .await
        .expect("directory entry")
        .expect("temporary entry");
    tokio::fs::remove_file(&path).await.expect("writer cleanup");

    assert!(
        !prune_temporary_artifact_entry(entry)
            .await
            .expect("concurrent cleanup is benign")
    );
}

#[tokio::test]
async fn debug_projection_includes_events_and_artifacts() {
    let store = RuntimeStore::in_memory().await.expect("store opens");
    let session_id = SessionId::new();
    let mut event = RuntimeEvent {
        schema_version: golutra_core::RUNTIME_EVENT_SCHEMA_VERSION,
        causal_context: Default::default(),
        causal_links: Vec::new(),
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
    event.payload_ref = Some(artifact.artifact_id);
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
        removed: false,
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
    assert!(!loaded.removed);
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].session_id, thread.session_id);
}

#[tokio::test]
async fn removed_threads_retain_ownership_and_events_but_leave_normal_windows() {
    let store = RuntimeStore::in_memory().await.expect("store opens");
    let now = Utc::now();
    let session_id = SessionId::new();
    let task_id = TaskId::new();
    let thread = ThreadRecord {
        thread_id: ThreadId::new(),
        session_id,
        parent_thread_id: None,
        forked_from_turn_id: None,
        forked_from_sequence_no: None,
        workspace_root: Some("/workspace".to_owned()),
        rebound_from_workspace_root: None,
        rollout_path: None,
        title: "Removed thread".to_owned(),
        preview: "retained for audit".to_owned(),
        created_at: now,
        updated_at: now,
        recency_at: now,
        archived: false,
        removed: false,
    };
    store.upsert_thread(&thread).await.expect("thread");
    store
        .append_event_assigning_sequence(RuntimeEvent {
            schema_version: golutra_core::RUNTIME_EVENT_SCHEMA_VERSION,
            causal_context: Default::default(),
            causal_links: Vec::new(),
            id: EventId::new(),
            sequence_no: 0,
            session_id,
            turn_id: Some(TurnId::new()),
            task_id: Some(task_id),
            parent_event_id: None,
            event_type: RuntimeEventType::TaskCompleted,
            timestamp: now,
            source: RuntimeEventSource::Runtime,
            payload: json!({
                "status": "completed",
                "post_task_governance": {"status": "pending"}
            }),
            payload_ref: None,
            durable: true,
        })
        .await
        .expect("terminal event");
    let deleted = store
        .delete_thread_with_event(
            thread.thread_id,
            RuntimeEvent {
                schema_version: golutra_core::RUNTIME_EVENT_SCHEMA_VERSION,
                causal_context: Default::default(),
                causal_links: Vec::new(),
                id: EventId::new(),
                sequence_no: 0,
                session_id,
                turn_id: None,
                task_id: None,
                parent_event_id: None,
                event_type: RuntimeEventType::ThreadDeleted,
                timestamp: now,
                source: RuntimeEventSource::User,
                payload: json!({"thread_id": thread.thread_id}),
                payload_ref: None,
                durable: true,
            },
        )
        .await
        .expect("remove thread")
        .expect("delete event");

    let retained = store
        .thread_by_id(thread.thread_id)
        .await
        .expect("thread by id")
        .expect("retained tombstone");
    assert!(retained.removed);
    assert!(retained.archived);
    assert_eq!(
        store
            .thread_by_session(session_id)
            .await
            .expect("thread by session")
            .expect("ownership tombstone")
            .thread_id,
        thread.thread_id
    );
    assert!(
        store
            .list_threads(Some("/workspace"), 10)
            .await
            .expect("thread list")
            .is_empty()
    );
    assert!(
        store
            .list_threads_page(Some("/workspace"), None, 10)
            .await
            .expect("thread page")
            .is_empty()
    );
    assert!(
        store
            .thread_window(
                Some("/workspace"),
                &retained,
                SessionRangeDirection::Single,
                1,
            )
            .await
            .expect("thread window")
            .is_empty()
    );
    assert_eq!(
        store
            .unscheduled_post_task_terminal_events(Some("/workspace"))
            .await
            .expect("post-task recovery"),
        store
            .load_events(session_id, Some(task_id), None)
            .await
            .expect("task events")
            .into_iter()
            .filter(|event| event.event_type == RuntimeEventType::TaskCompleted)
            .collect::<Vec<_>>()
    );
    assert_eq!(deleted.event_type, RuntimeEventType::ThreadDeleted);
    assert!(
        store
            .load_events(session_id, None, None)
            .await
            .expect("session events")
            .iter()
            .any(|event| event.event_type == RuntimeEventType::ThreadDeleted)
    );
}

#[tokio::test]
async fn fork_transaction_copies_boundary_history_and_remaps_runtime_ids() {
    let store = RuntimeStore::in_memory().await.expect("store opens");
    let parent_session_id = SessionId::new();
    let parent_task_id = TaskId::new();
    let parent_turn_id = TurnId::new();
    let first = store
        .append_event_assigning_sequence(RuntimeEvent {
            schema_version: golutra_core::RUNTIME_EVENT_SCHEMA_VERSION,
            causal_context: Default::default(),
            causal_links: Vec::new(),
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
            schema_version: golutra_core::RUNTIME_EVENT_SCHEMA_VERSION,
            causal_context: Default::default(),
            causal_links: Vec::new(),
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
            schema_version: golutra_core::RUNTIME_EVENT_SCHEMA_VERSION,
            causal_context: Default::default(),
            causal_links: Vec::new(),
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
        removed: false,
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
        removed: false,
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
async fn thread_metadata_and_event_commit_or_rollback_together() {
    let store = RuntimeStore::in_memory().await.expect("store");
    let now = Utc::now();
    let session_id = SessionId::new();
    let mut thread = ThreadRecord {
        thread_id: ThreadId::new(),
        session_id,
        parent_thread_id: None,
        forked_from_turn_id: None,
        forked_from_sequence_no: None,
        workspace_root: Some("/workspace".to_owned()),
        rebound_from_workspace_root: None,
        rollout_path: None,
        title: "before".to_owned(),
        preview: "preview".to_owned(),
        created_at: now,
        updated_at: now,
        recency_at: now,
        archived: false,
        removed: false,
    };
    store.upsert_thread(&thread).await.expect("thread");
    let thread_id = thread.thread_id;
    let event_id = EventId::new();
    let event = |event_type| RuntimeEvent {
        schema_version: golutra_core::RUNTIME_EVENT_SCHEMA_VERSION,
        causal_context: Default::default(),
        causal_links: Vec::new(),
        id: event_id,
        sequence_no: 0,
        session_id,
        turn_id: None,
        task_id: None,
        parent_event_id: None,
        event_type,
        timestamp: Utc::now(),
        source: RuntimeEventSource::User,
        payload: json!({"thread_id": thread_id}),
        payload_ref: None,
        durable: true,
    };
    store
        .append_event_assigning_sequence(event(RuntimeEventType::CommandReceived))
        .await
        .expect("conflicting event");

    thread.title = "after".to_owned();
    assert!(
        store
            .upsert_thread_with_event(&thread, event(RuntimeEventType::ThreadRenamed))
            .await
            .is_err()
    );
    assert_eq!(
        store
            .thread_by_id(thread_id)
            .await
            .expect("thread lookup")
            .expect("thread remains")
            .title,
        "before"
    );

    assert!(
        store
            .delete_thread_with_event(thread_id, event(RuntimeEventType::ThreadDeleted))
            .await
            .is_err()
    );
    assert!(
        store
            .thread_by_id(thread_id)
            .await
            .expect("thread lookup")
            .is_some()
    );
    assert_eq!(store.max_sequence_no().await.expect("sequence"), 1);
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
    let child_session_id = SessionId::new();
    let child_thread_id = ThreadId::new();
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
    sqlx::query(
        r#"
            INSERT INTO threads (
                thread_id, session_id, parent_thread_id, workspace_root, title, preview,
                created_at, updated_at, recency_at, archived
            ) VALUES (?, ?, ?, '/workspace', 'Child', 'Child preview', ?, ?, ?, 0)
            "#,
    )
    .bind(child_thread_id.to_string())
    .bind(child_session_id.to_string())
    .bind(older_thread_id.to_string())
    .bind("2026-07-12T00:00:00Z")
    .bind("2026-07-12T00:00:00Z")
    .bind("2026-07-12T00:00:00Z")
    .execute(&pool)
    .await
    .expect("legacy child thread");
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
    assert!(!thread.removed);
    let child = store
        .thread_by_id(child_thread_id)
        .await
        .expect("child thread query")
        .expect("child thread");
    assert_eq!(child.parent_thread_id, Some(newer_thread_id));
    let now = Utc::now();
    let forked = ThreadRecord {
        thread_id: ThreadId::new(),
        session_id: SessionId::new(),
        parent_thread_id: Some(child.thread_id),
        forked_from_turn_id: None,
        forked_from_sequence_no: Some(0),
        workspace_root: child.workspace_root.clone(),
        rebound_from_workspace_root: None,
        rollout_path: None,
        title: "Fork after migration".to_owned(),
        preview: child.preview.clone(),
        created_at: now,
        updated_at: now,
        recency_at: now,
        archived: false,
        removed: false,
    };
    let forked_events = store
        .create_forked_thread(&forked, child.session_id, 0)
        .await
        .expect("fork after migration");
    assert!(forked_events.is_empty());
    assert_eq!(
        store
            .thread_by_id(forked.thread_id)
            .await
            .expect("forked thread query")
            .expect("forked thread")
            .parent_thread_id,
        Some(child.thread_id)
    );
    assert!(
        store
            .thread_by_id(older_thread_id)
            .await
            .expect("duplicate thread query")
            .is_none()
    );
    assert_eq!(
        store
            .list_threads(Some("/workspace"), 10)
            .await
            .expect("thread list")
            .len(),
        3
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
        schema_version: golutra_core::RUNTIME_EVENT_SCHEMA_VERSION,
        causal_context: Default::default(),
        causal_links: Vec::new(),
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

#[tokio::test]
async fn legacy_projection_without_verification_check_kind_remains_readable() {
    let store = RuntimeStore::in_memory().await.expect("store opens");
    let session_id = SessionId::new();
    let task_id = TaskId::new();
    let projection = json!({
        "session_id": session_id,
        "active_task_id": task_id,
        "task_status": "completed",
        "runtime_lane": null,
        "last_sequence_no": 1,
        "visible_steps": [],
        "pending_approval": null,
        "final_message": "done",
        "last_loop_decision": null,
        "last_verification": {
            "verification_id": golutra_core::VerificationId::new(),
            "task_id": task_id,
            "objective": "respond",
            "completion_criteria": ["assistant responds"],
            "checks": [{
                "name": "assistant_response",
                "command": null,
                "passed": true,
                "evidence_refs": [],
                "message": "assistant response produced"
            }],
            "evidence_refs": [],
            "result": "pass",
            "policy_status": "conversation_response",
            "residual_risks": []
        }
    });
    sqlx::query(
        "INSERT INTO state_projections (
            session_id, last_sequence_no, projection_json, updated_at
         ) VALUES (?, 1, ?, ?)",
    )
    .bind(session_id.to_string())
    .bind(projection.to_string())
    .bind(Utc::now().to_rfc3339())
    .execute(&store.pool)
    .await
    .expect("legacy projection");

    let restored = store
        .query_state(session_id, None)
        .await
        .expect("legacy projection remains readable");

    assert_eq!(
        restored.last_verification.expect("verification").checks[0].kind,
        golutra_core::VerificationCheckKind::AssistantResponse
    );
}

#[tokio::test]
async fn post_task_job_queue_event_and_job_commit_atomically() {
    let store = RuntimeStore::in_memory().await.expect("store");
    let session_id = SessionId::new();
    let task_id = TaskId::new();
    let job = PostTaskJob {
        job_id: PostTaskJobId::new(),
        kind: PostTaskJobKind::DeepEvaluation,
        workspace_id: WorkspaceId::new().to_string(),
        session_id: session_id.to_string(),
        task_id,
        input_refs: vec![format!("task:{task_id}")],
        status: PostTaskJobStatus::Queued,
        attempt: 0,
        max_attempts: 3,
        lease_owner: None,
        lease_expires_at: None,
        result_refs: Vec::new(),
        last_error: None,
        created_at: Utc::now(),
        started_at: None,
        completed_at: None,
    };
    let event = RuntimeEvent {
        schema_version: golutra_core::RUNTIME_EVENT_SCHEMA_VERSION,
        causal_context: Default::default(),
        causal_links: Vec::new(),
        id: EventId::new(),
        sequence_no: 0,
        session_id,
        turn_id: Some(TurnId::new()),
        task_id: Some(task_id),
        parent_event_id: None,
        event_type: RuntimeEventType::PostTaskJobQueued,
        timestamp: Utc::now(),
        source: RuntimeEventSource::Evaluator,
        payload: json!({"job_id": job.job_id}),
        payload_ref: None,
        durable: true,
    };
    let committed = store
        .enqueue_post_task_job_with_event(&job, event)
        .await
        .expect("atomic queue")
        .expect("new job");

    assert_eq!(committed.sequence_no, 1);
    assert_eq!(
        store
            .post_task_job(task_id)
            .await
            .expect("job")
            .unwrap()
            .job_id,
        job.job_id
    );
    assert_eq!(
        store
            .load_events(session_id, None, None)
            .await
            .expect("events")
            .len(),
        1
    );

    let mut duplicate = job.clone();
    duplicate.job_id = PostTaskJobId::new();
    let duplicate_event = RuntimeEvent {
        id: EventId::new(),
        sequence_no: 0,
        payload: json!({"job_id": duplicate.job_id}),
        ..committed
    };
    assert!(
        store
            .enqueue_post_task_job_with_event(&duplicate, duplicate_event)
            .await
            .expect("duplicate queue is idempotent")
            .is_none()
    );
    assert_eq!(
        store
            .list_post_task_jobs(task_id)
            .await
            .expect("jobs")
            .len(),
        1
    );
    assert_eq!(
        store
            .load_events(session_id, None, None)
            .await
            .expect("events")
            .len(),
        1
    );
}

#[tokio::test]
async fn unscheduled_post_task_scan_requires_pending_governance_without_terminal_failure() {
    let store = RuntimeStore::in_memory().await.expect("store");
    let session_id = SessionId::new();
    let task_id = TaskId::new();
    let workspace_root = "/workspace/recovery";
    let now = Utc::now();
    store
        .upsert_thread(&ThreadRecord {
            thread_id: ThreadId::new(),
            session_id,
            parent_thread_id: None,
            forked_from_turn_id: None,
            forked_from_sequence_no: None,
            workspace_root: Some(workspace_root.to_owned()),
            rebound_from_workspace_root: None,
            rollout_path: None,
            title: "recovery".to_owned(),
            preview: "recovery".to_owned(),
            created_at: now,
            updated_at: now,
            recency_at: now,
            archived: false,
            removed: false,
        })
        .await
        .expect("thread");
    let terminal = RuntimeEvent {
        schema_version: golutra_core::RUNTIME_EVENT_SCHEMA_VERSION,
        causal_context: Default::default(),
        causal_links: Vec::new(),
        id: EventId::new(),
        sequence_no: 0,
        session_id,
        turn_id: Some(TurnId::new()),
        task_id: Some(task_id),
        parent_event_id: None,
        event_type: RuntimeEventType::TaskCompleted,
        timestamp: now,
        source: RuntimeEventSource::Runtime,
        payload: json!({
            "status": "failed",
            "post_task_governance": {"status": "pending"}
        }),
        payload_ref: None,
        durable: true,
    };
    store
        .append_event_assigning_sequence(terminal)
        .await
        .expect("terminal");

    assert_eq!(
        store
            .unscheduled_post_task_terminal_events(Some(workspace_root))
            .await
            .expect("pending terminals")
            .len(),
        1
    );
    assert!(
        store
            .unscheduled_post_task_terminal_events(Some("/other/workspace"))
            .await
            .expect("foreign terminals")
            .is_empty()
    );

    store
        .append_event_assigning_sequence(RuntimeEvent {
            id: EventId::new(),
            sequence_no: 0,
            event_type: RuntimeEventType::PostTaskStageFailed,
            payload: json!({"phase": "evaluation_scheduling", "terminal": true}),
            ..store
                .load_events(session_id, Some(task_id), None)
                .await
                .expect("events")
                .into_iter()
                .next()
                .expect("terminal event")
        })
        .await
        .expect("stage failure");
    assert!(
        store
            .unscheduled_post_task_terminal_events(Some(workspace_root))
            .await
            .expect("terminal failure suppresses recovery")
            .is_empty()
    );
}

#[tokio::test]
async fn expired_post_task_lease_is_requeued_and_retry_can_reset_terminal_job() {
    let store = RuntimeStore::in_memory().await.expect("store");
    let task_id = TaskId::new();
    let now = Utc::now();
    let workspace_id = WorkspaceId::new().to_string();
    let job = PostTaskJob {
        job_id: PostTaskJobId::new(),
        kind: PostTaskJobKind::DeepEvaluation,
        workspace_id: workspace_id.clone(),
        session_id: SessionId::new().to_string(),
        task_id,
        input_refs: Vec::new(),
        status: PostTaskJobStatus::Queued,
        attempt: 0,
        max_attempts: 2,
        lease_owner: None,
        lease_expires_at: None,
        result_refs: Vec::new(),
        last_error: None,
        created_at: now,
        started_at: None,
        completed_at: None,
    };
    store.enqueue_post_task_job(&job).await.expect("enqueue");
    let claimed = store
        .claim_post_task_job("worker-a", now, chrono::Duration::seconds(-1))
        .await
        .expect("claim")
        .expect("claimed job");
    assert_eq!(claimed.status, PostTaskJobStatus::Leased);
    assert_eq!(
        store
            .recover_expired_post_task_jobs(&workspace_id, now)
            .await
            .expect("recover"),
        1
    );
    assert_eq!(
        store
            .post_task_job(task_id)
            .await
            .expect("job")
            .unwrap()
            .status,
        PostTaskJobStatus::Queued
    );

    let claimed = store
        .claim_post_task_job("worker-b", now, chrono::Duration::minutes(1))
        .await
        .expect("claim again")
        .expect("claimed again");
    store
        .start_post_task_job(claimed.job_id, "worker-b", now)
        .await
        .expect("start");
    store
        .finish_post_task_job(
            claimed.job_id,
            "worker-b",
            PostTaskJobStatus::Failed,
            &[],
            Some("failed"),
            now,
        )
        .await
        .expect("finish");
    assert!(
        store
            .retry_post_task_job(claimed.job_id)
            .await
            .expect("retry")
    );
    assert_eq!(
        store
            .post_task_job(task_id)
            .await
            .expect("job")
            .unwrap()
            .attempt,
        0
    );
}

#[tokio::test]
async fn post_task_claim_is_scoped_to_the_worker_workspace() {
    let store = RuntimeStore::in_memory().await.expect("store");
    let now = Utc::now();
    let workspace_a = WorkspaceId::new().to_string();
    let workspace_b = WorkspaceId::new().to_string();
    let workspace_c = WorkspaceId::new().to_string();
    let job_a = PostTaskJob {
        job_id: PostTaskJobId::new(),
        kind: PostTaskJobKind::DeepEvaluation,
        workspace_id: workspace_a.clone(),
        session_id: SessionId::new().to_string(),
        task_id: TaskId::new(),
        input_refs: Vec::new(),
        status: PostTaskJobStatus::Queued,
        attempt: 0,
        max_attempts: 2,
        lease_owner: None,
        lease_expires_at: None,
        result_refs: Vec::new(),
        last_error: None,
        created_at: now,
        started_at: None,
        completed_at: None,
    };
    let job_b = PostTaskJob {
        job_id: PostTaskJobId::new(),
        workspace_id: workspace_b.clone(),
        session_id: SessionId::new().to_string(),
        task_id: TaskId::new(),
        ..job_a.clone()
    };
    store.enqueue_post_task_job(&job_a).await.expect("job a");
    store.enqueue_post_task_job(&job_b).await.expect("job b");

    let claimed_b = store
        .claim_post_task_job_for_workspace(
            "worker-b",
            &workspace_b,
            now,
            chrono::Duration::minutes(1),
        )
        .await
        .expect("workspace b claim")
        .expect("workspace b job");
    let unrelated = store
        .claim_post_task_job_for_workspace(
            "worker-c",
            &workspace_c,
            now,
            chrono::Duration::minutes(1),
        )
        .await
        .expect("unrelated workspace claim");
    let claimed_a = store
        .claim_post_task_job_for_workspace(
            "worker-a",
            &workspace_a,
            now,
            chrono::Duration::minutes(1),
        )
        .await
        .expect("workspace a claim")
        .expect("workspace a job");

    assert_eq!(claimed_b.job_id, job_b.job_id);
    assert_eq!(claimed_b.workspace_id, workspace_b);
    assert!(unrelated.is_none());
    assert_eq!(claimed_a.job_id, job_a.job_id);
    assert_eq!(claimed_a.workspace_id, workspace_a);
}

#[tokio::test]
async fn nonterminal_post_task_query_is_scoped_and_status_aware() {
    let store = RuntimeStore::in_memory().await.expect("store");
    let now = Utc::now();
    let make_job = |workspace_id: &str, status: PostTaskJobStatus| PostTaskJob {
        job_id: PostTaskJobId::new(),
        kind: PostTaskJobKind::DeepEvaluation,
        workspace_id: workspace_id.to_owned(),
        session_id: SessionId::new().to_string(),
        task_id: TaskId::new(),
        input_refs: Vec::new(),
        status,
        attempt: 0,
        max_attempts: 2,
        lease_owner: None,
        lease_expires_at: None,
        result_refs: Vec::new(),
        last_error: None,
        created_at: now,
        started_at: None,
        completed_at: None,
    };

    for status in [
        PostTaskJobStatus::Queued,
        PostTaskJobStatus::Leased,
        PostTaskJobStatus::Running,
    ] {
        let workspace_id = WorkspaceId::new().to_string();
        let job = make_job(&workspace_id, status);
        store.enqueue_post_task_job(&job).await.expect("active job");
        assert!(
            store
                .has_nonterminal_post_task_jobs(&workspace_id)
                .await
                .expect("active status query")
        );
    }

    let terminal_workspace = WorkspaceId::new().to_string();
    for status in [
        PostTaskJobStatus::Succeeded,
        PostTaskJobStatus::Failed,
        PostTaskJobStatus::Cancelled,
    ] {
        let job = make_job(&terminal_workspace, status);
        store
            .enqueue_post_task_job(&job)
            .await
            .expect("terminal job");
    }
    assert!(
        !store
            .has_nonterminal_post_task_jobs(&terminal_workspace)
            .await
            .expect("terminal status query")
    );
    assert!(
        !store
            .has_nonterminal_post_task_jobs("workspace-with-no-jobs")
            .await
            .expect("empty status query")
    );
}

#[tokio::test]
async fn expired_post_task_recovery_does_not_mutate_another_workspace() {
    let store = RuntimeStore::in_memory().await.expect("store");
    let now = Utc::now();
    let workspace_a = WorkspaceId::new().to_string();
    let workspace_b = WorkspaceId::new().to_string();
    let job_a = PostTaskJob {
        job_id: PostTaskJobId::new(),
        kind: PostTaskJobKind::DeepEvaluation,
        workspace_id: workspace_a.clone(),
        session_id: SessionId::new().to_string(),
        task_id: TaskId::new(),
        input_refs: Vec::new(),
        status: PostTaskJobStatus::Queued,
        attempt: 0,
        max_attempts: 2,
        lease_owner: None,
        lease_expires_at: None,
        result_refs: Vec::new(),
        last_error: None,
        created_at: now,
        started_at: None,
        completed_at: None,
    };
    let job_b = PostTaskJob {
        job_id: PostTaskJobId::new(),
        workspace_id: workspace_b.clone(),
        session_id: SessionId::new().to_string(),
        task_id: TaskId::new(),
        ..job_a.clone()
    };
    store.enqueue_post_task_job(&job_a).await.expect("job a");
    store.enqueue_post_task_job(&job_b).await.expect("job b");
    for (worker, workspace) in [("worker-a", &workspace_a), ("worker-b", &workspace_b)] {
        store
            .claim_post_task_job_for_workspace(
                worker,
                workspace,
                now,
                chrono::Duration::seconds(-1),
            )
            .await
            .expect("claim")
            .expect("leased job");
    }

    assert_eq!(
        store
            .recover_expired_post_task_jobs(&workspace_a, now)
            .await
            .expect("recover workspace a"),
        1
    );
    let recovered_a = store
        .post_task_job(job_a.task_id)
        .await
        .expect("job a")
        .expect("job a exists");
    let untouched_b = store
        .post_task_job(job_b.task_id)
        .await
        .expect("job b")
        .expect("job b exists");
    assert_eq!(recovered_a.status, PostTaskJobStatus::Queued);
    assert_eq!(untouched_b.status, PostTaskJobStatus::Leased);
    assert_eq!(untouched_b.attempt, 1);
    assert_eq!(untouched_b.lease_owner.as_deref(), Some("worker-b"));
}

#[tokio::test]
async fn expired_post_task_lease_is_failed_when_retry_budget_is_exhausted() {
    let directory = tempdir().expect("directory");
    let database_url = format!(
        "sqlite://{}",
        directory.path().join("runtime.sqlite").display()
    );
    let store = RuntimeStore::connect(&database_url).await.expect("store");
    let task_id = TaskId::new();
    let now = Utc::now();
    let workspace_id = WorkspaceId::new().to_string();
    let job = PostTaskJob {
        job_id: PostTaskJobId::new(),
        kind: PostTaskJobKind::DeepEvaluation,
        workspace_id: workspace_id.clone(),
        session_id: SessionId::new().to_string(),
        task_id,
        input_refs: vec![format!("task:{task_id}")],
        status: PostTaskJobStatus::Queued,
        attempt: 0,
        max_attempts: 1,
        lease_owner: None,
        lease_expires_at: None,
        result_refs: Vec::new(),
        last_error: None,
        created_at: now,
        started_at: None,
        completed_at: None,
    };
    store.enqueue_post_task_job(&job).await.expect("enqueue");
    store
        .claim_post_task_job("worker", now, chrono::Duration::seconds(-1))
        .await
        .expect("claim")
        .expect("claimed");

    assert_eq!(
        store
            .recover_expired_post_task_jobs(&workspace_id, now)
            .await
            .expect("recover"),
        1
    );
    let failed = store
        .post_task_job(task_id)
        .await
        .expect("job")
        .expect("failed job");
    assert_eq!(failed.status, PostTaskJobStatus::Failed);
    assert!(failed.completed_at.is_some());
    drop(store);

    let reopened = RuntimeStore::connect(&database_url).await.expect("reopen");
    let persisted = reopened
        .post_task_job(task_id)
        .await
        .expect("persisted job")
        .expect("persisted failed job");
    assert_eq!(persisted.status, PostTaskJobStatus::Failed);
    assert!(
        reopened
            .claim_post_task_job("other-worker", now, chrono::Duration::minutes(1))
            .await
            .expect("claim after exhaustion")
            .is_none()
    );
}

#[tokio::test]
async fn artifact_range_reads_are_bounded_and_full_reads_verify_checksums() {
    let store = RuntimeStore::in_memory().await.expect("store");
    let bytes = b"abcdef";
    let artifact = ArtifactRecord {
        artifact_id: ArtifactId::new(),
        session_id: SessionId::new(),
        turn_id: Some(TurnId::new()),
        tool_call_id: Some(ToolCallId::new()),
        artifact_type: "stdout".to_owned(),
        uri: "artifact://fixture/range".to_owned(),
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
        .expect("artifact");

    let range = store
        .read_artifact_range(&ArtifactReadRequest {
            artifact_id: artifact.artifact_id,
            offset: 2,
            length: 3,
        })
        .await
        .expect("range")
        .expect("range exists");
    assert_eq!(range.bytes, b"cde");
    assert_eq!(range.offset, 2);
    assert_eq!(range.artifact.size_bytes, 6);
    assert!(
        store
            .read_artifact_range(&ArtifactReadRequest {
                artifact_id: artifact.artifact_id,
                offset: 0,
                length: 0,
            })
            .await
            .is_err()
    );
    assert!(
        store
            .read_artifact_range(&ArtifactReadRequest {
                artifact_id: artifact.artifact_id,
                offset: 0,
                length: MAX_ARTIFACT_READ_BYTES.saturating_add(1),
            })
            .await
            .is_err()
    );

    tokio::fs::write(store.artifact_blob_path(artifact.artifact_id), b"tampered")
        .await
        .expect("tamper fixture");
    let range_error = store
        .read_artifact_range(&ArtifactReadRequest {
            artifact_id: artifact.artifact_id,
            offset: 0,
            length: 3,
        })
        .await
        .expect_err("range size mismatch");
    assert!(range_error.to_string().contains("size mismatch"));
    let error = store
        .load_artifact_bytes(artifact.artifact_id)
        .await
        .expect_err("checksum mismatch");
    assert!(error.to_string().contains("checksum"));
}

fn tool_completion_artifact(
    artifact_id: ArtifactId,
    session_id: SessionId,
    event_id: EventId,
    bytes: &[u8],
    label: &str,
) -> ArtifactRecord {
    ArtifactRecord {
        artifact_id,
        session_id,
        turn_id: Some(TurnId::new()),
        tool_call_id: Some(ToolCallId::new()),
        artifact_type: "tool_result".to_owned(),
        uri: format!("artifact://fixture/{label}"),
        checksum: artifact_checksum(bytes),
        size_bytes: u64::try_from(bytes.len()).expect("fixture size"),
        created_at: Utc::now(),
        producer: "test".to_owned(),
        redaction_status: RedactionStatus::NotRequired,
        retention_policy: "test".to_owned(),
        provenance_refs: vec![event_id],
    }
}

fn tool_completed_event(
    event_id: EventId,
    session_id: SessionId,
    artifact_id: ArtifactId,
    evidence_id: EvidenceId,
) -> RuntimeEvent {
    RuntimeEvent {
        schema_version: golutra_core::RUNTIME_EVENT_SCHEMA_VERSION,
        causal_context: Default::default(),
        causal_links: Vec::new(),
        id: event_id,
        sequence_no: 0,
        session_id,
        turn_id: Some(TurnId::new()),
        task_id: Some(TaskId::new()),
        parent_event_id: None,
        event_type: RuntimeEventType::ToolCompleted,
        timestamp: Utc::now(),
        source: RuntimeEventSource::Tool,
        payload: json!({
            "summary": "tool completed",
            "envelope": ToolResultEnvelope {
                tool_call_id: ToolCallId::new(),
                tool_name: "shell".to_owned(),
                status: ToolResultStatus::Ok,
                summary: "tool completed".to_owned(),
                structured_facts: json!({"exit_code": 0}),
                model_visible_excerpt: Some("ok".to_owned()),
                raw_artifact_ref: Some(artifact_id),
                evidence_refs: vec![evidence_id],
                risk: "low".to_owned(),
                verification_hint: None,
            },
        }),
        payload_ref: Some(artifact_id),
        durable: true,
    }
}

#[cfg(unix)]
#[tokio::test]
async fn tool_completed_bundle_atomically_commits_facts_and_deduplicates_blobs() {
    use std::os::unix::fs::MetadataExt;

    let store = RuntimeStore::in_memory().await.expect("store");
    let session_id = SessionId::new();
    let event_id = EventId::new();
    let evidence_id = EvidenceId::new();
    let first_id = ArtifactId::new();
    let second_id = ArtifactId::new();
    let bytes = b"shared tool completion output";
    let first = tool_completion_artifact(first_id, session_id, event_id, bytes, "first");
    let second = tool_completion_artifact(second_id, session_id, event_id, bytes, "second");
    let evidence = EvidenceRecord {
        evidence_id,
        claim: "tool output was captured".to_owned(),
        artifact_refs: vec![first_id, second_id],
        source_event_refs: vec![event_id],
        evidence_strength: EvidenceStrength::Strong,
        verifier: "test".to_owned(),
        confidence: 1.0,
        limitations: "fixture".to_owned(),
    };

    let committed = store
        .repositories()
        .events
        .append_tool_completed_bundle(
            tool_completed_event(event_id, session_id, first_id, evidence_id),
            &[
                (first.clone(), bytes.to_vec()),
                (second.clone(), bytes.to_vec()),
            ],
            std::slice::from_ref(&evidence),
        )
        .await
        .expect("atomic bundle");

    assert_eq!(committed.sequence_no, 1);
    assert_eq!(
        store.load_artifact(first_id).await.expect("first"),
        Some(first)
    );
    assert_eq!(
        store.load_artifact(second_id).await.expect("second"),
        Some(second)
    );
    assert_eq!(
        store
            .load_evidence_by_ids(&HashSet::from([evidence_id]))
            .await
            .expect("evidence"),
        vec![evidence]
    );
    assert_eq!(
        store
            .load_events(session_id, None, None)
            .await
            .expect("events"),
        vec![committed]
    );
    let projection = store
        .query_state(session_id, None)
        .await
        .expect("projection");
    assert_eq!(projection.last_sequence_no, 1);
    let first_metadata = std::fs::metadata(store.artifact_blob_path(first_id)).expect("first blob");
    let second_metadata =
        std::fs::metadata(store.artifact_blob_path(second_id)).expect("second blob");
    assert_eq!(first_metadata.ino(), second_metadata.ino());
}

#[tokio::test]
async fn tool_completed_bundle_rolls_back_database_facts_and_unique_blob_on_event_conflict() {
    let store = RuntimeStore::in_memory().await.expect("store");
    let conflicting_event_id = EventId::new();
    let existing_session_id = SessionId::new();
    store
        .append_event_assigning_sequence(RuntimeEvent {
            schema_version: golutra_core::RUNTIME_EVENT_SCHEMA_VERSION,
            causal_context: Default::default(),
            causal_links: Vec::new(),
            id: conflicting_event_id,
            sequence_no: 0,
            session_id: existing_session_id,
            turn_id: None,
            task_id: None,
            parent_event_id: None,
            event_type: RuntimeEventType::ToolStarted,
            timestamp: Utc::now(),
            source: RuntimeEventSource::Tool,
            payload: json!({"summary": "existing event"}),
            payload_ref: None,
            durable: true,
        })
        .await
        .expect("conflicting event");

    let session_id = SessionId::new();
    let artifact_id = ArtifactId::new();
    let evidence_id = EvidenceId::new();
    let bytes = b"unique rollback output";
    let artifact = tool_completion_artifact(
        artifact_id,
        session_id,
        conflicting_event_id,
        bytes,
        "rollback",
    );
    let evidence = EvidenceRecord {
        evidence_id,
        claim: "must roll back".to_owned(),
        artifact_refs: vec![artifact_id],
        source_event_refs: vec![conflicting_event_id],
        evidence_strength: EvidenceStrength::Strong,
        verifier: "test".to_owned(),
        confidence: 1.0,
        limitations: "fixture".to_owned(),
    };

    store
        .append_tool_completed_bundle(
            tool_completed_event(conflicting_event_id, session_id, artifact_id, evidence_id),
            &[(artifact, bytes.to_vec())],
            &[evidence],
        )
        .await
        .expect_err("event primary key conflict must roll back bundle");

    assert_eq!(
        store.load_artifact(artifact_id).await.expect("metadata"),
        None
    );
    assert!(
        store
            .load_evidence_by_ids(&HashSet::from([evidence_id]))
            .await
            .expect("evidence")
            .is_empty()
    );
    assert!(
        store
            .load_events(session_id, None, None)
            .await
            .expect("target events")
            .is_empty()
    );
    assert!(!store.artifact_blob_path(artifact_id).exists());
    assert_eq!(store.max_sequence_no().await.expect("sequence"), 1);
}

#[tokio::test]
async fn tool_completed_bundle_compensates_final_link_when_temporary_cleanup_fails() {
    let store = RuntimeStore::in_memory().await.expect("store");
    let session_id = SessionId::new();
    let event_id = EventId::new();
    let artifact_id = ArtifactId::new();
    let evidence_id = EvidenceId::new();
    let bytes = b"cleanup failure output";
    let artifact =
        tool_completion_artifact(artifact_id, session_id, event_id, bytes, "cleanup-failure");
    let evidence = EvidenceRecord {
        evidence_id,
        claim: "must not survive prewrite failure".to_owned(),
        artifact_refs: vec![artifact_id],
        source_event_refs: vec![event_id],
        evidence_strength: EvidenceStrength::Strong,
        verifier: "test".to_owned(),
        confidence: 1.0,
        limitations: "fixture".to_owned(),
    };
    inject_temporary_cleanup_failure(artifact_id);

    let error = store
        .append_tool_completed_bundle(
            tool_completed_event(event_id, session_id, artifact_id, evidence_id),
            &[(artifact, bytes.to_vec())],
            &[evidence],
        )
        .await
        .expect_err("temporary cleanup failure must abort the bundle");

    assert!(
        error
            .to_string()
            .contains("injected temporary cleanup failure")
    );
    assert!(!store.artifact_blob_path(artifact_id).exists());
    assert_eq!(
        store.load_artifact(artifact_id).await.expect("metadata"),
        None
    );
    assert!(
        store
            .load_evidence_by_ids(&HashSet::from([evidence_id]))
            .await
            .expect("evidence")
            .is_empty()
    );
    assert!(
        store
            .load_events(session_id, None, None)
            .await
            .expect("events")
            .is_empty()
    );
    assert_eq!(store.max_sequence_no().await.expect("sequence"), 0);
    let temporary = std::fs::read_dir(&store.artifact_root)
        .expect("artifact directory")
        .collect::<Result<Vec<_>, _>>()
        .expect("artifact entries")
        .into_iter()
        .map(|entry| entry.path())
        .find(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.contains(".tmp-"))
        })
        .expect("failed cleanup leaves its temporary file for maintenance");
    let old = std::time::SystemTime::now()
        .checked_sub(Duration::from_secs(
            TEMPORARY_ARTIFACT_RETENTION_HOURS * 60 * 60 + 1,
        ))
        .expect("old timestamp");
    std::fs::File::open(&temporary)
        .expect("temporary file")
        .set_times(std::fs::FileTimes::new().set_modified(old))
        .expect("age temporary file");

    let report = store
        .run_artifact_maintenance(Utc::now())
        .await
        .expect("maintenance");
    assert_eq!(report.temporary_artifacts_removed, 1);
    assert!(!temporary.exists());
}

#[tokio::test]
async fn concurrent_bundle_rollback_cannot_remove_a_committed_blob() {
    let directory = tempdir().expect("directory");
    let database_url = format!(
        "sqlite://{}",
        directory.path().join("runtime.sqlite").display()
    );
    let artifact_root = directory.path().join("artifacts");
    let first = RuntimeStore::connect_with_artifact_root(&database_url, &artifact_root)
        .await
        .expect("first store");
    let second = RuntimeStore::connect_with_artifact_root(&database_url, &artifact_root)
        .await
        .expect("second store");
    let conflicting_event_id = EventId::new();
    first
        .append_event_assigning_sequence(RuntimeEvent {
            schema_version: golutra_core::RUNTIME_EVENT_SCHEMA_VERSION,
            causal_context: Default::default(),
            causal_links: Vec::new(),
            id: conflicting_event_id,
            sequence_no: 0,
            session_id: SessionId::new(),
            turn_id: None,
            task_id: None,
            parent_event_id: None,
            event_type: RuntimeEventType::ToolStarted,
            timestamp: Utc::now(),
            source: RuntimeEventSource::Tool,
            payload: json!({"summary": "conflicting event"}),
            payload_ref: None,
            durable: true,
        })
        .await
        .expect("conflicting event");

    let session_id = SessionId::new();
    let success_event_id = EventId::new();
    let artifact_id = ArtifactId::new();
    let bytes = b"concurrent immutable output";
    let artifact = tool_completion_artifact(
        artifact_id,
        session_id,
        success_event_id,
        bytes,
        "concurrent",
    );
    let success_evidence_id = EvidenceId::new();
    let failed_evidence_id = EvidenceId::new();
    let evidence = |evidence_id, event_id| EvidenceRecord {
        evidence_id,
        claim: "concurrent bundle output".to_owned(),
        artifact_refs: vec![artifact_id],
        source_event_refs: vec![event_id],
        evidence_strength: EvidenceStrength::Strong,
        verifier: "test".to_owned(),
        confidence: 1.0,
        limitations: "fixture".to_owned(),
    };
    let failed_artifacts = [(artifact.clone(), bytes.to_vec())];
    let committed_artifacts = [(artifact.clone(), bytes.to_vec())];
    let failed_evidence = [evidence(failed_evidence_id, conflicting_event_id)];
    let committed_evidence = [evidence(success_evidence_id, success_event_id)];

    let (failed, committed) = tokio::join!(
        first.append_tool_completed_bundle(
            tool_completed_event(
                conflicting_event_id,
                session_id,
                artifact_id,
                failed_evidence_id,
            ),
            &failed_artifacts,
            &failed_evidence,
        ),
        second.append_tool_completed_bundle(
            tool_completed_event(
                success_event_id,
                session_id,
                artifact_id,
                success_evidence_id,
            ),
            &committed_artifacts,
            &committed_evidence,
        ),
    );

    failed.expect_err("conflicting bundle must roll back");
    committed.expect("other bundle commits");
    assert_eq!(
        first
            .load_artifact_bytes(artifact_id)
            .await
            .expect("committed blob"),
        Some(bytes.to_vec())
    );
    assert_eq!(
        first.load_artifact(artifact_id).await.expect("metadata"),
        Some(artifact)
    );
}

#[tokio::test]
async fn failed_tombstone_retry_removes_the_recreated_blob() {
    let store = RuntimeStore::in_memory().await.expect("store");
    let session_id = SessionId::new();
    let artifact_id = ArtifactId::new();
    let event_id = EventId::new();
    let bytes = b"expired output retried";
    let mut artifact =
        tool_completion_artifact(artifact_id, session_id, event_id, bytes, "tombstone");
    artifact.created_at = Utc::now() - chrono::Duration::days(31);
    artifact.retention_policy = "debug_default".to_owned();
    store
        .store_artifact(&artifact, bytes)
        .await
        .expect("initial artifact");
    store
        .run_artifact_maintenance(Utc::now())
        .await
        .expect("expire artifact");
    assert!(!store.artifact_blob_path(artifact_id).exists());

    store
        .append_event_assigning_sequence(RuntimeEvent {
            schema_version: golutra_core::RUNTIME_EVENT_SCHEMA_VERSION,
            causal_context: Default::default(),
            causal_links: Vec::new(),
            id: event_id,
            sequence_no: 0,
            session_id: SessionId::new(),
            turn_id: None,
            task_id: None,
            parent_event_id: None,
            event_type: RuntimeEventType::ToolStarted,
            timestamp: Utc::now(),
            source: RuntimeEventSource::Tool,
            payload: json!({"summary": "event collision"}),
            payload_ref: None,
            durable: true,
        })
        .await
        .expect("conflicting event");
    let evidence_id = EvidenceId::new();
    let evidence = EvidenceRecord {
        evidence_id,
        claim: "retry must roll back".to_owned(),
        artifact_refs: vec![artifact_id],
        source_event_refs: vec![event_id],
        evidence_strength: EvidenceStrength::Strong,
        verifier: "test".to_owned(),
        confidence: 1.0,
        limitations: "fixture".to_owned(),
    };

    store
        .append_tool_completed_bundle(
            tool_completed_event(event_id, session_id, artifact_id, evidence_id),
            &[(artifact, bytes.to_vec())],
            &[evidence],
        )
        .await
        .expect_err("event collision rolls back retry");

    assert!(!store.artifact_blob_path(artifact_id).exists());
    let tombstoned: Option<String> =
        sqlx::query_scalar("SELECT blob_deleted_at FROM artifact_records WHERE artifact_id = ?")
            .bind(artifact_id.to_string())
            .fetch_one(&store.pool)
            .await
            .expect("tombstone state");
    assert!(tombstoned.is_some());
}

#[tokio::test]
async fn maintenance_removes_final_blobs_without_live_metadata() {
    let store = RuntimeStore::in_memory().await.expect("store");
    tokio::fs::create_dir_all(&store.artifact_root)
        .await
        .expect("artifact root");
    let orphan_id = ArtifactId::new();
    let orphan_path = store.artifact_blob_path(orphan_id);
    tokio::fs::write(&orphan_path, b"cancelled precommit")
        .await
        .expect("orphan blob");

    let report = store
        .run_artifact_maintenance(Utc::now())
        .await
        .expect("maintenance");

    assert_eq!(report.artifact_blobs_removed, 1);
    assert!(!orphan_path.exists());
}

#[tokio::test]
async fn tool_completed_bundle_accepts_an_empty_tool_output_artifact() {
    let store = RuntimeStore::in_memory().await.expect("store");
    let session_id = SessionId::new();
    let event_id = EventId::new();
    let artifact_id = ArtifactId::new();
    let evidence_id = EvidenceId::new();
    let bytes = b"";
    let artifact =
        tool_completion_artifact(artifact_id, session_id, event_id, bytes, "empty-output");
    let evidence = EvidenceRecord {
        evidence_id,
        claim: "empty output is still an observed result".to_owned(),
        artifact_refs: vec![artifact_id],
        source_event_refs: vec![event_id],
        evidence_strength: EvidenceStrength::Medium,
        verifier: "test".to_owned(),
        confidence: 1.0,
        limitations: "fixture".to_owned(),
    };

    let committed = store
        .append_tool_completed_bundle(
            tool_completed_event(event_id, session_id, artifact_id, evidence_id),
            &[(artifact.clone(), bytes.to_vec())],
            std::slice::from_ref(&evidence),
        )
        .await
        .expect("empty artifact bundle");

    assert_eq!(committed.sequence_no, 1);
    assert_eq!(
        store.load_artifact(artifact_id).await.expect("metadata"),
        Some(artifact)
    );
    assert_eq!(
        store
            .load_artifact_bytes(artifact_id)
            .await
            .expect("artifact bytes"),
        Some(Vec::new())
    );
    assert_eq!(
        store
            .load_evidence_by_ids(&HashSet::from([evidence_id]))
            .await
            .expect("evidence"),
        vec![evidence]
    );
}

#[tokio::test]
async fn cancelled_bundle_callers_leave_commit_and_compensation_running() {
    let store = RuntimeStore::in_memory().await.expect("store");

    let committed_session_id = SessionId::new();
    let committed_event_id = EventId::new();
    let committed_artifact_id = ArtifactId::new();
    let committed_evidence_id = EvidenceId::new();
    let committed_bytes = b"commit survives caller cancellation";
    let committed_artifact = tool_completion_artifact(
        committed_artifact_id,
        committed_session_id,
        committed_event_id,
        committed_bytes,
        "cancelled-commit",
    );
    let committed_evidence = EvidenceRecord {
        evidence_id: committed_evidence_id,
        claim: "cancelled caller still commits".to_owned(),
        artifact_refs: vec![committed_artifact_id],
        source_event_refs: vec![committed_event_id],
        evidence_strength: EvidenceStrength::Strong,
        verifier: "test".to_owned(),
        confidence: 1.0,
        limitations: "fixture".to_owned(),
    };
    let pause = install_store_test_pause(
        &store.database_identity,
        StoreTestPausePoint::BundleAfterBlobPublication,
    );
    let operation = {
        let store = store.clone();
        let artifact = committed_artifact.clone();
        let evidence = committed_evidence.clone();
        tokio::spawn(async move {
            store
                .append_tool_completed_bundle(
                    tool_completed_event(
                        committed_event_id,
                        committed_session_id,
                        committed_artifact_id,
                        committed_evidence_id,
                    ),
                    &[(artifact, committed_bytes.to_vec())],
                    &[evidence],
                )
                .await
        })
    };
    pause.reached.notified().await;
    assert!(store.artifact_blob_path(committed_artifact_id).exists());
    operation.abort();
    assert!(
        operation
            .await
            .expect_err("caller cancelled")
            .is_cancelled()
    );
    pause.release.notify_one();

    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if store
                .load_artifact(committed_artifact_id)
                .await
                .expect("committed metadata")
                .is_some()
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("detached commit completes");
    assert_eq!(
        store
            .load_artifact_bytes(committed_artifact_id)
            .await
            .expect("committed bytes"),
        Some(committed_bytes.to_vec())
    );

    let conflicting_event_id = EventId::new();
    store
        .append_event_assigning_sequence(RuntimeEvent {
            schema_version: golutra_core::RUNTIME_EVENT_SCHEMA_VERSION,
            causal_context: Default::default(),
            causal_links: Vec::new(),
            id: conflicting_event_id,
            sequence_no: 0,
            session_id: SessionId::new(),
            turn_id: None,
            task_id: None,
            parent_event_id: None,
            event_type: RuntimeEventType::ToolStarted,
            timestamp: Utc::now(),
            source: RuntimeEventSource::Tool,
            payload: json!({"summary": "conflicting event"}),
            payload_ref: None,
            durable: true,
        })
        .await
        .expect("conflicting event");
    let failed_session_id = SessionId::new();
    let failed_artifact_id = ArtifactId::new();
    let failed_evidence_id = EvidenceId::new();
    let failed_bytes = b"compensation survives caller cancellation";
    let failed_artifact = tool_completion_artifact(
        failed_artifact_id,
        failed_session_id,
        conflicting_event_id,
        failed_bytes,
        "cancelled-compensation",
    );
    let failed_evidence = EvidenceRecord {
        evidence_id: failed_evidence_id,
        claim: "cancelled caller still compensates".to_owned(),
        artifact_refs: vec![failed_artifact_id],
        source_event_refs: vec![conflicting_event_id],
        evidence_strength: EvidenceStrength::Strong,
        verifier: "test".to_owned(),
        confidence: 1.0,
        limitations: "fixture".to_owned(),
    };
    let pause = install_store_test_pause(
        &store.database_identity,
        StoreTestPausePoint::BundleAfterBlobPublication,
    );
    let operation = {
        let store = store.clone();
        tokio::spawn(async move {
            store
                .append_tool_completed_bundle(
                    tool_completed_event(
                        conflicting_event_id,
                        failed_session_id,
                        failed_artifact_id,
                        failed_evidence_id,
                    ),
                    &[(failed_artifact, failed_bytes.to_vec())],
                    &[failed_evidence],
                )
                .await
        })
    };
    pause.reached.notified().await;
    assert!(store.artifact_blob_path(failed_artifact_id).exists());
    operation.abort();
    assert!(
        operation
            .await
            .expect_err("caller cancelled")
            .is_cancelled()
    );
    pause.release.notify_one();

    tokio::time::timeout(Duration::from_secs(5), async {
        while store.artifact_blob_path(failed_artifact_id).exists() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("detached compensation completes");
    assert_eq!(
        store
            .load_artifact(failed_artifact_id)
            .await
            .expect("rolled back metadata"),
        None
    );
    assert!(
        store
            .load_evidence_by_ids(&HashSet::from([failed_evidence_id]))
            .await
            .expect("rolled back evidence")
            .is_empty()
    );
}

#[tokio::test]
async fn cancelled_store_artifact_caller_leaves_the_commit_running() {
    let store = RuntimeStore::in_memory().await.expect("store");
    let bytes = b"standalone artifact commit survives cancellation";
    let artifact = tool_completion_artifact(
        ArtifactId::new(),
        SessionId::new(),
        EventId::new(),
        bytes,
        "cancelled-store-artifact",
    );
    let pause = install_store_test_pause(
        &store.database_identity,
        StoreTestPausePoint::ArtifactAfterBlobPublication,
    );
    let operation = {
        let store = store.clone();
        let artifact = artifact.clone();
        tokio::spawn(async move { store.store_artifact(&artifact, bytes).await })
    };
    pause.reached.notified().await;
    assert!(store.artifact_blob_path(artifact.artifact_id).exists());
    operation.abort();
    assert!(
        operation
            .await
            .expect_err("caller cancelled")
            .is_cancelled()
    );
    pause.release.notify_one();

    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if store
                .load_artifact(artifact.artifact_id)
                .await
                .expect("metadata")
                .is_some()
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("detached artifact commit completes");
    assert_eq!(
        store
            .load_artifact_bytes(artifact.artifact_id)
            .await
            .expect("bytes"),
        Some(bytes.to_vec())
    );
}

#[tokio::test]
async fn cancelled_maintenance_finishes_deleting_a_durable_tombstone() {
    let store = RuntimeStore::in_memory().await.expect("store");
    let bytes = b"expired cancellation fixture";
    let mut artifact = tool_completion_artifact(
        ArtifactId::new(),
        SessionId::new(),
        EventId::new(),
        bytes,
        "cancelled-maintenance",
    );
    artifact.created_at = Utc::now() - chrono::Duration::days(31);
    artifact.retention_policy = "debug_default".to_owned();
    store
        .store_artifact(&artifact, bytes)
        .await
        .expect("artifact");

    let pause = install_store_test_pause(
        &store.database_identity,
        StoreTestPausePoint::MaintenanceAfterTombstone,
    );
    let operation = {
        let store = store.clone();
        tokio::spawn(async move { store.run_artifact_maintenance(Utc::now()).await })
    };
    pause.reached.notified().await;
    let tombstone: Option<String> =
        sqlx::query_scalar("SELECT blob_deleted_at FROM artifact_records WHERE artifact_id = ?")
            .bind(artifact.artifact_id.to_string())
            .fetch_one(&store.pool)
            .await
            .expect("durable tombstone");
    assert!(tombstone.is_some());
    assert!(store.artifact_blob_path(artifact.artifact_id).exists());
    operation.abort();
    assert!(
        operation
            .await
            .expect_err("caller cancelled")
            .is_cancelled()
    );
    pause.release.notify_one();

    tokio::time::timeout(Duration::from_secs(5), async {
        while store.artifact_blob_path(artifact.artifact_id).exists() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("detached maintenance completes");
}

#[tokio::test]
async fn evidence_and_maintenance_share_the_artifact_lock() {
    let store = RuntimeStore::in_memory().await.expect("store");
    let bytes = b"protected during maintenance race";
    let mut artifact = tool_completion_artifact(
        ArtifactId::new(),
        SessionId::new(),
        EventId::new(),
        bytes,
        "evidence-race",
    );
    artifact.created_at = Utc::now() - chrono::Duration::days(31);
    artifact.retention_policy = "debug_default".to_owned();
    store
        .store_artifact(&artifact, bytes)
        .await
        .expect("artifact");
    let evidence = EvidenceRecord {
        evidence_id: EvidenceId::new(),
        claim: "artifact becomes protected before maintenance snapshots evidence".to_owned(),
        artifact_refs: vec![artifact.artifact_id],
        source_event_refs: Vec::new(),
        evidence_strength: EvidenceStrength::Strong,
        verifier: "test".to_owned(),
        confidence: 1.0,
        limitations: "fixture".to_owned(),
    };

    let pause = install_store_test_pause(
        &store.database_identity,
        StoreTestPausePoint::EvidenceAfterLock,
    );
    let evidence_operation = {
        let store = store.clone();
        let evidence = evidence.clone();
        tokio::spawn(async move { store.store_evidence(&evidence).await })
    };
    pause.reached.notified().await;
    let maintenance_operation = {
        let store = store.clone();
        tokio::spawn(async move { store.run_artifact_maintenance(Utc::now()).await })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!maintenance_operation.is_finished());
    pause.release.notify_one();
    evidence_operation
        .await
        .expect("evidence caller")
        .expect("evidence commit");
    let report = maintenance_operation
        .await
        .expect("maintenance caller")
        .expect("maintenance");

    assert_eq!(report.artifact_blobs_removed, 0);
    assert_eq!(report.protected_artifacts_skipped, 1);
    assert_eq!(
        store
            .load_artifact_bytes(artifact.artifact_id)
            .await
            .expect("protected blob"),
        Some(bytes.to_vec())
    );
}

#[tokio::test]
async fn artifact_root_is_bound_to_one_logical_database() {
    let directory = tempdir().expect("directory");
    let artifact_root = directory.path().join("shared-artifacts");
    let first_database_url = format!(
        "sqlite://{}",
        directory.path().join("first.sqlite").display()
    );
    let second_database_url = format!(
        "sqlite://{}",
        directory.path().join("second.sqlite").display()
    );

    RuntimeStore::connect_with_artifact_root(&first_database_url, &artifact_root)
        .await
        .expect("first database claims root");
    RuntimeStore::connect_with_artifact_root(&first_database_url, &artifact_root)
        .await
        .expect("same database may reopen root");
    let error = RuntimeStore::connect_with_artifact_root(&second_database_url, &artifact_root)
        .await
        .expect_err("different database must not share root");

    assert!(error.to_string().contains("different runtime database"));
    assert!(artifact_root.join(ARTIFACT_STORE_IDENTITY_FILE).is_file());
}

#[tokio::test]
async fn maintenance_reclaims_a_blob_left_behind_after_a_durable_tombstone() {
    let store = RuntimeStore::in_memory().await.expect("store");
    let bytes = b"tombstoned residual";
    let artifact = tool_completion_artifact(
        ArtifactId::new(),
        SessionId::new(),
        EventId::new(),
        bytes,
        "tombstone-residual",
    );
    store
        .store_artifact(&artifact, bytes)
        .await
        .expect("artifact");
    sqlx::query("UPDATE artifact_records SET blob_deleted_at = ? WHERE artifact_id = ?")
        .bind(Utc::now().to_rfc3339())
        .bind(artifact.artifact_id.to_string())
        .execute(&store.pool)
        .await
        .expect("simulate committed tombstone before process exit");
    assert!(store.artifact_blob_path(artifact.artifact_id).exists());

    let report = store
        .run_artifact_maintenance(Utc::now())
        .await
        .expect("maintenance");

    assert_eq!(report.artifact_blobs_removed, 1);
    assert!(!store.artifact_blob_path(artifact.artifact_id).exists());
}
