use std::{ffi::OsString, fs, sync::RwLock};

use golutra_auth::{AuthService, CredentialRef, MemorySecretStore, SecretKind, SecretStore};
use golutra_config::{
    ProviderConfigPaths, ProviderConfigScope, ProviderInstallPlan, ProviderProfile,
    ProviderSettings, runtime_env_from_settings,
};
use golutra_context::{
    ContextBuilder, ContextCompactionRecord, ContextContributor, context_snapshot_from_request,
    provider_request_from_plan,
};
use golutra_core::{
    Actor, ActorKind, ArtifactId, ArtifactRecord, CommandId, EvidenceId, FileChangeKind,
    FileChangeSummary, PostTaskJob, PostTaskJobId, PostTaskJobKind, PostTaskJobStatus, QueryId,
    RedactionStatus, TaskId, TaskStatus, ToolCallId, TraceView, TurnChangeSummary, TurnId,
    VerificationId, VerificationRecord, VerificationResult, WorkspaceId,
};
use golutra_llm::{ConfiguredProvider, MockProvider, ProviderError, ProviderMessage, ProviderRole};
use golutra_protocol::{
    ArtifactReadRequest, EventFilter, RuntimeEventType, RuntimeQueryKind, TaskTraceRequest,
};
use tempfile::{TempDir, tempdir};
use tokio::{
    sync::{Mutex, MutexGuard},
    time::{Duration, sleep},
};

use crate::event_codec::{ObservationIntegrityClass, observation_descriptor};
use crate::governance_commands::memory_support_matches;

use super::*;

#[test]
fn hosted_tasks_use_only_explicit_completion_criteria() {
    assert!(completion_criteria_from_payload(&json!({"prompt": "hello"})).is_empty());
    assert_eq!(
        completion_criteria_from_payload(&json!({
            "prompt": "change code",
            "completion_criteria": [" tests pass ", "", 7, "diff matches request"]
        })),
        vec!["tests pass".to_owned(), "diff matches request".to_owned()]
    );
}

#[test]
fn memory_support_requires_a_claim_related_objective() {
    let memory = "Objective: list workspace files\nVerified outcome: files listed";
    assert!(memory_support_matches("list workspace files again", memory));
    assert!(!memory_support_matches(
        "rotate provider credentials",
        memory
    ));
}

#[test]
fn observation_catalog_classifies_loop_facts_before_persistence() {
    let required = observation_descriptor(&RuntimeObservation::ToolStarted {
        tool_call_id: ToolCallId::new(),
        tool_name: "read_file".to_owned(),
        display_arguments: json!({"path": "README.md"}),
    });
    assert_eq!(required.event_type, RuntimeEventType::ToolStarted);
    assert_eq!(required.source, RuntimeEventSource::Tool);
    assert_eq!(required.integrity, ObservationIntegrityClass::Supporting);

    let diagnostic = observation_descriptor(&RuntimeObservation::ProviderStreamed {
        provider_id: "provider".to_owned(),
        model_id: "model".to_owned(),
        event: golutra_llm::ProviderStreamEvent::TextDelta {
            text: "delta".to_owned(),
        },
    });
    assert_eq!(diagnostic.event_type, RuntimeEventType::ProviderStreamed);
    assert_eq!(diagnostic.integrity, ObservationIntegrityClass::Diagnostic);

    let fallback = observation_descriptor(&RuntimeObservation::ProviderTransportFallback {
        provider_id: "provider".to_owned(),
        from_transport: "streaming".to_owned(),
        to_transport: "buffered".to_owned(),
        reason: "stream disconnected".to_owned(),
    });
    assert_eq!(
        fallback.event_type,
        RuntimeEventType::ProviderTransportFallback
    );
    assert_eq!(fallback.source, RuntimeEventSource::Runtime);
    assert_eq!(fallback.integrity, ObservationIntegrityClass::Supporting);
}

#[test]
fn provider_transport_fallback_event_preserves_recovery_facts() {
    let (event_type, source, payload) =
        trace_event_payload(AgentLoopTraceEvent::ProviderTransportFallback {
            provider_id: "openai".to_owned(),
            from_transport: "streaming".to_owned(),
            to_transport: "buffered".to_owned(),
            reason: "stream idle for 300000 ms".to_owned(),
        })
        .expect("transport fallback event");

    assert_eq!(event_type, RuntimeEventType::ProviderTransportFallback);
    assert_eq!(source, RuntimeEventSource::Runtime);
    assert_eq!(payload["provider_id"], "openai");
    assert_eq!(payload["from_transport"], "streaming");
    assert_eq!(payload["to_transport"], "buffered");
    assert_eq!(payload["reason"], "stream idle for 300000 ms");
}

static ENV_LOCK: Mutex<()> = Mutex::const_new(());

struct IsolatedGlobalMockProvider {
    previous_home: Option<OsString>,
    _home: TempDir,
    _guard: MutexGuard<'static, ()>,
}

impl IsolatedGlobalMockProvider {
    async fn empty() -> Self {
        let guard = ENV_LOCK.lock().await;
        let home = tempdir().expect("golutra home");
        let previous_home = std::env::var_os("GOLUTRA_HOME");
        unsafe {
            std::env::set_var("GOLUTRA_HOME", home.path());
        }
        Self {
            previous_home,
            _home: home,
            _guard: guard,
        }
    }

    async fn install() -> Self {
        let isolated = Self::empty().await;
        install_user_mock_provider();
        isolated
    }

    fn install_blocking() -> Self {
        let guard = ENV_LOCK.blocking_lock();
        let home = tempdir().expect("golutra home");
        let previous_home = std::env::var_os("GOLUTRA_HOME");
        unsafe {
            std::env::set_var("GOLUTRA_HOME", home.path());
        }
        install_user_mock_provider();
        Self {
            previous_home,
            _home: home,
            _guard: guard,
        }
    }
}

impl Drop for IsolatedGlobalMockProvider {
    fn drop(&mut self) {
        match &self.previous_home {
            Some(value) => unsafe {
                std::env::set_var("GOLUTRA_HOME", value);
            },
            None => unsafe {
                std::env::remove_var("GOLUTRA_HOME");
            },
        }
    }
}

#[test]
fn local_app_server_endpoint_requires_loopback_root_http_url() {
    for valid in ["http://127.0.0.1:47831", "http://[::1]:47831"] {
        validate_local_app_server_base_url(valid).expect("loopback endpoint");
    }

    for invalid in [
        "https://127.0.0.1:47831",
        "http://0.0.0.0:47831",
        "http://192.168.1.2:47831",
        "http://127.0.0.1:47831/runtime",
        "http://user@127.0.0.1:47831",
    ] {
        let error = validate_local_app_server_base_url(invalid)
            .expect_err("unsafe workspace endpoint must be rejected");
        assert!(error.to_string().contains("loopback address"));
    }
}

#[test]
fn remote_app_server_endpoint_requires_https_or_loopback_http() {
    for valid in [
        "https://runtime.example.com",
        "http://127.0.0.1:47831",
        "http://localhost:47831",
    ] {
        validate_remote_app_server_base_url(valid).expect("safe remote endpoint");
    }
    for invalid in [
        "http://runtime.example.com",
        "https://user@runtime.example.com",
        "https://runtime.example.com/api",
        "ftp://runtime.example.com",
    ] {
        validate_remote_app_server_base_url(invalid)
            .expect_err("unsafe remote endpoint must be rejected");
    }
}

#[test]
fn runtime_paths_reject_a_file_as_cwd() {
    let home = tempdir().expect("home");
    let directory = tempdir().expect("directory");
    let file = directory.path().join("not-a-directory");
    fs::write(&file, "content").expect("file");

    let error =
        RuntimePaths::from_home_and_cwd(home.path(), &file).expect_err("file cwd must be rejected");

    assert!(error.to_string().contains("cwd is not a directory"));
}

#[test]
fn session_and_command_leases_are_global_across_cwds() {
    let home = tempdir().expect("home");
    let cwd_a = tempdir().expect("cwd a");
    let cwd_b = tempdir().expect("cwd b");
    let paths_a = RuntimePaths::from_home_and_cwd(home.path(), cwd_a.path()).expect("paths a");
    let paths_b = RuntimePaths::from_home_and_cwd(home.path(), cwd_b.path()).expect("paths b");
    let session_id = SessionId::new();

    assert_eq!(
        paths_a.session_lock(session_id),
        paths_b.session_lock(session_id)
    );
    assert_eq!(
        paths_a.command_lock("shared-command"),
        paths_b.command_lock("shared-command")
    );
    assert_ne!(paths_a.memory_file, paths_b.memory_file);
}

#[test]
fn http_transport_uses_the_connected_url_instead_of_advertised_runtime_url() {
    let connected_url = "http://127.0.0.1:49123";
    let transport = HttpSseTransport {
        client: reqwest::Client::new(),
        base_url: connected_url.to_owned(),
        server_info: AppServerInfo {
            instance_id: "server".to_owned(),
            pid: 1,
            base_url: "http://127.0.0.1:9".to_owned(),
            ipc_path: None,
            protocol_versions: ProtocolVersionRange::runtime(),
            started_at: chrono::Utc::now(),
        },
        info: RuntimeHostInfo {
            instance_id: "runtime".to_owned(),
            pid: 1,
            base_url: "http://127.0.0.1:9".to_owned(),
            cwd: "/workspace".to_owned(),
            workspace_id: WorkspaceId::new(),
            default_session_id: SessionId::new(),
            default_thread_id: ThreadId::new(),
            started_at: chrono::Utc::now(),
        },
        cwd: PathBuf::from("/workspace"),
        attachment_id: Arc::new(RwLock::new("attachment".to_owned())),
        transport_token: Arc::new(secrecy::SecretString::from("a".repeat(64))),
    };

    assert_eq!(
        transport.url("/commands"),
        format!("{connected_url}/commands")
    );
}

#[tokio::test]
async fn event_writer_assigns_sequence_numbers_in_record_order() {
    let host = RuntimeHost::in_memory().await.expect("host");
    let session_id = host.default_session_id();
    let later_allocated = host_event(
        200,
        session_id,
        None,
        RuntimeEventType::CommandAccepted,
        RuntimeEventSource::Runtime,
        json!({"summary": "recorded first"}),
    );
    let earlier_allocated = host_event(
        100,
        session_id,
        None,
        RuntimeEventType::CommandRejected,
        RuntimeEventSource::Runtime,
        json!({"summary": "recorded second"}),
    );

    host.record_event(later_allocated).await.expect("first");
    host.record_event(earlier_allocated).await.expect("second");

    let events = host
        .replay_events(EventFilter {
            session_id,
            task_id: None,
            after_sequence_no: None,
        })
        .await
        .expect("events");
    assert_eq!(event_sequence_no(&events[0]), Some(1));
    assert_eq!(event_sequence_no(&events[1]), Some(2));
    assert_eq!(
        events[0].get("event_type").and_then(Value::as_str),
        Some("command_accepted")
    );
    assert_eq!(
        events[1].get("event_type").and_then(Value::as_str),
        Some("command_rejected")
    );
}

#[tokio::test]
async fn runtime_host_reuses_one_process_supervisor_across_tool_executors() {
    let workspace = tempdir().expect("workspace");
    fs::write(
        workspace.path().join("host_process.sh"),
        "sleep 0.05\nprintf host-survived-turn\n",
    )
    .expect("process script");
    let host = RuntimeHost::in_memory().await.expect("host");
    let session_id = host.default_session_id();
    let root = workspace.path().to_path_buf();
    let first = host
        .build_tool_executor(
            WorkspacePolicy::new(&root).expect("first policy"),
            root.clone(),
        )
        .await
        .expect("first executor");
    let start_request = ToolRequest {
        tool_call_id: ToolCallId::new(),
        session_id,
        turn_id: Some(TurnId::new()),
        tool_name: "shell".to_owned(),
        arguments: json!({
            "command": "sh host_process.sh",
            "background": true,
            "yield_time_ms": 0,
        }),
    };
    let policy = first.evaluate(&start_request).expect("shell policy");
    let started = first
        .execute_with_policy(start_request, policy, true, CancellationToken::new())
        .await
        .expect("start process");
    let process_id = started.envelope.structured_facts["process_id"]
        .as_str()
        .expect("process id")
        .to_owned();

    let second = host
        .build_tool_executor(WorkspacePolicy::new(&root).expect("second policy"), root)
        .await
        .expect("second executor");
    sleep(Duration::from_millis(100)).await;
    let reconnected = second
        .execute(
            ToolRequest {
                tool_call_id: ToolCallId::new(),
                session_id,
                turn_id: Some(TurnId::new()),
                tool_name: "process_reconnect".to_owned(),
                arguments: json!({"process_id": process_id, "cursor": 0}),
            },
            CancellationToken::new(),
        )
        .await
        .expect("reconnect from next turn");

    assert_eq!(
        reconnected.envelope.structured_facts["process_state"],
        "exited"
    );
    assert!(
        String::from_utf8_lossy(&reconnected.artifact_contents[0].bytes)
            .contains("host-survived-turn")
    );
}

#[tokio::test]
async fn dropping_runtime_host_terminates_its_background_processes() {
    let workspace = tempdir().expect("workspace");
    fs::write(
        workspace.path().join("delayed_write.sh"),
        "sleep 1\nprintf escaped > should-not-exist.txt\n",
    )
    .expect("process script");
    let host = RuntimeHost::in_memory().await.expect("host");
    let session_id = host.default_session_id();
    let root = workspace.path().to_path_buf();
    let executor = host
        .build_tool_executor(WorkspacePolicy::new(&root).expect("policy"), root.clone())
        .await
        .expect("executor");
    let start_request = ToolRequest {
        tool_call_id: ToolCallId::new(),
        session_id,
        turn_id: Some(TurnId::new()),
        tool_name: "shell".to_owned(),
        arguments: json!({
            "command": "sh delayed_write.sh",
            "background": true,
            "yield_time_ms": 0,
        }),
    };
    let policy = executor.evaluate(&start_request).expect("shell policy");
    let started = executor
        .execute_with_policy(start_request, policy, true, CancellationToken::new())
        .await
        .expect("start process");
    let process_id = started.envelope.structured_facts["process_id"]
        .as_str()
        .expect("process id")
        .to_owned();

    drop(host);
    sleep(Duration::from_millis(100)).await;
    let reconnected = executor
        .execute(
            ToolRequest {
                tool_call_id: ToolCallId::new(),
                session_id,
                turn_id: Some(TurnId::new()),
                tool_name: "process_reconnect".to_owned(),
                arguments: json!({"process_id": process_id, "cursor": 0}),
            },
            CancellationToken::new(),
        )
        .await
        .expect("terminal process snapshot");

    assert_eq!(
        reconnected.envelope.structured_facts["process_state"],
        "cancelled"
    );
    assert!(!root.join("should-not-exist.txt").exists());
}

#[tokio::test]
async fn event_pages_move_backward_and_forward_without_overlap() {
    let host = RuntimeHost::in_memory().await.expect("host");
    let session_id = host.default_session_id();
    for index in 0..5 {
        host.record_event(host_event(
            0,
            session_id,
            None,
            RuntimeEventType::CommandAccepted,
            RuntimeEventSource::Runtime,
            json!({"summary": format!("event {index}")}),
        ))
        .await
        .expect("event");
    }

    let latest = host
        .event_page(EventPageRequest {
            session_id,
            task_id: None,
            cursor: None,
            direction: EventPageDirection::Backward,
            limit: 2,
        })
        .await
        .expect("latest page");
    assert_eq!(
        latest
            .events
            .iter()
            .map(|event| event.sequence_no)
            .collect::<Vec<_>>(),
        vec![4, 5]
    );
    assert!(latest.has_more);

    let older = host
        .event_page(EventPageRequest {
            session_id,
            task_id: None,
            cursor: latest.start_cursor,
            direction: EventPageDirection::Backward,
            limit: 2,
        })
        .await
        .expect("older page");
    assert_eq!(
        older
            .events
            .iter()
            .map(|event| event.sequence_no)
            .collect::<Vec<_>>(),
        vec![2, 3]
    );
    assert!(older.has_more);

    let forward = host
        .event_page(EventPageRequest {
            session_id,
            task_id: None,
            cursor: Some(3),
            direction: EventPageDirection::Forward,
            limit: 2,
        })
        .await
        .expect("forward page");
    assert_eq!(
        forward
            .events
            .iter()
            .map(|event| event.sequence_no)
            .collect::<Vec<_>>(),
        vec![4, 5]
    );
    assert!(!forward.has_more);
}

#[tokio::test]
async fn task_trace_paginates_more_than_the_single_page_limit_and_reports_incomplete_sections() {
    let host = RuntimeHost::in_memory().await.expect("host");
    let session_id = host.default_session_id();
    let task_id = TaskId::new();
    for index in 0..513 {
        host.record_event(host_event(
            0,
            session_id,
            Some(task_id),
            RuntimeEventType::CommandAccepted,
            RuntimeEventSource::Runtime,
            json!({"summary": format!("event {index}")}),
        ))
        .await
        .expect("event");
    }

    let first = host
        .task_trace(TaskTraceRequest {
            session_id,
            task_id,
            view: TraceView::Full,
            cursor: None,
            limit: 512,
            wait_for_evaluation: false,
        })
        .await
        .expect("first trace page");
    assert_eq!(first.events.len(), 512);
    assert!(first.has_more);
    assert!(!first.integrity.complete);
    assert!(
        first
            .integrity
            .missing_sections
            .contains(&"context_snapshot".to_owned())
    );

    let second = host
        .task_trace(TaskTraceRequest {
            session_id,
            task_id,
            view: TraceView::Full,
            cursor: first.next_cursor,
            limit: 512,
            wait_for_evaluation: false,
        })
        .await
        .expect("second trace page");
    assert_eq!(second.events.len(), 1);
    assert!(!second.has_more);
    assert_eq!(second.integrity.event_count, 513);
    assert!(first.events.last().unwrap().sequence_no < second.events[0].sequence_no);

    let complete = TaskTraceService::new(host)
        .read_complete(TaskTraceRequest {
            session_id,
            task_id,
            view: TraceView::Full,
            cursor: None,
            limit: 512,
            wait_for_evaluation: false,
        })
        .await
        .expect("complete trace");
    assert_eq!(complete.events.len(), 513);
    assert!(!complete.has_more);
    assert_eq!(complete.integrity.event_count, 513);
}

#[tokio::test]
async fn task_scoped_governance_reads_reject_a_different_session() {
    let host = RuntimeHost::in_memory().await.expect("host");
    let owner_session_id = host.default_session_id();
    let other_session_id = SessionId::new();
    let task_id = TaskId::new();
    host.record_event(host_event(
        0,
        owner_session_id,
        Some(task_id),
        RuntimeEventType::TaskCreated,
        RuntimeEventSource::Runtime,
        json!({"prompt": "private task"}),
    ))
    .await
    .expect("task event");

    let trace_error = host
        .task_trace(TaskTraceRequest {
            session_id: other_session_id,
            task_id,
            view: TraceView::Full,
            cursor: None,
            limit: 64,
            wait_for_evaluation: false,
        })
        .await
        .expect_err("cross-session trace must fail");
    let projection_error = host
        .query(RuntimeQuery {
            query_id: QueryId::new(),
            session_id: other_session_id,
            task_id: Some(task_id),
            kind: RuntimeQueryKind::EvaluationProjection,
            requester: ActorKind::Sdk,
            cursor: None,
            timestamp: chrono::Utc::now(),
        })
        .await
        .expect_err("cross-session projection must fail");

    assert!(matches!(trace_error, ClientError::InvalidSession(_)));
    assert!(matches!(projection_error, ClientError::InvalidSession(_)));
}

#[tokio::test]
async fn artifact_reads_reject_a_different_workspace_before_loading_blob_bytes() {
    let home = tempdir().expect("home");
    let workspace_a = tempdir().expect("workspace a");
    let workspace_b = tempdir().expect("workspace b");
    let host_a = RuntimeHost::from_home_and_cwd(home.path(), workspace_a.path())
        .await
        .expect("host a");
    let host_b = RuntimeHost::from_home_and_cwd(home.path(), workspace_b.path())
        .await
        .expect("host b");
    let bytes = b"workspace-a-only";
    let artifact_id = ArtifactId::new();
    host_a
        .repositories
        .artifacts
        .store(
            &ArtifactRecord {
                artifact_id,
                session_id: host_a.default_session_id(),
                turn_id: None,
                tool_call_id: None,
                artifact_type: "test-private".to_owned(),
                uri: format!("artifact://test/{artifact_id}"),
                checksum: format!("sha256:{:x}", Sha256::digest(bytes)),
                size_bytes: bytes.len() as u64,
                created_at: chrono::Utc::now(),
                producer: "test".to_owned(),
                redaction_status: RedactionStatus::NotRequired,
                retention_policy: "debug_default".to_owned(),
                provenance_refs: Vec::new(),
            },
            bytes,
        )
        .await
        .expect("artifact");

    let error = host_b
        .read_artifact_chunk(ArtifactReadRequest {
            artifact_id,
            offset: 0,
            length: 64,
        })
        .await
        .expect_err("foreign workspace artifact must be rejected");

    assert!(matches!(error, ClientError::InvalidSession(_)));
}

#[tokio::test]
async fn durable_post_task_worker_recovers_a_queued_job_after_host_restart() {
    let home = tempdir().expect("home");
    let workspace = tempdir().expect("workspace");
    let paths = RuntimePaths::from_home_and_cwd(home.path(), workspace.path()).expect("paths");
    let store =
        RuntimeStore::connect_with_artifact_root(&paths.sqlite_url(), paths.artifacts_dir.clone())
            .await
            .expect("store");
    let session_id = SessionId::new();
    let task_id = TaskId::new();
    let turn_id = TurnId::new();
    store
        .append_event_assigning_sequence(host_event(
            0,
            session_id,
            Some(task_id),
            RuntimeEventType::TaskCreated,
            RuntimeEventSource::Runtime,
            json!({"prompt": "recovered evaluation"}),
        ))
        .await
        .expect("task event");
    store
        .append_event_assigning_sequence(host_event(
            0,
            session_id,
            Some(task_id),
            RuntimeEventType::LoopDecided,
            RuntimeEventSource::Runtime,
            json!({"status": "completed", "summary": "recovered"}),
        ))
        .await
        .expect("terminal event");
    let now = chrono::Utc::now();
    let job = PostTaskJob {
        job_id: PostTaskJobId::new(),
        kind: PostTaskJobKind::DeepEvaluation,
        workspace_id: paths.workspace_id().to_string(),
        session_id: session_id.to_string(),
        task_id,
        input_refs: vec![
            format!("session:{session_id}"),
            format!("task:{task_id}"),
            format!("turn:{turn_id}"),
        ],
        status: PostTaskJobStatus::Queued,
        attempt: 0,
        max_attempts: 3,
        lease_owner: None,
        lease_expires_at: None,
        result_refs: Vec::new(),
        last_error: None,
        created_at: now,
        started_at: None,
        completed_at: None,
    };
    store.enqueue_post_task_job(&job).await.expect("queued job");
    drop(store);

    let host = RuntimeHost::from_home_and_cwd(home.path(), workspace.path())
        .await
        .expect("restarted host");
    host.wait_for_deep_task_evaluation(task_id).await;
    let recovered = host
        .store
        .post_task_job(task_id)
        .await
        .expect("job state")
        .expect("job");
    assert_eq!(recovered.status, PostTaskJobStatus::Succeeded);
    assert!(
        host.store
            .load_events(session_id, Some(task_id), None)
            .await
            .expect("evaluation events")
            .iter()
            .any(|event| event.event_type == RuntimeEventType::PostTaskJobCompleted)
    );
}

#[tokio::test]
async fn storage_maintenance_command_records_a_report_and_exposes_stats() {
    let transport = EmbeddedTransport::in_memory().await.expect("transport");
    let session_id = transport.default_session_id();
    let ack = transport
        .send_command(runtime_command(
            session_id,
            SessionCommandKind::RunStorageMaintenance,
            json!({}),
        ))
        .await
        .expect("maintenance command");
    assert!(ack.accepted);

    let stats = transport
        .query(RuntimeQuery {
            query_id: QueryId::new(),
            session_id,
            task_id: None,
            kind: RuntimeQueryKind::StorageStatus,
            requester: ActorKind::Sdk,
            cursor: None,
            timestamp: chrono::Utc::now(),
        })
        .await
        .expect("storage stats");
    assert_eq!(stats["artifact_records"], 0);
    assert!(
        transport
            .replay_events(EventFilter {
                session_id,
                task_id: None,
                after_sequence_no: None,
            })
            .await
            .expect("events")
            .into_iter()
            .any(|event| {
                event["event_type"] == json!(RuntimeEventType::StorageMaintenanceCompleted)
            })
    );
}

#[tokio::test]
async fn failure_objective_uses_the_started_queued_turn() {
    let host = RuntimeHost::in_memory().await.expect("host");
    let task = HostedAgentTask {
        session_id: host.default_session_id(),
        task_id: TaskId::new(),
        turn_id: TurnId::new(),
        payload: json!({"prompt": "first turn"}),
    };
    let queued_turn_id = TurnId::new();
    host.record_event(agent_event_for_turn(
        host.next_sequence_no(),
        &task,
        queued_turn_id,
        RuntimeEventType::TurnStarted,
        RuntimeEventSource::User,
        json!({"summary": "queued turn started", "prompt": "second turn"}),
    ))
    .await
    .expect("turn event");

    let objective = host
        .objective_for_task_turn(&task, queued_turn_id)
        .await
        .expect("objective");

    assert_eq!(objective, "second turn");
}

#[cfg(unix)]
#[tokio::test]
async fn cwd_transport_ignores_project_golutra_directory() {
    use std::os::unix::fs::symlink;

    let workspace = tempdir().expect("workspace");
    let outside = tempdir().expect("outside");
    let _home = IsolatedGlobalMockProvider::empty().await;
    symlink(outside.path(), workspace.path().join(".golutra")).expect("symlink");

    let transport = EmbeddedTransport::for_cwd(workspace.path())
        .await
        .expect("project runtime directory is ignored");

    assert_eq!(
        transport.cwd(),
        Some(workspace.path().canonicalize().expect("cwd").as_path())
    );
    assert!(
        fs::read_dir(outside.path())
            .expect("outside dir")
            .next()
            .is_none()
    );
}

#[tokio::test]
async fn command_query_and_subscribe_share_state() {
    let transport = EmbeddedTransport::in_memory().await.expect("transport");
    let session_id = SessionId::new();
    let command = command(session_id, "list workspace");

    let ack = transport.send_command(command).await.expect("accepted");
    let state = wait_for_status(&transport, session_id, TaskStatus::Completed).await;
    let events = transport
        .replay_events(EventFilter {
            session_id,
            task_id: None,
            after_sequence_no: None,
        })
        .await
        .expect("events");

    assert!(ack.accepted);
    assert_eq!(projection_status(&state), Some(TaskStatus::Completed));
    assert!(events.len() >= 7);
}

#[tokio::test]
async fn governed_runtime_facade_routes_the_canonical_task_and_trace_chain() {
    let _provider = IsolatedGlobalMockProvider::install().await;
    let application = RuntimeApplication::in_memory().await.expect("application");
    let session_id = application.session_service().default_session_id();
    let transport = EmbeddedTransport::from_application(application.clone());

    let ack = application
        .command_service()
        .execute(command(session_id, "list workspace"))
        .await
        .expect("command");
    let state = wait_for_status(&transport, session_id, TaskStatus::Completed).await;
    let task_id = state
        .get("active_task_id")
        .and_then(Value::as_str)
        .and_then(|value| value.parse::<TaskId>().ok())
        .expect("completed task id");
    let trace = application
        .trace_service()
        .read(TaskTraceRequest {
            session_id,
            task_id,
            view: TraceView::Full,
            cursor: None,
            limit: 512,
            wait_for_evaluation: true,
        })
        .await
        .expect("trace");
    let summary_trace = application
        .trace_service()
        .read(TaskTraceRequest {
            session_id,
            task_id,
            view: TraceView::Summary,
            cursor: None,
            limit: 512,
            wait_for_evaluation: true,
        })
        .await
        .expect("summary trace");
    let forensic_trace = application
        .trace_service()
        .read(TaskTraceRequest {
            session_id,
            task_id,
            view: TraceView::Forensic,
            cursor: None,
            limit: 512,
            wait_for_evaluation: true,
        })
        .await
        .expect("forensic trace");
    let post_task = application
        .post_task_service()
        .wait_for_terminal(task_id)
        .await
        .expect("post-task status")
        .expect("post-task job");

    assert!(ack.accepted);
    assert!(!trace.events.is_empty());
    assert!(trace.verification_plan.is_some());
    assert!(trace.post_task_jobs.iter().any(|job| {
        matches!(
            job.status,
            PostTaskJobStatus::Succeeded | PostTaskJobStatus::Failed | PostTaskJobStatus::Cancelled
        )
    }));
    assert!(matches!(
        post_task.status,
        PostTaskJobStatus::Succeeded | PostTaskJobStatus::Failed | PostTaskJobStatus::Cancelled
    ));
    assert!(trace.evaluation.terminal);
    assert!(trace.evaluation.integrity_warnings.is_empty());
    assert!(trace.integrity.complete, "{:?}", trace.integrity);
    assert!(summary_trace.context_snapshots.is_empty());
    assert!(summary_trace.artifacts.is_empty());
    assert!(summary_trace.evidence.is_empty());
    assert!(summary_trace.integrity.complete);
    assert!(
        summary_trace
            .integrity
            .redacted_fields
            .contains(&"event_payload_details".to_owned())
    );
    assert!(summary_trace.events.iter().all(|event| {
        event.payload.as_object().is_some_and(|payload| {
            payload.keys().all(|key| {
                matches!(
                    key.as_str(),
                    "summary" | "status" | "result" | "decision" | "action" | "reason" | "error"
                )
            })
        }) && event.payload_ref.is_none()
    }));
    assert!(!forensic_trace.integrity.complete);
    assert!(
        forensic_trace
            .integrity
            .retention_losses
            .contains(&"restricted_context_capture_disabled".to_owned())
    );
}

#[tokio::test]
async fn completed_task_allows_next_prompt_in_same_session() {
    let transport = EmbeddedTransport::in_memory().await.expect("transport");
    let session_id = SessionId::new();

    let first = transport
        .send_command(command(session_id, "hi"))
        .await
        .expect("first prompt");
    wait_for_task_completed_count(&transport, session_id, 1).await;
    let second = transport
        .send_command(command(session_id, "what next"))
        .await
        .expect("second prompt");
    let events = wait_for_task_completed_count(&transport, session_id, 2).await;

    assert!(first.accepted);
    assert!(second.accepted);
    assert!(
        second
            .reason
            .as_deref()
            .is_some_and(|reason| reason.starts_with("started task"))
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == RuntimeEventType::TaskCreated)
            .count(),
        2
    );
    assert!(
        events
            .iter()
            .all(|event| event.event_type != RuntimeEventType::BusyPolicyDecided)
    );
}

#[tokio::test]
async fn queued_prompt_records_each_user_and_assistant_turn() {
    let transport = EmbeddedTransport::in_memory().await.expect("transport");
    let session_id = SessionId::new();
    transport
        .send_command(command(session_id, "sleep"))
        .await
        .expect("first prompt");
    let waiting = wait_for_status(&transport, session_id, TaskStatus::WaitingApproval).await;
    let approval_id = waiting
        .get("pending_approval")
        .and_then(Value::as_str)
        .expect("pending approval")
        .to_owned();

    let queued = transport
        .send_command(command(session_id, "what happened next"))
        .await
        .expect("queued prompt");
    let mut deny = command(session_id, "unused");
    deny.kind = SessionCommandKind::Deny;
    deny.payload = json!({"approval_id": approval_id});
    transport.send_command(deny).await.expect("deny approval");
    let events = wait_for_task_completed_count(&transport, session_id, 1).await;

    assert!(queued.accepted);
    assert_eq!(
        queued.reason.as_deref(),
        Some("prompt appended to active runtime lane")
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(
                event.event_type,
                RuntimeEventType::TaskCreated | RuntimeEventType::TurnQueued
            ))
            .count(),
        2
    );
    let mut user_turns = events
        .iter()
        .filter(|event| {
            matches!(
                event.event_type,
                RuntimeEventType::TaskCreated | RuntimeEventType::TurnQueued
            )
        })
        .filter_map(|event| event.turn_id)
        .collect::<Vec<_>>();
    user_turns.sort_unstable();
    user_turns.dedup();
    let mut assistant_turns = events
        .iter()
        .filter(|event| event.event_type == RuntimeEventType::AssistantMessage)
        .filter_map(|event| event.turn_id)
        .collect::<Vec<_>>();
    assistant_turns.sort_unstable();
    assistant_turns.dedup();
    assert_eq!(assistant_turns, user_turns);
    let started = events
        .iter()
        .find(|event| event.event_type == RuntimeEventType::TurnStarted)
        .expect("queued turn started");
    let queued_turn_id = started.turn_id.expect("queued turn id");
    for event in events.iter().filter(|event| {
        event.sequence_no > started.sequence_no
            && matches!(
                event.event_type,
                RuntimeEventType::ContextBuilt
                    | RuntimeEventType::ProviderStarted
                    | RuntimeEventType::ProviderCompleted
                    | RuntimeEventType::TokenUsageRecorded
            )
    }) {
        assert_eq!(event.turn_id, Some(queued_turn_id));
    }
    assert!(
        events
            .iter()
            .filter(|event| matches!(
                event.event_type,
                RuntimeEventType::PostTaskReviewed | RuntimeEventType::EvaluationCompleted
            ))
            .all(|event| event.turn_id == Some(queued_turn_id))
    );
    let evaluation = transport
        .query(RuntimeQuery {
            query_id: QueryId::new(),
            session_id,
            task_id: None,
            kind: RuntimeQueryKind::EvaluationResults,
            requester: ActorKind::Cli,
            cursor: None,
            timestamp: chrono::Utc::now(),
        })
        .await
        .expect("evaluation results");
    assert!(evaluation["cases"].as_array().is_some_and(|cases| {
        cases
            .iter()
            .any(|case| case["objective"] == "what happened next")
    }));
}

#[tokio::test]
async fn control_command_after_completion_does_not_reactivate_the_lane() {
    let transport = EmbeddedTransport::in_memory().await.expect("transport");
    let session_id = SessionId::new();
    transport
        .send_command(command(session_id, "hi"))
        .await
        .expect("prompt");
    wait_for_task_completed_count(&transport, session_id, 1).await;
    let mut abort = command(session_id, "");
    abort.kind = SessionCommandKind::Abort;
    abort.payload = json!({});

    let ack = transport.send_command(abort).await.expect("abort response");
    let state = transport
        .query(RuntimeQuery {
            query_id: QueryId::new(),
            session_id,
            task_id: None,
            kind: RuntimeQueryKind::SessionState,
            requester: ActorKind::Cli,
            cursor: None,
            timestamp: chrono::Utc::now(),
        })
        .await
        .expect("state");

    assert!(!ack.accepted);
    assert_eq!(projection_status(&state), Some(TaskStatus::Completed));
    assert_eq!(state["runtime_lane"]["status"], "completed");
}

#[tokio::test]
async fn duplicate_idempotency_key_does_not_start_a_second_task() {
    let transport = EmbeddedTransport::in_memory().await.expect("transport");
    let session_id = SessionId::new();
    let command = command(session_id, "hi");

    let first = transport
        .send_command(command.clone())
        .await
        .expect("first command");
    let duplicate = transport
        .send_command(command)
        .await
        .expect("duplicate command");
    let events = wait_for_task_completed_count(&transport, session_id, 1).await;

    assert_eq!(duplicate, first);
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == RuntimeEventType::TaskCreated)
            .count(),
        1
    );
}

#[tokio::test]
async fn reused_idempotency_key_with_a_different_command_id_is_rejected() {
    let transport = EmbeddedTransport::in_memory().await.expect("transport");
    let session_id = SessionId::new();
    let first = command(session_id, "hi");
    let mut conflicting = command(session_id, "different prompt");
    conflicting.idempotency_key = first.idempotency_key.clone();

    transport.send_command(first).await.expect("first command");
    let ack = transport
        .send_command(conflicting)
        .await
        .expect("conflicting command ack");

    assert!(!ack.accepted);
    assert!(
        ack.reason
            .as_deref()
            .is_some_and(|reason| reason.contains("already assigned"))
    );
}

#[tokio::test]
async fn oversized_command_metadata_is_rejected_before_recording_events() {
    let transport = EmbeddedTransport::in_memory().await.expect("transport");
    let session_id = SessionId::new();
    let mut oversized = command(session_id, "x");
    oversized.payload = json!({
        "prompt": "x".repeat(MAX_COMMAND_PAYLOAD_JSON_BYTES + 1)
    });

    let payload_ack = transport
        .send_command(oversized)
        .await
        .expect("payload rejection");
    let mut invalid_actor = command(session_id, "hello");
    invalid_actor.actor.id = String::new();
    let actor_ack = transport
        .send_command(invalid_actor)
        .await
        .expect("actor rejection");
    let events = transport
        .replay_events(EventFilter {
            session_id,
            task_id: None,
            after_sequence_no: None,
        })
        .await
        .expect("events");

    assert!(!payload_ack.accepted);
    assert!(!actor_ack.accepted);
    assert!(events.is_empty());
}

#[tokio::test]
async fn duplicate_command_is_serialized_across_embedded_hosts() {
    let workspace = tempdir().expect("workspace");
    let _provider = IsolatedGlobalMockProvider::install().await;
    let first = EmbeddedTransport::for_cwd(workspace.path())
        .await
        .expect("first host");
    let second = EmbeddedTransport::for_cwd(workspace.path())
        .await
        .expect("second host");
    let session_id = first.default_session_id();
    let command = command(session_id, "one durable command");

    let (first_ack, second_ack) = tokio::join!(
        first.send_command(command.clone()),
        second.send_command(command),
    );
    let first_ack = first_ack.expect("first ack");
    let second_ack = second_ack.expect("second ack");
    wait_for_status(&first, session_id, TaskStatus::Completed).await;
    let events = first
        .host
        .store
        .load_events(session_id, None, None)
        .await
        .expect("events");

    assert_eq!(first_ack, second_ack);
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == RuntimeEventType::TaskCreated)
            .count(),
        1
    );
}

#[tokio::test]
async fn idempotency_keys_are_scoped_to_the_attached_workspace() {
    let workspace_a = tempdir().expect("workspace a");
    let workspace_b = tempdir().expect("workspace b");
    let _provider = IsolatedGlobalMockProvider::install().await;
    let transport_a = EmbeddedTransport::for_cwd(workspace_a.path())
        .await
        .expect("workspace a transport");
    let transport_b = EmbeddedTransport::for_cwd(workspace_b.path())
        .await
        .expect("workspace b transport");
    let session_a = transport_a.default_session_id();
    let session_b = transport_b.default_session_id();
    let shared_key = "same-caller-key".to_owned();
    let mut command_a = command(session_a, "hello from a");
    command_a.idempotency_key = shared_key.clone();
    let mut command_b = command(session_b, "hello from b");
    command_b.idempotency_key = shared_key;

    let (ack_a, ack_b) = tokio::join!(
        transport_a.send_command(command_a),
        transport_b.send_command(command_b),
    );
    assert!(ack_a.expect("workspace a ack").accepted);
    assert!(ack_b.expect("workspace b ack").accepted);
    wait_for_status(&transport_a, session_a, TaskStatus::Completed).await;
    wait_for_status(&transport_b, session_b, TaskStatus::Completed).await;
}

#[tokio::test]
async fn processing_command_journal_entry_is_reconciled_after_owner_exit() {
    let workspace = tempdir().expect("workspace");
    let _provider = IsolatedGlobalMockProvider::install().await;
    let host = RuntimeHost::for_cwd(workspace.path()).await.expect("host");
    let transport = EmbeddedTransport::new(host.clone());
    let session_id = transport.default_session_id();
    let command = command(session_id, "recover claimed command");
    let claim = host
        .store
        .claim_command(
            &host.scoped_idempotency_key(&command.idempotency_key),
            command.command_id,
            &CommandAck {
                command_id: command.command_id,
                accepted: true,
                reason: Some(PROVISIONAL_COMMAND_ACK_REASON.to_owned()),
            },
            host_event(
                0,
                session_id,
                None,
                RuntimeEventType::CommandReceived,
                RuntimeEventSource::Runtime,
                json!({"command_id": command.command_id}),
            ),
        )
        .await
        .expect("processing command claim");
    assert!(matches!(
        claim,
        CommandClaim::Claimed {
            receipt_event: Some(_)
        }
    ));

    let ack = transport
        .send_command(command)
        .await
        .expect("stale command is retried");
    wait_for_status(&transport, session_id, TaskStatus::Completed).await;

    assert!(ack.accepted);
    assert_ne!(ack.reason.as_deref(), Some(PROVISIONAL_COMMAND_ACK_REASON));
}

#[tokio::test]
async fn successful_task_quarantines_reviews_retrieves_and_rolls_back_project_memory() {
    let transport = EmbeddedTransport::in_memory().await.expect("transport");
    let session_id = SessionId::new();
    transport
        .send_command(command(session_id, "list workspace files"))
        .await
        .expect("first prompt");
    wait_for_task_completed_count(&transport, session_id, 1).await;
    let memories = transport
        .query(RuntimeQuery {
            query_id: QueryId::new(),
            session_id,
            task_id: None,
            kind: RuntimeQueryKind::MemoryList,
            requester: ActorKind::Cli,
            cursor: None,
            timestamp: chrono::Utc::now(),
        })
        .await
        .expect("memory list");
    let memory_id = memories
        .as_array()
        .and_then(|records| records.first())
        .and_then(|record| record.get("memory_id"))
        .and_then(Value::as_str)
        .expect("quarantined memory id")
        .to_owned();
    assert_eq!(memories[0]["status"], "quarantined");

    transport
        .send_command(command(session_id, "list workspace files again"))
        .await
        .expect("second prompt");
    let events = wait_for_task_completed_count(&transport, session_id, 2).await;
    let retrieved_before_review = events
        .iter()
        .filter(|event| event.event_type == RuntimeEventType::MemoryRetrieved)
        .filter_map(|event| event.payload.get("retrieved").and_then(Value::as_array))
        .any(|records| !records.is_empty());

    let review = transport
        .send_command(SessionCommand {
            command_id: CommandId::new(),
            session_id: Some(session_id),
            kind: SessionCommandKind::ReviewMemoryCandidate,
            idempotency_key: "review-memory".to_owned(),
            actor: Actor {
                kind: ActorKind::Cli,
                id: "test-reviewer".to_owned(),
            },
            payload: json!({
                "memory_id": memory_id,
                "decision": "approve",
                "human_approval": true,
                "reason": "reviewed the durable task result"
            }),
            timestamp: chrono::Utc::now(),
        })
        .await
        .expect("memory review");
    assert!(review.accepted);

    transport
        .send_command(command(session_id, "list workspace files once more"))
        .await
        .expect("third prompt");
    let reviewed_events = wait_for_task_completed_count(&transport, session_id, 3).await;
    let retrieved_after_review = reviewed_events
        .iter()
        .filter(|event| event.event_type == RuntimeEventType::MemoryRetrieved)
        .filter_map(|event| event.payload.get("retrieved").and_then(Value::as_array))
        .any(|records| !records.is_empty());

    let rollback = transport
        .send_command(SessionCommand {
            command_id: CommandId::new(),
            session_id: Some(session_id),
            kind: SessionCommandKind::MemoryRollback,
            idempotency_key: "rollback-memory".to_owned(),
            actor: Actor {
                kind: ActorKind::Cli,
                id: "test".to_owned(),
            },
            payload: json!({"memory_id": memory_id, "reason": "test rollback"}),
            timestamp: chrono::Utc::now(),
        })
        .await
        .expect("memory rollback");

    assert!(!retrieved_before_review);
    assert!(retrieved_after_review);
    assert!(rollback.accepted);
}

#[tokio::test]
async fn failed_task_reaches_rejected_promotion_without_polluting_memory() {
    let _provider = IsolatedGlobalMockProvider::install().await;
    let application = RuntimeApplication::in_memory().await.expect("application");
    let session_id = application.session_service().default_session_id();
    let transport = EmbeddedTransport::from_application(application.clone());
    let mut failed_prompt = command(session_id, "reproduce provider failure");
    failed_prompt.payload["mock_provider_failure"] = json!(true);

    let accepted = application
        .send_command(failed_prompt)
        .await
        .expect("failed task accepted");
    let state = wait_for_status(&transport, session_id, TaskStatus::Failed).await;
    let task_id = state
        .get("active_task_id")
        .and_then(Value::as_str)
        .and_then(|value| value.parse::<TaskId>().ok())
        .expect("failed task id");
    let job = application
        .post_task_service()
        .wait_for_terminal(task_id)
        .await
        .expect("post-task wait")
        .expect("post-task job");
    let trace = application
        .task_trace(TaskTraceRequest {
            session_id,
            task_id,
            view: TraceView::Full,
            cursor: None,
            limit: 512,
            wait_for_evaluation: true,
        })
        .await
        .expect("failed task trace");
    let candidate_id = trace
        .evaluation
        .automation_candidates
        .iter()
        .find(|candidate| candidate.id.starts_with("automation-benchmark-"))
        .map(|candidate| candidate.id.clone())
        .expect("benchmark candidate");
    let memories = application
        .query(RuntimeQuery {
            query_id: QueryId::new(),
            session_id,
            task_id: Some(task_id),
            kind: RuntimeQueryKind::MemoryList,
            requester: ActorKind::Cli,
            cursor: None,
            timestamp: chrono::Utc::now(),
        })
        .await
        .expect("memory projection");
    let apply_without_regression = application
        .send_command(runtime_command(
            session_id,
            SessionCommandKind::ApplyCandidate,
            json!({"candidate_id": &candidate_id}),
        ))
        .await;
    let evaluation_store = application.host().evaluation_store.clone();
    let second_task_id = TaskId::new();
    let second_bundle = EvaluationRunner.evaluate_task(TaskEvaluationInput {
        task_id: second_task_id,
        objective: "exercise a distinct holdout objective".to_owned(),
        task_status: TaskStatus::Failed,
        verification: None,
        event_count: 1,
        artifact_count: 0,
        tool_count: 0,
        latency_ms: Some(1),
        failure_summary: Some("independent historical failure".to_owned()),
        token_usage: Vec::new(),
        provider_config_ref: "provider:test".to_owned(),
        runtime_config_ref: "runtime:test".to_owned(),
        policy_violation_count: 0,
    });
    let second_case_ref = second_bundle.case.case_id.clone();
    evaluation_store
        .record_task_evaluation(second_bundle)
        .expect("second regression case");
    let source_case_ref = evaluation_store
        .snapshot()
        .expect("evaluation state")
        .cases
        .iter()
        .find(|case| case.source_task_id == Some(task_id))
        .map(|case| case.case_id.clone())
        .expect("source regression case");
    let regression = application
        .send_command(runtime_command(
            session_id,
            SessionCommandKind::RunRegression,
            json!({
                "candidate_id": &candidate_id,
                "case_refs": [&source_case_ref, &second_case_ref],
                "candidate_files": {
                    "regression-marker.txt": "candidate workspace"
                }
            }),
        ))
        .await
        .expect("regression");
    let evaluation_state = evaluation_store.snapshot().expect("evaluation state");
    let campaign = evaluation_state
        .regression_campaigns
        .iter()
        .rev()
        .find(|campaign| campaign.candidate_id == candidate_id)
        .expect("campaign");
    let executions = evaluation_state
        .regression_executions
        .iter()
        .filter(|execution| execution.campaign_id == campaign.campaign_id)
        .collect::<Vec<_>>();
    assert_eq!(executions.len(), 4);
    assert!(campaign.case_refs.iter().all(|case_ref| {
        executions
            .iter()
            .filter(|execution| execution.case_ref == *case_ref)
            .count()
            == 2
    }));
    for case_ref in &campaign.case_refs {
        let pair = executions
            .iter()
            .filter(|execution| execution.case_ref == *case_ref)
            .collect::<Vec<_>>();
        assert_ne!(
            pair[0].workspace_snapshot_digest,
            pair[1].workspace_snapshot_digest
        );
        assert_ne!(pair[0].task_trace_ref, pair[1].task_trace_ref);
    }
    assert!(executions.iter().all(
        |execution| execution.task_trace_ref.is_some() && execution.verification_ref.is_some()
    ));
    for execution in &executions {
        let reference = execution.task_trace_ref.as_deref().expect("trace ref");
        let artifact_id = reference
            .strip_prefix("artifact://regression-trace/")
            .and_then(|value| value.split('?').next())
            .and_then(|value| value.parse::<ArtifactId>().ok())
            .expect("durable regression trace artifact ref");
        let bytes = application
            .host()
            .repositories
            .artifacts
            .bytes(artifact_id)
            .await
            .expect("trace artifact read")
            .expect("trace artifact bytes");
        let bundle: Value = serde_json::from_slice(&bytes).expect("trace bundle JSON");
        assert_eq!(bundle["format"], "golutra.regression-trace-bundle.v1");
        assert_eq!(bundle["case_ref"], execution.case_ref);
    }
    let apply_after_regression = application
        .send_command(runtime_command(
            session_id,
            SessionCommandKind::ApplyCandidate,
            json!({"candidate_id": &candidate_id}),
        ))
        .await;
    let governed = application
        .governance_service()
        .evaluation_projection(session_id, task_id)
        .await
        .expect("evaluation projection");
    let governed_trace = application
        .complete_task_trace(TaskTraceRequest {
            session_id,
            task_id,
            view: TraceView::Full,
            cursor: None,
            limit: 512,
            wait_for_evaluation: true,
        })
        .await
        .expect("governed task trace");

    assert!(accepted.accepted);
    assert_eq!(job.status, PostTaskJobStatus::Succeeded);
    assert_eq!(
        trace.verification.as_ref().map(|record| record.result),
        Some(VerificationResult::Fail)
    );
    assert!(trace.verification_plan.is_some());
    assert!(trace.evaluation.terminal);
    assert!(!trace.evaluation.reviews.is_empty());
    assert!(!trace.evaluation.results.is_empty());
    assert!(!trace.evaluation.improvement_candidates.is_empty());
    assert!(trace.integrity.complete, "{:?}", trace.integrity);
    assert!(memories.as_array().is_some_and(Vec::is_empty));
    assert!(matches!(
        apply_without_regression,
        Err(ClientError::Evaluation(EvaluationError::PromotionRequired(
            _
        )))
    ));
    assert!(regression.accepted);
    assert!(matches!(
        apply_after_regression,
        Err(ClientError::Evaluation(EvaluationError::PromotionRequired(
            _
        )))
    ));
    assert!(!governed.regressions.is_empty());
    assert!(governed.integrity_warnings.is_empty(), "{governed:?}");
    assert!(governed.promotion_decisions.iter().any(|decision| {
        decision.candidate_id == candidate_id && decision.decision == PromotionDecisionKind::Reject
    }));
    assert!(
        governed_trace.integrity.complete,
        "{:?}",
        governed_trace.integrity
    );
    assert!(
        governed_trace
            .events
            .iter()
            .any(|event| event.event_type == RuntimeEventType::RegressionCompleted)
    );
    assert!(executions.iter().all(|execution| {
        execution
            .task_trace_ref
            .as_deref()
            .and_then(|reference| reference.strip_prefix("artifact://regression-trace/"))
            .and_then(|value| value.split('?').next())
            .and_then(|value| value.parse::<ArtifactId>().ok())
            .is_some_and(|artifact_id| {
                governed_trace
                    .artifacts
                    .iter()
                    .any(|artifact| artifact.artifact_id == artifact_id)
            })
    }));
}

#[tokio::test]
async fn evolution_plan_executes_generated_task_in_isolated_mock_runtime() {
    let workspace = tempdir().expect("workspace");
    let home = tempdir().expect("home");
    let transport = EmbeddedTransport::from_home_and_cwd(home.path(), workspace.path())
        .await
        .expect("transport");
    let session_id = transport.default_session_id();
    let task_id = TaskId::new();
    let task = HostedAgentTask {
        session_id,
        task_id,
        turn_id: TurnId::new(),
        payload: json!({"prompt": "reproduce provider failure"}),
    };
    let evaluation_input = transport
        .host
        .evaluate_completed_task(
            &task,
            HostedTaskEvaluation {
                objective: "reproduce provider failure",
                task_status: TaskStatus::Failed,
                verification: None,
                tool_reports: &[],
                failure_summary: Some("provider failed".to_owned()),
                latency: Duration::ZERO,
            },
        )
        .await
        .expect("evaluation");
    transport
        .host
        .record_task_evaluation(&task, EvaluationRunner.evaluate_task(evaluation_input))
        .await
        .expect("deep evaluation");

    let plan = transport
        .send_command(runtime_command(
            session_id,
            SessionCommandKind::PlanEvolution,
            json!({"objective": "expand provider robustness"}),
        ))
        .await
        .expect("plan");
    let run = transport
        .send_command(runtime_command(
            session_id,
            SessionCommandKind::RunEvolution,
            json!({}),
        ))
        .await
        .expect("run");
    let state = transport
        .query(RuntimeQuery {
            query_id: QueryId::new(),
            session_id,
            task_id: None,
            kind: RuntimeQueryKind::EvolutionState,
            requester: ActorKind::Cli,
            cursor: None,
            timestamp: chrono::Utc::now(),
        })
        .await
        .expect("evolution state");

    assert!(plan.accepted);
    assert!(run.accepted);
    assert_eq!(state["runs"][0]["status"], "completed");
    assert_eq!(state["executions"][0]["status"], "completed");
    let sandbox_workspace = state["executions"][0]["sandbox_workspace"]
        .as_str()
        .expect("sandbox workspace");
    assert!(
        sandbox_workspace.starts_with(
            home.path()
                .canonicalize()
                .expect("canonical home")
                .to_string_lossy()
                .as_ref()
        )
    );
    assert!(
        !sandbox_workspace.starts_with(
            workspace
                .path()
                .canonicalize()
                .expect("canonical workspace")
                .to_string_lossy()
                .as_ref()
        )
    );
}

#[tokio::test]
async fn evaluation_persistence_failure_is_reported_to_the_durable_worker() {
    let workspace = tempdir().expect("workspace");
    let home = tempdir().expect("home");
    let transport = EmbeddedTransport::from_home_and_cwd(home.path(), workspace.path())
        .await
        .expect("transport");
    let paths = transport
        .host
        .runtime_paths
        .as_ref()
        .expect("runtime paths");
    if paths.evaluation_file.exists() {
        fs::remove_file(&paths.evaluation_file).expect("remove evaluation file");
    }
    fs::create_dir(&paths.evaluation_file).expect("block evaluation state file");
    let task = HostedAgentTask {
        session_id: transport.default_session_id(),
        task_id: TaskId::new(),
        turn_id: TurnId::new(),
        payload: json!({"prompt": "evaluate persistence failure"}),
    };
    let bundle = EvaluationRunner.evaluate_minimal(TaskEvaluationInput {
        task_id: task.task_id,
        objective: "evaluate persistence failure".to_owned(),
        task_status: TaskStatus::Completed,
        verification: None,
        event_count: 1,
        artifact_count: 0,
        tool_count: 0,
        latency_ms: Some(1),
        failure_summary: None,
        token_usage: Vec::new(),
        provider_config_ref: "provider:test".to_owned(),
        runtime_config_ref: "runtime:test".to_owned(),
        policy_violation_count: 0,
    });

    assert!(
        !transport
            .host
            .record_task_evaluation(&task, bundle)
            .await
            .expect("failure event is durable")
    );
    let events = transport
        .host
        .repositories
        .events
        .load(task.session_id, Some(task.task_id), None)
        .await
        .expect("events");
    assert!(events.iter().any(|event| {
        event.event_type == RuntimeEventType::EvaluationCompleted
            && event
                .payload
                .get("summary")
                .and_then(serde_json::Value::as_str)
                == Some("durable task evaluation failed")
    }));
}

#[tokio::test]
async fn reviewed_skill_is_installed_and_injected_only_for_matching_objectives() {
    let transport = EmbeddedTransport::in_memory().await.expect("transport");
    let session_id = SessionId::new();
    let task_id = TaskId::new();
    let task = HostedAgentTask {
        session_id,
        task_id,
        turn_id: TurnId::new(),
        payload: json!({"prompt": "list workspace files"}),
    };
    let evidence_id = EvidenceId::new();
    let evaluation_input = transport
        .host
        .evaluate_completed_task(
            &task,
            HostedTaskEvaluation {
                objective: "list workspace files",
                task_status: TaskStatus::Completed,
                verification: Some(VerificationRecord {
                    verification_id: VerificationId::new(),
                    task_id,
                    objective: "list workspace files".to_owned(),
                    completion_criteria: vec!["files listed".to_owned()],
                    checks: Vec::new(),
                    evidence_refs: vec![evidence_id],
                    result: VerificationResult::Pass,
                    policy_status: "allowed".to_owned(),
                    residual_risks: Vec::new(),
                }),
                tool_reports: &[],
                failure_summary: None,
                latency: Duration::ZERO,
            },
        )
        .await
        .expect("evaluation");
    transport
        .host
        .record_task_evaluation(&task, EvaluationRunner.evaluate_task(evaluation_input))
        .await
        .expect("deep evaluation");
    let skill_id = format!("skill-{task_id}");

    for command in [
        runtime_command(
            session_id,
            SessionCommandKind::StageSkill,
            json!({"candidate_id": skill_id}),
        ),
        runtime_command(
            session_id,
            SessionCommandKind::ReviewSkill,
            json!({
                "skill_id": skill_id,
                "decision": "approve",
                "reason": "verified by maintainer",
                "regression_refs": ["regression-pass"],
            }),
        ),
        runtime_command(
            session_id,
            SessionCommandKind::InstallSkill,
            json!({"skill_id": skill_id}),
        ),
    ] {
        assert!(
            transport
                .send_command(command)
                .await
                .expect("skill command")
                .accepted
        );
    }

    assert!(
        transport
            .host
            .active_skill_context("list workspace files")
            .await
            .expect("skill context")
            .is_some()
    );
    assert!(
        transport
            .host
            .active_skill_context("configure an unrelated provider")
            .await
            .expect("unrelated context")
            .is_none()
    );
}

#[tokio::test]
async fn explicit_home_transport_reuses_latest_session_without_process_env() {
    let workspace = tempdir().expect("workspace");
    let home = tempdir().expect("home");
    let provider_paths = ProviderConfigPaths::from_home(home.path()).expect("provider paths");
    ProviderInstallPlan {
        scope: ProviderConfigScope::User,
        profile: ProviderProfile::mock(),
        activate: true,
        pending_secret: None,
    }
    .apply(&provider_paths)
    .expect("mock provider");
    let paths =
        RuntimePaths::from_home_and_cwd(home.path(), workspace.path()).expect("runtime paths");
    let first = EmbeddedTransport::from_home_and_cwd(home.path(), workspace.path())
        .await
        .expect("first transport");
    let session_id = first.default_session_id();
    first
        .send_command(command(session_id, "list workspace"))
        .await
        .expect("command");
    wait_for_status(&first, session_id, TaskStatus::Completed).await;

    let second = EmbeddedTransport::from_home_and_cwd(home.path(), workspace.path())
        .await
        .expect("second transport");
    let events = second
        .replay_events(EventFilter {
            session_id: second.default_session_id(),
            task_id: None,
            after_sequence_no: None,
        })
        .await
        .expect("events");

    assert_eq!(second.default_session_id(), session_id);
    assert_eq!(second.host.workspace_id, first.host.workspace_id);
    assert!(events.len() >= 7);
    assert!(paths.runtime_db.exists());
    assert!(paths.workspace_state_dir.exists());
    assert!(!workspace.path().join(".golutra").exists());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let mode = |path: &Path| {
            fs::metadata(path)
                .expect("runtime metadata")
                .permissions()
                .mode()
                & 0o777
        };
        assert_eq!(mode(&paths.state_dir), 0o700);
        assert_eq!(mode(&paths.workspace_state_dir), 0o700);
        assert_eq!(mode(&paths.runtime_db), 0o600);
    }
}

#[tokio::test]
async fn list_threads_is_empty_before_first_prompt() {
    let workspace = tempdir().expect("workspace");
    let _home = IsolatedGlobalMockProvider::empty().await;
    let transport = EmbeddedTransport::for_cwd(workspace.path())
        .await
        .expect("transport");

    let threads = transport.list_threads(10).await.expect("threads");

    assert!(threads.is_empty());
}

#[tokio::test]
async fn cwd_transport_does_not_persist_bootstrap_thread_or_project_pointers() {
    let workspace = tempdir().expect("workspace");
    let _home = IsolatedGlobalMockProvider::empty().await;

    let transport = EmbeddedTransport::for_cwd(workspace.path())
        .await
        .expect("transport");
    let error = transport
        .resume_thread(transport.default_thread_id())
        .await
        .expect_err("bootstrap thread is not persisted");

    assert!(error.to_string().contains("not found"));
    assert!(
        transport
            .list_threads(10)
            .await
            .expect("threads")
            .is_empty()
    );
    assert!(!workspace.path().join(".golutra").exists());
}

#[tokio::test]
async fn cwd_transport_selects_latest_thread_without_pointer_files() {
    let workspace = tempdir().expect("workspace");
    let _provider = IsolatedGlobalMockProvider::install().await;
    let first = EmbeddedTransport::for_cwd(workspace.path())
        .await
        .expect("first transport");
    let session_id = first.default_session_id();
    first
        .send_command(command(session_id, "list workspace"))
        .await
        .expect("command");
    wait_for_status(&first, session_id, TaskStatus::Completed).await;
    let original_thread_id = first.default_thread_id();

    let reopened = EmbeddedTransport::for_cwd(workspace.path())
        .await
        .expect("transport selects latest thread");

    assert_eq!(reopened.default_thread_id(), original_thread_id);
    assert_eq!(reopened.default_session_id(), session_id);
    assert!(!workspace.path().join(".golutra").exists());
}

#[tokio::test]
async fn global_store_filters_latest_threads_by_cwd() {
    let cwd_a = tempdir().expect("cwd a");
    let cwd_b = tempdir().expect("cwd b");
    let _provider = IsolatedGlobalMockProvider::install().await;
    let transport_a = EmbeddedTransport::for_cwd(cwd_a.path())
        .await
        .expect("cwd a transport");
    let transport_b = EmbeddedTransport::for_cwd(cwd_b.path())
        .await
        .expect("cwd b transport");
    let session_a = transport_a.default_session_id();
    let session_b = transport_b.default_session_id();
    transport_a
        .send_command(command(session_a, "hello from a"))
        .await
        .expect("cwd a command");
    wait_for_status(&transport_a, session_a, TaskStatus::Completed).await;
    transport_b
        .send_command(command(session_b, "hello from b"))
        .await
        .expect("cwd b command");
    wait_for_status(&transport_b, session_b, TaskStatus::Completed).await;

    let reopened_a = EmbeddedTransport::for_cwd(cwd_a.path())
        .await
        .expect("reopened cwd a");
    let reopened_b = EmbeddedTransport::for_cwd(cwd_b.path())
        .await
        .expect("reopened cwd b");

    assert_eq!(reopened_a.default_session_id(), session_a);
    assert_eq!(reopened_b.default_session_id(), session_b);
    assert_ne!(
        reopened_a.default_thread_id(),
        reopened_b.default_thread_id()
    );
}

#[tokio::test]
async fn session_window_selects_adjacent_newer_and_older_threads_from_an_anchor() {
    let home = tempdir().expect("home");
    let cwd = tempdir().expect("cwd");
    let transport = EmbeddedTransport::from_home_and_cwd(home.path(), cwd.path())
        .await
        .expect("transport");
    let workspace_root = cwd.path().canonicalize().expect("canonical cwd");
    let base = chrono::Utc::now() - chrono::Duration::minutes(10);
    let mut threads = Vec::new();
    for index in 0..5 {
        let at = base + chrono::Duration::minutes(index);
        let thread = ThreadRecord {
            thread_id: ThreadId::new(),
            session_id: SessionId::new(),
            parent_thread_id: None,
            forked_from_turn_id: None,
            forked_from_sequence_no: None,
            workspace_root: Some(workspace_root.display().to_string()),
            rebound_from_workspace_root: None,
            rollout_path: None,
            title: format!("thread-{index}"),
            preview: format!("preview-{index}"),
            created_at: at,
            updated_at: at,
            recency_at: at,
            archived: false,
        };
        transport
            .host
            .repositories
            .threads
            .upsert(&thread)
            .await
            .expect("thread");
        threads.push(thread);
    }

    let newer = transport
        .session_window(SessionWindowRequest {
            anchor_thread_id: threads[2].thread_id,
            range: SessionRangeSpec {
                direction: SessionRangeDirection::Newer,
                count: 3,
            },
        })
        .await
        .expect("newer window");
    assert_eq!(
        newer
            .sessions
            .iter()
            .map(|session| session.title.as_str())
            .collect::<Vec<_>>(),
        vec!["thread-4", "thread-3", "thread-2"]
    );

    let older = transport
        .session_window(SessionWindowRequest {
            anchor_thread_id: threads[2].thread_id,
            range: SessionRangeSpec {
                direction: SessionRangeDirection::Older,
                count: 3,
            },
        })
        .await
        .expect("older window");
    assert_eq!(
        older
            .sessions
            .iter()
            .map(|session| session.title.as_str())
            .collect::<Vec<_>>(),
        vec!["thread-2", "thread-1", "thread-0"]
    );

    let first_page = transport
        .session_page(SessionPageRequest {
            cursor: None,
            limit: 2,
        })
        .await
        .expect("first page");
    assert!(first_page.has_more);
    assert_eq!(
        first_page
            .sessions
            .iter()
            .map(|session| session.title.as_str())
            .collect::<Vec<_>>(),
        vec!["thread-4", "thread-3"]
    );
    let second_page = transport
        .session_page(SessionPageRequest {
            cursor: first_page.next_cursor,
            limit: 2,
        })
        .await
        .expect("second page");
    assert_eq!(
        second_page
            .sessions
            .iter()
            .map(|session| session.title.as_str())
            .collect::<Vec<_>>(),
        vec!["thread-2", "thread-1"]
    );
    assert!(
        first_page
            .sessions
            .iter()
            .all(|session| !second_page.sessions.contains(session))
    );

    let last_page = transport
        .session_page(SessionPageRequest {
            cursor: second_page.next_cursor,
            limit: 2,
        })
        .await
        .expect("last page");
    assert_eq!(
        last_page
            .sessions
            .iter()
            .map(|session| session.title.as_str())
            .collect::<Vec<_>>(),
        vec!["thread-0"]
    );
    assert!(!last_page.has_more);
    assert!(last_page.next_cursor.is_none());
}

#[tokio::test]
async fn session_window_validates_ranges_workspace_and_archived_anchors() {
    let home = tempdir().expect("home");
    let cwd_a = tempdir().expect("cwd a");
    let cwd_b = tempdir().expect("cwd b");
    let transport_a = EmbeddedTransport::from_home_and_cwd(home.path(), cwd_a.path())
        .await
        .expect("transport a");
    let transport_b = EmbeddedTransport::from_home_and_cwd(home.path(), cwd_b.path())
        .await
        .expect("transport b");
    let at = chrono::Utc::now();
    let thread = ThreadRecord {
        thread_id: ThreadId::new(),
        session_id: SessionId::new(),
        parent_thread_id: None,
        forked_from_turn_id: None,
        forked_from_sequence_no: None,
        workspace_root: Some(
            cwd_a
                .path()
                .canonicalize()
                .expect("canonical cwd a")
                .display()
                .to_string(),
        ),
        rebound_from_workspace_root: None,
        rollout_path: None,
        title: "validation-anchor".to_owned(),
        preview: String::new(),
        created_at: at,
        updated_at: at,
        recency_at: at,
        archived: false,
    };
    transport_a
        .host
        .repositories
        .threads
        .upsert(&thread)
        .await
        .expect("thread");

    for count in [0, 501] {
        let error = transport_a
            .session_window(SessionWindowRequest {
                anchor_thread_id: thread.thread_id,
                range: SessionRangeSpec {
                    direction: SessionRangeDirection::Older,
                    count,
                },
            })
            .await
            .expect_err("invalid count must be rejected");
        assert!(error.to_string().contains("between 1 and 500"));
    }
    for limit in [0, 501] {
        let error = transport_a
            .session_page(SessionPageRequest {
                cursor: None,
                limit,
            })
            .await
            .expect_err("invalid page limit must be rejected");
        assert!(error.to_string().contains("between 1 and 500"));
    }
    let error = transport_a
        .session_window(SessionWindowRequest {
            anchor_thread_id: thread.thread_id,
            range: SessionRangeSpec {
                direction: SessionRangeDirection::Single,
                count: 2,
            },
        })
        .await
        .expect_err("single range must contain one session");
    assert!(error.to_string().contains("must have count 1"));

    let error = transport_b
        .session_window(SessionWindowRequest {
            anchor_thread_id: thread.thread_id,
            range: SessionRangeSpec {
                direction: SessionRangeDirection::Single,
                count: 1,
            },
        })
        .await
        .expect_err("foreign workspace anchor must be rejected");
    assert!(error.to_string().contains("does not belong to workspace"));

    let mut archived = thread;
    archived.archived = true;
    transport_a
        .host
        .repositories
        .threads
        .upsert(&archived)
        .await
        .expect("archive thread");
    let error = transport_a
        .session_window(SessionWindowRequest {
            anchor_thread_id: archived.thread_id,
            range: SessionRangeSpec {
                direction: SessionRangeDirection::Single,
                count: 1,
            },
        })
        .await
        .expect_err("archived anchor must be rejected");
    assert!(error.to_string().contains("is archived"));
}

#[tokio::test]
async fn session_page_uses_thread_id_as_a_stable_cursor_tiebreaker() {
    let home = tempdir().expect("home");
    let cwd = tempdir().expect("cwd");
    let transport = EmbeddedTransport::from_home_and_cwd(home.path(), cwd.path())
        .await
        .expect("transport");
    let workspace_root = cwd
        .path()
        .canonicalize()
        .expect("canonical cwd")
        .display()
        .to_string();
    let at = chrono::Utc::now();
    let mut threads = Vec::new();
    for index in 0..4 {
        let thread = ThreadRecord {
            thread_id: ThreadId::new(),
            session_id: SessionId::new(),
            parent_thread_id: None,
            forked_from_turn_id: None,
            forked_from_sequence_no: None,
            workspace_root: Some(workspace_root.clone()),
            rebound_from_workspace_root: None,
            rollout_path: None,
            title: format!("same-time-{index}"),
            preview: String::new(),
            created_at: at,
            updated_at: at,
            recency_at: at,
            archived: false,
        };
        transport
            .host
            .repositories
            .threads
            .upsert(&thread)
            .await
            .expect("thread");
        threads.push(thread);
    }
    threads.sort_by_key(|thread| std::cmp::Reverse(thread.thread_id));

    let first_page = transport
        .session_page(SessionPageRequest {
            cursor: None,
            limit: 2,
        })
        .await
        .expect("first page");
    let second_page = transport
        .session_page(SessionPageRequest {
            cursor: first_page.next_cursor,
            limit: 2,
        })
        .await
        .expect("second page");
    let actual = first_page
        .sessions
        .into_iter()
        .chain(second_page.sessions)
        .map(|session| session.thread_id)
        .collect::<Vec<_>>();
    let expected = threads
        .into_iter()
        .map(|thread| thread.thread_id)
        .collect::<Vec<_>>();

    assert_eq!(actual, expected);
    assert!(!second_page.has_more);
}

#[tokio::test]
async fn debug_export_writes_redacted_session_bundle_atomically() {
    let host = RuntimeHost::in_memory().await.expect("host");
    let session_id = SessionId::new();
    let thread_id = ThreadId::new();
    let task_id = TaskId::new();
    let at = chrono::Utc::now();
    host.repositories
        .threads
        .upsert(&ThreadRecord {
            thread_id,
            session_id,
            parent_thread_id: None,
            forked_from_turn_id: None,
            forked_from_sequence_no: None,
            workspace_root: None,
            rebound_from_workspace_root: None,
            rollout_path: None,
            title: "export fixture".to_owned(),
            preview: "hello".to_owned(),
            created_at: at,
            updated_at: at,
            recency_at: at,
            archived: false,
        })
        .await
        .expect("thread");
    for (event_type, payload) in [
        (
            RuntimeEventType::TaskCreated,
            json!({"payload": {"prompt": "hello export"}}),
        ),
        (
            RuntimeEventType::AssistantMessage,
            json!({"content": "export response"}),
        ),
        (
            RuntimeEventType::TaskCompleted,
            json!({"status": "completed"}),
        ),
    ] {
        host.record_event(host_event(
            0,
            session_id,
            Some(task_id),
            event_type,
            RuntimeEventSource::Runtime,
            payload,
        ))
        .await
        .expect("event");
    }
    let parent = tempdir().expect("export parent");
    let destination = parent.path().join("bundle");
    let transport = RuntimeTransport::Embedded(EmbeddedTransport::new(host));
    let receipt = DebugExportCoordinator::new(&transport)
        .export(DebugExportRequest {
            selection: SessionWindowRequest {
                anchor_thread_id: thread_id,
                range: SessionRangeSpec {
                    direction: SessionRangeDirection::Single,
                    count: 1,
                },
            },
            destination: destination.clone(),
        })
        .await
        .expect("export");

    assert_eq!(receipt.session_count, 1);
    assert_eq!(receipt.task_count, 1);
    assert!(destination.join("manifest.json").is_file());
    assert!(destination.join("conversation.md").is_file());
    assert!(
        destination
            .join(format!("sessions/{session_id}/events.jsonl"))
            .is_file()
    );
    assert!(
        destination
            .join(format!("sessions/{session_id}/conversation.jsonl"))
            .is_file()
    );
    assert!(
        destination
            .join(format!("sessions/{session_id}/tasks/{task_id}/trace.json"))
            .is_file()
    );
    let conversation =
        fs::read_to_string(destination.join(format!("sessions/{session_id}/conversation.jsonl")))
            .expect("conversation");
    assert!(conversation.contains("hello export"));
    assert!(conversation.contains("export response"));
    let manifest: DebugExportManifest =
        serde_json::from_slice(&fs::read(destination.join("manifest.json")).expect("manifest"))
            .expect("manifest json");
    assert_eq!(manifest.mode, "full-redacted");
    assert!(manifest.redacted);
    assert_eq!(manifest.sessions.len(), 1);
}

#[tokio::test]
async fn cwd_attachment_rejects_foreign_session_access() {
    let cwd_a = tempdir().expect("cwd a");
    let cwd_b = tempdir().expect("cwd b");
    let _provider = IsolatedGlobalMockProvider::install().await;
    let transport_a = EmbeddedTransport::for_cwd(cwd_a.path())
        .await
        .expect("cwd a transport");
    let transport_b = EmbeddedTransport::for_cwd(cwd_b.path())
        .await
        .expect("cwd b transport");
    let session_a = transport_a.default_session_id();
    transport_a
        .send_command(command(session_a, "private cwd a conversation"))
        .await
        .expect("cwd a command");
    wait_for_status(&transport_a, session_a, TaskStatus::Completed).await;

    let query_error = transport_b
        .query(RuntimeQuery {
            query_id: QueryId::new(),
            session_id: session_a,
            task_id: None,
            kind: RuntimeQueryKind::SessionState,
            requester: ActorKind::Sdk,
            cursor: None,
            timestamp: chrono::Utc::now(),
        })
        .await
        .expect_err("foreign query must be rejected");
    let replay_error = transport_b
        .replay_events(EventFilter {
            session_id: session_a,
            task_id: None,
            after_sequence_no: None,
        })
        .await
        .expect_err("foreign replay must be rejected");
    let subscription_error = transport_b
        .subscribe(EventFilter {
            session_id: session_a,
            task_id: None,
            after_sequence_no: None,
        })
        .await
        .expect_err("foreign subscription must be rejected");
    let command_error = transport_b
        .send_command(command(session_a, "move this session to cwd b"))
        .await
        .expect_err("foreign command must be rejected");

    for error in [query_error, replay_error, subscription_error, command_error] {
        assert!(matches!(error, ClientError::InvalidSession(_)));
    }
    assert!(
        transport_b
            .list_threads(10)
            .await
            .expect("cwd b threads")
            .is_empty()
    );
    assert_eq!(
        transport_a
            .list_threads(10)
            .await
            .expect("cwd a threads")
            .len(),
        1
    );
}

#[tokio::test]
async fn prompt_updates_resumed_thread_metadata_by_session() {
    let workspace = tempdir().expect("workspace");
    let _provider = IsolatedGlobalMockProvider::install().await;
    let transport = EmbeddedTransport::for_cwd(workspace.path())
        .await
        .expect("transport");
    let parent_thread_id = transport.default_thread_id();
    let parent_session_id = transport.default_session_id();
    transport
        .send_command(command(parent_session_id, "hello parent conversation"))
        .await
        .expect("parent command");
    wait_for_status(&transport, parent_session_id, TaskStatus::Completed).await;
    let child = transport
        .fork_thread(parent_thread_id, None)
        .await
        .expect("fork thread");

    transport
        .send_command(command_with_payload(
            child.session_id,
            json!({
                "prompt": "write child output",
                "path": "child.txt",
                "content": "child",
            }),
        ))
        .await
        .expect("command");
    wait_for_status(&transport, child.session_id, TaskStatus::Completed).await;

    let threads = transport.list_threads(10).await.expect("threads");
    let child_after = threads
        .iter()
        .find(|thread| thread.thread_id == child.thread_id)
        .expect("child thread remains indexed");

    assert_eq!(child_after.preview, "write child output");
    assert_eq!(child_after.parent_thread_id, Some(parent_thread_id));
}

#[tokio::test]
async fn rollout_jsonl_is_complete_checksummed_redacted_and_owner_only() {
    let workspace = tempdir().expect("workspace");
    let _provider = IsolatedGlobalMockProvider::install().await;
    let transport = EmbeddedTransport::for_cwd(workspace.path())
        .await
        .expect("transport");
    let session_id = transport.default_session_id();
    transport
        .send_command(command_with_payload(
            session_id,
            json!({
                "prompt": "hello rollout",
                "api_key": "sk-rollout-secret-123456789",
            }),
        ))
        .await
        .expect("command");
    wait_for_status(&transport, session_id, TaskStatus::Completed).await;

    let export = transport
        .export_thread_rollout(transport.default_thread_id())
        .await
        .expect("rollout export");
    let content = fs::read_to_string(&export.path).expect("rollout content");
    assert!(!content.contains("sk-rollout-secret-123456789"));
    let envelopes = content
        .lines()
        .map(|line| serde_json::from_str::<RolloutEnvelope>(line).expect("rollout line"))
        .collect::<Vec<_>>();
    assert_eq!(envelopes.len(), export.event_count);
    assert_eq!(
        export.last_sequence_no,
        envelopes.last().map(|envelope| envelope.sequence_no)
    );
    for envelope in &envelopes {
        assert_eq!(envelope.version, ROLLOUT_FORMAT_VERSION);
        assert_eq!(envelope.thread_id, transport.default_thread_id());
        assert_eq!(envelope.session_id, session_id);
        let bytes = serde_json::to_vec(&envelope.event).expect("event JSON");
        assert_eq!(
            envelope.checksum,
            format!("sha256:{:x}", Sha256::digest(bytes))
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        assert_eq!(
            fs::metadata(&export.path)
                .expect("rollout metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(rollout_lock_path(Path::new(&export.path)))
                .expect("rollout lock metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}

#[tokio::test]
async fn fork_from_turn_copies_complete_history_with_fresh_runtime_ids() {
    let workspace = tempdir().expect("workspace");
    let _provider = IsolatedGlobalMockProvider::install().await;
    let transport = EmbeddedTransport::for_cwd(workspace.path())
        .await
        .expect("transport");
    let parent_session_id = transport.default_session_id();
    transport
        .send_command(command_with_payload(
            parent_session_id,
            json!({
                "prompt": "first fork turn writes an artifact",
                "path": "fork-parent.txt",
                "content": "parent artifact",
            }),
        ))
        .await
        .expect("first command");
    wait_for_status(&transport, parent_session_id, TaskStatus::Completed).await;
    let after_first = transport
        .host
        .store
        .load_events(parent_session_id, None, None)
        .await
        .expect("first history");
    let first_turn_id = after_first
        .iter()
        .find_map(|event| event.turn_id)
        .expect("first turn");
    transport
        .send_command(command(parent_session_id, "second fork turn"))
        .await
        .expect("second command");
    wait_for_status(&transport, parent_session_id, TaskStatus::Completed).await;

    let child = transport
        .fork_thread(transport.default_thread_id(), Some(first_turn_id))
        .await
        .expect("fork at first turn");
    let child_events = transport
        .host
        .store
        .load_events(child.session_id, None, None)
        .await
        .expect("child history");
    let child_history = child_events
        .iter()
        .filter_map(conversation_history_line)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(child_history.contains("first fork turn writes an artifact"));
    assert!(!child_history.contains("second fork turn"));
    assert_eq!(child.parent_thread_id, Some(transport.default_thread_id()));
    assert_eq!(child.forked_from_turn_id, Some(first_turn_id));
    assert!(child.forked_from_sequence_no.is_some());

    let parent_event_ids = after_first
        .iter()
        .map(|event| event.id)
        .collect::<std::collections::HashSet<_>>();
    assert!(
        child_events
            .iter()
            .all(|event| !parent_event_ids.contains(&event.id))
    );
    assert!(
        child_events
            .iter()
            .all(|event| event.session_id == child.session_id)
    );
    assert!(!is_active_status(
        transport
            .host
            .store
            .query_state(child.session_id, None)
            .await
            .expect("child state")
            .task_status
    ));

    let contributors = transport
        .host
        .context_contributors_for_task(
            child.session_id,
            TaskId::new(),
            "continue child".to_owned(),
            None,
        )
        .await
        .expect("child context");
    let history = contributors
        .iter()
        .find(|contributor| contributor.name == "conversation_history")
        .expect("fork history contributor");
    assert!(
        history
            .content
            .contains("first fork turn writes an artifact")
    );
    assert!(!history.content.contains("second fork turn"));
    let debug = transport
        .host
        .store
        .debug_projection(child.session_id, None)
        .await
        .expect("child debug projection");
    let inherited_artifact = debug
        .artifacts
        .iter()
        .find(|artifact| artifact.session_id == parent_session_id)
        .expect("fork retains immutable parent artifact lineage");
    assert!(
        transport
            .host
            .store
            .load_artifact_bytes(inherited_artifact.artifact_id)
            .await
            .expect("inherited artifact bytes")
            .is_some()
    );
    let export = transport
        .export_thread_rollout(child.thread_id)
        .await
        .expect("child rollout");
    assert_eq!(export.event_count, child_events.len() + 1);
}

#[test]
fn rollout_redaction_preserves_token_counts_and_redacts_credentials() {
    let mut payload = json!({
        "input_tokens": 12,
        "output_tokens": 3,
        "access_token": "secret-access-token",
        "nested": {
            "provider_api_key": "secret-api-key",
            "token": "secret-token",
        }
    });

    redact_rollout_value(&mut payload, None);

    assert_eq!(payload["input_tokens"], 12);
    assert_eq!(payload["output_tokens"], 3);
    assert_eq!(payload["access_token"], "<redacted-secret>");
    assert_eq!(payload["nested"]["provider_api_key"], "<redacted-secret>");
    assert_eq!(payload["nested"]["token"], "<redacted-secret>");
}

#[tokio::test]
async fn rebind_moves_thread_to_current_cwd_and_rebuilds_rollout() {
    let old_workspace = tempdir().expect("old workspace");
    let new_workspace = tempdir().expect("new workspace");
    let _provider = IsolatedGlobalMockProvider::install().await;
    let old_transport = EmbeddedTransport::for_cwd(old_workspace.path())
        .await
        .expect("old transport");
    old_transport
        .send_command(command(
            old_transport.default_session_id(),
            "history before path migration",
        ))
        .await
        .expect("old command");
    wait_for_status(
        &old_transport,
        old_transport.default_session_id(),
        TaskStatus::Completed,
    )
    .await;
    let thread_id = old_transport.default_thread_id();
    let old_thread = old_transport
        .resume_thread(thread_id)
        .await
        .expect("old thread");
    let old_rollout = PathBuf::from(old_thread.rollout_path.expect("old rollout"));
    assert!(old_rollout.exists());

    let new_transport = EmbeddedTransport::for_cwd(new_workspace.path())
        .await
        .expect("new transport");
    let result = new_transport
        .rebind_thread(thread_id, old_workspace.path())
        .await
        .expect("thread rebound");

    assert_eq!(
        result.thread.workspace_root.as_deref(),
        Some(
            new_workspace
                .path()
                .canonicalize()
                .expect("new canonical")
                .to_string_lossy()
                .as_ref()
        )
    );
    assert_eq!(result.checkpoint_compatibility, "historical_only");
    assert!(result.rollout_rebuilt);
    assert!(!old_rollout.exists());
    let new_rollout = PathBuf::from(result.thread.rollout_path.as_ref().expect("new rollout"));
    assert!(new_rollout.exists());
    assert!(
        old_transport
            .list_threads(10)
            .await
            .expect("old threads")
            .is_empty()
    );
    assert_eq!(
        new_transport
            .resume_thread(thread_id)
            .await
            .expect("new thread")
            .session_id,
        old_thread.session_id
    );
    let events = new_transport
        .host
        .store
        .load_events(old_thread.session_id, None, None)
        .await
        .expect("rebound events");
    assert!(
        events
            .iter()
            .any(|event| event.event_type == RuntimeEventType::ThreadRebound)
    );
}

#[tokio::test]
async fn post_task_worker_does_not_claim_or_rewrite_a_foreign_workspace_job() {
    let home = tempdir().expect("home");
    let old_workspace = tempdir().expect("old workspace");
    let new_workspace = tempdir().expect("new workspace");
    let old_paths =
        RuntimePaths::from_home_and_cwd(home.path(), old_workspace.path()).expect("old paths");
    let new_paths =
        RuntimePaths::from_home_and_cwd(home.path(), new_workspace.path()).expect("new paths");
    let store = RuntimeStore::connect_with_artifact_root(
        &old_paths.sqlite_url(),
        old_paths.artifacts_dir.clone(),
    )
    .await
    .expect("store");
    let now = chrono::Utc::now();
    let thread_id = ThreadId::new();
    let session_id = SessionId::new();
    let old_rollout_path = old_paths.rollout_path(thread_id).display().to_string();
    store
        .upsert_thread(&ThreadRecord {
            thread_id,
            session_id,
            parent_thread_id: None,
            forked_from_turn_id: None,
            forked_from_sequence_no: None,
            workspace_root: Some(old_paths.cwd.display().to_string()),
            rebound_from_workspace_root: None,
            rollout_path: Some(old_rollout_path.clone()),
            title: "foreign post-task job".to_owned(),
            preview: "foreign post-task job".to_owned(),
            created_at: now,
            updated_at: now,
            recency_at: now,
            archived: false,
        })
        .await
        .expect("thread");
    let foreign_job = PostTaskJob {
        job_id: PostTaskJobId::new(),
        kind: PostTaskJobKind::DeepEvaluation,
        workspace_id: old_paths.workspace_id().to_string(),
        session_id: session_id.to_string(),
        task_id: TaskId::new(),
        input_refs: Vec::new(),
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
    let local_job = PostTaskJob {
        job_id: PostTaskJobId::new(),
        workspace_id: new_paths.workspace_id().to_string(),
        session_id: SessionId::new().to_string(),
        task_id: TaskId::new(),
        created_at: now + chrono::Duration::milliseconds(1),
        ..foreign_job.clone()
    };
    store
        .enqueue_post_task_job(&foreign_job)
        .await
        .expect("foreign job");
    store
        .enqueue_post_task_job(&local_job)
        .await
        .expect("local job");

    let host = RuntimeHost::from_home_and_cwd(home.path(), new_workspace.path())
        .await
        .expect("new workspace host");
    let mut processed_local = None;
    for _ in 0..40 {
        let job = store
            .post_task_job_by_id(local_job.job_id)
            .await
            .expect("local job state")
            .expect("local job exists");
        if job.attempt > 0 {
            processed_local = Some(job);
            break;
        }
        sleep(Duration::from_millis(25)).await;
    }
    let foreign_after = store
        .post_task_job_by_id(foreign_job.job_id)
        .await
        .expect("foreign job state")
        .expect("foreign job exists");
    let thread_after = store
        .thread_by_id(thread_id)
        .await
        .expect("thread state")
        .expect("thread exists");
    drop(host);

    assert!(
        processed_local.is_some(),
        "local worker did not poll its queue"
    );
    assert_eq!(foreign_after.status, PostTaskJobStatus::Queued);
    assert_eq!(foreign_after.attempt, 0);
    assert_eq!(
        thread_after.rollout_path.as_deref(),
        Some(old_rollout_path.as_str())
    );
}

#[tokio::test]
async fn rebind_rejects_a_rollout_path_outside_the_source_workspace_partition() {
    let old_workspace = tempdir().expect("old workspace");
    let new_workspace = tempdir().expect("new workspace");
    let victim_directory = tempdir().expect("victim directory");
    let victim = victim_directory.path().join("must-remain.txt");
    fs::write(&victim, "keep").expect("victim file");
    let _provider = IsolatedGlobalMockProvider::install().await;
    let old_transport = EmbeddedTransport::for_cwd(old_workspace.path())
        .await
        .expect("old transport");
    old_transport
        .send_command(command(
            old_transport.default_session_id(),
            "history before invalid rebind",
        ))
        .await
        .expect("old command");
    wait_for_status(
        &old_transport,
        old_transport.default_session_id(),
        TaskStatus::Completed,
    )
    .await;
    let task_id = old_transport
        .host
        .store
        .query_state(old_transport.default_session_id(), None)
        .await
        .expect("completed state")
        .active_task_id
        .expect("completed task");
    old_transport
        .host
        .wait_for_deep_task_evaluation(task_id)
        .await;
    let thread_id = old_transport.default_thread_id();
    let mut thread = old_transport
        .resume_thread(thread_id)
        .await
        .expect("old thread");
    thread.rollout_path = Some(victim.display().to_string());
    old_transport
        .host
        .store
        .upsert_thread(&thread)
        .await
        .expect("tampered rollout metadata");
    let new_transport = EmbeddedTransport::for_cwd(new_workspace.path())
        .await
        .expect("new transport");

    let error = new_transport
        .rebind_thread(thread_id, old_workspace.path())
        .await
        .expect_err("foreign rollout path must be rejected");

    assert!(
        error
            .to_string()
            .contains("does not match source workspace")
    );
    assert_eq!(fs::read_to_string(&victim).expect("victim remains"), "keep");
    assert_eq!(
        new_transport
            .host
            .store
            .thread_by_id(thread_id)
            .await
            .expect("thread query")
            .expect("thread remains")
            .workspace_root,
        thread.workspace_root
    );
}

#[tokio::test]
async fn first_prompt_sets_thread_title_from_prompt() {
    let workspace = tempdir().expect("workspace");
    let _provider = IsolatedGlobalMockProvider::install().await;
    let transport = EmbeddedTransport::for_cwd(workspace.path())
        .await
        .expect("transport");
    let default_thread_id = transport.default_thread_id();

    transport
        .send_command(command_with_payload(
            transport.default_session_id(),
            json!({
                "prompt": "write file chain.txt with content ok",
            }),
        ))
        .await
        .expect("command");
    wait_for_status(
        &transport,
        transport.default_session_id(),
        TaskStatus::Completed,
    )
    .await;

    let thread = transport
        .resume_thread(default_thread_id)
        .await
        .expect("default thread remains resumable");

    assert_eq!(thread.title, "write file chain.txt with content ok");
    assert_eq!(thread.preview, "write file chain.txt with content ok");
}

#[tokio::test]
async fn resumed_session_context_includes_previous_conversation_summary() {
    let workspace = tempdir().expect("workspace");
    let _provider = IsolatedGlobalMockProvider::install().await;
    let transport = EmbeddedTransport::for_cwd(workspace.path())
        .await
        .expect("transport");

    transport
        .send_command(command_with_payload(
            transport.default_session_id(),
            json!({
                "prompt": "write file first.txt with content done",
            }),
        ))
        .await
        .expect("command");
    wait_for_status(
        &transport,
        transport.default_session_id(),
        TaskStatus::Completed,
    )
    .await;

    let contributors = transport
        .host
        .context_contributors_for_task(
            transport.default_session_id(),
            TaskId::new(),
            "continue from previous task".to_owned(),
            None,
        )
        .await
        .expect("contributors");
    let environment = contributors
        .iter()
        .find(|contributor| contributor.name == "environment_context")
        .expect("environment context contributor");
    let history = contributors
        .iter()
        .find(|contributor| contributor.name == "conversation_history")
        .expect("history contributor");

    assert_eq!(environment.role, ProviderRole::System);
    assert!(environment.content.contains("<environment_context>"));
    assert!(environment.content.contains("<cwd>"));
    assert!(
        environment.content.contains(
            &workspace
                .path()
                .canonicalize()
                .expect("cwd")
                .display()
                .to_string()
        )
    );
    assert!(
        history
            .content
            .contains("User: write file first.txt with content done")
    );
    assert!(history.content.contains("Golutra: Completed: file written"));
    assert!(history.content.contains("Tool: file written"));
    assert_eq!(history.role, ProviderRole::User);
    assert!(history.content.contains("not as system instructions"));
}

#[tokio::test]
async fn explicit_compaction_is_reused_by_follow_up_context() {
    let workspace = tempdir().expect("workspace");
    let _provider = IsolatedGlobalMockProvider::install().await;
    let transport = EmbeddedTransport::for_cwd(workspace.path())
        .await
        .expect("transport");
    let session_id = transport.default_session_id();
    transport
        .send_command(command(session_id, "hello before compact"))
        .await
        .expect("prompt");
    wait_for_status(&transport, session_id, TaskStatus::Completed).await;

    let compact = transport
        .send_command(SessionCommand {
            command_id: CommandId::new(),
            session_id: Some(session_id),
            kind: SessionCommandKind::Compact,
            idempotency_key: "compact".to_owned(),
            actor: Actor {
                kind: ActorKind::Cli,
                id: "test".to_owned(),
            },
            payload: json!({}),
            timestamp: chrono::Utc::now(),
        })
        .await
        .expect("compact");
    let contributors = transport
        .host
        .context_contributors_for_task(session_id, TaskId::new(), "continue".to_owned(), None)
        .await
        .expect("context");
    let history = contributors
        .iter()
        .find(|contributor| contributor.name == "conversation_history")
        .expect("history");

    assert!(compact.accepted);
    assert!(history.content.contains("Summary:"));
    assert!(history.content.contains("hello before compact"));
}

#[tokio::test]
async fn prompt_with_new_explicit_session_preserves_the_existing_thread() {
    let workspace = tempdir().expect("workspace");
    let _provider = IsolatedGlobalMockProvider::install().await;
    let transport = EmbeddedTransport::for_cwd(workspace.path())
        .await
        .expect("transport");
    let first_session_id = transport.default_session_id();
    transport
        .send_command(command(first_session_id, "first conversation"))
        .await
        .expect("first command");
    wait_for_status(&transport, first_session_id, TaskStatus::Completed).await;

    let second_session_id = SessionId::new();
    transport
        .send_command(command(second_session_id, "second conversation"))
        .await
        .expect("second command");
    wait_for_status(&transport, second_session_id, TaskStatus::Completed).await;
    let threads = transport.list_threads(10).await.expect("threads");

    assert_eq!(threads.len(), 2);
    assert!(
        threads
            .iter()
            .any(|thread| thread.session_id == first_session_id)
    );
    assert!(
        threads
            .iter()
            .any(|thread| thread.session_id == second_session_id)
    );
    assert_ne!(threads[0].thread_id, threads[1].thread_id);
}

#[tokio::test]
async fn prompt_with_explicit_thread_id_does_not_persist_bootstrap_default() {
    let workspace = tempdir().expect("workspace");
    let _provider = IsolatedGlobalMockProvider::install().await;
    let transport = EmbeddedTransport::for_cwd(workspace.path())
        .await
        .expect("transport");
    let default_thread_id = transport.default_thread_id();
    let tui_thread_id = ThreadId::new();
    let tui_session_id = SessionId::new();

    transport
        .send_command(command_with_payload(
            tui_session_id,
            json!({
                "prompt": "write file tui.txt with content ok",
                "_thread_id": tui_thread_id.to_string(),
            }),
        ))
        .await
        .expect("command");
    wait_for_status(&transport, tui_session_id, TaskStatus::Completed).await;
    let threads = transport.list_threads(10).await.expect("threads");
    let tui_thread = threads
        .iter()
        .find(|thread| thread.thread_id == tui_thread_id)
        .expect("tui thread indexed");
    let default_error = transport
        .resume_thread(default_thread_id)
        .await
        .expect_err("bootstrap default remains transient");

    assert_eq!(tui_thread.session_id, tui_session_id);
    assert_eq!(tui_thread.preview, "write file tui.txt with content ok");
    assert!(default_error.to_string().contains("not found"));
}

#[tokio::test]
async fn prompt_runs_mock_agent_loop_and_writes_file() {
    let workspace = tempdir().expect("workspace");
    fs::write(workspace.path().join("result.txt"), "before").expect("before image");
    let _provider = IsolatedGlobalMockProvider::install().await;
    let transport = EmbeddedTransport::for_cwd(workspace.path())
        .await
        .expect("transport");
    let session_id = transport.default_session_id();

    let ack = transport
        .send_command(command_with_payload(
            session_id,
            json!({
                "prompt": "write file",
                "path": "result.txt",
                "content": "done",
            }),
        ))
        .await
        .expect("command");
    let state = wait_for_status(&transport, session_id, TaskStatus::Completed).await;
    let debug = transport
        .query(RuntimeQuery {
            query_id: QueryId::new(),
            session_id,
            task_id: None,
            kind: RuntimeQueryKind::DebugProjection,
            requester: ActorKind::Cli,
            cursor: None,
            timestamp: chrono::Utc::now(),
        })
        .await
        .expect("debug projection");

    assert!(ack.accepted);
    assert_eq!(projection_status(&state), Some(TaskStatus::Completed));
    assert_eq!(
        fs::read_to_string(workspace.path().join("result.txt")).expect("file"),
        "done"
    );
    assert!(
        transport
            .host
            .runtime_paths
            .as_ref()
            .is_some_and(|paths| paths.checkpoints_dir.exists())
    );
    assert!(
        debug["tool_results"]
            .as_array()
            .is_some_and(|items| !items.is_empty())
    );
    assert!(
        debug["artifacts"]
            .as_array()
            .is_some_and(|items| !items.is_empty())
    );
    assert!(debug["events"].as_array().is_some_and(|events| {
        events
            .iter()
            .any(|event| event["event_type"] == json!(RuntimeEventType::TokenUsageRecorded))
    }));
    let events = debug["events"].as_array().expect("debug events");
    let context_snapshot = events
        .iter()
        .find(|event| event["event_type"] == json!(RuntimeEventType::ContextSnapshotCreated))
        .and_then(|event| event["payload"]["snapshot"].as_object())
        .expect("context snapshot event");
    let request_artifact_id = context_snapshot["redacted_request_artifact_ref"]
        .as_str()
        .expect("redacted request artifact ref")
        .parse::<uuid::Uuid>()
        .expect("artifact id");
    assert!(
        context_snapshot["contributor_manifest"]
            .as_array()
            .is_some_and(|contributors| contributors
                .iter()
                .filter(|item| item["included"] == true)
                .all(|item| item["redacted_content_ref"]
                    == Value::String(request_artifact_id.to_string())))
    );
    let request_bytes = transport
        .host
        .store
        .load_artifact_bytes(ArtifactId(request_artifact_id))
        .await
        .expect("request artifact")
        .expect("request artifact bytes");
    let request_json: Value = serde_json::from_slice(&request_bytes).expect("request JSON");
    assert!(request_json["messages"].as_array().is_some_and(|messages| {
        messages
            .iter()
            .any(|message| message["content"] == "write file")
    }));
    assert!(debug["artifacts"].as_array().is_some_and(|artifacts| {
        artifacts
            .iter()
            .any(|artifact| artifact["artifact_type"] == "context_request_redacted")
    }));
    let checkpoint_index = events
        .iter()
        .position(|event| event["event_type"] == json!(RuntimeEventType::CheckpointCreated))
        .expect("checkpoint event");
    let tool_started_index = events
        .iter()
        .position(|event| event["event_type"] == json!(RuntimeEventType::ToolStarted))
        .expect("tool started event");
    let policy_index = events
        .iter()
        .position(|event| event["event_type"] == json!(RuntimeEventType::PolicyEvaluated))
        .expect("policy event");
    let tool_completed_index = events
        .iter()
        .position(|event| event["event_type"] == json!(RuntimeEventType::ToolCompleted))
        .expect("tool completed event");
    let tool_progress = events
        .iter()
        .filter(|event| event["event_type"] == json!(RuntimeEventType::ToolProgress))
        .collect::<Vec<_>>();
    let tool_payload = &events[tool_completed_index]["payload"];
    let operation_changes: Vec<FileChangeSummary> =
        serde_json::from_value(tool_payload["file_changes"].clone())
            .expect("typed operation changes");
    let turn_changes: TurnChangeSummary =
        serde_json::from_value(tool_payload["turn_change_summary"].clone())
            .expect("typed turn changes");
    assert!(tool_started_index < checkpoint_index);
    assert!(policy_index < checkpoint_index);
    assert!(checkpoint_index < tool_completed_index);
    assert_eq!(
        events[tool_started_index]["payload"]["tool_call_id"],
        tool_payload["envelope"]["tool_call_id"]
    );
    assert!(!tool_progress.is_empty());
    assert!(tool_progress.iter().all(|event| {
        event["payload"]["tool_call_id"] == tool_payload["envelope"]["tool_call_id"]
    }));
    assert_eq!(tool_payload["metrics"]["item_count"], 1);
    assert!(tool_payload["metrics"]["duration_ms"].is_u64());
    assert_eq!(
        operation_changes,
        vec![FileChangeSummary {
            path: "result.txt".to_owned(),
            kind: FileChangeKind::Modified,
            added_lines: Some(1),
            removed_lines: Some(1),
        }]
    );
    assert_eq!(turn_changes.files, operation_changes);
    assert_eq!(turn_changes.added_lines, Some(1));
    assert_eq!(turn_changes.removed_lines, Some(1));
    assert!(turn_changes.stats_complete);
    assert!(
        tool_payload["diff_previews"]
            .as_array()
            .is_some_and(|previews| !previews.is_empty())
    );
    let diff_artifact_ref = tool_payload["diff_artifact_ref"]
        .as_str()
        .expect("workspace diff artifact ref");
    assert!(debug["artifacts"].as_array().is_some_and(|artifacts| {
        artifacts.iter().any(|artifact| {
            artifact["artifact_id"] == diff_artifact_ref
                && artifact["artifact_type"] == "workspace_diff"
                && artifact["checksum"]
                    .as_str()
                    .is_some_and(|checksum| checksum.starts_with("sha256:"))
        })
    }));
    assert!(
        events[checkpoint_index]["payload"]["checkpoint"]["artifact_refs"]
            .as_array()
            .is_some_and(|references| !references.is_empty())
    );
    for artifact in debug["artifacts"].as_array().expect("debug artifacts") {
        assert!(
            artifact["provenance_refs"]
                .as_array()
                .is_some_and(|references| {
                    !references.is_empty()
                        && references
                            .iter()
                            .all(|reference| events.iter().any(|event| event["id"] == *reference))
                })
        );
    }
}

#[tokio::test]
async fn prompt_plain_conversation_completes_without_tool_evidence() {
    let workspace = tempdir().expect("workspace");
    let _provider = IsolatedGlobalMockProvider::install().await;
    let transport = EmbeddedTransport::for_cwd(workspace.path())
        .await
        .expect("transport");
    let session_id = transport.default_session_id();

    let ack = transport
        .send_command(command(session_id, "你好"))
        .await
        .expect("command");
    let state = wait_for_status(&transport, session_id, TaskStatus::Completed).await;
    let projection = transport
        .query(RuntimeQuery {
            query_id: QueryId::new(),
            session_id,
            task_id: None,
            kind: RuntimeQueryKind::UserProjection,
            requester: ActorKind::Cli,
            cursor: None,
            timestamp: chrono::Utc::now(),
        })
        .await
        .expect("projection");

    assert!(ack.accepted);
    assert_eq!(projection_status(&state), Some(TaskStatus::Completed));
    assert_eq!(
        projection.get("final_message").and_then(Value::as_str),
        Some("mock provider completed without tool calls")
    );
    let events = transport
        .replay_events(EventFilter {
            session_id,
            task_id: None,
            after_sequence_no: None,
        })
        .await
        .expect("stream events")
        .into_iter()
        .map(|event| serde_json::from_value::<RuntimeEvent>(event).expect("runtime event"))
        .collect::<Vec<_>>();
    let streamed = events
        .iter()
        .position(|event| event.event_type == RuntimeEventType::ProviderStreamed)
        .expect("provider delta");
    let completed = events
        .iter()
        .position(|event| event.event_type == RuntimeEventType::AssistantMessage)
        .expect("assistant message");
    assert!(streamed < completed);
}

#[tokio::test]
async fn approval_command_unblocks_waiting_tool_and_records_resolution() {
    let workspace = tempdir().expect("workspace");
    let _provider = IsolatedGlobalMockProvider::install().await;
    let transport = EmbeddedTransport::for_cwd(workspace.path())
        .await
        .expect("transport");
    let session_id = transport.default_session_id();

    transport
        .send_command(command(session_id, "sleep"))
        .await
        .expect("command");
    let waiting = wait_for_status(&transport, session_id, TaskStatus::WaitingApproval).await;
    let approval_id = waiting
        .get("pending_approval")
        .and_then(Value::as_str)
        .expect("pending approval")
        .to_owned();
    let resolution = transport
        .send_command(SessionCommand {
            command_id: CommandId::new(),
            session_id: Some(session_id),
            kind: SessionCommandKind::Deny,
            idempotency_key: "deny-tool".to_owned(),
            actor: Actor {
                kind: ActorKind::Cli,
                id: "test".to_owned(),
            },
            payload: json!({"approval_id": approval_id}),
            timestamp: chrono::Utc::now(),
        })
        .await
        .expect("approval resolution");
    wait_for_status(&transport, session_id, TaskStatus::Partial).await;
    let events = transport
        .host
        .store
        .load_events(session_id, None, None)
        .await
        .expect("events");

    assert!(resolution.accepted);
    assert!(
        events
            .iter()
            .any(|event| event.event_type == RuntimeEventType::ApprovalRequested)
    );
    assert!(
        events
            .iter()
            .any(|event| event.event_type == RuntimeEventType::ApprovalResolved)
    );
}

#[tokio::test]
async fn observer_must_take_over_before_controlling_or_approving_a_task() {
    let transport = EmbeddedTransport::in_memory().await.expect("transport");
    let session_id = transport.default_session_id();
    transport
        .send_command(command(session_id, "sleep"))
        .await
        .expect("task");
    let waiting = wait_for_status(&transport, session_id, TaskStatus::WaitingApproval).await;
    let approval_id = waiting["pending_approval"]
        .as_str()
        .expect("approval id")
        .to_owned();
    let observer = Actor {
        kind: ActorKind::Tui,
        id: "observer".to_owned(),
    };
    let observer_command = |kind, payload| SessionCommand {
        command_id: CommandId::new(),
        session_id: Some(session_id),
        kind,
        idempotency_key: CommandId::new().to_string(),
        actor: observer.clone(),
        payload,
        timestamp: chrono::Utc::now(),
    };

    let denied = transport
        .send_command(observer_command(
            SessionCommandKind::Deny,
            json!({"approval_id": approval_id}),
        ))
        .await
        .expect("observer deny");
    let abort = transport
        .send_command(observer_command(SessionCommandKind::Abort, json!({})))
        .await
        .expect("observer abort");
    let takeover = transport
        .send_command(observer_command(SessionCommandKind::Takeover, json!({})))
        .await
        .expect("takeover");
    let resolved = transport
        .send_command(observer_command(
            SessionCommandKind::Deny,
            json!({"approval_id": approval_id}),
        ))
        .await
        .expect("new controller deny");
    wait_for_status(&transport, session_id, TaskStatus::Partial).await;

    assert!(!denied.accepted);
    assert!(!abort.accepted);
    assert!(takeover.accepted);
    assert!(resolved.accepted);
}

#[test]
fn plain_conversation_plan_does_not_send_workspace_tools() {
    let _provider = IsolatedGlobalMockProvider::install_blocking();
    let provider_paths = ProviderConfigPaths::global().expect("provider paths");

    let plan = mock_provider_plan(Some(&provider_paths), &json!({"prompt": "你好"}), "你好")
        .expect("provider plan");

    assert!(!plan.touched_code);
    assert!(!plan.workspace_tools_enabled);
}

#[test]
fn live_provider_keeps_workspace_tools_available_for_queued_turns() {
    let home = tempdir().expect("home");
    let store = Arc::new(MemorySecretStore::default());
    let reference = CredentialRef::disk(SecretKind::ApiKey);
    store
        .set(
            &reference,
            &secrecy::SecretString::from("secret".to_owned()),
        )
        .expect("secret");
    let auth = AuthService::new(home.path(), store).expect("auth");
    let mut settings = ProviderSettings::default();
    let profile =
        ProviderProfile::openai_compatible("live", "https://example.com/v1", "model", reference)
            .expect("profile");
    settings.upsert_profile(profile, true);
    let environment = runtime_env_from_settings(&settings, &auth).expect("environment");

    let plan = configured_provider_plan(
        Some(&environment),
        MockProvider::text_response("unused fallback"),
        false,
        false,
    )
    .expect("live provider plan");

    assert!(matches!(
        plan.provider,
        ConfiguredProvider::OpenAiCompatible(_)
    ));
    assert!(plan.workspace_tools_enabled);
}

#[test]
fn workspace_objective_plan_still_sends_workspace_tools() {
    let _provider = IsolatedGlobalMockProvider::install_blocking();
    let provider_paths = ProviderConfigPaths::global().expect("provider paths");

    let plan = mock_provider_plan(
        Some(&provider_paths),
        &json!({"prompt": "读取 README.md"}),
        "读取 README.md",
    )
    .expect("provider plan");

    assert!(!plan.touched_code);
    assert!(plan.workspace_tools_enabled);
}

#[tokio::test]
async fn malformed_provider_config_does_not_silently_fallback_to_mock() {
    let _home = IsolatedGlobalMockProvider::empty().await;
    let paths = ProviderConfigPaths::global().expect("provider paths");
    fs::write(&paths.user_config, "{invalid-json").expect("malformed provider config");

    let error = mock_provider_plan(Some(&paths), &json!({}), "hello")
        .expect_err("malformed config must fail");

    assert!(matches!(error, ProviderError::NotConfigured { .. }));
    assert!(error.to_string().contains("could not be loaded"));
}

#[tokio::test]
async fn ephemeral_runtime_separates_global_provider_config_from_temporary_state() {
    let workspace = tempdir().expect("workspace");
    let provider = IsolatedGlobalMockProvider::install().await;
    let global_provider_paths = ProviderConfigPaths::global().expect("provider paths");
    let transport = EmbeddedTransport::ephemeral_for_cwd(workspace.path())
        .await
        .expect("ephemeral transport");
    let host = transport.host.clone();
    let runtime_paths = host
        .runtime_paths
        .as_ref()
        .expect("temporary runtime paths");

    assert_ne!(runtime_paths.home, global_provider_paths.home);
    assert_eq!(
        host.provider_config_paths
            .as_ref()
            .expect("global provider paths"),
        &global_provider_paths
    );

    let ack = transport
        .send_command(command_with_payload(
            transport.default_session_id(),
            json!({
                "prompt": "write a file",
                "path": "ephemeral.txt",
                "content": "temporary state",
            }),
        ))
        .await
        .expect("write command");
    assert!(ack.accepted);
    wait_for_status(
        &transport,
        transport.default_session_id(),
        TaskStatus::Completed,
    )
    .await;

    assert_eq!(
        fs::read_to_string(workspace.path().join("ephemeral.txt")).expect("written file"),
        "temporary state"
    );
    assert!(
        fs::read_dir(&runtime_paths.checkpoints_dir)
            .expect("checkpoint directory")
            .next()
            .is_some(),
        "ephemeral writes must still produce checkpoints"
    );
    drop(provider);
}

#[tokio::test]
async fn runtime_waits_for_provider_auth_and_resumes_after_verified_reload() {
    let workspace = tempdir().expect("workspace");
    let home = IsolatedGlobalMockProvider::empty().await;
    let paths = ProviderConfigPaths::global().expect("provider paths");
    fs::write(&paths.user_config, "{invalid-json").expect("malformed provider config");
    let transport = EmbeddedTransport::from_home_and_cwd(home._home.path(), workspace.path())
        .await
        .expect("transport");
    let session_id = transport.default_session_id();

    let ack = transport
        .send_command(command(session_id, "hello"))
        .await
        .expect("prompt");
    assert!(ack.accepted);
    wait_for_status(&transport, session_id, TaskStatus::WaitingAuthentication).await;
    let events = transport
        .replay_events(EventFilter {
            session_id,
            task_id: None,
            after_sequence_no: None,
        })
        .await
        .expect("auth events")
        .into_iter()
        .map(|event| serde_json::from_value::<RuntimeEvent>(event).expect("runtime event"))
        .collect::<Vec<_>>();
    let request_id = events
        .iter()
        .find(|event| event.event_type == RuntimeEventType::ProviderAuthRequired)
        .and_then(|event| event.payload.get("request_id"))
        .and_then(Value::as_str)
        .expect("provider auth request")
        .to_owned();

    let mut settings = ProviderSettings::default();
    settings.upsert_profile(ProviderProfile::mock(), true);
    settings
        .save(&paths.user_config)
        .expect("valid provider config");
    let reload = transport
        .send_command(runtime_command(
            session_id,
            SessionCommandKind::ProviderConfigured,
            json!({"request_id": request_id, "verified": true}),
        ))
        .await
        .expect("provider reload");
    assert!(reload.accepted, "{:?}", reload.reason);
    wait_for_status(&transport, session_id, TaskStatus::Completed).await;
    let provider_state = transport
        .query(RuntimeQuery {
            query_id: QueryId::new(),
            session_id,
            task_id: None,
            kind: RuntimeQueryKind::ProviderState,
            requester: ActorKind::Cli,
            cursor: None,
            timestamp: chrono::Utc::now(),
        })
        .await
        .expect("provider state");
    assert_eq!(provider_state["provider"]["protocol"], "mock");

    let events = transport
        .replay_events(EventFilter {
            session_id,
            task_id: None,
            after_sequence_no: None,
        })
        .await
        .expect("completed auth events")
        .into_iter()
        .map(|event| serde_json::from_value::<RuntimeEvent>(event).expect("runtime event"))
        .collect::<Vec<_>>();
    for expected in [
        RuntimeEventType::ProviderAuthRequired,
        RuntimeEventType::ProviderConfigured,
        RuntimeEventType::ProviderProbeCompleted,
        RuntimeEventType::ProviderAuthSubmitted,
        RuntimeEventType::AssistantMessage,
    ] {
        assert!(
            events.iter().any(|event| event.event_type == expected),
            "missing {expected:?}"
        );
    }
}

#[tokio::test]
async fn provider_auth_cancellation_stops_the_waiting_task() {
    let workspace = tempdir().expect("workspace");
    let home = IsolatedGlobalMockProvider::empty().await;
    let paths = ProviderConfigPaths::global().expect("provider paths");
    fs::write(&paths.user_config, "{invalid-json").expect("malformed provider config");
    let transport = EmbeddedTransport::from_home_and_cwd(home._home.path(), workspace.path())
        .await
        .expect("transport");
    let session_id = transport.default_session_id();
    transport
        .send_command(command(session_id, "hello"))
        .await
        .expect("prompt");
    wait_for_status(&transport, session_id, TaskStatus::WaitingAuthentication).await;
    let request_id = transport
        .replay_events(EventFilter {
            session_id,
            task_id: None,
            after_sequence_no: None,
        })
        .await
        .expect("auth events")
        .into_iter()
        .filter_map(|event| serde_json::from_value::<RuntimeEvent>(event).ok())
        .find(|event| event.event_type == RuntimeEventType::ProviderAuthRequired)
        .and_then(|event| event.payload.get("request_id").cloned())
        .expect("request id");

    let ack = transport
        .send_command(runtime_command(
            session_id,
            SessionCommandKind::ProviderAuthCancelled,
            json!({"request_id": request_id}),
        ))
        .await
        .expect("cancel auth");

    assert!(ack.accepted);
    wait_for_status(&transport, session_id, TaskStatus::Cancelled).await;
    let events = transport
        .replay_events(EventFilter {
            session_id,
            task_id: None,
            after_sequence_no: None,
        })
        .await
        .expect("cancel events");
    assert!(events.into_iter().any(|event| {
        serde_json::from_value::<RuntimeEvent>(event)
            .is_ok_and(|event| event.event_type == RuntimeEventType::ProviderAuthCancelled)
    }));
}

#[tokio::test]
async fn prompt_write_file_natural_language_uses_requested_path_and_content() {
    let workspace = tempdir().expect("workspace");
    let _provider = IsolatedGlobalMockProvider::install().await;
    let transport = EmbeddedTransport::for_cwd(workspace.path())
        .await
        .expect("transport");
    let session_id = transport.default_session_id();

    let ack = transport
        .send_command(command(session_id, "write file smoke.txt with content ok"))
        .await
        .expect("command");
    let state = wait_for_status(&transport, session_id, TaskStatus::Completed).await;

    assert!(ack.accepted);
    assert_eq!(projection_status(&state), Some(TaskStatus::Completed));
    assert_eq!(
        fs::read_to_string(workspace.path().join("smoke.txt")).expect("file"),
        "ok"
    );
    assert!(!workspace.path().join("golutra-agent-output.txt").exists());
}

#[test]
fn mock_write_file_args_prefers_payload_over_prompt() {
    let args = mock_write_file_args(
        &json!({
            "path": "explicit.txt",
            "content": "explicit",
        }),
        "write file prompt.txt with content prompt",
    );

    assert_eq!(
        args,
        MockWriteFileArgs {
            path: "explicit.txt".to_owned(),
            content: "explicit".to_owned(),
        }
    );
}

#[test]
fn environment_context_prompt_escapes_xml_text() {
    let prompt = environment_context_prompt(Path::new("/tmp/a&b<c>d"));

    assert!(prompt.contains("<cwd>/tmp/a&amp;b&lt;c&gt;d</cwd>"));
}

#[test]
fn provider_raw_metadata_redacts_secret_assignments_inside_strings() {
    let mut metadata = json!({
        "message": "API_KEY=plain-secret-value",
        "authorization": "Bearer plain-secret-value",
        "token_usage": {"total_tokens": 42}
    });

    redact_provider_json(&mut metadata);

    let serialized = metadata.to_string();
    assert!(!serialized.contains("plain-secret-value"));
    assert_eq!(metadata["message"], "API_KEY=<redacted-secret>");
    assert_eq!(metadata["authorization"], "<redacted-secret>");
    assert_eq!(metadata["token_usage"]["total_tokens"], 42);
}

#[test]
fn provider_raw_artifact_reports_whether_redaction_changed_metadata() {
    let task = HostedAgentTask {
        session_id: SessionId::new(),
        task_id: TaskId::new(),
        turn_id: TurnId::new(),
        payload: json!({}),
    };
    let clean = provider_raw_artifact(&task, task.turn_id, &json!({"finish": "stop"}))
        .expect("clean artifact")
        .0;
    let redacted = provider_raw_artifact(
        &task,
        task.turn_id,
        &json!({"authorization": "Bearer plain-secret-value"}),
    )
    .expect("redacted artifact")
    .0;

    assert_eq!(clean.redaction_status, RedactionStatus::NotRequired);
    assert_eq!(redacted.redaction_status, RedactionStatus::Redacted);
}

#[test]
fn context_request_artifact_redacts_secrets_inside_model_visible_messages() {
    let task = HostedAgentTask {
        session_id: SessionId::new(),
        task_id: TaskId::new(),
        turn_id: TurnId::new(),
        payload: json!({}),
    };
    let plan = ContextBuilder::default()
        .build(
            task.task_id,
            task.turn_id,
            vec![ContextContributor {
                name: "objective".to_owned(),
                role: ProviderRole::User,
                content: "API_KEY=plain-secret-value".to_owned(),
                token_budget_hint: 32,
                source_refs: vec!["fixture:objective".to_owned()],
            }],
        )
        .expect("context plan");
    let request = provider_request_from_plan(
        &plan,
        task.task_id,
        task.turn_id,
        "mock",
        "mock-model",
        Vec::new(),
    );
    let snapshot = context_snapshot_from_request(task.session_id, &plan, &request);
    let (artifact, bytes) =
        context_request_artifact(&task, &snapshot, &request).expect("context artifact");
    let encoded = String::from_utf8(bytes).expect("artifact UTF-8");

    assert_eq!(artifact.artifact_type, "context_request_redacted");
    assert_eq!(artifact.redaction_status, RedactionStatus::Redacted);
    assert!(!encoded.contains("plain-secret-value"));
    assert!(encoded.contains("<redacted-secret>"));
}

#[test]
fn context_compaction_persists_a_redacted_baseline_outside_the_event_payload() {
    let task = HostedAgentTask {
        session_id: SessionId::new(),
        task_id: TaskId::new(),
        turn_id: TurnId::new(),
        payload: json!({}),
    };
    let record = ContextCompactionRecord {
        turn_id: task.turn_id,
        mode: "automatic".to_owned(),
        strategy: "protected_prefix_summary_tail".to_owned(),
        original_message_count: 3,
        replacement_message_count: 2,
        dropped_message_count: 1,
        protected_prefix_len: 1,
        original_estimated_tokens: 120,
        replacement_estimated_tokens: 64,
        planned_tool_tokens: 8,
        budget_limit: 80,
        summary: "provider call completed".to_owned(),
        checksum: "sha256:source".to_owned(),
        replacement_messages: vec![ProviderMessage {
            role: ProviderRole::User,
            content: "API_KEY=plain-secret-value".to_owned(),
            tool_call_id: None,
            tool_name: None,
            tool_calls: Vec::new(),
            metadata: Default::default(),
        }],
    };

    let (artifact, bytes) =
        context_compaction_artifact(&task, &record).expect("compaction artifact");
    let encoded = String::from_utf8(bytes).expect("artifact UTF-8");
    let artifact_payload: Value = serde_json::from_str(&encoded).expect("artifact JSON");
    let (_, _, payload) = trace_event_payload(AgentLoopTraceEvent::ContextAutoCompacted(record))
        .expect("compaction event");

    assert_eq!(artifact.artifact_type, "context_compaction_baseline");
    assert_eq!(artifact.redaction_status, RedactionStatus::Redacted);
    assert!(!encoded.contains("plain-secret-value"));
    assert!(encoded.contains("<redacted-secret>"));
    assert_eq!(artifact_payload["source_checksum"], "sha256:source");
    assert_ne!(artifact_payload["checksum"], "sha256:source");
    assert_eq!(payload["content"], "provider call completed");
    assert!(payload.get("replacement_messages").is_none());
}

#[tokio::test]
async fn context_loads_bounded_root_agents_instructions_as_system_context() {
    let workspace = tempdir().expect("workspace");
    fs::write(
        workspace.path().join("AGENTS.md"),
        "Run cargo fmt before reporting completion.",
    )
    .expect("AGENTS.md");
    let _provider = IsolatedGlobalMockProvider::install().await;
    let transport = EmbeddedTransport::for_cwd(workspace.path())
        .await
        .expect("transport");

    let contributors = transport
        .host
        .context_contributors_for_task(
            transport.default_session_id(),
            TaskId::new(),
            "inspect project".to_owned(),
            None,
        )
        .await
        .expect("contributors");
    let instructions = contributors
        .iter()
        .find(|contributor| contributor.name == "project_instructions")
        .expect("project instructions");

    assert_eq!(instructions.role, ProviderRole::System);
    assert!(instructions.content.contains("Run cargo fmt"));
}

#[cfg(unix)]
#[tokio::test]
async fn project_instruction_symlink_cannot_escape_the_workspace() {
    use std::os::unix::fs::symlink;

    let workspace = tempdir().expect("workspace");
    let outside = tempdir().expect("outside");
    let outside_file = outside.path().join("AGENTS.md");
    fs::write(&outside_file, "outside instructions").expect("outside instructions");
    symlink(&outside_file, workspace.path().join("AGENTS.md")).expect("symlink");
    let canonical_workspace = workspace.path().canonicalize().expect("workspace");

    let error = load_project_instructions(&canonical_workspace)
        .await
        .expect_err("outside symlink must be rejected");

    assert!(error.to_string().contains("outside the workspace"));
}

#[test]
fn bounded_sse_parser_handles_crlf_comments_and_multiline_data() {
    let frame = b": keepalive\r\nevent: message\r\ndata: {\"part\":\r\ndata: true}\r\n\r\n";

    assert!(sse_frame_complete(frame));
    assert_eq!(
        parse_sse_frame(frame).expect("SSE frame"),
        Some(ParsedSseEvent {
            event: "message".to_owned(),
            data: "{\"part\":\ntrue}".to_owned(),
        })
    );
    assert_eq!(parse_sse_frame(b": keepalive\n\n").unwrap(), None);
}

#[tokio::test]
async fn second_embedded_process_cannot_control_a_live_session() {
    let workspace = tempdir().expect("workspace");
    let _provider = IsolatedGlobalMockProvider::install().await;
    let first = EmbeddedTransport::for_cwd(workspace.path())
        .await
        .expect("first transport");
    let session_id = first.default_session_id();
    first
        .send_command(command(session_id, "sleep"))
        .await
        .expect("long-running prompt");

    let second = EmbeddedTransport::for_cwd(workspace.path())
        .await
        .expect("second transport");
    let rejected = second
        .send_command(command(second.default_session_id(), "second"))
        .await
        .expect("rejected command ack");
    let abort = second
        .send_command(SessionCommand {
            command_id: CommandId::new(),
            session_id: Some(second.default_session_id()),
            kind: SessionCommandKind::Abort,
            idempotency_key: "abort".to_owned(),
            actor: Actor {
                kind: ActorKind::Cli,
                id: "test".to_owned(),
            },
            payload: json!({}),
            timestamp: chrono::Utc::now(),
        })
        .await
        .expect("abort");
    let owner_abort = first
        .send_command(SessionCommand {
            command_id: CommandId::new(),
            session_id: Some(session_id),
            kind: SessionCommandKind::Abort,
            idempotency_key: "owner-abort".to_owned(),
            actor: Actor {
                kind: ActorKind::Cli,
                id: "test".to_owned(),
            },
            payload: json!({}),
            timestamp: chrono::Utc::now(),
        })
        .await
        .expect("owner abort");
    wait_for_status(&first, session_id, TaskStatus::Cancelled).await;

    assert!(!rejected.accepted);
    assert!(!abort.accepted);
    assert!(owner_abort.accepted);
}

#[tokio::test]
async fn aborting_lane_rejects_a_new_prompt_until_cancellation_finishes() {
    let host = RuntimeHost::in_memory().await.expect("host");
    let session_id = host.default_session_id();
    let task_id = TaskId::new();
    let actor = Actor {
        kind: ActorKind::Cli,
        id: "test".to_owned(),
    };
    {
        let mut lanes = host.lane_manager.lock().await;
        lanes
            .start_task(
                host.workspace_id,
                session_id,
                task_id,
                TurnId::new(),
                actor,
                1,
            )
            .expect("task starts");
        lanes.abort(session_id, 2).expect("task starts aborting");
    }

    let ack = host
        .clone()
        .handle_command(command(session_id, "start another task"))
        .await
        .expect("command ack");
    let lanes = host.lane_manager.lock().await;
    let lane = lanes.lane(session_id).expect("lane remains active");

    assert!(!ack.accepted);
    assert_eq!(lane.task_id, task_id);
    assert_eq!(lane.status, TaskStatus::Aborting);
}

#[tokio::test]
async fn runtime_recovery_interrupts_unlocked_orphaned_active_tasks() {
    let workspace = tempdir().expect("workspace");
    let _home = IsolatedGlobalMockProvider::empty().await;
    let host = RuntimeHost::for_cwd(workspace.path()).await.expect("host");
    let session_id = host.default_session_id();
    let task_id = TaskId::new();
    host.upsert_current_thread(session_id, &json!({"prompt": "orphaned task"}))
        .await
        .expect("thread");
    host.record_event(host_event(
        host.next_sequence_no(),
        session_id,
        Some(task_id),
        RuntimeEventType::TaskCreated,
        RuntimeEventSource::Runtime,
        json!({"summary": "orphaned task"}),
    ))
    .await
    .expect("event");

    let recovered = host.recover_orphaned_tasks().await.expect("recovery");
    let state = host
        .store
        .query_state(session_id, None)
        .await
        .expect("state");

    assert_eq!(recovered, 1);
    assert_eq!(state.task_status, TaskStatus::Interrupted);
    let events = host
        .store
        .load_events(session_id, Some(task_id), None)
        .await
        .expect("recovery events");
    let recovery = events
        .iter()
        .find(|event| event.event_type == RuntimeEventType::TaskInterrupted)
        .expect("interrupted recovery event");
    assert_eq!(recovery.payload["safe_to_replay"], false);
    assert_eq!(recovery.payload["record"]["reconciliation_required"], false);
    assert_eq!(state.active_task_id, Some(task_id));
}

#[tokio::test]
async fn runtime_recovery_marks_unclosed_side_effects_uncertain_without_replay() {
    let workspace = tempdir().expect("workspace");
    let _home = IsolatedGlobalMockProvider::empty().await;
    let host = RuntimeHost::for_cwd(workspace.path()).await.expect("host");
    let session_id = host.default_session_id();
    let task_id = TaskId::new();
    let turn_id = TurnId::new();
    let tool_call_id = ToolCallId::new();
    host.upsert_current_thread(session_id, &json!({"prompt": "uncertain task"}))
        .await
        .expect("thread");
    let mut task_created = host_event(
        host.next_sequence_no(),
        session_id,
        Some(task_id),
        RuntimeEventType::TaskCreated,
        RuntimeEventSource::Runtime,
        json!({"summary": "task with an unclosed side effect", "runtime_identity": "old"}),
    );
    task_created.turn_id = Some(turn_id);
    host.record_event(task_created).await.expect("task event");
    host.record_event(host_event(
        host.next_sequence_no(),
        session_id,
        Some(task_id),
        RuntimeEventType::CheckpointCreated,
        RuntimeEventSource::Runtime,
        json!({"summary": "before image persisted", "before_image_complete": true}),
    ))
    .await
    .expect("checkpoint event");
    host.record_event(host_event(
        host.next_sequence_no(),
        session_id,
        Some(task_id),
        RuntimeEventType::ToolStarted,
        RuntimeEventSource::Tool,
        json!({
            "summary": "tool write_file started",
            "tool_call_id": tool_call_id,
            "tool_name": "write_file",
            "arguments": {"path": "out.txt"},
        }),
    ))
    .await
    .expect("tool start event");

    assert_eq!(host.recover_orphaned_tasks().await.expect("recovery"), 1);
    let state = host
        .store
        .query_state(session_id, None)
        .await
        .expect("state");
    let events = host
        .store
        .load_events(session_id, Some(task_id), None)
        .await
        .expect("events");
    let recovery = events
        .iter()
        .find(|event| event.event_type == RuntimeEventType::TaskUncertain)
        .expect("uncertain recovery event");

    assert_eq!(state.task_status, TaskStatus::Uncertain);
    assert_eq!(recovery.payload["status"], json!(TaskStatus::Uncertain));
    assert_eq!(recovery.payload["safe_to_replay"], false);
    assert_eq!(recovery.payload["record"]["reconciliation_required"], true);
    assert_eq!(
        recovery.payload["record"]["incomplete_tool_calls"][0]["tool_name"],
        "write_file"
    );
    assert_eq!(
        recovery.payload["record"]["checkpoint_event_refs"]
            .as_array()
            .map(Vec::len),
        Some(1)
    );
}

#[tokio::test]
async fn uncertain_recovery_requires_explicit_reconciliation_before_new_work() {
    let workspace = tempdir().expect("workspace");
    let _home = IsolatedGlobalMockProvider::empty().await;
    let host = RuntimeHost::for_cwd(workspace.path()).await.expect("host");
    let transport = EmbeddedTransport::new(host.clone());
    let session_id = host.default_session_id();
    let task_id = TaskId::new();
    let turn_id = TurnId::new();
    let tool_call_id = ToolCallId::new();

    host.upsert_current_thread(session_id, &json!({"prompt": "uncertain task"}))
        .await
        .expect("thread");
    let mut task_created = host_event(
        host.next_sequence_no(),
        session_id,
        Some(task_id),
        RuntimeEventType::TaskCreated,
        RuntimeEventSource::Runtime,
        json!({"summary": "uncertain task"}),
    );
    task_created.turn_id = Some(turn_id);
    host.record_event(task_created).await.expect("task event");
    host.record_event(host_event(
        host.next_sequence_no(),
        session_id,
        Some(task_id),
        RuntimeEventType::ToolStarted,
        RuntimeEventSource::Tool,
        json!({"tool_call_id": tool_call_id, "tool_name": "write_file"}),
    ))
    .await
    .expect("tool event");
    assert_eq!(host.recover_orphaned_tasks().await.expect("recovery"), 1);

    let rejected = transport
        .send_command(command(session_id, "must wait for reconciliation"))
        .await
        .expect("prompt ack");
    assert!(!rejected.accepted);
    assert!(
        rejected
            .reason
            .as_deref()
            .is_some_and(|reason| reason.contains("reconciliation"))
    );

    let mut reconcile = command(session_id, "");
    reconcile.kind = SessionCommandKind::ReconcileTask;
    reconcile.payload = json!({
        "task_id": task_id,
        "decision": TaskReconciliationDecision::SideEffectObserved,
        "note": "verified the file was written before the host stopped",
    });
    let reconciled = transport
        .send_command(reconcile)
        .await
        .expect("reconciliation ack");
    assert!(reconciled.accepted);

    let state = host
        .store
        .query_state(session_id, None)
        .await
        .expect("state");
    assert_eq!(state.task_status, TaskStatus::Interrupted);
    let events = host
        .store
        .load_events(session_id, Some(task_id), None)
        .await
        .expect("events");
    let reconciliation = events
        .iter()
        .find(|event| event.event_type == RuntimeEventType::TaskReconciled)
        .expect("reconciliation event");
    assert_eq!(
        reconciliation.payload["record"]["decision"],
        "side_effect_observed"
    );
    assert_eq!(
        reconciliation.payload["status"],
        json!(TaskStatus::Interrupted)
    );
}

#[tokio::test]
async fn runtime_recovery_restarts_durable_unstarted_pending_turns() {
    let workspace = tempdir().expect("workspace");
    let _provider = IsolatedGlobalMockProvider::install().await;
    let host = RuntimeHost::for_cwd(workspace.path()).await.expect("host");
    let session_id = host.default_session_id();
    let task_id = TaskId::new();
    let active_turn_id = TurnId::new();
    let pending_turn_id = TurnId::new();
    let second_pending_turn_id = TurnId::new();
    let command_id = CommandId::new();
    let actor = Actor {
        kind: ActorKind::Cli,
        id: "durable-queue-owner".to_owned(),
    };
    host.upsert_current_thread(session_id, &json!({"prompt": "orphaned task"}))
        .await
        .expect("thread");
    let started = host
        .lane_manager
        .lock()
        .await
        .start_task(
            host.workspace_id,
            session_id,
            task_id,
            active_turn_id,
            actor,
            host.next_sequence_no(),
        )
        .expect("orphan task starts");
    host.record_event(started.event).await.expect("task event");
    let queued = host
        .lane_manager
        .lock()
        .await
        .queue_turn(session_id, pending_turn_id, host.next_sequence_no())
        .expect("turn queues");
    host.record_event(with_command_payload(
        queued.event,
        command_id,
        json!({"prompt": "recovered follow up"}),
    ))
    .await
    .expect("queued event");
    let second_queued = host
        .lane_manager
        .lock()
        .await
        .queue_turn(session_id, second_pending_turn_id, host.next_sequence_no())
        .expect("second turn queues");
    host.record_event(with_command_payload(
        second_queued.event,
        CommandId::new(),
        json!({"prompt": "second recovered follow up"}),
    ))
    .await
    .expect("second queued event");
    drop(host);

    let reopened = RuntimeHost::for_cwd(workspace.path())
        .await
        .expect("reopened host");
    let transport = EmbeddedTransport::new(reopened);
    let events = wait_for_task_completed_count(&transport, session_id, 1).await;

    assert!(events.iter().any(|event| {
        event.event_type == RuntimeEventType::TaskInterrupted
            && event.task_id == Some(task_id)
            && event.payload["recovery"] == "runtime_process_restart"
    }));
    assert!(events.iter().any(|event| {
        event.event_type == RuntimeEventType::TurnStarted
            && event.turn_id == Some(pending_turn_id)
            && event.payload["recovery"] == "durable_pending_turn"
    }));
    assert!(events.iter().any(|event| {
        event.event_type == RuntimeEventType::AssistantMessage
            && event.turn_id == Some(pending_turn_id)
    }));
    assert!(events.iter().any(|event| {
        event.event_type == RuntimeEventType::TurnStarted
            && event.turn_id == Some(second_pending_turn_id)
    }));
    assert!(events.iter().any(|event| {
        event.event_type == RuntimeEventType::AssistantMessage
            && event.turn_id == Some(second_pending_turn_id)
    }));
    assert_eq!(
        projection_status(
            &transport
                .query(RuntimeQuery {
                    query_id: QueryId::new(),
                    session_id,
                    task_id: None,
                    kind: RuntimeQueryKind::SessionState,
                    requester: ActorKind::Cli,
                    cursor: None,
                    timestamp: chrono::Utc::now(),
                })
                .await
                .expect("state")
        ),
        Some(TaskStatus::Completed)
    );
}

#[tokio::test]
async fn uncertain_recovery_holds_pending_turns_until_reconciled() {
    let workspace = tempdir().expect("workspace");
    let _provider = IsolatedGlobalMockProvider::install().await;
    let host = RuntimeHost::for_cwd(workspace.path()).await.expect("host");
    let session_id = host.default_session_id();
    let task_id = TaskId::new();
    let active_turn_id = TurnId::new();
    let pending_turn_id = TurnId::new();
    host.upsert_current_thread(session_id, &json!({"prompt": "orphaned task"}))
        .await
        .expect("thread");
    let started = host
        .lane_manager
        .lock()
        .await
        .start_task(
            host.workspace_id,
            session_id,
            task_id,
            active_turn_id,
            Actor {
                kind: ActorKind::Cli,
                id: "uncertain-queue-owner".to_owned(),
            },
            host.next_sequence_no(),
        )
        .expect("task starts");
    host.record_event(started.event).await.expect("task event");
    host.record_event(host_event(
        host.next_sequence_no(),
        session_id,
        Some(task_id),
        RuntimeEventType::ToolStarted,
        RuntimeEventSource::Tool,
        json!({
            "tool_call_id": ToolCallId::new(),
            "tool_name": "write_file",
        }),
    ))
    .await
    .expect("side effect start");
    let queued = host
        .lane_manager
        .lock()
        .await
        .queue_turn(session_id, pending_turn_id, host.next_sequence_no())
        .expect("turn queues");
    host.record_event(with_command_payload(
        queued.event,
        CommandId::new(),
        json!({"prompt": "run only after reconciliation"}),
    ))
    .await
    .expect("queued event");
    drop(host);

    let reopened = RuntimeHost::for_cwd(workspace.path())
        .await
        .expect("reopened host");
    let transport = EmbeddedTransport::new(reopened.clone());
    let held_events = reopened
        .store
        .load_events(session_id, None, None)
        .await
        .expect("held events");
    assert!(held_events.iter().any(|event| {
        event.event_type == RuntimeEventType::TaskUncertain && event.task_id == Some(task_id)
    }));
    assert!(!held_events.iter().any(|event| {
        event.event_type == RuntimeEventType::TurnStarted && event.turn_id == Some(pending_turn_id)
    }));

    let mut reconcile = command(session_id, "");
    reconcile.kind = SessionCommandKind::ReconcileTask;
    reconcile.payload = json!({
        "task_id": task_id,
        "decision": TaskReconciliationDecision::NoSideEffectObserved,
    });
    assert!(
        transport
            .send_command(reconcile)
            .await
            .expect("reconcile")
            .accepted
    );
    let events = wait_for_task_completed_count(&transport, session_id, 1).await;
    assert_eq!(
        events
            .iter()
            .filter(|event| {
                event.event_type == RuntimeEventType::TurnStarted
                    && event.turn_id == Some(pending_turn_id)
            })
            .count(),
        1
    );
    assert!(events.iter().any(|event| {
        event.event_type == RuntimeEventType::AssistantMessage
            && event.turn_id == Some(pending_turn_id)
    }));
}

#[tokio::test]
async fn runtime_recovery_survives_a_crash_after_pending_turn_transfer() {
    let workspace = tempdir().expect("workspace");
    let _provider = IsolatedGlobalMockProvider::install().await;
    let host = RuntimeHost::for_cwd(workspace.path()).await.expect("host");
    let session_id = host.default_session_id();
    let orphaned_task_id = TaskId::new();
    let transferred_task_id = TaskId::new();
    let pending_turn_id = TurnId::new();
    host.upsert_current_thread(session_id, &json!({"prompt": "orphaned task"}))
        .await
        .expect("thread");
    let started = host
        .lane_manager
        .lock()
        .await
        .start_task(
            host.workspace_id,
            session_id,
            orphaned_task_id,
            TurnId::new(),
            Actor {
                kind: ActorKind::Cli,
                id: "transfer-owner".to_owned(),
            },
            host.next_sequence_no(),
        )
        .expect("orphan starts");
    host.record_event(started.event).await.expect("start event");
    let queued = host
        .lane_manager
        .lock()
        .await
        .queue_turn(session_id, pending_turn_id, host.next_sequence_no())
        .expect("turn queues");
    host.record_event(with_command_payload(
        queued.event,
        CommandId::new(),
        json!({"prompt": "recover transferred turn"}),
    ))
    .await
    .expect("queue event");
    let queued_sequence_no = host
        .store
        .load_events(session_id, Some(orphaned_task_id), None)
        .await
        .expect("events")
        .into_iter()
        .find(|event| event.event_type == RuntimeEventType::TurnQueued)
        .expect("queued event")
        .sequence_no;
    host.record_orphaned_task_cancelled(
        session_id,
        Some(orphaned_task_id),
        "runtime_process_restart",
        "orphaned task cancelled during runtime host recovery",
    )
    .await
    .expect("orphan cancelled");
    host.record_event(host_event(
        host.next_sequence_no(),
        session_id,
        Some(transferred_task_id),
        RuntimeEventType::TurnQueued,
        RuntimeEventSource::Runtime,
        json!({
            "summary": "pending transfer persisted before crash",
            "recovery": "durable_pending_turn_batch",
            "recovered_pending_sequence_nos": [queued_sequence_no],
        }),
    ))
    .await
    .expect("transfer batch");
    drop(host);

    let reopened = RuntimeHost::for_cwd(workspace.path())
        .await
        .expect("reopened host");
    let transport = EmbeddedTransport::new(reopened);
    let events = wait_for_task_completed_count(&transport, session_id, 1).await;

    assert!(events.iter().any(|event| {
        event.event_type == RuntimeEventType::AssistantMessage
            && event.turn_id == Some(pending_turn_id)
    }));
    assert_eq!(
        events
            .iter()
            .filter(|event| {
                event.event_type == RuntimeEventType::TurnStarted
                    && event.turn_id == Some(pending_turn_id)
            })
            .count(),
        1
    );
}

#[tokio::test]
async fn task_supervisor_converts_worker_panic_to_terminal_failure_and_cleans_control() {
    let host = RuntimeHost::in_memory().await.expect("host");
    let task = HostedAgentTask {
        session_id: host.default_session_id(),
        task_id: TaskId::new(),
        turn_id: TurnId::new(),
        payload: json!({"prompt": "panic fixture"}),
    };
    let transition = host
        .lane_manager
        .lock()
        .await
        .start_task(
            host.workspace_id,
            task.session_id,
            task.task_id,
            task.turn_id,
            Actor {
                kind: ActorKind::Sdk,
                id: "panic-test".to_owned(),
            },
            host.next_sequence_no(),
        )
        .expect("lane starts");
    host.record_event(transition.event)
        .await
        .expect("task event");
    let (execution, _control) = agent_execution_channel(1);
    let worker = tokio::spawn(async {
        panic!("intentional worker panic");
        #[allow(unreachable_code)]
        Ok::<(), ClientError>(())
    });
    let abort_handle = worker.abort_handle();
    let (completion_sender, completion) = watch::channel(false);
    host.task_controls.lock().await.insert(
        task.session_id,
        HostedTaskControl {
            task_id: task.task_id,
            execution,
            abort_handle,
            completion,
            _session_lease: None,
        },
    );

    host.clone()
        .supervise_agent_task(task.clone(), worker, completion_sender)
        .await;
    let state = host
        .store
        .query_state(task.session_id, None)
        .await
        .expect("state");

    assert_eq!(state.task_status, TaskStatus::Failed);
    assert!(
        !host
            .task_controls
            .lock()
            .await
            .contains_key(&task.session_id)
    );
}

#[tokio::test]
async fn long_lived_host_recovers_an_orphan_when_the_next_prompt_reacquires_its_lease() {
    let workspace = tempdir().expect("workspace");
    let _provider = IsolatedGlobalMockProvider::install().await;
    let host = RuntimeHost::for_cwd(workspace.path()).await.expect("host");
    let transport = EmbeddedTransport::new(host.clone());
    let session_id = host.default_session_id();
    let orphaned_task_id = TaskId::new();
    host.upsert_current_thread(session_id, &json!({"prompt": "orphaned task"}))
        .await
        .expect("thread");
    host.record_event(host_event(
        host.next_sequence_no(),
        session_id,
        Some(orphaned_task_id),
        RuntimeEventType::TaskCreated,
        RuntimeEventSource::Runtime,
        json!({"summary": "orphaned task"}),
    ))
    .await
    .expect("orphaned event");

    let ack = transport
        .send_command(command(session_id, "replacement prompt"))
        .await
        .expect("replacement command");
    let events = wait_for_task_completed_count(&transport, session_id, 1).await;

    assert!(ack.accepted);
    assert!(events.iter().any(|event| {
        event.event_type == RuntimeEventType::TaskInterrupted
            && event.task_id == Some(orphaned_task_id)
            && event.payload.get("recovery").and_then(Value::as_str)
                == Some("session_lease_reacquired")
    }));
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == RuntimeEventType::TaskCreated)
            .count(),
        2
    );
}

#[tokio::test]
async fn abort_cancels_an_unlocked_orphan_without_an_in_memory_task_handle() {
    let host = RuntimeHost::in_memory().await.expect("host");
    let transport = EmbeddedTransport::new(host.clone());
    let session_id = host.default_session_id();
    let orphaned_task_id = TaskId::new();
    host.record_event(host_event(
        host.next_sequence_no(),
        session_id,
        Some(orphaned_task_id),
        RuntimeEventType::TaskCreated,
        RuntimeEventSource::Runtime,
        json!({"summary": "orphaned task"}),
    ))
    .await
    .expect("orphaned event");
    let mut abort = command(session_id, "");
    abort.kind = SessionCommandKind::Abort;
    abort.payload = json!({});

    let ack = transport.send_command(abort).await.expect("abort");
    let state = host
        .store
        .query_state(session_id, None)
        .await
        .expect("state");

    assert!(ack.accepted);
    assert_eq!(state.task_status, TaskStatus::Cancelled);
    assert_eq!(state.active_task_id, Some(orphaned_task_id));
}

fn command(session_id: SessionId, prompt: &str) -> SessionCommand {
    command_with_payload(session_id, json!({"prompt": prompt}))
}

fn install_user_mock_provider() {
    let paths = ProviderConfigPaths::global().expect("provider paths");
    ProviderInstallPlan {
        scope: ProviderConfigScope::User,
        profile: ProviderProfile::mock(),
        activate: true,
        pending_secret: None,
    }
    .apply(&paths)
    .expect("global mock provider");
}

fn command_with_payload(session_id: SessionId, payload: Value) -> SessionCommand {
    SessionCommand {
        command_id: CommandId::new(),
        session_id: Some(session_id),
        kind: SessionCommandKind::Prompt,
        idempotency_key: CommandId::new().to_string(),
        actor: Actor {
            kind: ActorKind::Cli,
            id: "test".to_owned(),
        },
        payload,
        timestamp: chrono::Utc::now(),
    }
}

fn runtime_command(
    session_id: SessionId,
    kind: SessionCommandKind,
    payload: Value,
) -> SessionCommand {
    SessionCommand {
        command_id: CommandId::new(),
        session_id: Some(session_id),
        kind,
        idempotency_key: CommandId::new().to_string(),
        actor: Actor {
            kind: ActorKind::Cli,
            id: "test".to_owned(),
        },
        payload,
        timestamp: chrono::Utc::now(),
    }
}

async fn wait_for_status(
    transport: &EmbeddedTransport,
    session_id: SessionId,
    expected: TaskStatus,
) -> Value {
    for _ in 0..40 {
        let state = transport
            .query(RuntimeQuery {
                query_id: QueryId::new(),
                session_id,
                task_id: None,
                kind: RuntimeQueryKind::SessionState,
                requester: ActorKind::Cli,
                cursor: None,
                timestamp: chrono::Utc::now(),
            })
            .await
            .expect("state");
        if projection_status(&state) == Some(expected) {
            return state;
        }
        sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for status {expected:?}");
}

async fn wait_for_task_completed_count(
    transport: &EmbeddedTransport,
    session_id: SessionId,
    expected_count: usize,
) -> Vec<RuntimeEvent> {
    for _ in 0..40 {
        let event_values = transport
            .replay_events(EventFilter {
                session_id,
                task_id: None,
                after_sequence_no: None,
            })
            .await
            .expect("events");
        let events = event_values
            .into_iter()
            .map(serde_json::from_value::<RuntimeEvent>)
            .collect::<Result<Vec<_>, _>>()
            .expect("typed events");
        let completed_count = events
            .iter()
            .filter(|event| event.event_type == RuntimeEventType::TaskCompleted)
            .count();
        if completed_count >= expected_count {
            return events;
        }
        sleep(Duration::from_millis(25)).await;
    }
    panic!("session did not record {expected_count} completed tasks");
}
