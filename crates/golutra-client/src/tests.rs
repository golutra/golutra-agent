use std::{ffi::OsString, fs, sync::RwLock};

use golutra_auth::{AuthService, CredentialRef, MemorySecretStore, SecretKind, SecretStore};
use golutra_config::{
    ProviderConfigPaths, ProviderConfigScope, ProviderInstallPlan, ProviderProfile,
    ProviderSettings, runtime_env_from_settings,
};
use golutra_core::{
    Actor, ActorKind, CommandId, EvidenceId, QueryId, VerificationCheck, VerificationId,
    VerificationRecord, VerificationResult,
};
use golutra_llm::{ConfiguredProvider, MockProvider, ProviderError};
use golutra_protocol::RuntimeQueryKind;
use tempfile::{TempDir, tempdir};
use tokio::{
    sync::{Mutex, MutexGuard},
    time::{Duration, sleep},
};

use super::*;

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
async fn stale_provisional_command_ack_is_reprocessed_after_owner_exit() {
    let workspace = tempdir().expect("workspace");
    let _provider = IsolatedGlobalMockProvider::install().await;
    let host = RuntimeHost::for_cwd(workspace.path()).await.expect("host");
    let transport = EmbeddedTransport::new(host.clone());
    let session_id = transport.default_session_id();
    let command = command(session_id, "recover claimed command");
    host.store
        .store_command_ack(
            &host.scoped_idempotency_key(&command.idempotency_key),
            &CommandAck {
                command_id: command.command_id,
                accepted: true,
                reason: Some(PROVISIONAL_COMMAND_ACK_REASON.to_owned()),
            },
        )
        .await
        .expect("provisional ack");

    let ack = transport
        .send_command(command)
        .await
        .expect("stale command is retried");
    wait_for_status(&transport, session_id, TaskStatus::Completed).await;

    assert!(ack.accepted);
    assert_ne!(ack.reason.as_deref(), Some(PROVISIONAL_COMMAND_ACK_REASON));
}

#[tokio::test]
async fn successful_task_promotes_retrieves_and_rolls_back_project_memory() {
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
        .expect("promoted memory id")
        .to_owned();

    transport
        .send_command(command(session_id, "list workspace files again"))
        .await
        .expect("second prompt");
    let events = wait_for_task_completed_count(&transport, session_id, 2).await;
    let retrieved = events
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

    assert!(retrieved);
    assert!(rollback.accepted);
}

#[tokio::test]
async fn evaluation_candidate_requires_regression_and_supports_rollback() {
    let transport = EmbeddedTransport::in_memory().await.expect("transport");
    let session_id = SessionId::new();
    let task_id = TaskId::new();
    let task = HostedAgentTask {
        session_id,
        task_id,
        turn_id: TurnId::new(),
        payload: json!({"prompt": "reproduce provider failure"}),
    };
    let evidence_id = EvidenceId::new();
    transport
        .host
        .evaluate_completed_task(
            &task,
            HostedTaskEvaluation {
                objective: "reproduce provider failure",
                task_status: TaskStatus::Failed,
                verification: Some(VerificationRecord {
                    verification_id: VerificationId::new(),
                    task_id,
                    objective: "reproduce provider failure".to_owned(),
                    completion_criteria: vec!["provider succeeds".to_owned()],
                    checks: vec![VerificationCheck {
                        name: "provider".to_owned(),
                        command: None,
                        passed: false,
                        evidence_refs: vec![evidence_id],
                        message: "provider failed".to_owned(),
                    }],
                    evidence_refs: vec![evidence_id],
                    result: VerificationResult::Fail,
                    policy_status: "allowed".to_owned(),
                    residual_risks: vec!["provider request failed".to_owned()],
                }),
                tool_reports: &[],
                failure_summary: Some("provider failed".to_owned()),
                latency: Duration::ZERO,
            },
        )
        .await
        .expect("evaluation");
    let candidate_id = format!("automation-benchmark-{task_id}");
    let apply_without_regression = transport
        .send_command(runtime_command(
            session_id,
            SessionCommandKind::ApplyCandidate,
            json!({"candidate_id": candidate_id}),
        ))
        .await;
    let regression = transport
        .send_command(runtime_command(
            session_id,
            SessionCommandKind::RunRegression,
            json!({"candidate_id": candidate_id}),
        ))
        .await
        .expect("regression");
    let apply = transport
        .send_command(runtime_command(
            session_id,
            SessionCommandKind::ApplyCandidate,
            json!({"candidate_id": candidate_id}),
        ))
        .await
        .expect("apply");
    let rollback = transport
        .send_command(runtime_command(
            session_id,
            SessionCommandKind::RollbackCandidate,
            json!({"candidate_id": candidate_id, "reason": "test rollback"}),
        ))
        .await
        .expect("rollback");

    assert!(matches!(
        apply_without_regression,
        Err(ClientError::Evaluation(
            EvaluationError::InvalidCandidateState { .. }
        ))
    ));
    assert!(regression.accepted);
    assert!(apply.accepted);
    assert!(rollback.accepted);
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
        .context_contributors_for_task(child.session_id, TaskId::new(), "continue child".to_owned())
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
        .context_contributors_for_task(session_id, TaskId::new(), "continue".to_owned())
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
    assert!(tool_started_index < checkpoint_index);
    assert!(policy_index < checkpoint_index);
    assert!(checkpoint_index < tool_completed_index);
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
    let workspace = tempdir().expect("workspace");
    let provider = IsolatedGlobalMockProvider::install_blocking();
    let runtime_paths = RuntimePaths::from_home_and_cwd(provider._home.path(), workspace.path())
        .expect("runtime paths");

    let plan = mock_provider_plan(Some(&runtime_paths), &json!({"prompt": "你好"}), "你好")
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
    let workspace = tempdir().expect("workspace");
    let provider = IsolatedGlobalMockProvider::install_blocking();
    let runtime_paths = RuntimePaths::from_home_and_cwd(provider._home.path(), workspace.path())
        .expect("runtime paths");

    let plan = mock_provider_plan(
        Some(&runtime_paths),
        &json!({"prompt": "读取 README.md"}),
        "读取 README.md",
    )
    .expect("provider plan");

    assert!(!plan.touched_code);
    assert!(plan.workspace_tools_enabled);
}

#[tokio::test]
async fn malformed_provider_config_does_not_silently_fallback_to_mock() {
    let workspace = tempdir().expect("workspace");
    let home = IsolatedGlobalMockProvider::empty().await;
    let paths = ProviderConfigPaths::global().expect("provider paths");
    fs::write(&paths.user_config, "{invalid-json").expect("malformed provider config");
    let runtime_paths = RuntimePaths::from_home_and_cwd(home._home.path(), workspace.path())
        .expect("runtime paths");

    let error = mock_provider_plan(Some(&runtime_paths), &json!({}), "hello")
        .expect_err("malformed config must fail");

    assert!(matches!(error, ProviderError::NotConfigured { .. }));
    assert!(error.to_string().contains("could not be loaded"));
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
async fn runtime_recovery_cancels_unlocked_orphaned_active_tasks() {
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
    assert_eq!(state.task_status, TaskStatus::Cancelled);
    assert_eq!(state.active_task_id, Some(task_id));
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
        event.event_type == RuntimeEventType::TaskAborted
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
        event.event_type == RuntimeEventType::TaskAborted
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
