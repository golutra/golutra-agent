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
    Actor, ActorKind, ArtifactId, ArtifactRecord, CausalRelation, CommandId, EvidenceId,
    FileChangeKind, FileChangeSummary, PolicyId, PostTaskJob, PostTaskJobId, PostTaskJobKind,
    PostTaskJobStatus, QueryId, QuestionId, RedactionStatus, TaskContract, TaskId, TaskStatus,
    ToolCallId, TraceView, TurnChangeSummary, TurnId, UserQuestionMode, UserQuestionOption,
    UserQuestionPrompt, UserQuestionRequest, VerificationId, VerificationRecord,
    VerificationResult, WorkspaceChangeRequirement, WorkspaceId,
};
use golutra_llm::{ConfiguredProvider, MockProvider, ProviderError, ProviderMessage, ProviderRole};
use golutra_protocol::{
    ArtifactReadRequest, EventFilter, RuntimeEventType, RuntimeQueryKind, TaskTraceRequest,
};
use golutra_runtime::agent_execution_channel;
use tempfile::{TempDir, tempdir};
use tokio::{
    sync::{Mutex, MutexGuard},
    time::{Duration, sleep, timeout},
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
fn strict_wire_mode_builds_completion_contract_without_prompt_heuristics() {
    let contract = task_contract_from_payload(&json!({
        "execution_mode": "strict",
        "prompt": "inspect and summarize the workspace",
    }))
    .expect("strict contract");

    assert!(contract.require_objective_validation);
    assert_eq!(
        contract.verification,
        golutra_core::VerificationRequirement::Required
    );
    assert_eq!(contract.max_correction_rounds, 1);
}

#[test]
fn nullable_steering_profile_is_inheritance_not_an_explicit_override() {
    let mut event = host_event(
        1,
        SessionId::new(),
        Some(TaskId::new()),
        RuntimeEventType::TurnQueued,
        RuntimeEventSource::User,
        json!({
            "payload": {
                "prompt": "continue",
                "steer": true,
                "tool_profile": null,
            }
        }),
    );
    event.turn_id = Some(TurnId::new());

    let pending = recovered_pending_turn_from_event(&event)
        .expect("pending turn")
        .expect("valid pending turn");
    assert_eq!(pending.execution.tool_profile, None);
}

#[tokio::test]
async fn task_control_cleanup_wait_has_a_deadline() {
    let (_completion_sender, mut completion) = watch::channel(false);

    let error = wait_for_task_control_cleanup(&mut completion, Duration::from_millis(1))
        .await
        .expect_err("stalled supervisor cleanup must time out");

    assert!(matches!(
        error,
        ClientError::TaskExecution(message)
            if message.contains("did not release the session within 1 milliseconds")
    ));
}

#[tokio::test]
async fn live_subscription_applies_session_and_task_filters() {
    let transport = EmbeddedTransport::in_memory().await.expect("transport");
    let host = transport.host.clone();
    let session_id = transport.default_session_id();
    let selected_task_id = TaskId::new();
    let other_task_id = TaskId::new();
    let mut events = transport.subscribe_live(EventFilter {
        session_id,
        task_id: Some(selected_task_id),
        after_sequence_no: None,
    });

    host.record_event(host_event(
        host.next_sequence_no(),
        session_id,
        Some(other_task_id),
        RuntimeEventType::ToolCompleted,
        RuntimeEventSource::Tool,
        json!({"summary": "unselected task"}),
    ))
    .await
    .expect("other task event");
    host.record_event(host_event(
        host.next_sequence_no(),
        session_id,
        Some(selected_task_id),
        RuntimeEventType::ToolCompleted,
        RuntimeEventSource::Tool,
        json!({"summary": "selected task"}),
    ))
    .await
    .expect("selected task event");

    let event = timeout(Duration::from_secs(1), events.recv())
        .await
        .expect("filtered event timeout")
        .expect("filtered event channel closed");
    assert_eq!(event.task_id, Some(selected_task_id));
    assert_eq!(event.payload["summary"], "selected task");
    assert!(
        timeout(Duration::from_millis(20), events.recv())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn live_subscription_registry_prunes_dropped_receivers_and_stays_bounded() {
    let transport = EmbeddedTransport::in_memory().await.expect("transport");
    let host = transport.host.clone();
    let filter = EventFilter {
        session_id: transport.default_session_id(),
        task_id: None,
        after_sequence_no: None,
    };
    let mut receivers = Vec::new();
    for _ in 0..(MAX_LIVE_SUBSCRIPTIONS + 8) {
        receivers.push(transport.subscribe_live(filter.clone()));
    }
    assert_eq!(
        host.execution
            .live_subscriptions
            .lock()
            .expect("live subscription lock")
            .len(),
        MAX_LIVE_SUBSCRIPTIONS
    );

    drop(receivers);
    let retained_receiver = transport.subscribe_live(filter);
    let subscriptions = host
        .execution
        .live_subscriptions
        .lock()
        .expect("live subscription lock");
    assert_eq!(subscriptions.len(), 1);
    assert_eq!(subscriptions[0].sender.receiver_count(), 1);
    drop(retained_receiver);
}

#[test]
fn system_prompt_preserves_general_autonomy_and_verification_principles() {
    let prompt = system_prompt();
    for principle in [
        "understand the user's intent",
        "choose the most effective approach",
        "never invent observable facts",
        "Follow existing project conventions",
        "carry the task through implementation and verification",
        "consequential ambiguity",
        "user-facing path when relevant",
        "remaining blockers concisely",
    ] {
        assert!(prompt.contains(principle), "{principle}");
    }
    assert!(!prompt.contains("bash -lc"));
    assert!(!prompt.contains("write_file"));
    assert!(!prompt.contains("ask_user"));
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
        provider_tool_call_id: None,
        tool_name: "read_file".to_owned(),
        display_arguments: json!({"path": "README.md"}),
        recovery_policy: golutra_core::ToolRecoveryPolicy::for_side_effect(
            golutra_core::SideEffectType::None,
        ),
    });
    assert_eq!(required.event_type, RuntimeEventType::ToolStarted);
    assert_eq!(required.source, RuntimeEventSource::Tool);
    assert_eq!(required.integrity, ObservationIntegrityClass::Supporting);

    let candidate = observation_descriptor(&RuntimeObservation::CandidateReady {
        turn_id: TurnId::new(),
        tool_count: 1,
        has_assistant_message: true,
    });
    assert_eq!(candidate.event_type, RuntimeEventType::CandidateReady);
    assert_eq!(candidate.integrity, ObservationIntegrityClass::Required);

    let verification = observation_descriptor(&RuntimeObservation::VerificationReady {
        plan_id: golutra_core::VerificationPlanId::new(),
    });
    assert_eq!(verification.event_type, RuntimeEventType::VerificationReady);

    let diagnostic = observation_descriptor(&RuntimeObservation::ProviderStreamed {
        request_id: golutra_core::ProviderRequestId::new(),
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
fn ephemeral_state_directory_requires_a_new_absolute_directory() {
    let workspace = tempdir().expect("workspace");
    let parent = tempdir().expect("state parent");
    let state_dir = parent.path().join("run");

    let paths = RuntimePaths::for_ephemeral_state_dir(&state_dir, workspace.path())
        .expect("new state directory");
    let canonical_state_dir = state_dir.canonicalize().expect("state home");
    assert_eq!(paths.home, canonical_state_dir);
    assert!(paths.runtime_db.starts_with(&paths.home));

    let existing = RuntimePaths::for_ephemeral_state_dir(&state_dir, workspace.path())
        .expect_err("state directory cannot be reused");
    assert!(existing.to_string().contains("already exists"));

    let relative = RuntimePaths::for_ephemeral_state_dir("relative-run", workspace.path())
        .expect_err("state directory must be absolute");
    assert!(relative.to_string().contains("must be absolute"));
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
        protocol_version: RUNTIME_PROTOCOL_VERSION,
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
        refresh_lock: Arc::new(tokio::sync::Mutex::new(())),
        lifecycle: crate::transport::TransportLifecycle::default(),
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
async fn unimplemented_protocol_commands_fail_closed() {
    let transport = EmbeddedTransport::in_memory().await.expect("transport");
    let session_id = transport.default_session_id();

    for kind in [SessionCommandKind::Verify, SessionCommandKind::Export] {
        let ack = transport
            .send_command(runtime_command(session_id, kind, json!({})))
            .await
            .expect("unsupported command ack");
        assert!(!ack.accepted, "{kind:?}");
        assert!(
            ack.reason
                .as_deref()
                .is_some_and(|reason| reason.contains("is not supported")),
            "{kind:?}: {:?}",
            ack.reason
        );
    }

    let events = transport
        .replay_events(EventFilter {
            session_id,
            task_id: None,
            after_sequence_no: None,
        })
        .await
        .expect("events");
    assert_eq!(
        events
            .iter()
            .filter(|event| event["event_type"] == json!(RuntimeEventType::CommandRejected))
            .count(),
        2
    );
    assert!(
        events
            .iter()
            .all(|event| event["event_type"] != json!(RuntimeEventType::CommandAccepted))
    );
}

#[tokio::test]
async fn failed_event_append_does_not_pollute_causal_lifecycle_indexes() {
    let host = RuntimeHost::in_memory().await.expect("host");
    let session_id = host.default_session_id();
    let task_id = TaskId::new();
    let request_id = golutra_core::ProviderRequestId::new();
    let task = host_event(
        host.next_sequence_no(),
        session_id,
        Some(task_id),
        RuntimeEventType::TaskCreated,
        RuntimeEventSource::Runtime,
        json!({"summary": "causal rollback"}),
    );
    let task_event_id = task.id;
    host.record_event(task).await.expect("task event");

    let mut failed_provider_start = host_event(
        host.next_sequence_no(),
        session_id,
        Some(task_id),
        RuntimeEventType::ProviderStarted,
        RuntimeEventSource::Provider,
        json!({"provider_request_id": request_id}),
    );
    // Force the SQL append to fail after the in-memory ledger has been enriched.
    failed_provider_start.id = task_event_id;
    assert!(host.record_event(failed_provider_start).await.is_err());

    let provider_completed = host_event(
        host.next_sequence_no(),
        session_id,
        Some(task_id),
        RuntimeEventType::ProviderCompleted,
        RuntimeEventSource::Provider,
        json!({"provider_request_id": request_id}),
    );
    host.record_event(provider_completed)
        .await
        .expect("provider event");
    let events = host
        .storage
        .store
        .load_events(session_id, Some(task_id), None)
        .await
        .expect("events");
    let completed = events
        .iter()
        .find(|event| event.event_type == RuntimeEventType::ProviderCompleted)
        .expect("completed provider event");
    assert!(
        !completed
            .causal_links
            .iter()
            .any(|link| link.relation == CausalRelation::RespondsTo)
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
    let (execution, _control) = agent_execution_channel(1);
    let process_cancellation = execution.cancellation_token();
    let first = host
        .build_tool_executor(
            WorkspacePolicy::new(&root).expect("first policy"),
            root.clone(),
            false,
            false,
        )
        .await
        .expect("first executor");
    let start_request = ToolRequest {
        tool_call_id: ToolCallId::new(),
        provider_tool_call_id: None,
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
        .execute_with_policy(start_request, policy, true, process_cancellation)
        .await
        .expect("start process");
    let process_id = started.envelope.structured_facts["process_id"]
        .as_str()
        .expect("process id")
        .to_owned();

    let task_id = TaskId::new();
    let root_context = crate::delegation_policy::DelegationContext::root(
        session_id,
        Some(10_000),
        Some(1_024),
        None,
        execution.cancellation_token(),
    );
    let worker = tokio::spawn(async { Ok::<(), ClientError>(()) });
    let abort_handle = worker.abort_handle();
    let (completion_sender, completion) = watch::channel(false);
    host.execution.task_controls.lock().await.insert(
        session_id,
        HostedTaskControl {
            task_id,
            allow_network: false,
            yolo: false,
            provider_settings: ProviderTurnSettings::default(),
            execution,
            abort_handle,
            completion,
            delegation: Some(root_context),
            _session_lease: None,
        },
    );
    host.clone()
        .supervise_agent_task(
            HostedAgentTask {
                session_id,
                task_id,
                turn_id: TurnId::new(),
                payload: json!({"prompt": "ordinary root task"}),
            },
            worker,
            completion_sender,
        )
        .await;

    let second = host
        .build_tool_executor(
            WorkspacePolicy::new(&root).expect("second policy"),
            root,
            false,
            false,
        )
        .await
        .expect("second executor");
    sleep(Duration::from_millis(100)).await;
    let reconnected = second
        .execute(
            ToolRequest {
                tool_call_id: ToolCallId::new(),
                provider_tool_call_id: None,
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
async fn hosted_execution_channel_exposes_one_active_surface() {
    let (execution, _control) = agent_execution_channel(2);
    execution.set_active_execution_surface(
        Some(golutra_protocol::AgentExecutionMode::Open),
        golutra_protocol::AgentToolProfile::Coding,
    );
    assert_eq!(
        execution.active_execution_surface(),
        golutra_runtime::ActiveExecutionSurface {
            execution_mode: Some(golutra_protocol::AgentExecutionMode::Open),
            tool_profile: golutra_protocol::AgentToolProfile::Coding,
        }
    );
}

#[tokio::test]
async fn embedded_transport_close_awaits_managed_process_shutdown() {
    let workspace = tempdir().expect("workspace");
    let host = RuntimeHost::in_memory().await.expect("host");
    let transport = EmbeddedTransport::new(host.clone());
    let session_id = host.default_session_id();
    let executor = host
        .build_tool_executor(
            WorkspacePolicy::new(workspace.path()).expect("policy"),
            workspace.path().to_path_buf(),
            false,
            false,
        )
        .await
        .expect("executor");
    let request = ToolRequest {
        tool_call_id: ToolCallId::new(),
        provider_tool_call_id: None,
        session_id,
        turn_id: Some(TurnId::new()),
        tool_name: "shell".to_owned(),
        arguments: json!({
            "command": "sleep 30",
            "background": true,
            "yield_time_ms": 1,
        }),
    };
    let policy = executor.evaluate(&request).expect("shell policy");
    let started = executor
        .execute_with_policy(request, policy, true, CancellationToken::new())
        .await
        .expect("start managed process");
    assert_eq!(
        started.envelope.structured_facts["process_state"],
        "running"
    );

    RuntimeTransport::Embedded(transport)
        .close()
        .await
        .expect("embedded close waits for process shutdown");
    assert!(
        !host
            .execution
            .process_supervisor
            .has_running_processes()
            .await
    );
}

#[tokio::test]
async fn runtime_close_waits_for_an_inflight_command_dispatch() {
    let host = RuntimeHost::in_memory().await.expect("host");
    let command_guard = host.execution.command_mutex.lock().await;
    let closing_host = host.clone();
    let close_task = tokio::spawn(async move { closing_host.close().await });

    tokio::time::sleep(Duration::from_millis(25)).await;
    assert!(
        !close_task.is_finished(),
        "close must wait for a command already inside the dispatcher"
    );

    drop(command_guard);
    timeout(Duration::from_secs(1), close_task)
        .await
        .expect("close should not remain blocked after the command exits")
        .expect("close task")
        .expect("runtime close");
}

#[tokio::test]
async fn runtime_close_rejects_a_prompt_waiting_on_the_dispatch_barrier() {
    let host = RuntimeHost::in_memory().await.expect("host");
    let session_id = host.default_session_id();
    let command_guard = host.execution.command_mutex.lock().await;
    let closing_host = host.clone();
    let close_task = tokio::spawn(async move { closing_host.close().await });

    tokio::time::sleep(Duration::from_millis(25)).await;
    let command_host = host.clone();
    let prompt_task = tokio::spawn(async move {
        command_host
            .handle_command(command(session_id, "must not start during close"))
            .await
            .expect("prompt acknowledgement")
    });
    tokio::task::yield_now().await;
    drop(command_guard);

    timeout(Duration::from_secs(1), close_task)
        .await
        .expect("close should finish")
        .expect("close task")
        .expect("runtime close");
    let ack = timeout(Duration::from_secs(1), prompt_task)
        .await
        .expect("prompt should finish")
        .expect("prompt task");
    assert!(!ack.accepted);
    assert_eq!(ack.reason.as_deref(), Some("runtime host is shutting down"));
}

#[tokio::test]
async fn runtime_network_capability_requires_both_host_and_turn_grants() {
    let workspace = tempdir().expect("workspace");
    let root = workspace.path().to_path_buf();

    let isolated = RuntimeHost::ephemeral_for_cwd(&root)
        .await
        .expect("isolated host");
    let isolated_executor = isolated
        .build_tool_executor(
            WorkspacePolicy::new(&root).expect("isolated policy"),
            root.clone(),
            true,
            false,
        )
        .await
        .expect("isolated executor");
    let isolated_request = ToolRequest {
        tool_call_id: ToolCallId::new(),
        provider_tool_call_id: None,
        session_id: isolated.default_session_id(),
        turn_id: Some(TurnId::new()),
        tool_name: "shell".to_owned(),
        arguments: json!({"command": "echo isolated"}),
    };
    let isolated_policy = isolated_executor
        .evaluate(&isolated_request)
        .expect("isolated policy evaluation");
    let isolated_report = isolated_executor
        .execute_with_policy(
            isolated_request,
            isolated_policy,
            true,
            CancellationToken::new(),
        )
        .await
        .expect("isolated command");
    assert_eq!(
        isolated_report.envelope.structured_facts["network_access"],
        false
    );

    let enabled = RuntimeHost::ephemeral_for_cwd_with_options(
        &root,
        RuntimeExecutionOptions::with_network_access(true),
    )
    .await
    .expect("network-enabled host");
    let enabled_executor = enabled
        .build_tool_executor(
            WorkspacePolicy::new(&root).expect("enabled policy"),
            root,
            true,
            false,
        )
        .await
        .expect("enabled executor");
    let enabled_request = ToolRequest {
        tool_call_id: ToolCallId::new(),
        provider_tool_call_id: None,
        session_id: enabled.default_session_id(),
        turn_id: Some(TurnId::new()),
        tool_name: "shell".to_owned(),
        arguments: json!({"command": "echo enabled"}),
    };
    let enabled_policy = enabled_executor
        .evaluate(&enabled_request)
        .expect("enabled policy evaluation");
    let enabled_report = enabled_executor
        .execute_with_policy(
            enabled_request,
            enabled_policy,
            true,
            CancellationToken::new(),
        )
        .await
        .expect("enabled command");
    assert_eq!(
        enabled_report.envelope.structured_facts["network_access"],
        true
    );
}

#[tokio::test]
async fn delegated_task_inherits_overrides_and_archives_an_isolated_child() {
    const DELEGATION_FIXTURE_BUDGET_MS: u64 = 120_000;

    let workspace = tempdir().expect("workspace");
    let _provider = IsolatedGlobalMockProvider::install().await;
    let host = RuntimeHost::ephemeral_for_cwd(workspace.path())
        .await
        .expect("host");
    let parent_session_id = host.default_session_id();
    let parent_turn_id = TurnId::new();
    host.upsert_current_thread(
        parent_session_id,
        &json!({"prompt": "parent delegation fixture"}),
    )
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
    let parent_task_id = TaskId::new();
    let (execution, _control) = agent_execution_channel(1);
    execution.set_active_execution_surface(
        Some(golutra_protocol::AgentExecutionMode::Open),
        golutra_protocol::AgentToolProfile::Coding,
    );
    let execution_cancellation = execution.cancellation_token();
    let parent_worker = tokio::spawn(std::future::pending::<()>());
    let (completion_sender, completion) = watch::channel(false);
    host.execution.task_controls.lock().await.insert(
        parent_session_id,
        HostedTaskControl {
            task_id: parent_task_id,
            allow_network: false,
            yolo: false,
            provider_settings: ProviderTurnSettings {
                profile: Some(json!("mock")),
                model: Some(json!("parent-model")),
                generation_config: Some(json!({
                    "reasoning_effort": "medium",
                    "context_window_size": 32_000,
                    "max_tokens": 2_000,
                })),
            },
            execution,
            abort_handle: parent_worker.abort_handle(),
            completion,
            delegation: Some(crate::delegation_policy::DelegationContext::root(
                parent_session_id,
                Some(DELEGATION_FIXTURE_BUDGET_MS),
                Some(2_000),
                None,
                execution_cancellation,
            )),
            _session_lease: None,
        },
    );
    let backend = crate::delegation::RuntimeTaskDelegationBackend::new(Arc::downgrade(&host));
    let inherited_request = ToolRequest {
        tool_call_id: ToolCallId::new(),
        provider_tool_call_id: Some("delegate-inherited".to_owned()),
        session_id: parent_session_id,
        turn_id: Some(parent_turn_id),
        tool_name: "delegate_task".to_owned(),
        arguments: json!({"task": "Summarize the number seven."}),
    };

    let (inherited, concurrent_retry) = tokio::join!(
        golutra_tools::TaskDelegationBackend::delegate(
            &backend,
            &inherited_request,
            CancellationToken::new(),
        ),
        golutra_tools::TaskDelegationBackend::delegate(
            &backend,
            &inherited_request,
            CancellationToken::new(),
        ),
    );
    let inherited = inherited.expect("inherited delegation");
    let concurrent_retry = concurrent_retry.expect("concurrent idempotent retry");
    assert_eq!(inherited.status, golutra_core::ToolResultStatus::Ok);
    assert_eq!(concurrent_retry, inherited);
    assert_eq!(
        inherited.structured_facts["requested_model"],
        "parent-model"
    );
    assert_eq!(inherited.structured_facts["effective_model"], "mock-model");
    assert_eq!(
        inherited.structured_facts["effective_reasoning_effort"],
        "medium"
    );
    let inherited_session_id = inherited.structured_facts["child_session_id"]
        .as_str()
        .expect("child session id")
        .parse::<SessionId>()
        .expect("valid child session id");
    let inherited_thread = host
        .storage
        .repositories
        .threads
        .by_session(inherited_session_id)
        .await
        .expect("child lookup")
        .expect("child thread");
    assert_eq!(inherited_thread.parent_thread_id, Some(parent_thread_id));
    assert!(inherited_thread.archived);
    let inherited_events = host
        .storage
        .repositories
        .events
        .load(inherited_session_id, None, None)
        .await
        .expect("child events");
    let inherited_payload = inherited_events
        .iter()
        .find(|event| event.event_type == RuntimeEventType::TaskCreated)
        .and_then(|event| event.payload.get("payload"))
        .expect("child task payload");
    assert_eq!(inherited_payload["provider_profile"], "mock");
    assert_eq!(inherited_payload["provider_model"], "parent-model");
    assert_eq!(inherited_payload["execution_mode"], "open");
    assert_eq!(inherited_payload["tool_profile"], "coding");
    assert_eq!(
        inherited_payload["provider_generation_config"],
        json!({
            "reasoning_effort": "medium",
            "context_window_size": 32_000,
            "max_tokens": 2_000,
        })
    );
    assert_eq!(inherited_payload["_delegation"]["depth"], 1);
    assert_eq!(
        inherited_payload["_delegation"]["root_session_id"],
        json!(parent_session_id)
    );
    assert_eq!(
        inherited_payload["_delegation"]["parent_session_id"],
        json!(parent_session_id)
    );
    assert_eq!(
        inherited_payload["_delegation"]["parent_task_id"],
        json!(parent_task_id)
    );
    assert_eq!(
        inherited_payload["_delegation"]["parent_thread_id"],
        json!(parent_thread_id)
    );
    let parent_delegation = host
        .execution
        .task_controls
        .lock()
        .await
        .get(&parent_session_id)
        .and_then(|control| control.delegation.clone())
        .expect("parent delegation budget");
    assert_eq!(
        parent_delegation.metadata()["budget"]["started_children"],
        1
    );

    let retried = golutra_tools::TaskDelegationBackend::delegate(
        &backend,
        &inherited_request,
        CancellationToken::new(),
    )
    .await
    .expect("idempotent delegation retry");
    assert_eq!(
        retried.structured_facts["child_session_id"],
        inherited.structured_facts["child_session_id"]
    );
    assert_eq!(
        parent_delegation.metadata()["budget"]["started_children"],
        1
    );

    let changed_arguments = golutra_tools::TaskDelegationBackend::delegate(
        &backend,
        &ToolRequest {
            arguments: json!({"task": "Summarize the number eight."}),
            ..inherited_request.clone()
        },
        CancellationToken::new(),
    )
    .await
    .expect("changed delegation arguments");
    assert_ne!(
        changed_arguments.structured_facts["child_session_id"],
        inherited.structured_facts["child_session_id"]
    );

    let overridden = golutra_tools::TaskDelegationBackend::delegate(
        &backend,
        &ToolRequest {
            tool_call_id: ToolCallId::new(),
            provider_tool_call_id: Some("delegate-overridden".to_owned()),
            session_id: parent_session_id,
            turn_id: Some(parent_turn_id),
            tool_name: "delegate_task".to_owned(),
            arguments: json!({
                "task": "Summarize the number eight.",
                "model": "child-model",
                "reasoning_effort": "xhigh",
            }),
        },
        CancellationToken::new(),
    )
    .await
    .expect("overridden delegation");
    assert_eq!(
        overridden.structured_facts["requested_model"],
        "child-model"
    );
    assert_eq!(overridden.structured_facts["effective_model"], "mock-model");
    assert_eq!(
        overridden.structured_facts["effective_reasoning_effort"],
        "xhigh"
    );
    let overridden_session_id = overridden.structured_facts["child_session_id"]
        .as_str()
        .expect("overridden child session id")
        .parse::<SessionId>()
        .expect("valid overridden child session id");
    let overridden_thread = host
        .storage
        .repositories
        .threads
        .by_session(overridden_session_id)
        .await
        .expect("overridden child lookup")
        .expect("overridden child thread");
    assert_eq!(overridden_thread.parent_thread_id, Some(parent_thread_id));
    assert!(overridden_thread.archived);
    let overridden_events = host
        .storage
        .repositories
        .events
        .load(overridden_session_id, None, None)
        .await
        .expect("overridden child events");
    let overridden_payload = overridden_events
        .iter()
        .find(|event| event.event_type == RuntimeEventType::TaskCreated)
        .and_then(|event| event.payload.get("payload"))
        .expect("overridden child task payload");
    assert_eq!(overridden_payload["provider_model"], "child-model");
    assert_eq!(overridden_payload["execution_mode"], "open");
    assert_eq!(overridden_payload["tool_profile"], "coding");
    assert_eq!(
        overridden_payload["provider_generation_config"],
        json!({
            "reasoning_effort": "xhigh",
            "context_window_size": 32_000,
            "max_tokens": 2_000,
        })
    );

    host.execution
        .task_controls
        .lock()
        .await
        .remove(&parent_session_id);
    drop(completion_sender);
    parent_worker.abort();
}

#[tokio::test]
async fn cancelled_delegation_does_not_create_a_child_session() {
    let host = RuntimeHost::in_memory().await.expect("host");
    let backend = crate::delegation::RuntimeTaskDelegationBackend::new(Arc::downgrade(&host));
    let cancellation = CancellationToken::new();
    cancellation.cancel();

    let output = golutra_tools::TaskDelegationBackend::delegate(
        &backend,
        &ToolRequest {
            tool_call_id: ToolCallId::new(),
            provider_tool_call_id: Some("cancelled-delegation".to_owned()),
            session_id: host.default_session_id(),
            turn_id: Some(TurnId::new()),
            tool_name: "delegate_task".to_owned(),
            arguments: json!({"task": "this child must never start"}),
        },
        cancellation,
    )
    .await
    .expect("cancelled delegation returns a tool result");

    assert_eq!(output.status, golutra_core::ToolResultStatus::Cancelled);
    assert_eq!(output.structured_facts["cancelled"], true);
    assert!(
        host.storage
            .repositories
            .threads
            .list(None, 100)
            .await
            .expect("threads")
            .is_empty()
    );
}

#[tokio::test]
async fn cancelling_a_parent_execution_stops_and_archives_a_running_child() {
    let _provider = IsolatedGlobalMockProvider::install().await;
    let host = RuntimeHost::in_memory().await.expect("host");
    let parent_session_id = host.default_session_id();
    host.upsert_current_thread(
        parent_session_id,
        &json!({"prompt": "parent cancellation fixture"}),
    )
    .await
    .expect("parent thread");
    let parent_task_id = TaskId::new();
    let (parent_execution, _parent_control) = agent_execution_channel(1);
    let root_context = crate::delegation_policy::DelegationContext::root(
        parent_session_id,
        Some(10_000),
        Some(1_024),
        None,
        parent_execution.cancellation_token(),
    );
    let parent_worker = tokio::spawn(std::future::pending::<()>());
    let (completion_sender, completion) = watch::channel(false);
    host.execution.task_controls.lock().await.insert(
        parent_session_id,
        HostedTaskControl {
            task_id: parent_task_id,
            allow_network: false,
            yolo: false,
            provider_settings: ProviderTurnSettings::default(),
            execution: parent_execution.clone(),
            abort_handle: parent_worker.abort_handle(),
            completion,
            delegation: Some(root_context),
            _session_lease: None,
        },
    );

    let backend = crate::delegation::RuntimeTaskDelegationBackend::new(Arc::downgrade(&host));
    let request = ToolRequest {
        tool_call_id: ToolCallId::new(),
        provider_tool_call_id: Some("parent-abort-child".to_owned()),
        session_id: parent_session_id,
        turn_id: Some(TurnId::new()),
        tool_name: "delegate_task".to_owned(),
        arguments: json!({"task": "sleep while the parent is cancelled"}),
    };
    let delegation = tokio::spawn(async move {
        golutra_tools::TaskDelegationBackend::delegate(&backend, &request, CancellationToken::new())
            .await
    });
    let child_session_id = timeout(Duration::from_secs(20), async {
        loop {
            if let Some(session_id) = host
                .storage
                .repositories
                .threads
                .list(None, 100)
                .await
                .expect("threads")
                .into_iter()
                .find(|thread| thread.session_id != parent_session_id)
                .map(|thread| thread.session_id)
            {
                return session_id;
            }
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("child thread starts");

    delegation.abort();
    parent_execution.cancel();

    timeout(Duration::from_secs(10), async {
        loop {
            let child_control_present = host
                .execution
                .task_controls
                .lock()
                .await
                .contains_key(&child_session_id);
            let archived = host
                .storage
                .repositories
                .threads
                .by_session(child_session_id)
                .await
                .expect("child lookup")
                .is_some_and(|thread| thread.archived);
            if !child_control_present && archived {
                return;
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("parent cancellation cleans child");

    host.clear_task_control(parent_session_id, parent_task_id)
        .await;
    drop(completion_sender);
    parent_worker.abort();
}

#[tokio::test]
async fn delegated_payload_rejects_a_forged_runtime_actor() {
    let host = RuntimeHost::in_memory().await.expect("host");
    let parent_session_id = host.default_session_id();
    let parent_task_id = TaskId::new();
    let parent_thread_id = host.default_thread_id();
    let parent_tool_call_id = ToolCallId::new();
    let child_session_id = SessionId::new();
    let child_thread_id = ThreadId::new();
    let cancellation = CancellationToken::new();
    host.upsert_current_thread(
        parent_session_id,
        &json!({"_thread_id": parent_thread_id, "prompt": "parent"}),
    )
    .await
    .expect("parent thread");
    let root = crate::delegation_policy::DelegationContext::root(
        parent_session_id,
        Some(10_000),
        Some(1_024),
        None,
        cancellation.clone(),
    );
    let child = root
        .child(
            parent_session_id,
            parent_task_id,
            parent_thread_id,
            1_024,
            Some(0),
            &cancellation,
        )
        .expect("child context");
    let expected_actor_id = format!("delegate:parent:{parent_session_id}");
    let admission = crate::delegation::DelegationAdmission::new(
        child,
        parent_session_id,
        parent_tool_call_id,
        child_thread_id,
        expected_actor_id,
        "authorized child task",
    );
    let token = admission.token().to_owned();
    host.execution
        .delegation_admissions
        .lock()
        .await
        .insert(child_session_id, admission);

    let ack = host
        .clone()
        .handle_command(SessionCommand {
            command_id: CommandId::new(),
            session_id: Some(child_session_id),
            kind: SessionCommandKind::Prompt,
            idempotency_key: CommandId::new().to_string(),
            actor: Actor {
                kind: ActorKind::Runtime,
                id: "delegate:parent:forged".to_owned(),
            },
            payload: json!({
                "prompt": "authorized child task",
                "_delegated_task": true,
                "_delegation_admission_token": token,
                "_delegation_parent_session_id": parent_session_id,
                "_delegation_parent_tool_call_id": parent_tool_call_id,
            }),
            timestamp: chrono::Utc::now(),
        })
        .await
        .expect("forged prompt is handled");

    assert!(!ack.accepted);
    assert!(
        ack.reason
            .as_deref()
            .is_some_and(|reason| reason.contains("host-created child task"))
    );
    assert!(
        host.execution
            .delegation_admissions
            .lock()
            .await
            .contains_key(&child_session_id)
    );
    host.execution
        .delegation_admissions
        .lock()
        .await
        .remove(&child_session_id);
}

#[tokio::test]
async fn delegated_child_session_rejects_plain_prompt_between_create_and_start() {
    let host = RuntimeHost::in_memory().await.expect("host");
    let parent_session_id = host.default_session_id();
    let parent_task_id = TaskId::new();
    let parent_thread_id = host.default_thread_id();
    let parent_tool_call_id = ToolCallId::new();
    let child_session_id = SessionId::new();
    let child_thread_id = ThreadId::new();
    let cancellation = CancellationToken::new();
    host.upsert_current_thread(
        parent_session_id,
        &json!({"_thread_id": parent_thread_id, "prompt": "parent"}),
    )
    .await
    .expect("parent thread");
    let root = crate::delegation_policy::DelegationContext::root(
        parent_session_id,
        Some(10_000),
        Some(1_024),
        None,
        cancellation.clone(),
    );
    let child = root
        .child(
            parent_session_id,
            parent_task_id,
            parent_thread_id,
            1_024,
            Some(0),
            &cancellation,
        )
        .expect("child context");
    let actor = Actor {
        kind: ActorKind::Runtime,
        id: format!("delegate:parent:{parent_session_id}"),
    };
    let admission = crate::delegation::DelegationAdmission::new(
        child.clone(),
        parent_session_id,
        parent_tool_call_id,
        child_thread_id,
        actor.id.clone(),
        "authorized child task",
    );
    let token = admission.token().to_owned();
    host.execution
        .delegation_admissions
        .lock()
        .await
        .insert(child_session_id, admission);

    let create = host
        .clone()
        .handle_command(SessionCommand {
            command_id: CommandId::new(),
            session_id: Some(child_session_id),
            kind: SessionCommandKind::Create,
            idempotency_key: CommandId::new().to_string(),
            actor: actor.clone(),
            payload: json!({
                "_thread_id": child_thread_id,
                "_parent_thread_id": parent_thread_id,
                "title": "Delegated task",
                "prompt": "authorized child task",
                "_delegated_task": true,
                "_delegation_admission_token": token,
                "_delegation_parent_session_id": parent_session_id,
                "_delegation_parent_tool_call_id": parent_tool_call_id,
                "_delegation": child.metadata(),
            }),
            timestamp: chrono::Utc::now(),
        })
        .await
        .expect("delegated child create is handled");
    assert!(create.accepted);

    let plain_prompt = host
        .clone()
        .handle_command(SessionCommand {
            command_id: CommandId::new(),
            session_id: Some(child_session_id),
            kind: SessionCommandKind::Prompt,
            idempotency_key: CommandId::new().to_string(),
            actor: Actor {
                kind: ActorKind::User,
                id: "competing-client".to_owned(),
            },
            payload: json!({"prompt": "take over the delegated session"}),
            timestamp: chrono::Utc::now(),
        })
        .await
        .expect("plain prompt is handled");

    assert!(!plain_prompt.accepted);
    assert!(
        plain_prompt
            .reason
            .as_deref()
            .is_some_and(|reason| reason.contains("host admission"))
    );
    assert!(
        host.execution
            .delegation_admissions
            .lock()
            .await
            .contains_key(&child_session_id)
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
        .build_tool_executor(
            WorkspacePolicy::new(&root).expect("policy"),
            root.clone(),
            false,
            false,
        )
        .await
        .expect("executor");
    let start_request = ToolRequest {
        tool_call_id: ToolCallId::new(),
        provider_tool_call_id: None,
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
                provider_tool_call_id: None,
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
        .storage
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
async fn active_work_includes_a_nonterminal_post_task_job() {
    let host = RuntimeHost::in_memory().await.expect("host");
    let transport = EmbeddedTransport::new(host.clone());
    let job = PostTaskJob {
        job_id: PostTaskJobId::new(),
        kind: PostTaskJobKind::DeepEvaluation,
        workspace_id: host.workspace_id().to_string(),
        session_id: host.default_session_id().to_string(),
        task_id: TaskId::new(),
        input_refs: Vec::new(),
        status: PostTaskJobStatus::Running,
        attempt: 1,
        max_attempts: 2,
        lease_owner: Some("test-worker".to_owned()),
        lease_expires_at: None,
        result_refs: Vec::new(),
        last_error: None,
        created_at: chrono::Utc::now(),
        started_at: Some(chrono::Utc::now()),
        completed_at: None,
    };
    host.storage
        .store
        .enqueue_post_task_job(&job)
        .await
        .expect("enqueue post-task job");

    assert!(transport.has_active_work().await);

    host.storage
        .repositories
        .jobs
        .finish(
            job.job_id,
            "test-worker",
            PostTaskJobStatus::Succeeded,
            &[],
            None,
            chrono::Utc::now(),
        )
        .await
        .expect("finish post-task job");
    assert!(!transport.has_active_work().await);
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
        .storage
        .store
        .post_task_job(task_id)
        .await
        .expect("job state")
        .expect("job");
    assert_eq!(recovered.status, PostTaskJobStatus::Succeeded);
    assert!(
        host.storage
            .store
            .load_events(session_id, Some(task_id), None)
            .await
            .expect("evaluation events")
            .iter()
            .any(|event| event.event_type == RuntimeEventType::PostTaskJobCompleted)
    );
}

#[tokio::test]
async fn startup_recreates_a_post_task_job_missing_after_terminal_commit() {
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
    let now = chrono::Utc::now();
    store
        .upsert_thread(&ThreadRecord {
            thread_id: ThreadId::new(),
            session_id,
            parent_thread_id: None,
            forked_from_turn_id: None,
            forked_from_sequence_no: None,
            workspace_root: Some(paths.cwd.display().to_string()),
            rebound_from_workspace_root: None,
            rollout_path: None,
            title: "terminal recovery".to_owned(),
            preview: "terminal recovery".to_owned(),
            created_at: now,
            updated_at: now,
            recency_at: now,
            archived: false,
            removed: false,
        })
        .await
        .expect("thread");
    let mut created = host_event(
        0,
        session_id,
        Some(task_id),
        RuntimeEventType::TaskCreated,
        RuntimeEventSource::Runtime,
        json!({"prompt": "recover missing governance job"}),
    );
    created.turn_id = Some(turn_id);
    store
        .append_event_assigning_sequence(created)
        .await
        .expect("task event");
    let mut terminal = host_event(
        0,
        session_id,
        Some(task_id),
        RuntimeEventType::TaskCompleted,
        RuntimeEventSource::Runtime,
        json!({
            "status": "failed",
            "post_task_governance": {"status": "pending"}
        }),
    );
    terminal.turn_id = Some(turn_id);
    store
        .append_event_assigning_sequence(terminal)
        .await
        .expect("terminal event");
    assert!(
        store
            .post_task_job(task_id)
            .await
            .expect("job query")
            .is_none()
    );
    drop(store);

    let host = RuntimeHost::from_home_and_cwd(home.path(), workspace.path())
        .await
        .expect("restarted host");
    host.wait_for_deep_task_evaluation(task_id).await;
    let jobs = host
        .storage
        .repositories
        .jobs
        .list_for_task(task_id)
        .await
        .expect("jobs");
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].status, PostTaskJobStatus::Succeeded);
    let events = host
        .storage
        .repositories
        .events
        .load(session_id, Some(task_id), None)
        .await
        .expect("events");
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == RuntimeEventType::PostTaskJobQueued)
            .count(),
        1
    );
}

#[tokio::test]
async fn cross_process_settlement_waits_for_durable_scheduling_outcome() {
    let host = RuntimeHost::in_memory().await.expect("host");
    let session_id = host.default_session_id();
    let task_id = TaskId::new();
    host.record_event(host_event(
        host.next_sequence_no(),
        session_id,
        Some(task_id),
        RuntimeEventType::TaskCompleted,
        RuntimeEventSource::Runtime,
        json!({
            "status": "completed",
            "post_task_governance": {"status": "pending"}
        }),
    ))
    .await
    .expect("terminal event");

    let delayed_host = host.clone();
    tokio::spawn(async move {
        sleep(Duration::from_millis(350)).await;
        delayed_host
            .record_event(host_event(
                delayed_host.next_sequence_no(),
                session_id,
                Some(task_id),
                RuntimeEventType::PostTaskStageFailed,
                RuntimeEventSource::Evaluator,
                json!({
                    "phase": "evaluation_scheduling",
                    "terminal": true,
                    "execution_outcome_unchanged": true
                }),
            ))
            .await
            .expect("scheduling failure event");
    });

    let started = std::time::Instant::now();
    host.wait_for_deep_task_evaluation(task_id).await;
    assert!(started.elapsed() >= Duration::from_millis(250));
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
async fn failure_payload_uses_the_started_queued_turn() {
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

    let payload = host
        .payload_for_task_turn(&task, queued_turn_id)
        .await
        .expect("payload");

    assert_eq!(prompt_from_payload(&payload), "second turn");
}

#[tokio::test]
async fn queued_turn_options_drive_failure_outcome() {
    let host = RuntimeHost::in_memory().await.expect("host");
    let session_id = host.default_session_id();
    let task = HostedAgentTask {
        session_id,
        task_id: TaskId::new(),
        turn_id: TurnId::new(),
        payload: json!({
            "prompt": "first turn",
            "defer_external_verification": false,
        }),
    };
    let queued_turn_id = TurnId::new();
    let started = host
        .execution
        .lane_manager
        .lock()
        .await
        .start_task(
            host.workspace_id,
            session_id,
            task.task_id,
            task.turn_id,
            Actor {
                kind: ActorKind::Cli,
                id: "queued-failure-test".to_owned(),
            },
            host.next_sequence_no(),
        )
        .expect("task starts");
    host.record_event(started.event).await.expect("task event");
    let queued = host
        .execution
        .lane_manager
        .lock()
        .await
        .queue_turn(session_id, queued_turn_id, host.next_sequence_no())
        .expect("turn queues");
    host.record_event(with_command_payload(
        queued.event,
        CommandId::new(),
        json!({
            "prompt": "queued turn",
            "max_elapsed_ms": 345_000,
            "defer_external_verification": true,
        }),
    ))
    .await
    .expect("queued event");
    host.execution
        .lane_manager
        .lock()
        .await
        .start_queued_turn(session_id, queued_turn_id)
        .expect("queued turn starts");

    host.record_task_execution_failure(&task, ClientError::TaskCancelled)
        .await
        .expect("failure outcome");
    let events = host
        .storage
        .repositories
        .events
        .load(session_id, Some(task.task_id), None)
        .await
        .expect("events");
    let terminal = events
        .iter()
        .rev()
        .find(|event| event.event_type.is_task_terminal())
        .expect("terminal event");

    assert_eq!(terminal.turn_id, Some(queued_turn_id));
    assert_eq!(
        terminal.payload.pointer("/outcome/external_verification"),
        Some(&json!("pending"))
    );
    assert!(events.iter().any(|event| {
        event.event_type == RuntimeEventType::ExternalVerificationRequested
            && event.turn_id == Some(queued_turn_id)
    }));
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
    assert!(
        forensic_trace.integrity.complete,
        "{:?}",
        forensic_trace.integrity
    );
    assert!(forensic_trace.integrity.retention_losses.is_empty());
    assert!(forensic_trace.artifacts.iter().any(|artifact| {
        artifact.artifact_type == "provider_request_replay"
            && artifact.redaction_status == RedactionStatus::Raw
    }));
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
        .send_command(command_with_payload(
            session_id,
            json!({
                "prompt": "what happened next",
                "defer_external_verification": true,
            }),
        ))
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
    let terminal = events
        .iter()
        .rev()
        .find(|event| event.event_type.is_task_terminal())
        .expect("terminal event");
    assert_eq!(
        terminal.payload.pointer("/outcome/external_verification"),
        Some(&json!("pending"))
    );
    let task_id = terminal.task_id.expect("task id");
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
    transport.host.wait_for_deep_task_evaluation(task_id).await;
    let settled_events = transport
        .host
        .storage
        .repositories
        .events
        .load(session_id, Some(task_id), None)
        .await
        .expect("settled events");
    assert!(settled_events.iter().any(|event| {
        matches!(
            event.event_type,
            RuntimeEventType::PostTaskReviewed | RuntimeEventType::EvaluationCompleted
        ) && event.turn_id == Some(queued_turn_id)
    }));
    assert!(
        settled_events
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
async fn queued_prompts_can_be_updated_and_cancelled_durably() {
    let transport = EmbeddedTransport::in_memory().await.expect("transport");
    let session_id = SessionId::new();
    transport
        .send_command(command(session_id, "sleep"))
        .await
        .expect("blocking prompt");
    wait_for_status(&transport, session_id, TaskStatus::WaitingApproval).await;

    for prompt in ["original queued prompt", "cancelled queued prompt"] {
        let ack = transport
            .send_command(command_with_payload(
                session_id,
                json!({
                    "prompt": prompt,
                    "defer_external_verification": true,
                }),
            ))
            .await
            .expect("queued prompt");
        assert!(ack.accepted, "{ack:?}");
    }
    let queued_events = transport
        .host
        .storage
        .repositories
        .events
        .load(session_id, None, None)
        .await
        .expect("events")
        .into_iter()
        .filter(|event| event.event_type == RuntimeEventType::TurnQueued)
        .collect::<Vec<_>>();
    let active_task_id = queued_events[0].task_id.expect("active task");
    let updated_turn_id = queued_events
        .iter()
        .find(|event| {
            event
                .payload
                .pointer("/payload/prompt")
                .and_then(Value::as_str)
                == Some("original queued prompt")
        })
        .and_then(|event| event.turn_id)
        .expect("updated turn id");
    let cancelled_turn_id = queued_events
        .iter()
        .find(|event| {
            event
                .payload
                .pointer("/payload/prompt")
                .and_then(Value::as_str)
                == Some("cancelled queued prompt")
        })
        .and_then(|event| event.turn_id)
        .expect("cancelled turn id");

    let updated = transport
        .send_command(runtime_command(
            session_id,
            SessionCommandKind::UpdateQueuedTurn,
            json!({
                "turn_id": updated_turn_id,
                "prompt": "edited queued prompt",
            }),
        ))
        .await
        .expect("update queued prompt");
    assert!(updated.accepted, "{updated:?}");
    let cancelled = transport
        .send_command(runtime_command(
            session_id,
            SessionCommandKind::CancelQueuedTurn,
            json!({"turn_id": cancelled_turn_id}),
        ))
        .await
        .expect("cancel queued prompt");
    assert!(cancelled.accepted, "{cancelled:?}");

    let recovered = transport
        .host
        .recoverable_pending_turns(session_id, Some(active_task_id))
        .await
        .expect("recoverable turns");
    assert_eq!(recovered.len(), 1);
    assert_eq!(recovered[0].pending.turn_id, updated_turn_id);
    assert_eq!(recovered[0].pending.content, "edited queued prompt");

    let events = transport
        .host
        .storage
        .repositories
        .events
        .load(session_id, Some(active_task_id), None)
        .await
        .expect("events");
    assert!(events.iter().any(|event| {
        event.event_type == RuntimeEventType::TurnUpdated
            && event.turn_id == Some(updated_turn_id)
            && event
                .payload
                .pointer("/payload/prompt")
                .and_then(Value::as_str)
                == Some("edited queued prompt")
    }));
    assert!(events.iter().any(|event| {
        event.event_type == RuntimeEventType::TurnCancelled
            && event.turn_id == Some(cancelled_turn_id)
    }));

    transport
        .send_command(runtime_command(
            session_id,
            SessionCommandKind::Abort,
            json!({}),
        ))
        .await
        .expect("abort active task");
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
        .storage
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
        .storage
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
async fn processing_thread_delete_retry_reuses_the_semantic_event_and_rebuilds_rollout() {
    let workspace = tempdir().expect("workspace");
    let _home = IsolatedGlobalMockProvider::empty().await;
    let host = RuntimeHost::for_cwd(workspace.path()).await.expect("host");
    let transport = EmbeddedTransport::new(host.clone());
    let attached_session_id = transport.default_session_id();
    let target_session_id = SessionId::new();
    let target_thread_id = ThreadId::new();
    let now = chrono::Utc::now();
    let rollout_path = host
        .runtime_paths
        .as_ref()
        .expect("runtime paths")
        .rollout_path(target_thread_id);
    host.storage
        .repositories
        .threads
        .upsert(&ThreadRecord {
            thread_id: target_thread_id,
            session_id: target_session_id,
            parent_thread_id: None,
            forked_from_turn_id: None,
            forked_from_sequence_no: None,
            workspace_root: host.workspace_root_string(),
            rebound_from_workspace_root: None,
            rollout_path: Some(rollout_path.display().to_string()),
            title: "retry target".to_owned(),
            preview: "retry target".to_owned(),
            created_at: now,
            updated_at: now,
            recency_at: now,
            archived: false,
            removed: false,
        })
        .await
        .expect("target thread");
    let command = runtime_command(
        attached_session_id,
        SessionCommandKind::DeleteThread,
        json!({"thread_id": target_thread_id}),
    );
    let scoped_key = host.scoped_idempotency_key(&command.idempotency_key);
    host.storage
        .store
        .claim_command(
            &scoped_key,
            command.command_id,
            &CommandAck {
                command_id: command.command_id,
                accepted: true,
                reason: Some(PROVISIONAL_COMMAND_ACK_REASON.to_owned()),
            },
            host_event(
                0,
                attached_session_id,
                None,
                RuntimeEventType::CommandReceived,
                RuntimeEventSource::Runtime,
                json!({"command_id": command.command_id}),
            ),
        )
        .await
        .expect("processing command claim");
    host.storage
        .store
        .delete_thread_with_event(
            target_thread_id,
            host_event(
                0,
                target_session_id,
                None,
                RuntimeEventType::ThreadDeleted,
                RuntimeEventSource::User,
                json!({
                    "summary": "thread removed from history",
                    "thread_id": target_thread_id,
                    "actor": &command.actor,
                    "command_id": command.command_id.to_string(),
                }),
            ),
        )
        .await
        .expect("delete transaction")
        .expect("delete event");

    let ack = transport
        .send_command(command.clone())
        .await
        .expect("processing retry");

    assert!(ack.accepted);
    assert_eq!(ack.reason.as_deref(), Some("thread removed from history"));
    let events = host
        .storage
        .store
        .load_events(target_session_id, None, None)
        .await
        .expect("target events");
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == RuntimeEventType::ThreadDeleted)
            .count(),
        1
    );
    assert!(
        host.storage
            .store
            .thread_by_id(target_thread_id)
            .await
            .expect("thread lookup")
            .expect("tombstone")
            .removed
    );
    let rollout = fs::read_to_string(&rollout_path).expect("rebuilt rollout");
    assert_eq!(rollout.matches("thread_deleted").count(), 1);
    assert_eq!(
        host.storage
            .store
            .command_ack(&scoped_key)
            .await
            .expect("command journal")
            .expect("completed ack"),
        ack
    );
}

#[tokio::test]
async fn privacy_purge_requires_confirmation_and_removes_rollout_idempotently() {
    let workspace = tempdir().expect("workspace");
    let _home = IsolatedGlobalMockProvider::empty().await;
    let host = RuntimeHost::for_cwd(workspace.path()).await.expect("host");
    let transport = EmbeddedTransport::new(host.clone());
    let attached_session_id = transport.default_session_id();
    let target_session_id = SessionId::new();
    let target_thread_id = ThreadId::new();
    let now = chrono::Utc::now();
    let rollout_path = host
        .runtime_paths
        .as_ref()
        .expect("runtime paths")
        .rollout_path(target_thread_id);
    host.storage
        .repositories
        .threads
        .upsert(&ThreadRecord {
            thread_id: target_thread_id,
            session_id: target_session_id,
            parent_thread_id: None,
            forked_from_turn_id: None,
            forked_from_sequence_no: None,
            workspace_root: host.workspace_root_string(),
            rebound_from_workspace_root: None,
            rollout_path: Some(rollout_path.display().to_string()),
            title: "private target".to_owned(),
            preview: "private target".to_owned(),
            created_at: now,
            updated_at: now,
            recency_at: now,
            archived: false,
            removed: false,
        })
        .await
        .expect("target thread");
    fs::write(&rollout_path, "private projection\n").expect("rollout fixture");

    let rejected = transport
        .send_command(runtime_command(
            attached_session_id,
            SessionCommandKind::DeleteThread,
            json!({"thread_id": target_thread_id, "purge": true}),
        ))
        .await
        .expect("rejected purge");
    assert!(!rejected.accepted);
    assert_eq!(
        rejected.reason.as_deref(),
        Some("privacy purge requires confirm=PURGE")
    );
    assert!(rollout_path.exists());

    let purge = runtime_command(
        attached_session_id,
        SessionCommandKind::DeleteThread,
        json!({
            "thread_id": target_thread_id,
            "purge": true,
            "confirm": "PURGE"
        }),
    );
    let purged = transport.send_command(purge.clone()).await.expect("purge");
    assert!(purged.accepted);
    assert_eq!(
        purged.reason.as_deref(),
        Some("thread purged; audit tombstone retained")
    );
    assert!(!rollout_path.exists());
    assert!(
        host.storage
            .store
            .thread_by_id(target_thread_id)
            .await
            .expect("thread lookup")
            .expect("tombstone")
            .removed
    );

    let retried = transport.send_command(purge).await.expect("purge retry");
    assert_eq!(retried, purged);
    assert!(!rollout_path.exists());
    let events = host
        .storage
        .store
        .load_events(target_session_id, None, None)
        .await
        .expect("target events");
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == RuntimeEventType::ThreadDeleted)
            .count(),
        1
    );
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
    for event in reviewed_events
        .iter()
        .filter(|event| event.event_type == RuntimeEventType::MemoryRetrieved)
    {
        assert!(event.payload.get("query").is_none());
        assert!(
            event
                .payload
                .get("retrieved")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .all(|record| {
                    record.get("content").is_none()
                        && record.get("matched_terms").is_none()
                        && record.get("reason").is_none()
                        && record.get("matched_term_count").is_some()
                }),
            "durable memory retrieval events must not contain memory text"
        );
    }

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
async fn deferred_external_verification_survives_provider_failure() {
    let _provider = IsolatedGlobalMockProvider::install().await;
    let transport = EmbeddedTransport::in_memory().await.expect("transport");
    let session_id = transport.default_session_id();
    let mut prompt = command(session_id, "reproduce provider failure");
    prompt.payload["mock_provider_failure"] = json!(true);
    prompt.payload["defer_external_verification"] = json!(true);

    transport.send_command(prompt).await.expect("failed task");
    wait_for_status(&transport, session_id, TaskStatus::Failed).await;
    let events = transport
        .host
        .storage
        .repositories
        .events
        .load(session_id, None, None)
        .await
        .expect("events");
    let terminal = events
        .iter()
        .rev()
        .find(|event| event.event_type.is_task_terminal())
        .expect("terminal event");

    assert_eq!(
        terminal.payload.pointer("/outcome/external_verification"),
        Some(&json!("pending"))
    );
    assert!(events.iter().any(|event| {
        event.event_type == RuntimeEventType::ExternalVerificationRequested
            && event.payload.pointer("/outcome/external_verification") == Some(&json!("pending"))
    }));
}

#[tokio::test]
async fn deferred_external_verification_survives_abort() {
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
                "prompt": "sleep",
                "defer_external_verification": true,
            }),
        ))
        .await
        .expect("running task");
    wait_for_status(&transport, session_id, TaskStatus::WaitingApproval).await;
    let abort = transport
        .send_command(runtime_command(
            session_id,
            SessionCommandKind::Abort,
            json!({}),
        ))
        .await
        .expect("abort");
    assert!(abort.accepted);
    wait_for_status(&transport, session_id, TaskStatus::Cancelled).await;
    let events = transport
        .host
        .storage
        .repositories
        .events
        .load(session_id, None, None)
        .await
        .expect("events");
    let terminal = events
        .iter()
        .rev()
        .find(|event| event.event_type.is_task_terminal())
        .expect("terminal event");

    assert_eq!(
        terminal.payload.pointer("/outcome/external_verification"),
        Some(&json!("pending"))
    );
    assert!(events.iter().any(|event| {
        event.event_type == RuntimeEventType::ExternalVerificationRequested
            && event.payload.pointer("/outcome/external_verification") == Some(&json!("pending"))
    }));
}

#[tokio::test]
async fn post_task_wait_started_before_terminal_observes_the_durable_job() {
    let _provider = IsolatedGlobalMockProvider::install().await;
    let application = RuntimeApplication::in_memory().await.expect("application");
    let session_id = application.session_service().default_session_id();
    let transport = EmbeddedTransport::from_application(application.clone());

    application
        .send_command(command(session_id, "sleep"))
        .await
        .expect("running task");
    let state = wait_for_status(&transport, session_id, TaskStatus::WaitingApproval).await;
    let task_id = state
        .get("active_task_id")
        .and_then(Value::as_str)
        .and_then(|value| value.parse::<TaskId>().ok())
        .expect("running task id");

    let waiter_application = application.clone();
    let waiter = tokio::spawn(async move {
        waiter_application
            .post_task_service()
            .wait_for_terminal(task_id)
            .await
    });
    tokio::task::yield_now().await;

    application
        .send_command(runtime_command(
            session_id,
            SessionCommandKind::Abort,
            json!({}),
        ))
        .await
        .expect("abort");
    wait_for_status(&transport, session_id, TaskStatus::Cancelled).await;

    let job = tokio::time::timeout(Duration::from_secs(15), waiter)
        .await
        .expect("post-task wait deadline")
        .expect("post-task waiter")
        .expect("post-task wait")
        .expect("post-task job");
    assert!(matches!(
        job.status,
        PostTaskJobStatus::Succeeded | PostTaskJobStatus::Failed | PostTaskJobStatus::Cancelled
    ));
}

#[tokio::test]
async fn failed_task_blocks_unfrozen_regression_without_polluting_memory() {
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
    let runtime_candidate_id = format!("candidate-{task_id}");
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
    let evaluation_store = application.host().storage.evaluation_store.clone();
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
        trajectory_summary: TrajectorySummary::default(),
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
    let candidate_artifact_ref = campaign
        .candidate_artifact_ref
        .expect("campaign frozen candidate patch");
    let frozen_patch = evaluation_state
        .frozen_candidate_patches
        .iter()
        .find(|patch| patch.candidate_id == candidate_id)
        .expect("frozen patch record");
    assert_eq!(frozen_patch.artifact_ref, candidate_artifact_ref);
    assert_eq!(frozen_patch.file_count, 1);
    let candidate_artifact = application
        .host()
        .storage
        .repositories
        .artifacts
        .get(candidate_artifact_ref)
        .await
        .expect("candidate artifact metadata")
        .expect("candidate artifact");
    assert_eq!(candidate_artifact.artifact_type, "candidate_patch_set");
    let candidate_bytes = application
        .host()
        .storage
        .repositories
        .artifacts
        .bytes(candidate_artifact_ref)
        .await
        .expect("candidate artifact read")
        .expect("candidate artifact bytes");
    let candidate_bundle: Value =
        serde_json::from_slice(&candidate_bytes).expect("candidate patch JSON");
    assert_eq!(
        candidate_bundle["files"]["regression-marker.txt"],
        "candidate workspace"
    );
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
            .storage
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
    assert!(trace.evaluation.regressions.iter().any(|regression| {
        regression.candidate_id == runtime_candidate_id
            && regression.verdict == golutra_eval::RegressionVerdict::NeedsReview
    }));
    assert!(
        !trace
            .evaluation
            .promotion_decisions
            .iter()
            .any(|decision| { decision.candidate_id == runtime_candidate_id })
    );
    assert!(
        !evaluation_store
            .snapshot()
            .expect("automatic lifecycle state")
            .applied_candidates
            .iter()
            .any(|candidate| candidate.candidate_id == runtime_candidate_id)
    );
    assert!(trace.events.iter().any(|event| {
        event.event_type == RuntimeEventType::RegressionBlocked
            && event.payload["automatic"] == true
            && event.payload["record"]["candidate_id"] == runtime_candidate_id
    }));
    assert!(!trace.events.iter().any(|event| {
        event.event_type == RuntimeEventType::PromotionDecided
            && event.payload["record"]["candidate_id"] == runtime_candidate_id
    }));
    assert!(!trace.events.iter().any(|event| {
        event.event_type == RuntimeEventType::CandidateApplied
            && event.payload["record"]["candidate_id"] == runtime_candidate_id
    }));
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
    assert!(
        governed
            .frozen_candidate_patches
            .iter()
            .any(|patch| patch.candidate_id == candidate_id)
    );
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
    assert!(governed_trace.events.iter().any(|event| {
        event.event_type == RuntimeEventType::CandidatePatchFrozen
            && event.payload["record"]["candidate_id"] == candidate_id
    }));
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
        trajectory_summary: TrajectorySummary::default(),
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
        .storage
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
async fn post_task_governance_failure_does_not_rewrite_verified_terminal_status() {
    let transport = EmbeddedTransport::in_memory().await.expect("transport");
    let session_id = transport.default_session_id();
    transport
        .send_command(command(session_id, "list workspace"))
        .await
        .expect("task accepted");
    let state = wait_for_status(&transport, session_id, TaskStatus::Completed).await;
    let task_id = state
        .get("active_task_id")
        .and_then(Value::as_str)
        .and_then(|value| value.parse::<TaskId>().ok())
        .expect("task id");
    let turn_id = transport
        .host
        .storage
        .repositories
        .events
        .load(session_id, Some(task_id), None)
        .await
        .expect("events")
        .iter()
        .rev()
        .find_map(|event| event.turn_id)
        .expect("turn id");
    let task = HostedAgentTask {
        session_id,
        task_id,
        turn_id,
        payload: json!({"prompt": "list workspace"}),
    };
    transport
        .host
        .record_post_task_governance_failure(
            &task,
            "projection",
            false,
            &ClientError::TaskExecution("forced projection failure".to_owned()),
        )
        .await;
    let events = transport
        .host
        .storage
        .repositories
        .events
        .load(session_id, Some(task_id), None)
        .await
        .expect("events");

    assert!(events.iter().any(|event| {
        event.event_type == RuntimeEventType::TaskCompleted
            && event.payload.get("status").and_then(Value::as_str) == Some("completed")
    }));
    assert!(events.iter().any(|event| {
        event.event_type == RuntimeEventType::PostTaskStageFailed
            && event.payload.get("execution_outcome_unchanged") == Some(&json!(true))
            && event.payload.get("terminal") == Some(&json!(false))
    }));
    assert_eq!(
        transport
            .host
            .storage
            .repositories
            .projections
            .state(session_id, None)
            .await
            .expect("projection")
            .task_status,
        TaskStatus::Completed
    );
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
                    plan_id: None,
                    assertions: Vec::new(),
                    source: Default::default(),
                    independence: Default::default(),
                    environment_digest: None,
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
            removed: false,
        };
        transport
            .host
            .storage
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
        removed: false,
    };
    transport_a
        .host
        .storage
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
        .storage
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
            removed: false,
        };
        transport
            .host
            .storage
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
async fn thread_metadata_commands_rename_archive_and_delete_an_idle_thread() {
    let home = tempdir().expect("home");
    let cwd = tempdir().expect("cwd");
    let transport = EmbeddedTransport::from_home_and_cwd(home.path(), cwd.path())
        .await
        .expect("transport");
    let attached_session_id = transport.default_session_id();
    let at = chrono::Utc::now();
    let target = ThreadRecord {
        thread_id: ThreadId::new(),
        session_id: SessionId::new(),
        parent_thread_id: None,
        forked_from_turn_id: None,
        forked_from_sequence_no: None,
        workspace_root: Some(
            cwd.path()
                .canonicalize()
                .expect("canonical cwd")
                .display()
                .to_string(),
        ),
        rebound_from_workspace_root: None,
        rollout_path: None,
        title: "before".to_owned(),
        preview: "idle target".to_owned(),
        created_at: at,
        updated_at: at,
        recency_at: at,
        archived: false,
        removed: false,
    };
    transport
        .host
        .storage
        .repositories
        .threads
        .upsert(&target)
        .await
        .expect("target thread");

    for (kind, payload, reason) in [
        (
            SessionCommandKind::RenameThread,
            json!({"thread_id": target.thread_id, "title": "after"}),
            "thread renamed",
        ),
        (
            SessionCommandKind::ArchiveThread,
            json!({"thread_id": target.thread_id}),
            "thread archived",
        ),
    ] {
        let ack = transport
            .send_command(runtime_command(attached_session_id, kind, payload))
            .await
            .expect("thread metadata command");
        assert!(ack.accepted);
        assert_eq!(ack.reason.as_deref(), Some(reason));
    }
    let archived = transport
        .host
        .storage
        .repositories
        .threads
        .by_id(target.thread_id)
        .await
        .expect("archived lookup")
        .expect("archived thread");
    assert_eq!(archived.title, "after");
    assert!(archived.archived);

    let deleted = transport
        .send_command(runtime_command(
            attached_session_id,
            SessionCommandKind::DeleteThread,
            json!({"thread_id": target.thread_id}),
        ))
        .await
        .expect("delete thread");
    assert!(deleted.accepted);
    assert_eq!(
        deleted.reason.as_deref(),
        Some("thread removed from history")
    );
    let removed = transport
        .host
        .storage
        .repositories
        .threads
        .by_id(target.thread_id)
        .await
        .expect("deleted lookup")
        .expect("retained tombstone");
    assert!(removed.removed);
    assert!(removed.archived);
    let rollout = fs::read_to_string(removed.rollout_path.as_deref().expect("rollout path"))
        .expect("retained rollout");
    assert!(rollout.contains("thread_deleted"));
    assert!(
        transport
            .list_threads(10)
            .await
            .expect("visible threads")
            .is_empty()
    );

    let events = transport
        .host
        .storage
        .repositories
        .events
        .load(target.session_id, None, None)
        .await
        .expect("metadata events");
    assert!(events.iter().any(|event| {
        event.event_type == RuntimeEventType::ThreadRenamed
            && event.payload["thread_id"] == json!(target.thread_id)
    }));
    assert!(
        events
            .iter()
            .any(|event| event.event_type == RuntimeEventType::ThreadArchived)
    );
    assert!(
        events
            .iter()
            .any(|event| event.event_type == RuntimeEventType::ThreadDeleted)
    );
    assert!(
        events
            .iter()
            .filter(|event| matches!(
                event.event_type,
                RuntimeEventType::ThreadRenamed
                    | RuntimeEventType::ThreadArchived
                    | RuntimeEventType::ThreadDeleted
            ))
            .all(|event| event
                .payload
                .get("command_id")
                .and_then(Value::as_str)
                .is_some())
    );

    for result in [
        transport.resume_thread(target.thread_id).await.map(drop),
        transport
            .fork_thread(target.thread_id, None)
            .await
            .map(drop),
        transport
            .rebind_thread(target.thread_id, cwd.path())
            .await
            .map(drop),
    ] {
        assert!(
            matches!(result, Err(ClientError::InvalidSession(reason)) if reason.contains("was removed"))
        );
    }
    let prompt = transport
        .send_command(command_with_payload(
            target.session_id,
            json!({
                "prompt": "must not revive a removed session",
                "_thread_id": target.thread_id,
            }),
        ))
        .await;
    assert!(
        matches!(prompt, Err(ClientError::InvalidSession(reason)) if reason.contains("was removed"))
    );

    let foreign_workspace = tempdir().expect("foreign workspace");
    let foreign = EmbeddedTransport::from_home_and_cwd(home.path(), foreign_workspace.path())
        .await
        .expect("foreign transport");
    assert!(matches!(
        foreign.resume_thread(target.thread_id).await,
        Err(ClientError::InvalidSession(reason)) if reason.contains("does not belong to workspace")
    ));
}

#[tokio::test]
async fn another_runtime_session_lease_blocks_thread_metadata_mutation() {
    let home = tempdir().expect("home");
    let cwd = tempdir().expect("cwd");
    let _provider = IsolatedGlobalMockProvider::install().await;
    let owner = EmbeddedTransport::from_home_and_cwd(home.path(), cwd.path())
        .await
        .expect("owner transport");
    let session_id = owner.default_session_id();
    let thread_id = owner.default_thread_id();
    owner
        .send_command(command(session_id, "sleep"))
        .await
        .expect("blocking task");
    wait_for_status(&owner, session_id, TaskStatus::WaitingApproval).await;

    let contender = EmbeddedTransport::from_home_and_cwd(home.path(), cwd.path())
        .await
        .expect("contender transport");
    let ack = contender
        .send_command(runtime_command(
            contender.default_session_id(),
            SessionCommandKind::RenameThread,
            json!({"thread_id": thread_id, "title": "must not change"}),
        ))
        .await
        .expect("governed metadata command");

    assert!(!ack.accepted);
    assert!(
        ack.reason
            .as_deref()
            .is_some_and(|reason| reason.contains("another Golutra runtime process"))
    );
    assert_ne!(
        owner
            .host
            .storage
            .repositories
            .threads
            .by_id(thread_id)
            .await
            .expect("thread lookup")
            .expect("thread")
            .title,
        "must not change"
    );

    owner
        .send_command(runtime_command(
            session_id,
            SessionCommandKind::Abort,
            json!({}),
        ))
        .await
        .expect("abort task");
    if let Some(control) = owner
        .host
        .execution
        .task_controls
        .lock()
        .await
        .get(&session_id)
    {
        control.abort_handle.abort();
    }
    sleep(Duration::from_millis(100)).await;
}

#[tokio::test]
async fn implicit_default_session_cannot_be_archived_or_removed() {
    let home = tempdir().expect("home");
    let cwd = tempdir().expect("cwd");
    let transport = EmbeddedTransport::from_home_and_cwd(home.path(), cwd.path())
        .await
        .expect("transport");
    let session_id = transport.default_session_id();
    let now = chrono::Utc::now();
    let thread = ThreadRecord {
        thread_id: ThreadId::new(),
        session_id,
        parent_thread_id: None,
        forked_from_turn_id: None,
        forked_from_sequence_no: None,
        workspace_root: Some(
            cwd.path()
                .canonicalize()
                .expect("canonical cwd")
                .display()
                .to_string(),
        ),
        rebound_from_workspace_root: None,
        rollout_path: None,
        title: "attached".to_owned(),
        preview: "attached session".to_owned(),
        created_at: now,
        updated_at: now,
        recency_at: now,
        archived: false,
        removed: false,
    };
    transport
        .host
        .storage
        .repositories
        .threads
        .upsert(&thread)
        .await
        .expect("thread");

    for kind in [
        SessionCommandKind::ArchiveThread,
        SessionCommandKind::DeleteThread,
    ] {
        let mut command = runtime_command(session_id, kind, json!({"thread_id": thread.thread_id}));
        command.session_id = None;
        let ack = transport
            .send_command(command)
            .await
            .expect("metadata command");
        assert!(!ack.accepted);
        assert_eq!(
            ack.reason.as_deref(),
            Some("the currently attached session cannot be archived or deleted")
        );
    }
    assert!(
        transport
            .host
            .storage
            .repositories
            .threads
            .by_id(thread.thread_id)
            .await
            .expect("thread lookup")
            .is_some()
    );
}

#[tokio::test]
async fn debug_export_writes_redacted_session_bundle_atomically() {
    let host = RuntimeHost::in_memory().await.expect("host");
    let session_id = SessionId::new();
    let thread_id = ThreadId::new();
    let task_id = TaskId::new();
    let at = chrono::Utc::now();
    host.storage
        .repositories
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
            removed: false,
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
    fs::write(workspace.path().join("expected-child.txt"), "child").expect("expected child");
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
                "external_verifiers": [exact_file_verifier("expected-child.txt", "child.txt")],
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
async fn rollout_sync_removes_only_projections_without_thread_records() {
    let home = tempdir().expect("home");
    let workspace = tempdir().expect("workspace");
    let host = RuntimeHost::from_home_and_cwd(home.path(), workspace.path())
        .await
        .expect("host");
    let paths = host.runtime_paths.as_ref().expect("runtime paths");
    let workspace_root = workspace
        .path()
        .canonicalize()
        .expect("canonical workspace")
        .display()
        .to_string();
    let at = chrono::Utc::now();
    let archived_id = ThreadId::new();
    let removed_id = ThreadId::new();
    for (thread_id, archived, removed) in [(archived_id, true, false), (removed_id, true, true)] {
        let path = paths.rollout_path(thread_id);
        host.storage
            .repositories
            .threads
            .upsert(&ThreadRecord {
                thread_id,
                session_id: SessionId::new(),
                parent_thread_id: None,
                forked_from_turn_id: None,
                forked_from_sequence_no: None,
                workspace_root: Some(workspace_root.clone()),
                rebound_from_workspace_root: None,
                rollout_path: Some(path.display().to_string()),
                title: "retained history".to_owned(),
                preview: String::new(),
                created_at: at,
                updated_at: at,
                recency_at: at,
                archived,
                removed,
            })
            .await
            .expect("retained thread");
        fs::write(path, "retained\n").expect("retained projection");
    }

    let orphan = paths.rollout_path(ThreadId::new());
    let non_thread_file = paths.rollouts_dir.join("notes.jsonl");
    let uuid_directory = paths.rollout_path(ThreadId::new());
    fs::write(&orphan, "orphaned\n").expect("orphan projection");
    fs::write(&non_thread_file, "user data\n").expect("non-thread file");
    fs::create_dir(&uuid_directory).expect("uuid directory");

    host.synchronize_workspace_rollouts()
        .await
        .expect("rollout synchronization");

    assert!(!orphan.exists());
    assert_eq!(
        fs::read_to_string(paths.rollout_path(archived_id)).expect("archived projection"),
        "retained\n"
    );
    assert_eq!(
        fs::read_to_string(paths.rollout_path(removed_id)).expect("removed projection"),
        "retained\n"
    );
    assert_eq!(
        fs::read_to_string(non_thread_file).expect("non-thread file retained"),
        "user data\n"
    );
    assert!(uuid_directory.is_dir());
}

#[tokio::test]
async fn fork_from_turn_copies_complete_history_with_fresh_runtime_ids() {
    let workspace = tempdir().expect("workspace");
    fs::write(
        workspace.path().join("expected-fork-parent.txt"),
        "parent artifact",
    )
    .expect("expected fork artifact");
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
                "external_verifiers": [exact_file_verifier(
                    "expected-fork-parent.txt",
                    "fork-parent.txt",
                )],
            }),
        ))
        .await
        .expect("first command");
    wait_for_status(&transport, parent_session_id, TaskStatus::Completed).await;
    let after_first = transport
        .host
        .storage
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
        .storage
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
            .storage
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
        .storage
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
            .storage
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

#[cfg(unix)]
#[tokio::test]
async fn committed_event_repairs_a_failed_rollout_append_without_false_data_loss() {
    use std::os::unix::fs::symlink;

    let workspace = tempdir().expect("workspace");
    let state_parent = tempdir().expect("state parent");
    let state_home = state_parent.path().join("runtime");
    let host = RuntimeHost::ephemeral_persistent_for_cwd(workspace.path(), &state_home)
        .await
        .expect("host");
    let session_id = host.default_session_id();
    let thread_id = host.default_thread_id();
    let rollout_path = host
        .runtime_paths
        .as_ref()
        .expect("runtime paths")
        .rollout_path(thread_id);
    fs::create_dir_all(rollout_path.parent().expect("rollout parent")).expect("rollout parent");
    let symlink_target = state_home.join("stale-rollout.jsonl");
    fs::write(&symlink_target, "stale\n").expect("symlink target");
    symlink(&symlink_target, &rollout_path).expect("rollout symlink");
    let now = chrono::Utc::now();
    host.storage
        .repositories
        .threads
        .upsert(&ThreadRecord {
            thread_id,
            session_id,
            parent_thread_id: None,
            forked_from_turn_id: None,
            forked_from_sequence_no: None,
            workspace_root: host.workspace_root_string(),
            rebound_from_workspace_root: None,
            rollout_path: Some(rollout_path.display().to_string()),
            title: "rollout repair".to_owned(),
            preview: "rollout repair".to_owned(),
            created_at: now,
            updated_at: now,
            recency_at: now,
            archived: false,
            removed: false,
        })
        .await
        .expect("thread");

    host.record_event(host_event(
        0,
        session_id,
        None,
        RuntimeEventType::CommandReceived,
        RuntimeEventSource::Runtime,
        json!({"summary": "repair rollout"}),
    ))
    .await
    .expect("canonical event remains successful");

    assert!(
        !fs::symlink_metadata(&rollout_path)
            .expect("rebuilt rollout")
            .file_type()
            .is_symlink()
    );
    assert!(
        fs::read_to_string(&rollout_path)
            .expect("rebuilt rollout")
            .contains("repair rollout")
    );
    assert!(
        !host
            .execution
            .rollout_projection_failures
            .lock()
            .await
            .contains_key(&session_id)
    );
}

#[cfg(unix)]
#[tokio::test]
async fn committed_event_reports_rollout_loss_only_when_rebuild_also_fails() {
    use std::os::unix::fs::symlink;

    let workspace = tempdir().expect("workspace");
    let state_parent = tempdir().expect("state parent");
    let state_home = state_parent.path().join("runtime");
    let host = RuntimeHost::ephemeral_persistent_for_cwd(workspace.path(), &state_home)
        .await
        .expect("host");
    let session_id = host.default_session_id();
    let thread_id = host.default_thread_id();
    let rollout_path = host
        .runtime_paths
        .as_ref()
        .expect("runtime paths")
        .rollout_path(thread_id);
    fs::create_dir_all(rollout_path.parent().expect("rollout parent")).expect("rollout parent");
    let symlink_target = state_home.join("stale-rollout.jsonl");
    let lock_target = state_home.join("stale-rollout.lock");
    fs::write(&symlink_target, "stale\n").expect("symlink target");
    fs::write(&lock_target, "lock\n").expect("lock target");
    symlink(&symlink_target, &rollout_path).expect("rollout symlink");
    symlink(&lock_target, rollout::rollout_lock_path(&rollout_path)).expect("lock symlink");
    let now = chrono::Utc::now();
    host.storage
        .repositories
        .threads
        .upsert(&ThreadRecord {
            thread_id,
            session_id,
            parent_thread_id: None,
            forked_from_turn_id: None,
            forked_from_sequence_no: None,
            workspace_root: host.workspace_root_string(),
            rebound_from_workspace_root: None,
            rollout_path: Some(rollout_path.display().to_string()),
            title: "rollout failure".to_owned(),
            preview: "rollout failure".to_owned(),
            created_at: now,
            updated_at: now,
            recency_at: now,
            archived: false,
            removed: false,
        })
        .await
        .expect("thread");

    host.record_event(host_event(
        0,
        session_id,
        None,
        RuntimeEventType::CommandReceived,
        RuntimeEventSource::Runtime,
        json!({"summary": "retain canonical event"}),
    ))
    .await
    .expect("projection failure must not mask the canonical commit");

    let projection = host
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
    assert_eq!(projection["trace_complete"], false);
    assert!(
        projection["missing_sections"]
            .as_array()
            .expect("missing sections")
            .iter()
            .any(|section| section == "rollout_projection")
    );
    assert!(
        projection["retention_losses"]
            .as_array()
            .expect("retention losses")
            .iter()
            .any(|loss| loss
                .as_str()
                .is_some_and(|loss| loss.starts_with("rollout_projection_rebuild_failed:")))
    );
    assert!(
        host.storage
            .repositories
            .events
            .load(session_id, None, None)
            .await
            .expect("canonical events")
            .iter()
            .any(|event| event.payload["summary"] == "retain canonical event")
    );
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
        .storage
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
            removed: false,
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
        .storage
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
        .storage
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
            .storage
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
                "path": "chain.txt",
                "content": "ok",
            }),
        ))
        .await
        .expect("command");
    wait_for_terminal_status(&transport, transport.default_session_id()).await;

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
                "path": "first.txt",
                "content": "done",
            }),
        ))
        .await
        .expect("command");
    wait_for_terminal_status(&transport, transport.default_session_id()).await;

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
                "path": "tui.txt",
                "content": "ok",
                "_thread_id": tui_thread_id.to_string(),
            }),
        ))
        .await
        .expect("command");
    wait_for_terminal_status(&transport, tui_session_id).await;
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
    fs::write(workspace.path().join("expected-result.txt"), "done").expect("expected result");
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
                "external_verifiers": [exact_file_verifier("expected-result.txt", "result.txt")],
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
        .storage
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
    assert_eq!(operation_changes.len(), 1);
    assert_eq!(operation_changes[0].path, "result.txt");
    assert_eq!(operation_changes[0].kind, FileChangeKind::Modified);
    assert_eq!(operation_changes[0].added_lines, Some(1));
    assert_eq!(operation_changes[0].removed_lines, Some(1));
    assert!(
        operation_changes[0]
            .before
            .as_ref()
            .and_then(|state| state.checksum.as_deref())
            .is_some_and(|checksum| checksum.starts_with("sha256:"))
    );
    assert!(
        operation_changes[0]
            .after
            .as_ref()
            .and_then(|state| state.checksum.as_deref())
            .is_some_and(|checksum| checksum.starts_with("sha256:"))
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
    let change_manifest_ref = tool_payload["change_manifest_artifact_ref"]
        .as_str()
        .expect("workspace change manifest artifact ref");
    assert!(debug["artifacts"].as_array().is_some_and(|artifacts| {
        artifacts.iter().any(|artifact| {
            artifact["artifact_id"] == diff_artifact_ref
                && artifact["artifact_type"] == "workspace_diff"
                && artifact["checksum"]
                    .as_str()
                    .is_some_and(|checksum| checksum.starts_with("sha256:"))
        }) && artifacts.iter().any(|artifact| {
            artifact["artifact_id"] == change_manifest_ref
                && artifact["artifact_type"] == "workspace_change_manifest"
                && artifact["checksum"]
                    .as_str()
                    .is_some_and(|checksum| checksum.starts_with("sha256:"))
        })
    }));
    let change_manifest_bytes = transport
        .host
        .storage
        .store
        .load_artifact_bytes(
            change_manifest_ref
                .parse::<ArtifactId>()
                .expect("change manifest artifact id"),
        )
        .await
        .expect("change manifest artifact")
        .expect("change manifest bytes");
    let change_manifest: Value =
        serde_json::from_slice(&change_manifest_bytes).expect("change manifest JSON");
    assert_eq!(
        change_manifest["operation_changes"][0]["path"],
        "result.txt"
    );
    assert_eq!(change_manifest["turn_change_summary"]["file_count"], 1);
    assert!(
        events[checkpoint_index]["payload"]["checkpoint"]["artifact_refs"]
            .as_array()
            .is_some_and(Vec::is_empty)
    );
    let checkpoint_before_images = tool_payload["checkpoint_before_images"]
        .as_array()
        .expect("changed-file checkpoint before images");
    assert_eq!(checkpoint_before_images.len(), 1);
    assert_eq!(checkpoint_before_images[0]["path"], "result.txt");
    let checkpoint_artifact_ref = checkpoint_before_images[0]["artifact_ref"]
        .as_str()
        .expect("checkpoint artifact ref");
    assert!(debug["artifacts"].as_array().is_some_and(|artifacts| {
        artifacts.iter().any(|artifact| {
            artifact["artifact_id"] == checkpoint_artifact_ref
                && artifact["artifact_type"] == "checkpoint_before_image"
        })
    }));
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
async fn opaque_process_checkpoints_filter_ignored_before_images() {
    let workspace = tempdir().expect("workspace");
    fs::write(
        workspace.path().join(".gitignore"),
        ".gitignore\n*.secret\n",
    )
    .expect("gitignore");
    fs::write(workspace.path().join("safe.txt"), "safe").expect("safe file");
    fs::write(workspace.path().join("token.secret"), "secret").expect("ignored file");
    let _home = IsolatedGlobalMockProvider::empty().await;
    let transport = EmbeddedTransport::for_cwd(workspace.path())
        .await
        .expect("transport");
    let session_id = transport.default_session_id();
    let task = HostedAgentTask {
        session_id,
        task_id: TaskId::new(),
        turn_id: TurnId::new(),
        payload: json!({"prompt": "inspect workspace"}),
    };
    let before_images = vec![
        FileBeforeImage {
            path: workspace.path().join(".gitignore"),
            content: Some(b".gitignore\n*.secret\n".to_vec()),
            unix_mode: None,
            metadata: None,
        },
        FileBeforeImage {
            path: workspace.path().join("safe.txt"),
            content: Some(b"safe".to_vec()),
            unix_mode: None,
            metadata: None,
        },
        FileBeforeImage {
            path: workspace.path().join("token.secret"),
            content: Some(b"secret".to_vec()),
            unix_mode: None,
            metadata: None,
        },
    ];

    for tool_name in ["shell", "external_verifier"] {
        let request = ToolRequest {
            tool_call_id: ToolCallId::new(),
            provider_tool_call_id: None,
            session_id,
            turn_id: Some(task.turn_id),
            tool_name: tool_name.to_owned(),
            arguments: json!({"program": "test"}),
        };
        transport
            .host
            .persist_checkpoint_before_side_effect(&task, &request, &before_images, false)
            .await
            .expect("partial process checkpoint");
    }
    let events = transport
        .replay_events(EventFilter {
            session_id,
            task_id: Some(task.task_id),
            after_sequence_no: None,
        })
        .await
        .expect("checkpoint events");
    let checkpoints = events
        .iter()
        .filter(|event| event["event_type"] == json!(RuntimeEventType::CheckpointCreated))
        .collect::<Vec<_>>();

    assert_eq!(checkpoints.len(), 2);
    for checkpoint in checkpoints {
        assert_eq!(checkpoint["payload"]["before_image_complete"], false);
        assert_eq!(checkpoint["payload"]["omitted_before_image_count"], 2);
        assert_eq!(
            checkpoint["payload"]["checkpoint"]["changed_files"],
            json!(["safe.txt"])
        );
        assert_eq!(
            checkpoint["payload"]["checkpoint"]["artifact_refs"]
                .as_array()
                .map(Vec::len),
            Some(0)
        );
        assert_eq!(checkpoint["payload"]["candidate_before_image_count"], 1);
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
    assert_eq!(events[streamed].payload["coalescing"]["applied"], false);
    assert_eq!(
        events[streamed].payload["coalescing"]["omitted_event_count"],
        0
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
    let malformed = transport
        .send_command(SessionCommand {
            command_id: CommandId::new(),
            session_id: Some(session_id),
            kind: SessionCommandKind::Deny,
            idempotency_key: "deny-malformed-tool-id".to_owned(),
            actor: Actor {
                kind: ActorKind::Cli,
                id: "test".to_owned(),
            },
            payload: json!({"approval_id": "not-a-uuid"}),
            timestamp: chrono::Utc::now(),
        })
        .await
        .expect("malformed approval resolution");
    assert!(!malformed.accepted);
    assert_eq!(
        malformed.reason.as_deref(),
        Some("approval_id must be a valid UUID")
    );
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
    wait_for_status(&transport, session_id, TaskStatus::Failed).await;
    let events = transport
        .host
        .storage
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
async fn terminal_task_rejects_a_stale_structured_question_answer() {
    let host = RuntimeHost::in_memory().await.expect("host");
    let transport = EmbeddedTransport::new(host.clone());
    let session_id = host.default_session_id();
    let task_id = TaskId::new();
    let request = UserQuestionRequest {
        question_id: QuestionId::new(),
        task_id,
        turn_id: TurnId::new(),
        tool_call_id: ToolCallId::new(),
        questions: vec![UserQuestionPrompt {
            id: "format".to_owned(),
            header: "Output".to_owned(),
            question: "Choose the output format".to_owned(),
            mode: UserQuestionMode::Single,
            options: vec![
                UserQuestionOption {
                    id: "json".to_owned(),
                    label: "JSON".to_owned(),
                    description: None,
                },
                UserQuestionOption {
                    id: "text".to_owned(),
                    label: "Text".to_owned(),
                    description: None,
                },
            ],
        }],
    };
    host.record_event(host_event(
        host.next_sequence_no(),
        session_id,
        Some(task_id),
        RuntimeEventType::UserQuestionRequested,
        RuntimeEventSource::Runtime,
        json!({"request": request}),
    ))
    .await
    .expect("question event");
    host.record_event(host_event(
        host.next_sequence_no(),
        session_id,
        Some(task_id),
        RuntimeEventType::TaskCompleted,
        RuntimeEventSource::Runtime,
        json!({"summary": "task completed"}),
    ))
    .await
    .expect("terminal event");

    let ack = transport
        .send_command(runtime_command(
            session_id,
            SessionCommandKind::AnswerQuestion,
            json!({
                "question_id": request.question_id,
                "answers": [{"question_id": "format", "selected_option_ids": ["json"]}],
            }),
        ))
        .await
        .expect("stale answer command");
    let events = host
        .storage
        .store
        .load_events(session_id, Some(task_id), None)
        .await
        .expect("events");

    assert!(!ack.accepted);
    assert!(
        ack.reason
            .as_deref()
            .is_some_and(|reason| reason.contains("does not control an active task"))
    );
    assert!(
        !events
            .iter()
            .any(|event| { event.event_type == RuntimeEventType::UserQuestionResolved })
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
    wait_for_status(&transport, session_id, TaskStatus::Failed).await;

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
    fs::write(
        workspace.path().join("expected-ephemeral.txt"),
        "temporary state",
    )
    .expect("expected ephemeral state");
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
                "external_verifiers": [exact_file_verifier(
                    "expected-ephemeral.txt",
                    "ephemeral.txt",
                )],
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
async fn persisted_ephemeral_runtime_retains_isolated_state_and_full_run_bundle() {
    let workspace = tempdir().expect("workspace");
    let state_parent = tempdir().expect("state parent");
    let state_dir = state_parent.path().join("benchmark-run");
    let provider = IsolatedGlobalMockProvider::install().await;
    let global_provider_paths = ProviderConfigPaths::global().expect("provider paths");
    let global_runtime_db = global_provider_paths.home.join("state/runtime.sqlite");

    let transport = EmbeddedTransport::ephemeral_persistent_for_cwd(workspace.path(), &state_dir)
        .await
        .expect("persisted ephemeral transport");
    let host = transport.host.clone();
    let runtime_paths = host
        .runtime_paths
        .as_ref()
        .expect("persisted runtime paths")
        .clone();

    assert_eq!(
        runtime_paths.home,
        state_dir.canonicalize().expect("state home")
    );
    assert_ne!(runtime_paths.home, global_provider_paths.home);
    assert_eq!(
        host.provider_config_paths
            .as_ref()
            .expect("global provider paths"),
        &global_provider_paths
    );
    assert!(!global_runtime_db.exists());
    assert!(!state_dir.join("provider.json").exists());
    assert!(!state_dir.join("credentials.json").exists());

    let runtime_transport = RuntimeTransport::Embedded(transport.clone());
    let client = AgentClient::new(runtime_transport.clone());
    let thread = client.start_thread().await.expect("thread");
    let mut handle = thread
        .start_turn(
            "write file persisted.txt with content retained state",
            golutra_protocol::AgentTurnOptions {
                defer_external_verification: true,
                ..golutra_protocol::AgentTurnOptions::default()
            },
        )
        .await
        .expect("turn");
    let selection = SessionWindowRequest {
        anchor_thread_id: thread.thread_id(),
        range: SessionRangeSpec {
            direction: SessionRangeDirection::Single,
            count: 1,
        },
    };
    let checkpoint = RunBundleExporter::new(&runtime_transport)
        .checkpoint(RunBundleExportRequest {
            destination: state_dir.clone(),
            selection: selection.clone(),
            terminal_outcome: RunBundleTerminalOutcome::InProgress {
                reason: "turn running".to_owned(),
            },
        })
        .await
        .expect("in-progress run checkpoint");
    assert_eq!(checkpoint.session_count, 1);
    assert_eq!(checkpoint.task_count, 1);
    let checkpoint_manifest: RunBundleManifest = serde_json::from_slice(
        &fs::read(state_dir.join("manifest.json")).expect("checkpoint manifest"),
    )
    .expect("parse checkpoint manifest");
    assert!(matches!(
        checkpoint_manifest.terminal_outcome,
        RunBundleTerminalOutcome::InProgress { .. }
    ));
    while handle.next_event().await.expect("turn event").is_some() {}
    let result = handle.wait().await.expect("turn result");
    let task_id = result.task_id.expect("task id");

    assert_eq!(result.status, TaskStatus::Partial);
    assert_eq!(
        result.outcome.as_ref().map(|outcome| outcome.execution),
        Some(golutra_core::ExecutionOutcome::CandidateReady)
    );
    assert_eq!(
        result
            .outcome
            .as_ref()
            .map(|outcome| outcome.external_verification),
        Some(golutra_core::ExternalVerificationStatus::Pending)
    );
    assert!(!result.outcome.as_ref().expect("candidate outcome").scorable);
    assert_eq!(
        fs::read_to_string(workspace.path().join("persisted.txt")).expect("written file"),
        "retained state"
    );
    assert!(runtime_paths.runtime_db.is_file());
    assert!(runtime_paths.artifacts_dir.is_dir());
    assert!(
        fs::read_dir(&runtime_paths.checkpoints_dir)
            .expect("checkpoint directory")
            .next()
            .is_some(),
        "persisted ephemeral writes must keep checkpoints"
    );
    let runtime_events = host
        .storage
        .store
        .load_events(result.session_id, Some(task_id), None)
        .await
        .expect("runtime events");
    assert!(
        !runtime_events.is_empty(),
        "runtime events must remain queryable before shutdown"
    );
    let task_created = runtime_events
        .iter()
        .find(|event| event.event_type == RuntimeEventType::TaskCreated)
        .expect("task created event");
    assert_eq!(
        task_created
            .payload
            .pointer("/payload/task_contract/require_objective_validation"),
        Some(&json!(false)),
        "an explicitly deferred open task delegates final objective proof"
    );
    assert_eq!(
        task_created.payload.pointer("/payload/execution_mode"),
        Some(&json!("open")),
        "the high-level Rust API must use the same explicit default as other new clients"
    );
    assert_eq!(
        task_created.payload.pointer("/payload/tool_profile"),
        Some(&json!("coding"))
    );
    assert_eq!(
        task_created
            .payload
            .pointer("/payload/_task_contract_origin"),
        Some(&json!("open"))
    );
    assert!(
        !runtime_events
            .iter()
            .any(|event| event.event_type == RuntimeEventType::MemoryCandidateQuarantined)
    );

    let receipt = RunBundleExporter::new(&runtime_transport)
        .export(RunBundleExportRequest {
            destination: state_dir.clone(),
            selection,
            terminal_outcome: RunBundleTerminalOutcome::Result {
                result: result.clone(),
            },
        })
        .await
        .expect("full run bundle");
    assert_eq!(receipt.session_count, 1);
    assert!(receipt.complete);
    assert!(receipt.debug_export_error.is_none());
    assert_eq!(receipt.debug_export_path.as_deref(), Some("debug-export"));
    let run_manifest: RunBundleManifest = serde_json::from_slice(
        &fs::read(state_dir.join("manifest.json")).expect("run bundle manifest"),
    )
    .expect("parse run bundle manifest");
    assert_eq!(run_manifest.format, "golutra-run-bundle");
    assert_eq!(run_manifest.mode, "full-owner-only");
    assert!(matches!(
        &run_manifest.terminal_outcome,
        RunBundleTerminalOutcome::Result { result: exported } if exported.task_id == result.task_id
    ));
    assert!(run_manifest.raw_state.runtime_database.present);
    assert!(run_manifest.raw_state.runtime_database.checksum.is_some());
    let observations = &run_manifest.observations;
    assert!(observations.complete);
    assert_eq!(observations.sessions.len(), 1);
    assert!(observations.files.iter().any(|file| {
        file.path.ends_with("/events.jsonl") && file.checksum.starts_with("sha256:")
    }));
    let observation_root = state_dir.join("observations");
    assert!(observation_root.join("manifest.json").is_file());
    assert!(
        observation_root
            .join(format!("sessions/{}/events.jsonl", result.session_id))
            .is_file()
    );
    assert!(
        observation_root
            .join(format!(
                "sessions/{}/tasks/{task_id}/trace.json",
                result.session_id
            ))
            .is_file()
    );
    let conversation = fs::read_to_string(
        observation_root.join(format!("sessions/{}/conversation.jsonl", result.session_id)),
    )
    .expect("full conversation history");
    assert!(conversation.contains("write file persisted.txt"));
    let trace: golutra_protocol::TaskTracePage = serde_json::from_slice(
        &fs::read(observation_root.join(format!(
            "sessions/{}/tasks/{task_id}/trace.json",
            result.session_id
        )))
        .expect("task trace"),
    )
    .expect("parse task trace");
    assert_eq!(
        trace
            .verification
            .as_ref()
            .map(|verification| verification.result),
        Some(VerificationResult::Partial)
    );
    assert!(state_dir.join("debug-export/manifest.json").is_file());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let mode = |path: &std::path::Path| {
            fs::metadata(path)
                .expect("run bundle metadata")
                .permissions()
                .mode()
                & 0o777
        };
        assert_eq!(mode(&state_dir), 0o700);
        assert_eq!(mode(&observation_root), 0o700);
        assert_eq!(mode(&observation_root.join("manifest.json")), 0o600);
    }

    drop(client);
    drop(runtime_transport);
    drop(transport);
    drop(thread);
    drop(host);
    workspace.close().expect("remove original workspace");

    let reopened = RuntimeStore::connect_single_writer_with_artifact_root(
        &runtime_paths.sqlite_url(),
        runtime_paths.artifacts_dir.clone(),
    )
    .await
    .expect("reopen persisted runtime store");
    assert!(
        !reopened
            .load_events(result.session_id, Some(task_id), None)
            .await
            .expect("persisted runtime events")
            .is_empty(),
        "events must survive transport shutdown"
    );
    let reopened_transport = RuntimeTransport::open_persisted_run(&state_dir)
        .await
        .expect("reopen persisted run transport");
    assert_eq!(reopened_transport.default_session_id(), result.session_id);
    let trace = reopened_transport
        .complete_task_trace(TaskTraceRequest {
            session_id: result.session_id,
            task_id,
            view: TraceView::Full,
            cursor: None,
            limit: 512,
            wait_for_evaluation: true,
        })
        .await
        .expect("reopened trace");
    let evaluator_evidence = br#"{"case":"persisted-run","passed":true}"#;
    fs::write(state_dir.join("results.json"), evaluator_evidence)
        .expect("external evaluator evidence");
    let mut external = golutra_eval::ExternalEvaluationRecord {
        evaluation_id: "terminal-bench:test".to_owned(),
        source_task_id: task_id,
        evaluator_id: "terminal-bench".to_owned(),
        evaluator_version: "test".to_owned(),
        harness_id: "terminal-bench".to_owned(),
        harness_version: "test".to_owned(),
        dataset_id: "terminal-bench-core".to_owned(),
        dataset_version: "test".to_owned(),
        case_id: "persisted-run".to_owned(),
        verdict: golutra_eval::EvaluationVerdict::Pass,
        score: Some(1.0),
        score_max: Some(1.0),
        assertions: vec![golutra_eval::ExternalEvaluationAssertion {
            assertion_id: "persisted-result".to_owned(),
            name: "result_manifest".to_owned(),
            passed: true,
            message: "result manifest was accepted".to_owned(),
            evidence_refs: vec!["results.json".to_owned()],
        }],
        phases: Vec::new(),
        terminal_cause: None,
        artifact_refs: vec!["results.json".to_owned()],
        imported_artifacts: Vec::new(),
        imported_evidence_refs: Vec::new(),
        partition: golutra_eval::EvaluationPartitionKind::Source,
        seed: None,
        provider_variant: None,
        holdout_protected: false,
        comparison_group_id: None,
        candidate_id: None,
        campaign_id: None,
        role: None,
        base_trace_digest: trace.integrity.event_chain_digest,
        runtime_identity: trace.runtime_identity,
        result_digest: String::new(),
        trust: golutra_eval::ExternalEvaluationTrust::OwnerLocal,
        attestation: None,
        ingested_at: chrono::Utc::now(),
    };
    external.result_digest = golutra_eval::external_evaluation_result_digest(&external);
    let ack = reopened_transport
        .send_command(runtime_command(
            result.session_id,
            SessionCommandKind::IngestExternalEvaluation,
            json!({
                "record": external,
                "artifact_base_path": state_dir.to_string_lossy(),
            }),
        ))
        .await
        .expect("ingest external result into reopened run");
    assert!(ack.accepted);
    let post_evaluation_events = reopened
        .load_events(result.session_id, Some(task_id), None)
        .await
        .expect("post-evaluation events");
    let post_evaluation_terminal = post_evaluation_events
        .iter()
        .rev()
        .find(|event| event.event_type.is_task_terminal())
        .expect("post-evaluation terminal event");
    assert_eq!(
        post_evaluation_terminal
            .payload
            .pointer("/outcome/external_verification"),
        Some(&json!("pass"))
    );
    assert_eq!(
        post_evaluation_terminal
            .payload
            .pointer("/outcome/execution"),
        Some(&json!("completed"))
    );
    assert_eq!(
        post_evaluation_terminal.payload.get("status"),
        Some(&json!("completed"))
    );
    assert_eq!(
        post_evaluation_terminal
            .payload
            .pointer("/outcome/scorable"),
        Some(&json!(true))
    );
    fs::write(
        state_dir.join("results.json"),
        b"source changed after ingestion",
    )
    .expect("mutate evaluator source after ingestion");
    let refreshed = RunBundleExporter::new(&reopened_transport)
        .refresh(&state_dir)
        .await
        .expect("refresh observations");
    assert!(refreshed.complete);
    let refreshed_trace: golutra_protocol::TaskTracePage = serde_json::from_slice(
        &fs::read(observation_root.join(format!(
            "sessions/{}/tasks/{task_id}/trace.json",
            result.session_id
        )))
        .expect("refreshed trace"),
    )
    .expect("parse refreshed trace");
    assert_eq!(refreshed_trace.evaluation.external_evaluations.len(), 1);
    let imported_evaluation = &refreshed_trace.evaluation.external_evaluations[0];
    assert_eq!(imported_evaluation.imported_artifacts.len(), 1);
    assert_eq!(imported_evaluation.imported_evidence_refs.len(), 1);
    let imported_artifact = &imported_evaluation.imported_artifacts[0];
    assert_eq!(imported_artifact.source_ref, "results.json");
    assert_eq!(
        imported_artifact.checksum,
        format!("sha256:{:x}", sha2::Sha256::digest(evaluator_evidence))
    );
    assert_eq!(
        reopened
            .load_artifact_bytes(imported_artifact.artifact_ref)
            .await
            .expect("imported evidence artifact")
            .expect("imported evaluator evidence bytes"),
        evaluator_evidence
    );
    assert!(refreshed_trace.artifacts.iter().any(|artifact| {
        artifact.artifact_id == imported_artifact.artifact_ref
            && artifact.artifact_type == "external_evaluator_evidence"
    }));
    assert!(refreshed_trace.evidence.iter().any(|evidence| {
        imported_evaluation
            .imported_evidence_refs
            .contains(&evidence.evidence_id)
            && evidence
                .artifact_refs
                .contains(&imported_artifact.artifact_ref)
    }));
    assert!(
        refreshed_trace
            .events
            .iter()
            .any(|event| { event.event_type == RuntimeEventType::ExternalEvaluationIngested })
    );
    assert!(
        refreshed_trace
            .events
            .iter()
            .any(|event| { event.event_type == RuntimeEventType::ExternalVerificationRequested })
    );
    assert!(
        refreshed_trace
            .events
            .iter()
            .any(|event| { event.event_type == RuntimeEventType::ExternalVerificationFeedback })
    );
    assert!(!global_runtime_db.exists());
    drop(reopened_transport);
    drop(reopened);

    let refreshed_manifest_path = state_dir.join("manifest.json");
    let original_manifest_bytes = fs::read(&refreshed_manifest_path).expect("refreshed manifest");
    let refreshed_manifest: RunBundleManifest =
        serde_json::from_slice(&original_manifest_bytes).expect("refreshed manifest json");
    let runtime_database = fs::read(&runtime_paths.runtime_db).expect("runtime database bytes");
    let runtime_database_checksum = format!("sha256:{:x}", Sha256::digest(&runtime_database));
    assert_eq!(
        refreshed_manifest.raw_state.runtime_database.bytes,
        Some(runtime_database.len() as u64)
    );
    assert_eq!(
        refreshed_manifest
            .raw_state
            .runtime_database
            .checksum
            .as_deref(),
        Some(runtime_database_checksum.as_str())
    );
    assert!(
        !runtime_paths
            .runtime_db
            .with_extension("sqlite-wal")
            .exists()
    );
    assert!(
        !runtime_paths
            .runtime_db
            .with_extension("sqlite-shm")
            .exists()
    );
    let trace_path = observation_root.join(format!(
        "sessions/{}/tasks/{task_id}/trace.json",
        result.session_id
    ));
    let original_trace_bytes = fs::read(&trace_path).expect("refreshed trace bytes");
    let artifact_id = refreshed_trace
        .artifacts
        .first()
        .expect("persisted trace artifact")
        .artifact_id;
    let artifact_path = state_dir.join(format!("state/artifacts/{artifact_id}.blob"));
    let original_artifact_bytes = fs::read(&artifact_path).expect("persisted artifact blob");

    fs::write(&artifact_path, b"tampered artifact blob").expect("tamper artifact");
    let artifact_error = match RuntimeTransport::open_persisted_run(&state_dir).await {
        Ok(_) => panic!("tampered artifact must not open"),
        Err(error) => error,
    };
    assert!(artifact_error.to_string().contains("artifact"));
    assert!(artifact_error.to_string().contains("integrity"));
    fs::write(&artifact_path, &original_artifact_bytes).expect("restore artifact");

    let mut checksum_tampered_trace = original_trace_bytes.clone();
    checksum_tampered_trace.push(b'\n');
    fs::write(&trace_path, checksum_tampered_trace).expect("tamper trace checksum");
    let trace_error = match RuntimeTransport::open_persisted_run(&state_dir).await {
        Ok(_) => panic!("trace with a mismatched manifest checksum must not open"),
        Err(error) => error,
    };
    assert!(
        trace_error
            .to_string()
            .contains("observation manifest integrity")
    );
    fs::write(&trace_path, &original_trace_bytes).expect("restore trace");

    let mut tampered_trace = refreshed_trace;
    tampered_trace.events[0].payload["tampered"] = json!(true);
    let mut digest = Sha256::new();
    for event in &tampered_trace.events {
        digest.update(event.sequence_no.to_be_bytes());
        digest.update(serde_json::to_vec(event).expect("event json"));
    }
    tampered_trace.integrity.event_chain_digest = format!("sha256:{:x}", digest.finalize());
    let tampered_trace_bytes = serde_json::to_vec_pretty(&tampered_trace).expect("tampered trace");
    fs::write(&trace_path, &tampered_trace_bytes).expect("write self-consistent trace tamper");
    let mut tampered_manifest: RunBundleManifest =
        serde_json::from_slice(&original_manifest_bytes).expect("refreshed manifest json");
    let trace_relative = format!(
        "observations/sessions/{}/tasks/{task_id}/trace.json",
        result.session_id
    );
    let trace_entry = tampered_manifest
        .observations
        .files
        .iter_mut()
        .find(|file| file.path == trace_relative)
        .expect("trace manifest entry");
    trace_entry.bytes = tampered_trace_bytes.len() as u64;
    trace_entry.checksum = format!("sha256:{:x}", Sha256::digest(&tampered_trace_bytes));
    fs::write(
        &refreshed_manifest_path,
        serde_json::to_vec_pretty(&tampered_manifest).expect("tampered manifest"),
    )
    .expect("write tampered manifest");
    let prefix_error = match RuntimeTransport::open_persisted_run(&state_dir).await {
        Ok(_) => panic!("trace that diverges from SQLite source events must not open"),
        Err(error) => error,
    };
    assert!(prefix_error.to_string().contains("source event prefix"));
    drop(provider);
}

#[tokio::test]
async fn persisted_interruption_accepts_a_prefix_bound_owner_evaluation() {
    let workspace = tempdir().expect("workspace");
    let state_parent = tempdir().expect("state parent");
    let state_dir = state_parent.path().join("checkpoint-run");
    let _provider = IsolatedGlobalMockProvider::empty().await;
    let transport = EmbeddedTransport::ephemeral_persistent_for_cwd(workspace.path(), &state_dir)
        .await
        .expect("persisted ephemeral transport");
    let host = transport.host.clone();
    let session_id = host.default_session_id();
    let thread_id = host.default_thread_id;
    let task_id = TaskId::new();
    let turn_id = TurnId::new();
    let run_provenance =
        super::provenance::run_provenance(task_id, host.workspace_id, Some(workspace.path()), None);
    host.upsert_current_thread(session_id, &json!({"prompt": "checkpointed task"}))
        .await
        .expect("thread");
    let mut task_created = host_event(
        host.next_sequence_no(),
        session_id,
        Some(task_id),
        RuntimeEventType::TaskCreated,
        RuntimeEventSource::Runtime,
        json!({
            "summary": "checkpointed task",
            "runtime_identity": run_provenance.runtime_identity.clone(),
            "run_provenance": run_provenance,
        }),
    );
    task_created.turn_id = Some(turn_id);
    host.record_event(task_created).await.expect("task event");

    let runtime_transport = RuntimeTransport::Embedded(transport.clone());
    RunBundleExporter::new(&runtime_transport)
        .checkpoint(RunBundleExportRequest {
            destination: state_dir.clone(),
            selection: SessionWindowRequest {
                anchor_thread_id: host.default_thread_id,
                range: SessionRangeSpec {
                    direction: SessionRangeDirection::Single,
                    count: 1,
                },
            },
            terminal_outcome: RunBundleTerminalOutcome::InProgress {
                reason: "external harness stopped the active task".to_owned(),
            },
        })
        .await
        .expect("checkpoint bundle");
    let manifest_path = state_dir.join("manifest.json");
    let original_manifest_bytes = fs::read(&manifest_path).expect("checkpoint manifest");
    let original_manifest: RunBundleManifest =
        serde_json::from_slice(&original_manifest_bytes).expect("checkpoint manifest json");
    assert!(
        original_manifest
            .observations
            .sessions
            .iter()
            .flat_map(|session| &session.tasks)
            .any(|task| task.task_id == task_id.to_string() && !task.complete)
    );
    drop(runtime_transport);
    drop(transport);
    drop(host);

    let evaluation_for = |trace: &golutra_protocol::TaskTracePage| {
        let mut record = golutra_eval::ExternalEvaluationRecord {
            evaluation_id: "checkpoint-prefix-test".to_owned(),
            source_task_id: task_id,
            evaluator_id: "test-evaluator".to_owned(),
            evaluator_version: "1".to_owned(),
            harness_id: "test-harness".to_owned(),
            harness_version: "1".to_owned(),
            dataset_id: "test-dataset".to_owned(),
            dataset_version: "1".to_owned(),
            case_id: "checkpoint-prefix".to_owned(),
            verdict: golutra_eval::EvaluationVerdict::Fail,
            score: Some(0.0),
            score_max: Some(1.0),
            assertions: Vec::new(),
            phases: Vec::new(),
            terminal_cause: None,
            artifact_refs: Vec::new(),
            imported_artifacts: Vec::new(),
            imported_evidence_refs: Vec::new(),
            partition: golutra_eval::EvaluationPartitionKind::Source,
            seed: None,
            provider_variant: Some("mock".to_owned()),
            holdout_protected: false,
            comparison_group_id: None,
            candidate_id: None,
            campaign_id: None,
            role: None,
            base_trace_digest: trace.integrity.event_chain_digest.clone(),
            runtime_identity: trace.runtime_identity.clone(),
            result_digest: String::new(),
            trust: golutra_eval::ExternalEvaluationTrust::OwnerLocal,
            attestation: None,
            ingested_at: chrono::Utc::now(),
        };
        record.result_digest = golutra_eval::external_evaluation_result_digest(&record);
        record
    };

    let mut non_checkpoint_manifest = original_manifest.clone();
    non_checkpoint_manifest.terminal_outcome = RunBundleTerminalOutcome::Error {
        error: "ordinary failed bundle".to_owned(),
    };
    fs::write(
        &manifest_path,
        serde_json::to_vec_pretty(&non_checkpoint_manifest).expect("non-checkpoint manifest"),
    )
    .expect("write non-checkpoint manifest");
    let ordinary = RuntimeTransport::open_persisted_run(&state_dir)
        .await
        .expect("open ordinary incomplete bundle");
    let ordinary_trace = ordinary
        .complete_task_trace(TaskTraceRequest {
            session_id,
            task_id,
            view: TraceView::Full,
            cursor: None,
            limit: 512,
            wait_for_evaluation: true,
        })
        .await
        .expect("ordinary incomplete trace");
    assert!(!ordinary_trace.integrity.complete);
    let error = ordinary
        .send_command(runtime_command(
            session_id,
            SessionCommandKind::IngestExternalEvaluation,
            json!({"record": evaluation_for(&ordinary_trace)}),
        ))
        .await
        .expect_err("ordinary incomplete bundle must reject evaluator overlay");
    assert!(error.to_string().contains("base trace is incomplete"));
    drop(ordinary);

    let mut interrupted_manifest = original_manifest;
    interrupted_manifest.terminal_outcome = RunBundleTerminalOutcome::Result {
        result: golutra_protocol::AgentTurnResult {
            thread_id,
            session_id,
            task_id: Some(task_id),
            turn_id: Some(turn_id),
            status: TaskStatus::Cancelled,
            final_message: None,
            verification: None,
            outcome: None,
            last_sequence_no: None,
        },
    };
    fs::write(
        &manifest_path,
        serde_json::to_vec_pretty(&interrupted_manifest).expect("interrupted manifest"),
    )
    .expect("write interrupted manifest");
    let checkpoint = RuntimeTransport::open_persisted_run(&state_dir)
        .await
        .expect("open interrupted bundle");
    let checkpoint_trace = checkpoint
        .complete_task_trace(TaskTraceRequest {
            session_id,
            task_id,
            view: TraceView::Full,
            cursor: None,
            limit: 512,
            wait_for_evaluation: true,
        })
        .await
        .expect("checkpoint trace");
    assert!(!checkpoint_trace.integrity.complete);
    assert!(
        super::external_evaluation::checkpoint_trace_has_only_expected_incompleteness(
            &checkpoint_trace
        ),
        "interrupted trace integrity: {:#?}",
        checkpoint_trace.integrity
    );
    let mut untrusted = evaluation_for(&checkpoint_trace);
    untrusted.trust = golutra_eval::ExternalEvaluationTrust::UntrustedLocal;
    untrusted.result_digest = golutra_eval::external_evaluation_result_digest(&untrusted);
    let error = checkpoint
        .send_command(runtime_command(
            session_id,
            SessionCommandKind::IngestExternalEvaluation,
            json!({"record": untrusted}),
        ))
        .await
        .expect_err("untrusted evaluator cannot bind to an incomplete interruption");
    assert!(error.to_string().contains("base trace is incomplete"));
    let accepted = checkpoint
        .send_command(runtime_command(
            session_id,
            SessionCommandKind::IngestExternalEvaluation,
            json!({"record": evaluation_for(&checkpoint_trace)}),
        ))
        .await
        .expect("interrupted evaluator overlay");
    assert!(accepted.accepted);
    let refreshed = checkpoint
        .complete_task_trace(TaskTraceRequest {
            session_id,
            task_id,
            view: TraceView::Full,
            cursor: None,
            limit: 512,
            wait_for_evaluation: false,
        })
        .await
        .expect("trace with evaluator overlay");
    assert_eq!(refreshed.evaluation.external_evaluations.len(), 1);
}

#[tokio::test]
async fn external_evaluation_ingestion_rejects_trace_and_runtime_identity_mismatches() {
    let host = RuntimeHost::in_memory().await.expect("host");
    let transport = EmbeddedTransport::new(host.clone());
    let session_id = host.default_session_id();
    let accepted = transport
        .send_command(command(session_id, "produce a verifiable result"))
        .await
        .expect("prompt");
    assert!(accepted.accepted);
    let events = wait_for_task_completed_count(&transport, session_id, 1).await;
    let task_id = events
        .iter()
        .find(|event| event.event_type == RuntimeEventType::TaskCompleted)
        .and_then(|event| event.task_id)
        .expect("completed task");
    let trace = transport
        .complete_task_trace(TaskTraceRequest {
            session_id,
            task_id,
            view: TraceView::Full,
            cursor: None,
            limit: 512,
            wait_for_evaluation: true,
        })
        .await
        .expect("trace");

    let mut record = golutra_eval::ExternalEvaluationRecord {
        evaluation_id: "mismatch-test".to_owned(),
        source_task_id: task_id,
        evaluator_id: "test-evaluator".to_owned(),
        evaluator_version: "1".to_owned(),
        harness_id: "test-harness".to_owned(),
        harness_version: "1".to_owned(),
        dataset_id: "test-dataset".to_owned(),
        dataset_version: "1".to_owned(),
        case_id: "test-case".to_owned(),
        verdict: golutra_eval::EvaluationVerdict::Pass,
        score: Some(1.0),
        score_max: Some(1.0),
        assertions: Vec::new(),
        phases: Vec::new(),
        terminal_cause: None,
        artifact_refs: Vec::new(),
        imported_artifacts: Vec::new(),
        imported_evidence_refs: Vec::new(),
        partition: golutra_eval::EvaluationPartitionKind::Source,
        seed: Some(1),
        provider_variant: Some("mock".to_owned()),
        holdout_protected: false,
        comparison_group_id: None,
        candidate_id: None,
        campaign_id: None,
        role: None,
        base_trace_digest: trace.integrity.event_chain_digest.clone(),
        runtime_identity: trace.runtime_identity.clone(),
        result_digest: String::new(),
        trust: golutra_eval::ExternalEvaluationTrust::OwnerLocal,
        attestation: None,
        ingested_at: chrono::Utc::now(),
    };

    record.base_trace_digest = "sha256:wrong-trace".to_owned();
    record.result_digest = golutra_eval::external_evaluation_result_digest(&record);
    assert!(
        transport
            .send_command(runtime_command(
                session_id,
                SessionCommandKind::IngestExternalEvaluation,
                json!({"record": record}),
            ))
            .await
            .is_err()
    );

    record.base_trace_digest = trace.integrity.event_chain_digest;
    record.runtime_identity = "runtime:wrong".to_owned();
    record.result_digest = golutra_eval::external_evaluation_result_digest(&record);
    assert!(
        transport
            .send_command(runtime_command(
                session_id,
                SessionCommandKind::IngestExternalEvaluation,
                json!({"record": record}),
            ))
            .await
            .is_err()
    );
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
async fn natural_language_mock_write_preserves_delivery_without_claiming_unverified_success() {
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
    let state = wait_for_status(&transport, session_id, TaskStatus::Partial).await;

    assert!(ack.accepted);
    assert_eq!(projection_status(&state), Some(TaskStatus::Partial));
    assert_eq!(
        fs::read_to_string(workspace.path().join("smoke.txt")).expect("file"),
        "ok"
    );
    assert!(!workspace.path().join("golutra-agent-output.txt").exists());
}

#[tokio::test]
async fn natural_language_quoted_write_preserves_content_without_inventing_verification() {
    let workspace = tempdir().expect("workspace");
    let _provider = IsolatedGlobalMockProvider::install().await;
    let transport = EmbeddedTransport::for_cwd(workspace.path())
        .await
        .expect("transport");
    let session_id = transport.default_session_id();
    let prompt = "Create a file called hello.txt in the current directory. Write \"Hello, world!\" to it. Make sure it ends in a newline. Don't make any other files or folders.";

    let ack = transport
        .send_command(command(session_id, prompt))
        .await
        .expect("command");
    let state = wait_for_status(&transport, session_id, TaskStatus::Partial).await;

    assert!(ack.accepted);
    assert_eq!(projection_status(&state), Some(TaskStatus::Partial));
    assert_eq!(
        fs::read_to_string(workspace.path().join("hello.txt")).expect("file"),
        "Hello, world!\n"
    );
    assert!(!workspace.path().join("golutra-agent-output.txt").exists());
}

#[test]
fn security_policy_violations_exclude_recoverable_tool_contract_rejections() {
    let evaluation = PolicyEvaluation {
        policy_ref: PolicyId::new(),
        subject: "tool".to_owned(),
        action: "shell".to_owned(),
        resource: "git status && git diff".to_owned(),
        decision: PolicyDecision::Block,
        reason: "submit one argv command".to_owned(),
        evidence_refs: Vec::new(),
        block_disposition: Some(PolicyBlockDisposition::Recoverable),
    };

    assert!(!super::task_governance::is_security_policy_violation(
        &evaluation
    ));
}

#[test]
fn security_policy_violations_include_terminal_and_legacy_blocks() {
    let mut evaluation = PolicyEvaluation {
        policy_ref: PolicyId::new(),
        subject: "tool".to_owned(),
        action: "shell".to_owned(),
        resource: "dangerous command".to_owned(),
        decision: PolicyDecision::Block,
        reason: "blocked by policy".to_owned(),
        evidence_refs: Vec::new(),
        block_disposition: Some(PolicyBlockDisposition::Terminal),
    };

    assert!(super::task_governance::is_security_policy_violation(
        &evaluation
    ));
    evaluation.block_disposition = None;
    assert!(super::task_governance::is_security_policy_violation(
        &evaluation
    ));
}

#[tokio::test]
async fn task_contract_is_normalized_into_task_and_queued_turn_events() {
    let workspace = tempdir().expect("workspace");
    let _provider = IsolatedGlobalMockProvider::install().await;
    let transport = EmbeddedTransport::for_cwd(workspace.path())
        .await
        .expect("transport");
    let session_id = transport.default_session_id();

    transport
        .send_command(command(session_id, "sleep"))
        .await
        .expect("blocking task");
    wait_for_status(&transport, session_id, TaskStatus::WaitingApproval).await;

    let queued = transport
        .send_command(command(
            session_id,
            "write file queued.txt with content queued",
        ))
        .await
        .expect("queued task");
    assert!(queued.accepted);

    let events = transport
        .host
        .storage
        .repositories
        .events
        .load(session_id, None, None)
        .await
        .expect("events");
    let task_created = events
        .iter()
        .find(|event| event.event_type == RuntimeEventType::TaskCreated)
        .expect("task created");
    let queued_turn = events
        .iter()
        .find(|event| event.event_type == RuntimeEventType::TurnQueued)
        .expect("queued turn");

    let task_payload = task_created.payload.get("payload").expect("task payload");
    assert_eq!(task_payload["_task_contract_origin"], "legacy_adapter");
    assert_eq!(
        task_payload["task_contract"]["workspace_change"],
        "optional"
    );
    assert_eq!(task_payload["task_contract"]["schema_version"], 1);

    let queued_payload = queued_turn.payload.get("payload").expect("turn payload");
    assert_eq!(queued_payload["_task_contract_origin"], "legacy_adapter");
    assert_eq!(
        queued_payload["task_contract"]["workspace_change"],
        serde_json::to_value(WorkspaceChangeRequirement::Required).expect("enum")
    );
    assert_eq!(
        queued_payload["task_contract"]["required_paths"],
        json!(["queued.txt"])
    );
    assert_eq!(
        queued_payload["task_contract"]["require_objective_validation"],
        json!(true)
    );

    transport
        .send_command(runtime_command(
            session_id,
            SessionCommandKind::Abort,
            json!({}),
        ))
        .await
        .expect("release blocking task");
    if let Some(control) = transport
        .host
        .execution
        .task_controls
        .lock()
        .await
        .get(&session_id)
    {
        control.abort_handle.abort();
    }
    sleep(Duration::from_millis(100)).await;
}

#[tokio::test]
async fn open_turn_keeps_contract_open_and_skips_project_verifier_discovery() {
    let workspace = tempdir().expect("workspace");
    fs::write(
        workspace.path().join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\n",
    )
    .expect("cargo manifest");
    let _provider = IsolatedGlobalMockProvider::install().await;
    let transport = EmbeddedTransport::for_cwd(workspace.path())
        .await
        .expect("transport");
    let session_id = transport.default_session_id();

    transport
        .send_command(command(session_id, "sleep"))
        .await
        .expect("blocking task");
    wait_for_status(&transport, session_id, TaskStatus::WaitingApproval).await;

    let queued = transport
        .send_command(command_with_payload(
            session_id,
            json!({
                "prompt": "inspect the workspace and summarize it",
                "execution_mode": "open",
            }),
        ))
        .await
        .expect("queued task");
    assert!(queued.accepted);
    let steered = transport
        .send_command(command_with_payload(
            session_id,
            json!({
                "prompt": "fix src/lib.rs and focus on the public API",
                "steer": true,
            }),
        ))
        .await
        .expect("steering task");
    assert!(steered.accepted);
    let mode_override = transport
        .send_command(command_with_payload(
            session_id,
            json!({
                "prompt": "change completion mode",
                "execution_mode": "open",
                "steer": true,
            }),
        ))
        .await
        .expect("mode override response");
    assert!(!mode_override.accepted);
    assert!(
        mode_override
            .reason
            .as_deref()
            .is_some_and(|reason| reason.contains("only tool_profile"))
    );
    let profiled_steer = transport
        .send_command(command_with_payload(
            session_id,
            json!({
                "prompt": "use the full tool surface",
                "tool_profile": "full",
                "steer": true,
            }),
        ))
        .await
        .expect("profile steering task");
    assert!(profiled_steer.accepted);

    let events = transport
        .host
        .storage
        .repositories
        .events
        .load(session_id, None, None)
        .await
        .expect("events");
    let queued_payload = events
        .iter()
        .find(|event| event.event_type == RuntimeEventType::TurnQueued)
        .and_then(|event| event.payload.get("payload"))
        .expect("queued payload");

    assert_eq!(queued_payload["_task_contract_origin"], "open");
    assert_eq!(queued_payload["_execution_mode"], "open");
    assert_eq!(queued_payload["tool_profile"], "coding");
    assert_eq!(queued_payload["external_verifiers"], json!([]));
    assert_eq!(
        queued_payload["task_contract"]["workspace_change"],
        "optional"
    );
    assert_eq!(
        queued_payload["task_contract"]["require_objective_validation"],
        false
    );
    let steering_payloads = events
        .iter()
        .filter(|event| event.event_type == RuntimeEventType::TurnQueued)
        .filter_map(|event| event.payload.get("payload"))
        .filter(|payload| payload.get("steer").and_then(Value::as_bool) == Some(true))
        .collect::<Vec<_>>();
    assert_eq!(steering_payloads.len(), 2);
    let steering_payload = steering_payloads[0];
    assert_eq!(steering_payload["_task_contract_origin"], "active_task");
    assert!(steering_payload.get("execution_mode").is_none());
    assert!(steering_payload.get("_execution_mode").is_none());
    assert!(steering_payload.get("tool_profile").is_none());
    assert!(steering_payload.get("task_contract").is_none());
    assert!(steering_payload.get("external_verifiers").is_none());
    assert!(steering_payload.get("max_elapsed_ms").is_none());
    assert_eq!(steering_payloads[1]["tool_profile"], "full");

    transport
        .send_command(runtime_command(
            session_id,
            SessionCommandKind::Abort,
            json!({}),
        ))
        .await
        .expect("release blocking task");
    if let Some(control) = transport
        .host
        .execution
        .task_controls
        .lock()
        .await
        .get(&session_id)
    {
        control.abort_handle.abort();
    }
    sleep(Duration::from_millis(100)).await;
}

#[tokio::test]
async fn verify_on_change_auto_preserves_contract_and_discovers_one_project_verifier() {
    let workspace = tempdir().expect("workspace");
    fs::write(
        workspace.path().join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\n",
    )
    .expect("cargo manifest");
    let _provider = IsolatedGlobalMockProvider::install().await;
    let transport = EmbeddedTransport::for_cwd(workspace.path())
        .await
        .expect("transport");
    let session_id = transport.default_session_id();

    transport
        .send_command(command(session_id, "sleep"))
        .await
        .expect("blocking task");
    wait_for_status(&transport, session_id, TaskStatus::WaitingApproval).await;

    let queued = transport
        .send_command(command_with_payload(
            session_id,
            json!({
                "prompt": "write file queued.txt with content queued",
                "execution_mode": "open",
                "verify_on_change": "auto",
            }),
        ))
        .await
        .expect("queued task");
    assert!(queued.accepted);
    let events = transport
        .host
        .storage
        .repositories
        .events
        .load(session_id, None, None)
        .await
        .expect("events");
    let payload = events
        .iter()
        .find(|event| event.event_type == RuntimeEventType::TurnQueued)
        .and_then(|event| event.payload.get("payload"))
        .expect("queued payload");
    assert_eq!(payload[VERIFY_ON_CHANGE_KEY], "auto");
    assert_eq!(payload["_task_contract_origin"], "verify_on_change");
    assert_eq!(
        payload["task_contract"]["workspace_change"],
        serde_json::to_value(WorkspaceChangeRequirement::Required).expect("enum")
    );
    assert_eq!(
        payload["task_contract"]["require_objective_validation"],
        true
    );
    assert_eq!(
        payload["external_verifiers"].as_array().map(Vec::len),
        Some(1)
    );

    transport
        .send_command(runtime_command(
            session_id,
            SessionCommandKind::Abort,
            json!({}),
        ))
        .await
        .expect("release blocking task");
    if let Some(control) = transport
        .host
        .execution
        .task_controls
        .lock()
        .await
        .get(&session_id)
    {
        control.abort_handle.abort();
    }
    sleep(Duration::from_millis(100)).await;
}

#[tokio::test]
async fn steering_without_an_active_task_is_rejected() {
    let workspace = tempdir().expect("workspace");
    let _provider = IsolatedGlobalMockProvider::install().await;
    let transport = EmbeddedTransport::for_cwd(workspace.path())
        .await
        .expect("transport");
    let session_id = transport.default_session_id();

    let ack = transport
        .send_command(command_with_payload(
            session_id,
            json!({
                "prompt": "continue the active task",
                "steer": true,
            }),
        ))
        .await
        .expect("steering response");

    assert!(!ack.accepted);
    assert_eq!(
        ack.reason.as_deref(),
        Some("steering requires an active runtime task")
    );
}

#[tokio::test]
async fn leading_steer_recovery_materializes_the_active_execution_surface() {
    let workspace = tempdir().expect("workspace");
    let _provider = IsolatedGlobalMockProvider::install().await;
    let transport = EmbeddedTransport::for_cwd(workspace.path())
        .await
        .expect("transport");
    let session_id = transport.default_session_id();
    let task_contract = TaskContract {
        completion_criteria: vec!["preserve the active task contract".to_owned()],
        ..TaskContract::default()
    };
    let output_schema = json!({
        "type": "object",
        "required": ["result"],
        "properties": {"result": {"type": "string"}}
    });

    transport
        .send_command(command_with_payload(
            session_id,
            json!({
                "prompt": "sleep",
                "execution_mode": "open",
                "tool_profile": "coding",
                "task_contract": task_contract,
                "output_schema": output_schema,
                "max_elapsed_ms": 120_000,
                "defer_external_verification": true,
            }),
        ))
        .await
        .expect("blocking task");
    wait_for_status(&transport, session_id, TaskStatus::WaitingApproval).await;

    let steer = transport
        .send_command(command_with_payload(
            session_id,
            json!({
                "prompt": "focus on the public API",
                "steer": true,
            }),
        ))
        .await
        .expect("steering task");
    assert!(steer.accepted);
    let events = transport
        .host
        .storage
        .repositories
        .events
        .load(session_id, None, None)
        .await
        .expect("events");
    let active_task_id = events
        .iter()
        .find(|event| {
            event.event_type == RuntimeEventType::TurnQueued
                && event
                    .payload
                    .pointer("/payload/steer")
                    .and_then(Value::as_bool)
                    == Some(true)
        })
        .and_then(|event| event.task_id)
        .expect("active task id");

    let recovered = transport
        .host
        .recoverable_pending_turns(session_id, Some(active_task_id))
        .await
        .expect("recoverable steer");
    assert_eq!(recovered.len(), 1);
    assert!(recovered[0].pending.steer);
    assert_eq!(recovered[0].pending.task_contract, None);
    assert_eq!(recovered[0].payload["execution_mode"], "open");
    assert_eq!(recovered[0].payload["_execution_mode"], "open");
    assert_eq!(recovered[0].payload["tool_profile"], "coding");
    assert_eq!(
        recovered[0].payload["task_contract"]["completion_criteria"],
        json!(["preserve the active task contract"])
    );
    assert_eq!(recovered[0].payload["output_schema"], output_schema);
    assert!(
        recovered[0].payload["max_elapsed_ms"]
            .as_u64()
            .is_some_and(|remaining| (1..=120_000).contains(&remaining))
    );
    assert_eq!(recovered[0].payload["defer_external_verification"], true);

    transport
        .send_command(runtime_command(
            session_id,
            SessionCommandKind::Abort,
            json!({}),
        ))
        .await
        .expect("release blocking task");
}

#[tokio::test]
async fn leading_steer_recovery_uses_the_latest_started_turn_and_remaining_budget() {
    let host = RuntimeHost::in_memory().await.expect("host");
    let session_id = host.default_session_id();
    let task_id = TaskId::new();
    let initial_turn_id = TurnId::new();
    let active_turn_id = TurnId::new();
    let pending_steer_id = TurnId::new();

    let mut created = host_event(
        host.next_sequence_no(),
        session_id,
        Some(task_id),
        RuntimeEventType::TaskCreated,
        RuntimeEventSource::Runtime,
        json!({
            "payload": {
                "prompt": "initial turn",
                "execution_mode": "open",
                "tool_profile": "coding",
                "task_contract": TaskContract::conversational(vec!["initial".to_owned()]),
                "max_elapsed_ms": 120_000,
            }
        }),
    );
    created.turn_id = Some(initial_turn_id);
    host.record_event(created).await.expect("initial turn");

    let mut queued = host_event(
        host.next_sequence_no(),
        session_id,
        Some(task_id),
        RuntimeEventType::TurnQueued,
        RuntimeEventSource::User,
        json!({
            "command_id": CommandId::new(),
            "payload": {
                "prompt": "strict active turn",
                "execution_mode": "strict",
                "tool_profile": "full",
                "task_contract": TaskContract::conversational(vec!["latest".to_owned()]),
                "max_elapsed_ms": 5_000,
                "allow_network": false,
                "yolo": false,
            }
        }),
    );
    queued.turn_id = Some(active_turn_id);
    host.record_event(queued).await.expect("queued active turn");

    let mut started = host_event(
        host.next_sequence_no(),
        session_id,
        Some(task_id),
        RuntimeEventType::TurnStarted,
        RuntimeEventSource::User,
        json!({"summary": "queued user turn started"}),
    );
    started.turn_id = Some(active_turn_id);
    let budget_started_at = chrono::Utc::now() - chrono::Duration::milliseconds(1_000);
    started.timestamp = budget_started_at;
    host.record_event(started).await.expect("active turn start");
    let mut step_started = host_event(
        host.next_sequence_no(),
        session_id,
        Some(task_id),
        RuntimeEventType::StepStarted,
        RuntimeEventSource::Runtime,
        json!({"summary": "runtime step 0 started"}),
    );
    step_started.turn_id = Some(active_turn_id);
    step_started.timestamp = budget_started_at;
    host.record_event(step_started)
        .await
        .expect("runtime budget start");

    let mut steer = host_event(
        host.next_sequence_no(),
        session_id,
        Some(task_id),
        RuntimeEventType::TurnQueued,
        RuntimeEventSource::User,
        json!({
            "command_id": CommandId::new(),
            "payload": {
                "prompt": "continue with the current contract",
                "steer": true,
                "allow_network": false,
                "yolo": false,
            }
        }),
    );
    steer.turn_id = Some(pending_steer_id);
    host.record_event(steer).await.expect("pending steer");

    let recovered = host
        .recoverable_pending_turns(session_id, Some(task_id))
        .await
        .expect("recovered turns");
    assert_eq!(recovered.len(), 1);
    assert_eq!(recovered[0].payload["execution_mode"], "strict");
    assert_eq!(recovered[0].payload["tool_profile"], "full");
    assert_eq!(
        recovered[0].payload["task_contract"]["completion_criteria"],
        json!(["latest"])
    );
    assert!(
        recovered[0].payload["max_elapsed_ms"]
            .as_u64()
            .is_some_and(|remaining| (3_000..=4_000).contains(&remaining)),
        "recovery must deduct time already spent by the active turn"
    );
}

#[tokio::test]
async fn leading_steer_recovery_does_not_charge_pre_runtime_admission_time() {
    let host = RuntimeHost::in_memory().await.expect("host");
    let session_id = host.default_session_id();
    let task_id = TaskId::new();
    let active_turn_id = TurnId::new();
    let steer_turn_id = TurnId::new();
    let mut created = host_event(
        host.next_sequence_no(),
        session_id,
        Some(task_id),
        RuntimeEventType::TaskCreated,
        RuntimeEventSource::Runtime,
        json!({
            "payload": {
                "prompt": "waiting for provider admission",
                "execution_mode": "open",
                "tool_profile": "coding",
                "task_contract": TaskContract::conversational(vec!["inspect".to_owned()]),
                "max_elapsed_ms": 5_000,
                "allow_network": false,
                "yolo": false,
            }
        }),
    );
    created.turn_id = Some(active_turn_id);
    created.timestamp = chrono::Utc::now() - chrono::Duration::minutes(10);
    host.record_event(created).await.expect("active task");
    let mut steer = host_event(
        host.next_sequence_no(),
        session_id,
        Some(task_id),
        RuntimeEventType::TurnQueued,
        RuntimeEventSource::User,
        json!({
            "command_id": CommandId::new(),
            "payload": {
                "prompt": "continue after admission",
                "steer": true,
                "allow_network": false,
                "yolo": false,
            }
        }),
    );
    steer.turn_id = Some(steer_turn_id);
    host.record_event(steer).await.expect("pending steer");

    let recovered = host
        .recoverable_pending_turns(session_id, Some(task_id))
        .await
        .expect("recovered steer");
    assert_eq!(recovered[0].payload["max_elapsed_ms"], 5_000);
}

#[tokio::test]
async fn recovery_transfer_preserves_the_surface_after_a_later_turn_started() {
    let host = RuntimeHost::in_memory().await.expect("host");
    let session_id = host.default_session_id();
    let task_id = TaskId::new();
    let first_turn_id = TurnId::new();
    let second_turn_id = TurnId::new();
    let steer_turn_id = TurnId::new();
    let first_payload = json!({
        "prompt": "first recovered turn",
        "execution_mode": "open",
        "tool_profile": "coding",
        "task_contract": TaskContract::conversational(vec!["first".to_owned()]),
        "allow_network": false,
        "yolo": false,
    });
    let second_payload = json!({
        "prompt": "second recovered turn",
        "execution_mode": "strict",
        "tool_profile": "full",
        "task_contract": TaskContract::conversational(vec!["second".to_owned()]),
        "allow_network": false,
        "yolo": false,
    });
    host.record_event(host_event(
        host.next_sequence_no(),
        session_id,
        Some(task_id),
        RuntimeEventType::TurnQueued,
        RuntimeEventSource::Runtime,
        json!({
            "recovery": "durable_pending_turn_batch",
            "recovered_pending_turns": [
                {
                    "sequence_no": 1,
                    "turn_id": first_turn_id,
                    "command_id": CommandId::new(),
                    "actor": {"kind": "runtime", "id": "recovery-test"},
                    "payload": first_payload,
                },
                {
                    "sequence_no": 2,
                    "turn_id": second_turn_id,
                    "command_id": CommandId::new(),
                    "actor": {"kind": "runtime", "id": "recovery-test"},
                    "payload": second_payload,
                },
            ],
        }),
    ))
    .await
    .expect("transfer event");
    let mut first_started = host_event(
        host.next_sequence_no(),
        session_id,
        Some(task_id),
        RuntimeEventType::TurnStarted,
        RuntimeEventSource::Runtime,
        json!({
            "recovery": "durable_pending_turn",
            "payload": first_payload,
        }),
    );
    first_started.turn_id = Some(first_turn_id);
    host.record_event(first_started)
        .await
        .expect("first recovered turn");
    let mut second_started = host_event(
        host.next_sequence_no(),
        session_id,
        Some(task_id),
        RuntimeEventType::TurnStarted,
        RuntimeEventSource::User,
        json!({"summary": "queued user turn started"}),
    );
    second_started.turn_id = Some(second_turn_id);
    host.record_event(second_started)
        .await
        .expect("second recovered turn");
    let mut steer = host_event(
        host.next_sequence_no(),
        session_id,
        Some(task_id),
        RuntimeEventType::TurnQueued,
        RuntimeEventSource::User,
        json!({
            "command_id": CommandId::new(),
            "payload": {
                "prompt": "continue the second recovered turn",
                "steer": true,
                "allow_network": false,
                "yolo": false,
            },
        }),
    );
    steer.turn_id = Some(steer_turn_id);
    host.record_event(steer).await.expect("steering turn");

    let recovered = host
        .recoverable_pending_turns(session_id, Some(task_id))
        .await
        .expect("recoverable steer");
    assert_eq!(recovered.len(), 1);
    assert_eq!(recovered[0].payload["execution_mode"], "strict");
    assert_eq!(recovered[0].payload["tool_profile"], "full");
    assert_eq!(
        recovered[0].payload["task_contract"]["completion_criteria"],
        json!(["second"])
    );
    assert!(
        recovered[0].payload["max_elapsed_ms"]
            .as_u64()
            .is_some_and(|remaining| (1..=default_agent_max_elapsed_ms()).contains(&remaining))
    );
}

#[tokio::test]
async fn recovery_transfer_carries_cumulative_governor_and_delegation_state() {
    let host = RuntimeHost::in_memory().await.expect("host");
    let session_id = host.default_session_id();
    let task_id = TaskId::new();
    let turn_id = TurnId::new();
    let captured_at = chrono::Utc::now() - chrono::Duration::milliseconds(25);
    let delegation = delegation_policy::TimedDelegationRecoveryState {
        captured_at,
        state: delegation_policy::DelegationRecoveryState {
            root_session_id: session_id,
            parent_session_id: None,
            parent_task_id: None,
            parent_thread_id: None,
            depth: 0,
            remaining_elapsed_ms: 10_000,
            local_remaining_elapsed_ms: None,
            max_tokens: delegation_policy::MIN_DELEGATED_TOKEN_BUDGET,
            max_cost_microusd: Some(500),
            started_children: 2,
            spent_tokens: 7_000,
            spent_cost_microusd: 125,
        },
    };
    let continuation = RecoveredTaskContinuation {
        governor_usage: AgentGovernorUsage {
            iterations: 9,
            tool_calls: 12,
            failed_tool_calls: 3,
            consecutive_failed_tool_calls: 1,
            estimated_cost_microusd: Some(321),
        },
        accounted_cost_response_ids: Vec::new(),
        delegation: Some(delegation),
    };
    let actor = Actor {
        kind: ActorKind::Runtime,
        id: "recovery-continuation".to_owned(),
    };
    host.record_event(host_event(
        host.next_sequence_no(),
        session_id,
        Some(task_id),
        RuntimeEventType::TurnQueued,
        RuntimeEventSource::Runtime,
        json!({
            "recovery": "durable_pending_turn_batch",
            RECOVERED_PENDING_TURNS_KEY: [{
                "sequence_no": 1,
                "turn_id": turn_id,
                "command_id": CommandId::new(),
                "actor": actor,
                "payload": {
                    "prompt": "continue after recovery",
                    "execution_mode": "open",
                    "tool_profile": "coding",
                    "allow_network": false,
                    "yolo": false,
                },
            }],
            RECOVERY_CONTINUATION_KEY: continuation,
        }),
    ))
    .await
    .expect("transfer event");

    let recovered = host
        .recoverable_pending_turns(session_id, Some(task_id))
        .await
        .expect("recovered continuation");
    let continuation = recovered[0]
        .continuation
        .as_ref()
        .expect("continuation state");
    assert_eq!(continuation.governor_usage.tool_calls, 12);
    assert_eq!(
        continuation.governor_usage.estimated_cost_microusd,
        Some(321)
    );
    let delegation = continuation.delegation.as_ref().expect("delegation state");
    assert_eq!(delegation.state.started_children, 2);
    assert_eq!(delegation.state.spent_cost_microusd, 125);
    assert!(delegation.state.remaining_elapsed_ms < 10_000);
}

fn governor_recovery_transfer_event(
    sequence_no: u64,
    session_id: SessionId,
    task_id: TaskId,
    governor_usage: AgentGovernorUsage,
    accounted_cost_response_ids: Vec<String>,
) -> RuntimeEvent {
    host_event(
        sequence_no,
        session_id,
        Some(task_id),
        RuntimeEventType::TurnQueued,
        RuntimeEventSource::Runtime,
        json!({
            "recovered_pending_sequence_nos": [],
            RECOVERY_CONTINUATION_KEY: RecoveredTaskContinuation {
                governor_usage,
                accounted_cost_response_ids,
                delegation: None,
            },
        }),
    )
}

fn recovered_governor(events: &[RuntimeEvent]) -> RecoveredTaskContinuation {
    recovered_task_continuation(events, chrono::Utc::now())
        .expect("valid recovery facts")
        .expect("governor continuation")
}

#[test]
fn recovered_governor_cumulative_counters_do_not_regress_on_stale_facts() {
    let session_id = SessionId::new();
    let task_id = TaskId::new();
    let current = host_event(
        1,
        session_id,
        Some(task_id),
        RuntimeEventType::GovernorDecided,
        RuntimeEventSource::Governor,
        json!({
            "record": {
                "iteration": 9,
                "tool_calls": 12,
                "failed_tool_calls": 4,
                "consecutive_failed_tool_calls": 2,
            }
        }),
    );
    let stale_transfer = governor_recovery_transfer_event(
        2,
        session_id,
        task_id,
        AgentGovernorUsage {
            iterations: 3,
            tool_calls: 5,
            failed_tool_calls: 1,
            consecutive_failed_tool_calls: 1,
            estimated_cost_microusd: None,
        },
        Vec::new(),
    );
    let stale_governor = host_event(
        3,
        session_id,
        Some(task_id),
        RuntimeEventType::GovernorDecided,
        RuntimeEventSource::Governor,
        json!({
            "record": {
                "iteration": 4,
                "tool_calls": 6,
                "failed_tool_calls": 2,
                "consecutive_failed_tool_calls": 1,
            }
        }),
    );

    let usage = recovered_governor(&[current, stale_transfer, stale_governor]).governor_usage;
    assert_eq!(usage.iterations, 9);
    assert_eq!(usage.tool_calls, 12);
    assert_eq!(usage.failed_tool_calls, 4);
}

#[test]
fn recovered_governor_does_not_recharge_usage_accounted_by_transfer() {
    let session_id = SessionId::new();
    let task_id = TaskId::new();
    let turn_id = TurnId::new();
    let response_id = golutra_core::ProviderResponseId::new();
    let transfer = governor_recovery_transfer_event(
        1,
        session_id,
        task_id,
        AgentGovernorUsage {
            estimated_cost_microusd: Some(1_000),
            ..AgentGovernorUsage::default()
        },
        vec![response_id.to_string()],
    );
    let usage = TokenUsageRecord {
        task_id,
        turn_id,
        provider_id: "test-provider".to_owned(),
        model_id: "test-model".to_owned(),
        request_event_id: golutra_core::ProviderRequestId::new(),
        response_event_id: response_id,
        input_tokens: Some(10),
        output_tokens: Some(5),
        reasoning_tokens: None,
        cached_input_tokens: None,
        tool_result_tokens: None,
        total_tokens: Some(15),
        estimated_cost: Some(0.001),
        budget_snapshot_ref: golutra_core::TokenBudgetSnapshotId::new(),
        attribution_ref: None,
        usage_source: "provider".to_owned(),
    };
    let old_usage = host_event(
        2,
        session_id,
        Some(task_id),
        RuntimeEventType::TokenUsageRecorded,
        RuntimeEventSource::Provider,
        json!({"record": usage}),
    );

    let continuation = recovered_governor(&[transfer, old_usage]);
    assert_eq!(
        continuation.governor_usage.estimated_cost_microusd,
        Some(1_000)
    );
    assert_eq!(
        continuation.accounted_cost_response_ids,
        vec![response_id.to_string()]
    );
}

#[test]
fn recovered_governor_counts_step_started_before_governor_decision() {
    let session_id = SessionId::new();
    let task_id = TaskId::new();
    let turn_id = TurnId::new();
    let transfer = governor_recovery_transfer_event(
        1,
        session_id,
        task_id,
        AgentGovernorUsage {
            iterations: 4,
            ..AgentGovernorUsage::default()
        },
        Vec::new(),
    );
    let mut step_started = host_event(
        2,
        session_id,
        Some(task_id),
        RuntimeEventType::StepStarted,
        RuntimeEventSource::Runtime,
        json!({"step": {"step_no": 0, "turn_id": turn_id}}),
    );
    step_started.turn_id = Some(turn_id);

    assert_eq!(
        recovered_governor(&[transfer, step_started])
            .governor_usage
            .iterations,
        5
    );
}

#[test]
fn recovered_governor_counts_failed_model_tool_before_governor_decision() {
    let session_id = SessionId::new();
    let task_id = TaskId::new();
    let tool_call_id = ToolCallId::new();
    let transfer = governor_recovery_transfer_event(
        1,
        session_id,
        task_id,
        AgentGovernorUsage {
            tool_calls: 2,
            failed_tool_calls: 1,
            consecutive_failed_tool_calls: 1,
            ..AgentGovernorUsage::default()
        },
        Vec::new(),
    );
    let started = host_event(
        2,
        session_id,
        Some(task_id),
        RuntimeEventType::ToolStarted,
        RuntimeEventSource::Tool,
        json!({
            "tool_call_id": tool_call_id,
            "provider_tool_call_id": "provider-call-1",
            "tool_name": "read_file",
        }),
    );
    let failed = host_event(
        3,
        session_id,
        Some(task_id),
        RuntimeEventType::ToolCompleted,
        RuntimeEventSource::Tool,
        json!({
            "envelope": {
                "tool_call_id": tool_call_id,
                "status": "error",
            }
        }),
    );

    let usage = recovered_governor(&[transfer, started, failed]).governor_usage;
    assert_eq!(usage.tool_calls, 3);
    assert_eq!(usage.failed_tool_calls, 2);
    assert_eq!(usage.consecutive_failed_tool_calls, 2);
}

#[test]
fn recovered_governor_successful_model_tool_resets_consecutive_failures() {
    let session_id = SessionId::new();
    let task_id = TaskId::new();
    let tool_call_id = ToolCallId::new();
    let transfer = governor_recovery_transfer_event(
        1,
        session_id,
        task_id,
        AgentGovernorUsage {
            tool_calls: 4,
            failed_tool_calls: 2,
            consecutive_failed_tool_calls: 2,
            ..AgentGovernorUsage::default()
        },
        Vec::new(),
    );
    let started = host_event(
        2,
        session_id,
        Some(task_id),
        RuntimeEventType::ToolStarted,
        RuntimeEventSource::Tool,
        json!({
            "tool_call_id": tool_call_id,
            "provider_tool_call_id": "provider-call-2",
            "tool_name": "read_file",
        }),
    );
    let succeeded = host_event(
        3,
        session_id,
        Some(task_id),
        RuntimeEventType::ToolCompleted,
        RuntimeEventSource::Tool,
        json!({
            "envelope": {
                "tool_call_id": tool_call_id,
                "status": "ok",
            }
        }),
    );

    let usage = recovered_governor(&[transfer, started, succeeded]).governor_usage;
    assert_eq!(usage.tool_calls, 5);
    assert_eq!(usage.failed_tool_calls, 2);
    assert_eq!(usage.consecutive_failed_tool_calls, 0);
}

#[test]
fn recovered_governor_ignores_internal_verifier_tools() {
    let session_id = SessionId::new();
    let task_id = TaskId::new();
    let failed_verifier_id = ToolCallId::new();
    let successful_verifier_id = ToolCallId::new();
    let baseline = AgentGovernorUsage {
        iterations: 3,
        tool_calls: 6,
        failed_tool_calls: 2,
        consecutive_failed_tool_calls: 2,
        estimated_cost_microusd: Some(500),
    };
    let transfer = governor_recovery_transfer_event(1, session_id, task_id, baseline, Vec::new());
    let events = [
        transfer,
        host_event(
            2,
            session_id,
            Some(task_id),
            RuntimeEventType::ToolStarted,
            RuntimeEventSource::Tool,
            json!({
                "tool_call_id": failed_verifier_id,
                "provider_tool_call_id": null,
                "tool_name": "external_verifier",
            }),
        ),
        host_event(
            3,
            session_id,
            Some(task_id),
            RuntimeEventType::ToolCompleted,
            RuntimeEventSource::Tool,
            json!({
                "envelope": {
                    "tool_call_id": failed_verifier_id,
                    "status": "error",
                }
            }),
        ),
        host_event(
            4,
            session_id,
            Some(task_id),
            RuntimeEventType::ToolStarted,
            RuntimeEventSource::Tool,
            json!({
                "tool_call_id": successful_verifier_id,
                "provider_tool_call_id": null,
                "tool_name": "contract_path_verifier",
            }),
        ),
        host_event(
            5,
            session_id,
            Some(task_id),
            RuntimeEventType::ToolCompleted,
            RuntimeEventSource::Tool,
            json!({
                "envelope": {
                    "tool_call_id": successful_verifier_id,
                    "status": "ok",
                }
            }),
        ),
    ];

    assert_eq!(recovered_governor(&events).governor_usage, baseline);
}

#[tokio::test]
async fn inline_recovery_payload_wins_over_its_stale_source_reference() {
    let host = RuntimeHost::in_memory().await.expect("host");
    let session_id = host.default_session_id();
    let source_task_id = TaskId::new();
    let recovery_task_id = TaskId::new();
    let turn_id = TurnId::new();
    let command_id = CommandId::new();
    let actor = Actor {
        kind: ActorKind::Runtime,
        id: "inline-recovery".to_owned(),
    };
    let mut source = host_event(
        host.next_sequence_no(),
        session_id,
        Some(source_task_id),
        RuntimeEventType::TurnQueued,
        RuntimeEventSource::User,
        json!({
            "command_id": command_id,
            "payload": {"prompt": "stale prompt"},
        }),
    );
    source.turn_id = Some(turn_id);
    let source_sequence_no = source.sequence_no;
    host.record_event(source).await.expect("source turn");
    let transfer = host_event(
        host.next_sequence_no(),
        session_id,
        Some(recovery_task_id),
        RuntimeEventType::TurnQueued,
        RuntimeEventSource::Runtime,
        json!({
            "recovered_pending_sequence_nos": [source_sequence_no],
            RECOVERED_PENDING_TURNS_KEY: [{
                "sequence_no": source_sequence_no,
                "turn_id": turn_id,
                "command_id": command_id,
                "actor": actor,
                "payload": {"prompt": "updated prompt"},
            }],
        }),
    );

    let recovered = recoverable_transfer_turns(&host, session_id, &[transfer])
        .await
        .expect("transferred turns");
    assert_eq!(recovered[&turn_id].payload["prompt"], "updated prompt");
}

#[tokio::test]
async fn materialized_leading_steer_survives_a_crash_before_turn_started() {
    let host = RuntimeHost::in_memory().await.expect("host");
    let session_id = host.default_session_id();
    let task_id = TaskId::new();
    let turn_id = TurnId::new();
    host.record_event(host_event(
        host.next_sequence_no(),
        session_id,
        Some(task_id),
        RuntimeEventType::TurnQueued,
        RuntimeEventSource::Runtime,
        json!({
            "recovery": "durable_pending_turn_batch",
            RECOVERED_PENDING_TURNS_KEY: [{
                "sequence_no": 1,
                "turn_id": turn_id,
                "command_id": CommandId::new(),
                "actor": {"kind": "runtime", "id": "recovery-test"},
                "payload": {
                    "prompt": "materialized steer",
                    "steer": true,
                    "execution_mode": "open",
                    "_execution_mode": "open",
                    "tool_profile": "coding",
                    "task_contract": TaskContract::conversational(vec!["continue".to_owned()]),
                    "_task_contract_origin": "active_task",
                    "max_elapsed_ms": 5_000,
                    "allow_network": false,
                    "yolo": false,
                },
            }],
        }),
    ))
    .await
    .expect("transfer event");

    let recovered = host
        .recoverable_pending_turns(session_id, Some(task_id))
        .await
        .expect("materialized steer recovery");
    assert_eq!(recovered.len(), 1);
    assert!(recovered[0].pending.steer);
    assert_eq!(recovered[0].payload["prompt"], "materialized steer");
}

#[test]
fn recovery_transfer_does_not_get_overwritten_by_stale_restarted_turn_metadata() {
    let session_id = SessionId::new();
    let task_id = TaskId::new();
    let captured_at = chrono::Utc::now() - chrono::Duration::milliseconds(10);
    let continuation = RecoveredTaskContinuation {
        governor_usage: AgentGovernorUsage::default(),
        accounted_cost_response_ids: Vec::new(),
        delegation: Some(delegation_policy::TimedDelegationRecoveryState {
            captured_at,
            state: delegation_policy::DelegationRecoveryState {
                root_session_id: session_id,
                parent_session_id: None,
                parent_task_id: None,
                parent_thread_id: None,
                depth: 0,
                remaining_elapsed_ms: 10_000,
                local_remaining_elapsed_ms: None,
                max_tokens: delegation_policy::MIN_DELEGATED_TOKEN_BUDGET,
                max_cost_microusd: Some(500),
                started_children: 3,
                spent_tokens: 8_000,
                spent_cost_microusd: 240,
            },
        }),
    };
    let stale_metadata = json!({
        "root_session_id": session_id,
        "parent_session_id": null,
        "parent_task_id": null,
        "parent_thread_id": null,
        "depth": 0,
        "budget": {
            "remaining_elapsed_ms": 30_000,
            "max_tokens": delegation_policy::MIN_DELEGATED_TOKEN_BUDGET,
            "max_cost_microusd": 500,
            "started_children": 0,
            "spent_tokens": 0,
            "reserved_tokens": 0,
            "spent_cost_microusd": 0,
            "reserved_cost_microusd": 0,
        },
    });
    let transfer = host_event(
        1,
        session_id,
        Some(task_id),
        RuntimeEventType::TurnQueued,
        RuntimeEventSource::Runtime,
        json!({
            "recovered_pending_sequence_nos": [],
            RECOVERY_CONTINUATION_KEY: continuation,
        }),
    );
    let restarted = host_event(
        2,
        session_id,
        Some(task_id),
        RuntimeEventType::TurnStarted,
        RuntimeEventSource::Runtime,
        json!({
            "recovery": "durable_pending_turn",
            "payload": {"_delegation": stale_metadata},
        }),
    );

    let recovered = recovered_task_continuation(&[transfer, restarted], chrono::Utc::now())
        .expect("recovery continuation");
    let delegation = recovered
        .and_then(|continuation| continuation.delegation)
        .expect("delegation continuation");
    assert_eq!(delegation.state.started_children, 3);
    assert_eq!(delegation.state.spent_cost_microusd, 240);
}

fn canonical_delegation_recovery_state(
    root_session_id: SessionId,
    max_tokens: u64,
    max_cost_microusd: Option<u64>,
    started_children: usize,
    spent_tokens: u64,
) -> delegation_policy::TimedDelegationRecoveryState {
    delegation_policy::TimedDelegationRecoveryState {
        captured_at: chrono::Utc::now(),
        state: delegation_policy::DelegationRecoveryState {
            root_session_id,
            parent_session_id: None,
            parent_task_id: None,
            parent_thread_id: None,
            depth: 0,
            remaining_elapsed_ms: 10_000,
            local_remaining_elapsed_ms: Some(10_000),
            max_tokens,
            max_cost_microusd,
            started_children,
            spent_tokens,
            spent_cost_microusd: 0,
        },
    }
}

fn delegation_recovery_checkpoint_event(
    sequence_no: u64,
    session_id: SessionId,
    task_id: TaskId,
    recovery: &delegation_policy::TimedDelegationRecoveryState,
) -> RuntimeEvent {
    host_event(
        sequence_no,
        session_id,
        Some(task_id),
        RuntimeEventType::CheckpointCreated,
        RuntimeEventSource::Runtime,
        json!({
            "recovery_kind": "delegation_budget",
            "delegation_recovery": recovery,
        }),
    )
}

#[test]
fn delegation_metadata_requires_numeric_reservation_fields() {
    let session_id = SessionId::new();
    let metadata = json!({
        "root_session_id": session_id,
        "parent_session_id": null,
        "parent_task_id": null,
        "parent_thread_id": null,
        "depth": 0,
        "budget": {
            "remaining_elapsed_ms": 10_000,
            "max_tokens": delegation_policy::MIN_DELEGATED_TOKEN_BUDGET,
            "max_cost_microusd": null,
            "active_children": 0,
            "started_children": 0,
            "spent_tokens": 0,
            "reserved_tokens": 0,
            "spent_cost_microusd": 0,
            "reserved_cost_microusd": 0,
        },
    });

    for key in [
        "active_children",
        "reserved_tokens",
        "reserved_cost_microusd",
    ] {
        let mut missing = metadata.clone();
        missing["budget"]
            .as_object_mut()
            .expect("budget object")
            .remove(key);
        let error = delegation_recovery_from_metadata(&missing, chrono::Utc::now())
            .expect_err("missing reservation field must fail closed");
        assert!(error.to_string().contains(key), "{key}: {error}");

        let mut malformed = metadata.clone();
        malformed["budget"][key] = json!("0");
        let error = delegation_recovery_from_metadata(&malformed, chrono::Utc::now())
            .expect_err("malformed reservation field must fail closed");
        assert!(error.to_string().contains(key), "{key}: {error}");
    }
}

#[test]
fn canonical_delegation_checkpoint_rejects_foreign_or_child_state() {
    let session_id = SessionId::new();
    let task_id = TaskId::new();
    let foreign = canonical_delegation_recovery_state(
        SessionId::new(),
        delegation_policy::MIN_DELEGATED_TOKEN_BUDGET,
        None,
        1,
        0,
    );
    let error = recovered_task_continuation(
        &[delegation_recovery_checkpoint_event(
            1, session_id, task_id, &foreign,
        )],
        chrono::Utc::now(),
    )
    .expect_err("foreign root checkpoint must be rejected");
    assert!(error.to_string().contains("does not match event session"));

    let transfer = host_event(
        1,
        session_id,
        Some(task_id),
        RuntimeEventType::TurnQueued,
        RuntimeEventSource::Runtime,
        json!({
            "recovered_pending_sequence_nos": [],
            RECOVERY_CONTINUATION_KEY: RecoveredTaskContinuation {
                governor_usage: AgentGovernorUsage::default(),
                accounted_cost_response_ids: Vec::new(),
                delegation: Some(foreign.clone()),
            },
        }),
    );
    let error = recovered_task_continuation(&[transfer], chrono::Utc::now())
        .expect_err("foreign root transfer state must be rejected");
    assert!(error.to_string().contains("does not match event session"));

    let mut child = canonical_delegation_recovery_state(
        session_id,
        delegation_policy::MIN_DELEGATED_TOKEN_BUDGET,
        None,
        1,
        0,
    );
    child.state.depth = 1;
    child.state.parent_session_id = Some(SessionId::new());
    let error = recovered_task_continuation(
        &[delegation_recovery_checkpoint_event(
            1, session_id, task_id, &child,
        )],
        chrono::Utc::now(),
    )
    .expect_err("child state in the canonical stream must be rejected");
    assert!(error.to_string().contains("not a canonical root state"));
}

#[test]
fn canonical_delegation_checkpoint_rejects_budget_or_child_count_drift() {
    let session_id = SessionId::new();
    let task_id = TaskId::new();
    let baseline = canonical_delegation_recovery_state(
        session_id,
        delegation_policy::MIN_DELEGATED_TOKEN_BUDGET,
        Some(1_000),
        2,
        100,
    );
    let mut changed_token_cap = baseline.clone();
    changed_token_cap.state.max_tokens += 1;
    let mut changed_cost_cap = baseline.clone();
    changed_cost_cap.state.max_cost_microusd = Some(999);
    let mut regressed_children = baseline.clone();
    regressed_children.state.started_children = 1;

    for (label, candidate) in [
        ("token cap", changed_token_cap),
        ("cost cap", changed_cost_cap),
        ("started children", regressed_children),
    ] {
        let events = [
            delegation_recovery_checkpoint_event(1, session_id, task_id, &baseline),
            delegation_recovery_checkpoint_event(2, session_id, task_id, &candidate),
        ];
        assert!(
            recovered_task_continuation(&events, chrono::Utc::now()).is_err(),
            "{label} drift must be rejected"
        );
    }
}

#[test]
fn canonical_delegation_checkpoint_allows_reservation_settlement_usage_drop() {
    let session_id = SessionId::new();
    let task_id = TaskId::new();
    let reservation = canonical_delegation_recovery_state(
        session_id,
        delegation_policy::MIN_DELEGATED_TOKEN_BUDGET,
        Some(1_000),
        1,
        delegation_policy::MIN_DELEGATED_TOKEN_BUDGET,
    );
    let mut settlement = reservation.clone();
    settlement.state.spent_tokens = 100;
    settlement.state.spent_cost_microusd = 10;
    let events = [
        delegation_recovery_checkpoint_event(1, session_id, task_id, &reservation),
        delegation_recovery_checkpoint_event(2, session_id, task_id, &settlement),
    ];

    let recovered = recovered_task_continuation(&events, chrono::Utc::now())
        .expect("settlement checkpoint")
        .and_then(|continuation| continuation.delegation)
        .expect("delegation state");
    assert_eq!(recovered.state.spent_tokens, 100);
    assert_eq!(recovered.state.spent_cost_microusd, 10);
}

#[test]
fn delegation_metadata_depth_is_decoded_from_top_level_and_legacy_budget_location() {
    let session_id = SessionId::new();
    let base_budget = json!({
        "remaining_elapsed_ms": 10_000,
        "max_tokens": delegation_policy::MIN_DELEGATED_TOKEN_BUDGET,
        "max_cost_microusd": null,
        "active_children": 1,
        "started_children": 1,
        "spent_tokens": 100,
        "reserved_tokens": 20,
        "spent_cost_microusd": 0,
        "reserved_cost_microusd": 0,
    });
    let current = json!({
        "root_session_id": session_id,
        "parent_session_id": null,
        "parent_task_id": null,
        "parent_thread_id": null,
        "depth": 1,
        "budget": base_budget,
    });
    let current =
        delegation_recovery_from_metadata(&current, chrono::Utc::now()).expect("current metadata");
    assert_eq!(current.state.depth, 1);
    assert_eq!(
        current.state.spent_tokens,
        delegation_policy::MIN_DELEGATED_TOKEN_BUDGET
    );

    let settled = json!({
        "root_session_id": session_id,
        "parent_session_id": null,
        "parent_task_id": null,
        "parent_thread_id": null,
        "depth": 1,
        "budget": {
            "remaining_elapsed_ms": 10_000,
            "max_tokens": delegation_policy::MIN_DELEGATED_TOKEN_BUDGET,
            "max_cost_microusd": null,
            "active_children": 0,
            "started_children": 1,
            "spent_tokens": 100,
            "reserved_tokens": 0,
            "spent_cost_microusd": 0,
            "reserved_cost_microusd": 0,
        },
    });
    let settled =
        delegation_recovery_from_metadata(&settled, chrono::Utc::now()).expect("settled metadata");
    assert_eq!(settled.state.spent_tokens, 100);

    let legacy = json!({
        "root_session_id": session_id,
        "parent_session_id": null,
        "parent_task_id": null,
        "parent_thread_id": null,
        "budget": {
            "depth": 2,
            "remaining_elapsed_ms": 10_000,
            "max_tokens": delegation_policy::MIN_DELEGATED_TOKEN_BUDGET,
            "max_cost_microusd": null,
            "active_children": 0,
            "started_children": 2,
            "spent_tokens": 200,
            "reserved_tokens": 0,
            "spent_cost_microusd": 0,
            "reserved_cost_microusd": 0,
        },
    });
    let legacy =
        delegation_recovery_from_metadata(&legacy, chrono::Utc::now()).expect("legacy metadata");
    assert_eq!(legacy.state.depth, 2);
    assert_eq!(legacy.state.started_children, 2);
}

#[tokio::test]
async fn discovered_project_verifier_is_normalized_as_independent_on_a_queued_turn() {
    let workspace = tempdir().expect("workspace");
    fs::write(
        workspace.path().join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\n",
    )
    .expect("cargo manifest");
    let _provider = IsolatedGlobalMockProvider::install().await;
    let transport = EmbeddedTransport::for_cwd(workspace.path())
        .await
        .expect("transport");
    let session_id = transport.default_session_id();

    transport
        .send_command(command(session_id, "sleep"))
        .await
        .expect("blocking task");
    wait_for_status(&transport, session_id, TaskStatus::WaitingApproval).await;

    let queued = transport
        .send_command(command(
            session_id,
            "write file queued.txt with content queued",
        ))
        .await
        .expect("queued task");
    assert!(queued.accepted);

    let events = transport
        .host
        .storage
        .repositories
        .events
        .load(session_id, None, None)
        .await
        .expect("events");
    let queued_payload = events
        .iter()
        .find(|event| event.event_type == RuntimeEventType::TurnQueued)
        .and_then(|event| event.payload.get("payload"))
        .expect("queued payload");

    assert_eq!(queued_payload["external_verifiers"][0]["program"], "cargo");
    assert_eq!(
        queued_payload[EXTERNAL_VERIFIERS_REQUIRE_OS_SANDBOX_KEY],
        true
    );
    assert_eq!(
        queued_payload["task_contract"]["verification"],
        "independent"
    );
    assert_eq!(
        queued_payload["task_contract"]["require_objective_validation"],
        true
    );

    transport
        .send_command(runtime_command(
            session_id,
            SessionCommandKind::Abort,
            json!({}),
        ))
        .await
        .expect("release blocking task");
    if let Some(control) = transport
        .host
        .execution
        .task_controls
        .lock()
        .await
        .get(&session_id)
    {
        control.abort_handle.abort();
    }
    sleep(Duration::from_millis(100)).await;
}

#[tokio::test]
async fn direct_protocol_can_disable_project_verifier_discovery() {
    let workspace = tempdir().expect("workspace");
    fs::write(
        workspace.path().join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\n",
    )
    .expect("cargo manifest");
    let _provider = IsolatedGlobalMockProvider::install().await;
    let transport = EmbeddedTransport::for_cwd(workspace.path())
        .await
        .expect("transport");
    let session_id = transport.default_session_id();

    transport
        .send_command(command(session_id, "sleep"))
        .await
        .expect("blocking task");
    wait_for_status(&transport, session_id, TaskStatus::WaitingApproval).await;

    let queued = transport
        .send_command(command_with_payload(
            session_id,
            json!({
                "prompt": "write file queued.txt with content queued",
                "discover_project_verifiers": false,
            }),
        ))
        .await
        .expect("queued task");
    assert!(queued.accepted);

    let events = transport
        .host
        .storage
        .repositories
        .events
        .load(session_id, None, None)
        .await
        .expect("events");
    let queued_payload = events
        .iter()
        .find(|event| event.event_type == RuntimeEventType::TurnQueued)
        .and_then(|event| event.payload.get("payload"))
        .expect("queued payload");

    assert_eq!(queued_payload["external_verifiers"], json!([]));
    assert_eq!(
        queued_payload[EXTERNAL_VERIFIERS_REQUIRE_OS_SANDBOX_KEY],
        false
    );

    transport
        .send_command(runtime_command(
            session_id,
            SessionCommandKind::Abort,
            json!({}),
        ))
        .await
        .expect("release blocking task");
    if let Some(control) = transport
        .host
        .execution
        .task_controls
        .lock()
        .await
        .get(&session_id)
    {
        control.abort_handle.abort();
    }
    sleep(Duration::from_millis(100)).await;
}

#[tokio::test]
async fn direct_protocol_rejects_non_boolean_project_verifier_discovery() {
    let host = RuntimeHost::in_memory().await.expect("host");
    let session_id = host.default_session_id();

    let error = host
        .handle_command(command_with_payload(
            session_id,
            json!({
                "prompt": "do not start",
                "discover_project_verifiers": "false",
            }),
        ))
        .await
        .expect_err("malformed verifier discovery flag must be rejected");

    assert!(matches!(
        error,
        ClientError::TaskExecution(message)
            if message == "discover_project_verifiers must be a boolean"
    ));
}

#[tokio::test]
async fn queued_turn_cannot_change_the_active_network_capability() {
    let workspace = tempdir().expect("workspace");
    let _provider = IsolatedGlobalMockProvider::install().await;
    let transport = EmbeddedTransport::for_cwd(workspace.path())
        .await
        .expect("transport");
    let session_id = transport.default_session_id();

    transport
        .send_command(command(session_id, "sleep"))
        .await
        .expect("blocking task");
    wait_for_status(&transport, session_id, TaskStatus::WaitingApproval).await;

    let queued = transport
        .send_command(command_with_payload(
            session_id,
            json!({
                "prompt": "inspect the workspace",
                "allow_network": true,
            }),
        ))
        .await
        .expect("queued command is governed");

    assert!(!queued.accepted);
    assert_eq!(
        queued.reason.as_deref(),
        Some("queued prompt cannot change network capability while a task is active")
    );
    let events = transport
        .host
        .storage
        .repositories
        .events
        .load(session_id, None, None)
        .await
        .expect("events");
    assert!(!events.iter().any(|event| {
        event.event_type == RuntimeEventType::TurnQueued
            && event
                .payload
                .pointer("/payload/prompt")
                .and_then(Value::as_str)
                == Some("inspect the workspace")
    }));

    transport
        .send_command(runtime_command(
            session_id,
            SessionCommandKind::Abort,
            json!({}),
        ))
        .await
        .expect("release blocking task");
    if let Some(control) = transport
        .host
        .execution
        .task_controls
        .lock()
        .await
        .get(&session_id)
    {
        control.abort_handle.abort();
    }
    sleep(Duration::from_millis(100)).await;
}

#[tokio::test]
async fn queued_turn_inherits_and_persists_the_active_network_capability() {
    let workspace = tempdir().expect("workspace");
    let _provider = IsolatedGlobalMockProvider::install().await;
    let transport = EmbeddedTransport::for_cwd(workspace.path())
        .await
        .expect("transport");
    let session_id = transport.default_session_id();

    transport
        .send_command(command_with_payload(
            session_id,
            json!({"prompt": "sleep", "allow_network": true}),
        ))
        .await
        .expect("blocking task");
    wait_for_status(&transport, session_id, TaskStatus::WaitingApproval).await;

    let queued = transport
        .send_command(command(session_id, "inspect the workspace"))
        .await
        .expect("queued command");
    assert!(queued.accepted);

    let events = transport
        .host
        .storage
        .repositories
        .events
        .load(session_id, None, None)
        .await
        .expect("events");
    let queued_payload = events
        .iter()
        .find(|event| {
            event.event_type == RuntimeEventType::TurnQueued
                && event
                    .payload
                    .pointer("/payload/prompt")
                    .and_then(Value::as_str)
                    == Some("inspect the workspace")
        })
        .and_then(|event| event.payload.get("payload"))
        .expect("durable queued payload");
    assert_eq!(queued_payload["allow_network"], true);

    transport
        .send_command(runtime_command(
            session_id,
            SessionCommandKind::Abort,
            json!({}),
        ))
        .await
        .expect("release blocking task");
    if let Some(control) = transport
        .host
        .execution
        .task_controls
        .lock()
        .await
        .get(&session_id)
    {
        control.abort_handle.abort();
    }
    sleep(Duration::from_millis(100)).await;
}

#[tokio::test]
async fn direct_protocol_rejects_non_boolean_network_capability() {
    let host = RuntimeHost::in_memory().await.expect("host");
    let session_id = host.default_session_id();

    let error = host
        .handle_command(command_with_payload(
            session_id,
            json!({
                "prompt": "do not start",
                "allow_network": "true",
            }),
        ))
        .await
        .expect_err("malformed network capability must be rejected");

    assert!(matches!(
        error,
        ClientError::TaskExecution(message) if message == "allow_network must be a boolean"
    ));
}

#[tokio::test]
async fn queued_turn_cannot_change_the_active_yolo_capability() {
    let workspace = tempdir().expect("workspace");
    let _provider = IsolatedGlobalMockProvider::install().await;
    let transport = EmbeddedTransport::for_cwd(workspace.path())
        .await
        .expect("transport");
    let session_id = transport.default_session_id();

    transport
        .send_command(command(session_id, "sleep"))
        .await
        .expect("blocking task");
    wait_for_status(&transport, session_id, TaskStatus::WaitingApproval).await;

    let queued = transport
        .send_command(command_with_payload(
            session_id,
            json!({"prompt": "inspect the workspace", "yolo": true}),
        ))
        .await
        .expect("queued command is governed");
    assert!(!queued.accepted);
    assert_eq!(
        queued.reason.as_deref(),
        Some("queued prompt cannot change yolo capability while a task is active")
    );

    transport
        .send_command(runtime_command(
            session_id,
            SessionCommandKind::Abort,
            json!({}),
        ))
        .await
        .expect("release blocking task");
    if let Some(control) = transport
        .host
        .execution
        .task_controls
        .lock()
        .await
        .get(&session_id)
    {
        control.abort_handle.abort();
    }
    sleep(Duration::from_millis(100)).await;
}

#[tokio::test]
async fn queued_turn_inherits_and_persists_the_active_yolo_capability() {
    let workspace = tempdir().expect("workspace");
    let _provider = IsolatedGlobalMockProvider::install().await;
    let transport = EmbeddedTransport::for_cwd(workspace.path())
        .await
        .expect("transport");
    let session_id = transport.default_session_id();

    transport
        .send_command(command_with_payload(
            session_id,
            json!({"prompt": "sleep", "yolo": true}),
        ))
        .await
        .expect("blocking task");
    wait_for_status(&transport, session_id, TaskStatus::Running).await;

    let queued = transport
        .send_command(command(session_id, "inspect the workspace"))
        .await
        .expect("queued command");
    assert!(queued.accepted);

    let events = transport
        .host
        .storage
        .repositories
        .events
        .load(session_id, None, None)
        .await
        .expect("events");
    let queued_payload = events
        .iter()
        .find(|event| {
            event.event_type == RuntimeEventType::TurnQueued
                && event
                    .payload
                    .pointer("/payload/prompt")
                    .and_then(Value::as_str)
                    == Some("inspect the workspace")
        })
        .and_then(|event| event.payload.get("payload"))
        .expect("durable queued payload");
    assert_eq!(queued_payload["yolo"], true);

    transport
        .send_command(runtime_command(
            session_id,
            SessionCommandKind::Abort,
            json!({}),
        ))
        .await
        .expect("release blocking task");
    if let Some(control) = transport
        .host
        .execution
        .task_controls
        .lock()
        .await
        .get(&session_id)
    {
        control.abort_handle.abort();
    }
    sleep(Duration::from_millis(100)).await;
}

#[tokio::test]
async fn queued_turn_inherits_and_persists_active_provider_settings() {
    let workspace = tempdir().expect("workspace");
    let _provider = IsolatedGlobalMockProvider::empty().await;
    let active_settings = json!({
        "provider_profile": "mock",
        "provider_model": "active-model",
        "provider_generation_config": {"reasoning_effort": "high"},
    });
    let mut profile = ProviderProfile::mock();
    profile.model_id = Some("active-model".to_owned());
    profile.generation_config = Some(
        serde_json::from_value(active_settings["provider_generation_config"].clone())
            .expect("generation config"),
    );
    ProviderInstallPlan {
        scope: ProviderConfigScope::User,
        profile,
        activate: true,
        pending_secret: None,
    }
    .apply(&ProviderConfigPaths::global().expect("provider paths"))
    .expect("mock provider");
    let transport = EmbeddedTransport::for_cwd(workspace.path())
        .await
        .expect("transport");
    let session_id = transport.default_session_id();

    transport
        .send_command(command(session_id, "sleep"))
        .await
        .expect("blocking task");
    wait_for_status(&transport, session_id, TaskStatus::WaitingApproval).await;

    for payload in [
        json!({"prompt": "queued without overrides"}),
        json!({
            "prompt": "queued naming the active default",
            "provider_profile": active_settings["provider_profile"],
        }),
        json!({
            "prompt": "queued with matching overrides",
            "provider_profile": active_settings["provider_profile"],
            "provider_model": active_settings["provider_model"],
            "provider_generation_config": active_settings["provider_generation_config"],
        }),
    ] {
        let queued = transport
            .send_command(command_with_payload(session_id, payload))
            .await
            .expect("queued command");
        assert!(queued.accepted, "{:?}", queued.reason);
    }

    let events = transport
        .host
        .storage
        .repositories
        .events
        .load(session_id, None, None)
        .await
        .expect("events");
    let started_payload = events
        .iter()
        .find(|event| event.event_type == RuntimeEventType::TaskCreated)
        .and_then(|event| event.payload.get("payload"))
        .expect("durable task payload");
    assert_eq!(started_payload["provider_profile"], "mock");
    assert_eq!(started_payload["provider_model"], "active-model");
    assert_eq!(
        started_payload["provider_generation_config"],
        active_settings["provider_generation_config"]
    );
    let queued_payloads = events
        .iter()
        .filter(|event| event.event_type == RuntimeEventType::TurnQueued)
        .filter_map(|event| event.payload.get("payload"))
        .filter(|payload| {
            payload
                .get("prompt")
                .and_then(Value::as_str)
                .is_some_and(|prompt| prompt.starts_with("queued "))
        })
        .collect::<Vec<_>>();
    assert_eq!(queued_payloads.len(), 3);
    for payload in queued_payloads {
        assert_eq!(
            payload["provider_profile"],
            active_settings["provider_profile"]
        );
        assert_eq!(payload["provider_model"], active_settings["provider_model"]);
        assert_eq!(
            payload["provider_generation_config"],
            active_settings["provider_generation_config"]
        );
    }

    transport
        .send_command(runtime_command(
            session_id,
            SessionCommandKind::Abort,
            json!({}),
        ))
        .await
        .expect("release blocking task");
    if let Some(control) = transport
        .host
        .execution
        .task_controls
        .lock()
        .await
        .get(&session_id)
    {
        control.abort_handle.abort();
    }
    sleep(Duration::from_millis(100)).await;
}

#[tokio::test]
async fn queued_turn_cannot_change_active_provider_settings() {
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
                "prompt": "sleep",
                "provider_profile": "mock",
                "provider_model": "active-model",
                "provider_generation_config": {"reasoning_effort": "high"},
            }),
        ))
        .await
        .expect("blocking task");
    wait_for_status(&transport, session_id, TaskStatus::WaitingApproval).await;

    for (name, override_payload) in [
        ("profile", json!({"provider_profile": "other"})),
        ("model", json!({"provider_model": "other-model"})),
        (
            "generation config",
            json!({"provider_generation_config": {"reasoning_effort": "low"}}),
        ),
    ] {
        let mut payload = json!({"prompt": format!("change {name}")});
        payload.as_object_mut().expect("object").extend(
            override_payload
                .as_object()
                .expect("override object")
                .clone(),
        );
        let queued = transport
            .send_command(command_with_payload(session_id, payload))
            .await
            .expect("queued command is governed");
        assert!(!queued.accepted, "{name}");
        assert_eq!(
            queued.reason.as_deref(),
            Some("queued prompt cannot change provider settings while a task is active"),
            "{name}"
        );
    }

    let events = transport
        .host
        .storage
        .repositories
        .events
        .load(session_id, None, None)
        .await
        .expect("events");
    assert!(!events.iter().any(|event| {
        event.event_type == RuntimeEventType::TurnQueued
            && event
                .payload
                .pointer("/payload/prompt")
                .and_then(Value::as_str)
                .is_some_and(|prompt| prompt.starts_with("change "))
    }));

    transport
        .send_command(runtime_command(
            session_id,
            SessionCommandKind::Abort,
            json!({}),
        ))
        .await
        .expect("release blocking task");
    if let Some(control) = transport
        .host
        .execution
        .task_controls
        .lock()
        .await
        .get(&session_id)
    {
        control.abort_handle.abort();
    }
    sleep(Duration::from_millis(100)).await;
}

#[tokio::test]
async fn direct_protocol_rejects_non_boolean_yolo_capability() {
    let host = RuntimeHost::in_memory().await.expect("host");
    let session_id = host.default_session_id();

    let error = host
        .handle_command(command_with_payload(
            session_id,
            json!({"prompt": "do not start", "yolo": "true"}),
        ))
        .await
        .expect_err("malformed yolo capability must be rejected");

    assert!(matches!(
        error,
        ClientError::TaskExecution(message) if message == "yolo must be a boolean"
    ));
}

#[tokio::test]
async fn direct_protocol_rejects_invalid_elapsed_budgets() {
    let host = RuntimeHost::in_memory().await.expect("host");
    let session_id = host.default_session_id();

    for value in [json!(0), json!("1000"), json!(-1)] {
        let error = host
            .clone()
            .handle_command(command_with_payload(
                session_id,
                json!({"prompt": "do not start", "max_elapsed_ms": value}),
            ))
            .await
            .expect_err("invalid elapsed budget must be rejected");

        assert!(matches!(
            error,
            ClientError::TaskExecution(message)
                if message == "max_elapsed_ms must be a positive integer"
        ));
    }
}

#[tokio::test]
async fn direct_protocol_rejects_malformed_delegation_cost_budgets() {
    let host = RuntimeHost::in_memory().await.expect("host");
    let session_id = host.default_session_id();

    for value in [json!("100"), json!(-1), json!(1.5), json!(true)] {
        let error = host
            .clone()
            .handle_command(command_with_payload(
                session_id,
                json!({
                    "prompt": "do not start",
                    "_delegation_cost_budget_microusd": value,
                }),
            ))
            .await
            .expect_err("malformed delegation cost budget must be rejected");

        assert!(matches!(
            error,
            ClientError::TaskExecution(message)
                if message == "_delegation_cost_budget_microusd must be a non-negative integer"
        ));
    }
}

#[tokio::test]
async fn queued_turn_persists_its_runtime_and_evaluator_options() {
    let workspace = tempdir().expect("workspace");
    let _provider = IsolatedGlobalMockProvider::install().await;
    let transport = EmbeddedTransport::for_cwd(workspace.path())
        .await
        .expect("transport");
    let session_id = transport.default_session_id();

    transport
        .send_command(command(session_id, "sleep"))
        .await
        .expect("blocking task");
    wait_for_status(&transport, session_id, TaskStatus::WaitingApproval).await;

    let queued = transport
        .send_command(command_with_payload(
            session_id,
            json!({
                "prompt": "finish under the queued deadline",
                "max_elapsed_ms": 345_000,
                "defer_external_verification": true,
            }),
        ))
        .await
        .expect("queued command");
    assert!(queued.accepted);

    let events = transport
        .host
        .storage
        .repositories
        .events
        .load(session_id, None, None)
        .await
        .expect("events");
    let queued_payload = events
        .iter()
        .find(|event| {
            event.event_type == RuntimeEventType::TurnQueued
                && event
                    .payload
                    .pointer("/payload/prompt")
                    .and_then(Value::as_str)
                    == Some("finish under the queued deadline")
        })
        .and_then(|event| event.payload.get("payload"))
        .expect("durable queued payload");
    assert_eq!(queued_payload["max_elapsed_ms"], 345_000);
    assert_eq!(queued_payload["defer_external_verification"], true);

    transport
        .send_command(runtime_command(
            session_id,
            SessionCommandKind::Abort,
            json!({}),
        ))
        .await
        .expect("release blocking task");
    if let Some(control) = transport
        .host
        .execution
        .task_controls
        .lock()
        .await
        .get(&session_id)
    {
        control.abort_handle.abort();
    }
    sleep(Duration::from_millis(100)).await;
}

#[tokio::test]
async fn per_turn_elapsed_budget_clamps_shell_execution() {
    const MAX_ELAPSED_MS: u64 = 800;

    let workspace = tempdir().expect("workspace");
    let _provider = IsolatedGlobalMockProvider::install().await;
    let transport = EmbeddedTransport::for_cwd(workspace.path())
        .await
        .expect("transport");
    let session_id = transport.default_session_id();

    let ack = transport
        .send_command(command_with_payload(
            session_id,
            json!({
                "prompt": "sleep",
                "yolo": true,
                "max_elapsed_ms": MAX_ELAPSED_MS,
                "task_contract": {"max_correction_rounds": 0}
            }),
        ))
        .await
        .expect("bounded turn");
    assert!(ack.accepted);
    let state = wait_for_terminal_status(&transport, session_id).await;
    let task_id = state["active_task_id"]
        .as_str()
        .expect("task id")
        .parse::<TaskId>()
        .expect("task id format");
    let events = transport
        .host
        .storage
        .repositories
        .events
        .load(session_id, Some(task_id), None)
        .await
        .expect("events");

    assert!(events.iter().any(|event| {
        event.event_type == RuntimeEventType::ToolCompleted
            && event.payload["envelope"]["structured_facts"]["timed_out"] == true
            && event.payload["envelope"]["structured_facts"]["requested_timeout_ms"]
                .as_u64()
                .is_some_and(|timeout| timeout < MAX_ELAPSED_MS)
    }));
}

#[tokio::test]
async fn yolo_turn_writes_outside_the_workspace_without_approval() {
    let workspace = tempdir().expect("workspace");
    let outside = tempdir().expect("outside");
    let target = outside.path().join("secrets.7z");
    let _provider = IsolatedGlobalMockProvider::install().await;
    let transport = EmbeddedTransport::for_cwd(workspace.path())
        .await
        .expect("transport");
    let session_id = transport.default_session_id();

    let ack = transport
        .send_command(command_with_payload(
            session_id,
            json!({
                "prompt": "write the requested file",
                "path": target,
                "content": "unrestricted",
                "yolo": true,
                "discover_project_verifiers": false,
                "task_contract": {
                    "workspace_change": "optional",
                    "verification": "best_effort",
                    "max_correction_rounds": 0
                }
            }),
        ))
        .await
        .expect("yolo command");
    assert!(ack.accepted);
    let state = wait_for_terminal_status(&transport, session_id).await;
    assert_eq!(projection_status(&state), Some(TaskStatus::Partial));

    assert_eq!(
        fs::read_to_string(&target).expect("outside result"),
        "unrestricted"
    );
    let events = transport
        .host
        .storage
        .repositories
        .events
        .load(session_id, None, None)
        .await
        .expect("events");
    assert!(
        !events
            .iter()
            .any(|event| event.event_type == RuntimeEventType::ApprovalRequested)
    );
    let task_created = events
        .iter()
        .find(|event| event.event_type == RuntimeEventType::TaskCreated)
        .expect("task created");
    assert_eq!(
        task_created
            .payload
            .pointer("/execution_capabilities/policy/mode"),
        Some(&json!("unrestricted"))
    );
    assert_eq!(
        task_created
            .payload
            .pointer("/execution_capabilities/policy/tool_sandbox_mode"),
        Some(&json!("process_only"))
    );
    assert_eq!(
        task_created
            .payload
            .pointer("/execution_capabilities/policy/permission_profile"),
        Some(&json!("full_access"))
    );
    assert_eq!(
        task_created
            .payload
            .pointer("/execution_capabilities/policy/approval_mode"),
        Some(&json!("never"))
    );
    assert_eq!(
        task_created
            .payload
            .pointer("/execution_capabilities/network/requested"),
        Some(&json!(true))
    );
}

#[tokio::test]
async fn forbidden_workspace_contract_blocks_provider_side_effects() {
    let workspace = tempdir().expect("workspace");
    let _provider = IsolatedGlobalMockProvider::install().await;
    let transport = EmbeddedTransport::for_cwd(workspace.path())
        .await
        .expect("transport");
    let session_id = transport.default_session_id();
    let ack = transport
        .send_command(command_with_payload(
            session_id,
            json!({
                "prompt": "write file blocked.txt with content must-not-write",
                "task_contract": {
                    "workspace_change": "forbidden",
                    "verification": "required"
                }
            }),
        ))
        .await
        .expect("command");
    assert!(ack.accepted);
    let state = wait_for_status(&transport, session_id, TaskStatus::Partial).await;
    assert_eq!(projection_status(&state), Some(TaskStatus::Partial));
    assert!(!workspace.path().join("blocked.txt").exists());

    let task_id = state["active_task_id"]
        .as_str()
        .expect("task id")
        .parse::<TaskId>()
        .expect("task id format");
    let events = transport
        .host
        .storage
        .repositories
        .events
        .load(session_id, Some(task_id), None)
        .await
        .expect("events");
    assert!(events.iter().any(|event| {
        event.event_type == RuntimeEventType::ToolCompleted
            && event.payload["envelope"]["status"] == "error"
            && event.payload["envelope"]["structured_facts"]["error"]
                .as_str()
                .is_some_and(|summary| summary.contains("forbids side-effecting tools"))
    }));
}

#[test]
fn mock_write_file_args_prefers_payload_over_prompt() {
    let payload = json!({
        "path": "explicit.txt",
        "content": "explicit",
    });
    let args = LegacyTaskAdapter::new(&payload, "write file prompt.txt with content prompt")
        .write_file_args();

    assert_eq!(
        args,
        LegacyWriteFileArgs {
            path: "explicit.txt".to_owned(),
            content: "explicit".to_owned(),
        }
    );
    let mut contract = golutra_core::TaskContract::default();
    LegacyTaskAdapter::new(&payload, "write file prompt.txt with content prompt")
        .apply_to(&mut contract);
    assert_eq!(
        contract.required_file_contents,
        vec![golutra_core::RequiredFileContent {
            path: "explicit.txt".to_owned(),
            content: "explicit".to_owned(),
        }]
    );
}

#[test]
fn legacy_change_intent_handles_coding_verbs_without_inventing_delivery_paths() {
    let chinese = json!({"prompt": "修改 runtime 代码，修复验证链路"});
    let chinese_adapter =
        LegacyTaskAdapter::new(&chinese, chinese["prompt"].as_str().expect("prompt"));
    assert!(chinese_adapter.requests_workspace_change());
    assert_eq!(chinese_adapter.required_path(), None);

    let explicit = json!({
        "prompt": "refactor the runtime",
        "path": "src/runtime.rs",
    });
    assert_eq!(
        LegacyTaskAdapter::new(&explicit, explicit["prompt"].as_str().expect("prompt"))
            .required_path(),
        Some("src/runtime.rs".to_owned())
    );

    let mut contract = golutra_core::TaskContract::default();
    assert!(chinese_adapter.apply_to(&mut contract));
    assert_eq!(
        contract.workspace_change,
        WorkspaceChangeRequirement::Required
    );
    assert!(contract.required_paths.is_empty());
    assert!(contract.require_objective_validation);

    let absolute = json!({
        "prompt": "create a file named `/app/output/result.txt`",
    });
    let mut contract = golutra_core::TaskContract::default();
    assert!(
        LegacyTaskAdapter::new(&absolute, absolute["prompt"].as_str().expect("prompt"))
            .apply_to(&mut contract)
    );
    assert!(contract.required_paths.is_empty());
    contract
        .validate()
        .expect("legacy prose must not create a blocking path contract");

    let direct_write = json!({"prompt": "write result.txt with content alpha"});
    let direct_write_adapter = LegacyTaskAdapter::new(
        &direct_write,
        direct_write["prompt"].as_str().expect("prompt"),
    );
    assert!(direct_write_adapter.requests_workspace_change());
    assert_eq!(
        direct_write_adapter.required_paths(),
        vec!["result.txt".to_owned()]
    );
}

#[test]
fn legacy_delivery_prose_requires_change_without_inventing_blocking_paths() {
    let payload = json!({
        "prompt": r#"Create a report generator.
Save the collected records to a CSV file named 'export.csv'.
The summary should be saved to a file named 'summary.txt'."#,
    });
    let mut contract = golutra_core::TaskContract::default();

    assert!(
        LegacyTaskAdapter::new(&payload, payload["prompt"].as_str().expect("prompt"))
            .apply_to(&mut contract)
    );

    assert!(contract.required_paths.is_empty());
    assert_eq!(
        contract.workspace_change,
        WorkspaceChangeRequirement::Required
    );
    assert!(contract.require_objective_validation);
    contract.validate().expect("coarse legacy contract");
}

#[test]
fn legacy_contract_distinguishes_inputs_conversions_and_imperative_deliveries() {
    let tmux_prompt = "Fix the bug in project/src/process_data.py. Finally, use less to examine the final output.csv and verify it processed correctly.";
    let tmux_payload = json!({"prompt": tmux_prompt});
    let tmux = LegacyTaskAdapter::new(&tmux_payload, tmux_prompt);
    assert!(tmux.requests_workspace_change());
    assert!(tmux.required_paths().is_empty());

    let conversion_prompt =
        "Convert the file '/app/data.csv' into a Parquet file named '/app/data.parquet'.";
    let conversion_payload = json!({"prompt": conversion_prompt});
    let mut conversion_contract = golutra_core::TaskContract::default();
    assert!(
        LegacyTaskAdapter::new(&conversion_payload, conversion_prompt)
            .apply_to(&mut conversion_contract)
    );
    assert!(conversion_contract.required_paths.is_empty());
    assert!(conversion_contract.require_objective_validation);
    conversion_contract
        .validate()
        .expect("conversion change contract");

    let layout_prompt = "Call this folder \"processed_chunks/\". Name the script \"migrate.py\" and place it next to input_chunks/.";
    let layout_payload = json!({"prompt": layout_prompt});
    let mut layout_contract = golutra_core::TaskContract::default();
    assert!(LegacyTaskAdapter::new(&layout_payload, layout_prompt).apply_to(&mut layout_contract));
    assert!(layout_contract.required_paths.is_empty());
    layout_contract
        .validate()
        .expect("named delivery change contract");
}

#[test]
fn legacy_external_effects_do_not_invent_a_workspace_diff_contract() {
    for prompt in [
        "Create an S3 bucket named sample-bucket and set it to public read.",
        "Create a spreadsheet named Financial Report and add a sheet named Q1 Data.",
        "Create a GitHub repository named sample-project.",
        "Create a managed database server in AWS.",
        "Update the API gateway in the cloud account.",
        "Implement a managed cloud service for a remote account.",
        "Diagnose why a remote status endpoint is unreachable and explain how to repair it.",
        "Configure a hosted repository so clients can push updates to it.",
    ] {
        let payload = json!({"prompt": prompt});
        let adapter = LegacyTaskAdapter::new(&payload, prompt);
        assert!(!adapter.requests_workspace_change(), "{prompt}");
        assert!(adapter.required_paths().is_empty(), "{prompt}");
    }
}

#[test]
fn legacy_explicit_environment_repairs_require_validation_without_workspace_diff() {
    for prompt in [
        "Fix the globally installed package.",
        "Patch the system dependency installation.",
        "Repair the host service configuration.",
        "升级系统环境中的编译器工具。",
    ] {
        let payload = json!({"prompt": prompt});
        let adapter = LegacyTaskAdapter::new(&payload, prompt);
        let mut contract = golutra_core::TaskContract::default();

        assert!(!adapter.requests_workspace_change(), "{prompt}");
        assert!(adapter.apply_to(&mut contract), "{prompt}");
        assert_eq!(
            contract.workspace_change,
            WorkspaceChangeRequirement::Optional,
            "{prompt}"
        );
        assert!(contract.require_objective_validation, "{prompt}");
        assert_eq!(
            contract.verification,
            golutra_core::VerificationRequirement::Required,
            "{prompt}"
        );
    }
}

#[test]
fn legacy_code_and_service_implementation_still_require_workspace_evidence() {
    for prompt in [
        "Implement the parser.",
        "Fix the bug in the source code.",
        "Fix the Python package in this repository.",
        "Fix the existing parser code so the package works with the default interpreter.",
        "Update this library to support the system runtime.",
        "Fix the system runtime adapter.",
        "Update the global package client.",
        "Patch the runtime adapter for globally installed tool compatibility.",
        "Fix the globally installed tool adapter in this repository.",
        "Fix the GitHub integration in this repository.",
        "Implement a GitHub repository adapter.",
        "Implement a cloud API client.",
        "Implement a cloudish repository parser.",
        "Fix the GitHub Actions workflow.",
        "Fix the GitLab repository code.",
        "Implement an AWS client in this workspace.",
        "Update the cloud deployment module in this codebase.",
        "Implement and run a server on port 3000.",
        "Update the API gateway configuration in this workspace.",
        "修改 runtime 代码，修复验证链路",
        "修复系统运行时适配器。",
    ] {
        let payload = json!({"prompt": prompt});
        assert!(
            LegacyTaskAdapter::new(&payload, prompt).requests_workspace_change(),
            "{prompt}"
        );
    }
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
        compaction_limit: 80,
        target_input_tokens: 64,
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
        replacement_sources: Vec::new(),
        message_decisions: Vec::new(),
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

#[tokio::test]
async fn context_merges_parent_and_child_agents_instructions_in_order() {
    let parent = tempdir().expect("parent workspace");
    let repository = parent.path().join("repo");
    let workspace = repository.join("packages/app");
    fs::create_dir_all(&workspace).expect("nested workspace");
    fs::create_dir(repository.join(".git")).expect("git marker");
    fs::write(repository.join("AGENTS.md"), "parent rule").expect("parent instructions");
    fs::write(workspace.join("AGENTS.md"), "child rule").expect("child instructions");

    let instructions = load_project_instruction_bundle(&workspace)
        .await
        .expect("layered instructions")
        .expect("instructions present")
        .content;
    assert!(
        instructions.find("parent rule").expect("parent rule")
            < instructions.find("child rule").expect("child rule")
    );
    let bundle = load_project_instruction_bundle(&workspace)
        .await
        .expect("instruction bundle")
        .expect("bundle present");
    assert_eq!(bundle.source_refs.len(), 2);
    assert!(
        bundle
            .source_refs
            .iter()
            .any(|reference| reference.ends_with("repo/AGENTS.md"))
    );
    assert!(
        bundle
            .source_refs
            .iter()
            .any(|reference| reference.ends_with("packages/app/AGENTS.md"))
    );
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

    let error = load_project_instruction_bundle(&canonical_workspace)
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
        let mut lanes = host.execution.lane_manager.lock().await;
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
    let lanes = host.execution.lane_manager.lock().await;
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
        .storage
        .store
        .query_state(session_id, None)
        .await
        .expect("state");

    assert_eq!(recovered, 1);
    assert_eq!(state.task_status, TaskStatus::Interrupted);
    let events = host
        .storage
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
        .storage
        .store
        .query_state(session_id, None)
        .await
        .expect("state");
    let events = host
        .storage
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
        .storage
        .store
        .query_state(session_id, None)
        .await
        .expect("state");
    assert_eq!(state.task_status, TaskStatus::Interrupted);
    let events = host
        .storage
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
        .execution
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
        .execution
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
        .execution
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
async fn runtime_recovery_keeps_the_provider_binding_pinned_at_queue_time() {
    let workspace = tempdir().expect("workspace");
    let _provider = IsolatedGlobalMockProvider::empty().await;
    let provider_paths = ProviderConfigPaths::global().expect("provider paths");
    let mut profile_a = ProviderProfile::mock();
    profile_a.name = "profile-a".to_owned();
    profile_a.model_id = Some("model-a".to_owned());
    profile_a.generation_config = Some(
        serde_json::from_value(json!({"reasoning_effort": "high"}))
            .expect("profile A generation config"),
    );
    let mut profile_b = ProviderProfile::mock();
    profile_b.name = "profile-b".to_owned();
    profile_b.model_id = Some("model-b".to_owned());
    profile_b.generation_config = Some(
        serde_json::from_value(json!({"reasoning_effort": "low"}))
            .expect("profile B generation config"),
    );
    let mut settings = ProviderSettings::default();
    settings.upsert_profile(profile_a, true);
    settings.upsert_profile(profile_b, false);
    settings
        .save(&provider_paths.user_config)
        .expect("provider settings A");

    let host = RuntimeHost::for_cwd(workspace.path()).await.expect("host");
    let session_id = host.default_session_id();
    let task_id = TaskId::new();
    let active_turn_id = TurnId::new();
    let pending_turn_id = TurnId::new();
    let actor = Actor {
        kind: ActorKind::Cli,
        id: "provider-recovery-owner".to_owned(),
    };
    host.upsert_current_thread(session_id, &json!({"prompt": "orphaned task"}))
        .await
        .expect("thread");
    let started = host
        .execution
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
        .execution
        .lane_manager
        .lock()
        .await
        .queue_turn(session_id, pending_turn_id, host.next_sequence_no())
        .expect("turn queues");
    let mut queued_payload = json!({"prompt": "sleep"});
    pin_provider_turn_settings(host.provider_config_paths.as_ref(), &mut queued_payload);
    assert_eq!(queued_payload["provider_profile"], "profile-a");
    assert_eq!(queued_payload["provider_model"], "model-a");
    assert_eq!(
        queued_payload["provider_generation_config"],
        json!({"reasoning_effort": "high"})
    );
    host.record_event(with_command_payload(
        queued.event,
        CommandId::new(),
        queued_payload,
    ))
    .await
    .expect("queued event");

    settings
        .set_active_profile("profile-b")
        .expect("activate profile B");
    settings
        .save(&provider_paths.user_config)
        .expect("provider settings B");
    drop(host);

    let reopened = RuntimeHost::for_cwd(workspace.path())
        .await
        .expect("reopened host");
    let transport = EmbeddedTransport::new(reopened);
    wait_for_status(&transport, session_id, TaskStatus::WaitingApproval).await;
    let recovered_settings = transport
        .host
        .execution
        .task_controls
        .lock()
        .await
        .get(&session_id)
        .expect("recovered task control")
        .provider_settings
        .clone();
    assert_eq!(recovered_settings.profile, Some(json!("profile-a")));
    assert_eq!(recovered_settings.model, Some(json!("model-a")));
    assert_eq!(
        recovered_settings.generation_config,
        Some(json!({"reasoning_effort": "high"}))
    );

    transport
        .send_command(runtime_command(
            session_id,
            SessionCommandKind::Abort,
            json!({}),
        ))
        .await
        .expect("release recovered task");
    if let Some(control) = transport
        .host
        .execution
        .task_controls
        .lock()
        .await
        .get(&session_id)
    {
        control.abort_handle.abort();
    }
    sleep(Duration::from_millis(100)).await;
}

#[tokio::test]
async fn runtime_recovery_rejects_a_pending_batch_that_changes_yolo_capability() {
    let host = RuntimeHost::in_memory().await.expect("host");
    let session_id = host.default_session_id();
    let actor = Actor {
        kind: ActorKind::Runtime,
        id: "pending-recovery-test".to_owned(),
    };
    let recovered = [false, true]
        .into_iter()
        .enumerate()
        .map(|(index, yolo)| {
            let prompt = format!("pending turn {index}");
            RecoveredPendingTurn {
                sequence_no: u64::try_from(index).expect("sequence"),
                actor: actor.clone(),
                payload: json!({
                    "prompt": prompt,
                    "allow_network": false,
                    "yolo": yolo,
                }),
                pending: PendingAgentTurn {
                    command_id: CommandId::new(),
                    turn_id: TurnId::new(),
                    content: prompt,
                    task_contract: Some(TaskContract::default()),
                    output_schema: None,
                    external_verifiers: Vec::new(),
                    max_elapsed_ms: None,
                    defer_external_verification: false,
                    external_verifiers_require_os_sandbox: false,
                    allow_network: false,
                    yolo,
                    steer: false,
                },
                execution: PendingTurnExecutionOptions::default(),
                continuation: None,
            }
        })
        .collect::<Vec<_>>();

    let error = host
        .restart_pending_turns(session_id, recovered, None)
        .await
        .expect_err("mixed yolo recovery batch must fail");
    assert!(matches!(
        error,
        ClientError::TaskExecution(message)
            if message == "durable pending turn batch changes yolo capability"
    ));
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
        .execution
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
        .execution
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
        .storage
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
        .execution
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
        .execution
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
        .storage
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
        .execution
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
    let execution_cancellation = execution.cancellation_token();
    let worker = tokio::spawn(async {
        panic!("intentional worker panic");
        #[allow(unreachable_code)]
        Ok::<(), ClientError>(())
    });
    let abort_handle = worker.abort_handle();
    let (completion_sender, completion) = watch::channel(false);
    host.execution.task_controls.lock().await.insert(
        task.session_id,
        HostedTaskControl {
            task_id: task.task_id,
            allow_network: false,
            yolo: false,
            provider_settings: ProviderTurnSettings::default(),
            execution,
            abort_handle,
            completion,
            delegation: None,
            _session_lease: None,
        },
    );

    host.clone()
        .supervise_agent_task(task.clone(), worker, completion_sender)
        .await;
    let state = host
        .storage
        .store
        .query_state(task.session_id, None)
        .await
        .expect("state");

    assert_eq!(state.task_status, TaskStatus::Failed);
    assert!(execution_cancellation.is_cancelled());
    assert!(
        !host
            .execution
            .task_controls
            .lock()
            .await
            .contains_key(&task.session_id)
    );
}

#[tokio::test]
async fn aborting_a_hosted_observation_owner_releases_the_recorder_and_runtime_host() {
    let host = RuntimeHost::in_memory().await.expect("host");
    let weak_host = Arc::downgrade(&host);
    let task = HostedAgentTask {
        session_id: host.default_session_id(),
        task_id: TaskId::new(),
        turn_id: TurnId::new(),
        payload: json!({"prompt": "abort recorder fixture"}),
    };
    let (started_sender, started_receiver) = tokio::sync::oneshot::channel();
    let worker_host = host.clone();
    let worker = tokio::spawn(async move {
        let _recorder = crate::execution::HostedObservationRecorder::spawn(worker_host, task);
        let _ = started_sender.send(());
        std::future::pending::<()>().await;
    });

    started_receiver.await.expect("recorder started");
    drop(host);
    worker.abort();
    assert!(
        worker
            .await
            .expect_err("worker must be aborted")
            .is_cancelled()
    );

    timeout(Duration::from_secs(2), async {
        while weak_host.strong_count() != 0 {
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("observation recorder must release the runtime host");
}

#[tokio::test]
async fn panicking_a_hosted_observation_owner_releases_the_recorder_and_runtime_host() {
    let host = RuntimeHost::in_memory().await.expect("host");
    let weak_host = Arc::downgrade(&host);
    let task = HostedAgentTask {
        session_id: host.default_session_id(),
        task_id: TaskId::new(),
        turn_id: TurnId::new(),
        payload: json!({"prompt": "panic recorder fixture"}),
    };
    let (started_sender, started_receiver) = tokio::sync::oneshot::channel();
    let worker_host = host.clone();
    let worker = tokio::spawn(async move {
        let _recorder = crate::execution::HostedObservationRecorder::spawn(worker_host, task);
        let _ = started_sender.send(());
        panic!("intentional observation owner panic");
    });

    started_receiver.await.expect("recorder started");
    drop(host);
    assert!(worker.await.expect_err("worker must panic").is_panic());

    timeout(Duration::from_secs(2), async {
        while weak_host.strong_count() != 0 {
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("observation recorder must release the runtime host");
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
        .storage
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

fn exact_file_verifier(expected_path: &str, actual_path: &str) -> Value {
    json!({
        "program": "cmp",
        "args": [expected_path, actual_path],
        "cwd": ".",
        "timeout_ms": 5_000,
        "expected_exit_code": 0,
        "max_output_bytes": 1_024,
    })
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

async fn wait_for_terminal_status(transport: &EmbeddedTransport, session_id: SessionId) -> Value {
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
        if projection_status(&state).is_some_and(TaskStatus::is_terminal) {
            return state;
        }
        sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for terminal status");
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
