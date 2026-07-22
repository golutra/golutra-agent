use std::{fs, path::Path, process::Stdio, time::Duration};

#[cfg(unix)]
use golutra_client::UnixIpcTransport;
use golutra_client::{AppServerInfo, HttpSseTransport, RuntimeClient, TaskTraceClient};
use golutra_core::{Actor, ActorKind, CommandId, SessionId, TaskId, ThreadId, TraceView};
use golutra_protocol::{
    EventFilter, RuntimeEventType, SessionCommand, SessionCommandKind, SessionPageRequest,
    SessionRangeDirection, SessionRangeSpec, SessionWindowRequest, TaskTraceRequest,
};
use secrecy::SecretString;
use tempfile::tempdir;
use tokio::process::{Child, Command};

const DAEMON_READY_ATTEMPTS: usize = 1_200;

#[cfg(unix)]
#[tokio::test]
async fn unix_ipc_and_http_share_commands_history_and_event_streams() {
    use std::os::unix::fs::{FileTypeExt, PermissionsExt};

    let cwd = tempdir().expect("cwd");
    let home = tempdir().expect("home");
    install_mock_provider(home.path());
    let _child = spawn_daemon(home.path());
    let http = wait_for_transport(home.path(), cwd.path()).await;
    let ipc = wait_for_ipc_transport(home.path(), cwd.path()).await;
    assert_eq!(
        http.server_info().instance_id,
        ipc.server_info().instance_id
    );
    assert_eq!(http.info().workspace_id, ipc.info().workspace_id);
    let socket = fs::symlink_metadata(home.path().join("app-server/app-server.sock"))
        .expect("IPC socket metadata");
    assert!(socket.file_type().is_socket());
    assert_eq!(socket.permissions().mode() & 0o777, 0o600);

    let session_id = ipc.info().default_session_id;
    let mut stream = ipc
        .subscribe(EventFilter {
            session_id,
            task_id: None,
            after_sequence_no: None,
        })
        .await
        .expect("IPC subscription");
    assert!(
        ipc.send_command(prompt_command(session_id, "hello over IPC"))
            .await
            .expect("IPC command")
            .accepted
    );
    wait_for_completion(&mut stream).await;

    let filter = EventFilter {
        session_id,
        task_id: None,
        after_sequence_no: None,
    };
    let ipc_events = ipc.replay_events(filter.clone()).await.expect("IPC replay");
    let http_events = http.replay_events(filter).await.expect("HTTP replay");
    assert_eq!(ipc_events, http_events);
    assert_eq!(ipc.list_threads(20).await.expect("IPC threads").len(), 1);
    let ipc_page = ipc
        .session_page(SessionPageRequest {
            cursor: None,
            limit: 10,
        })
        .await
        .expect("IPC session page");
    let http_page = http
        .session_page(SessionPageRequest {
            cursor: None,
            limit: 10,
        })
        .await
        .expect("HTTP session page");
    assert_eq!(ipc_page, http_page);
    let anchor = ipc_page.sessions[0].thread_id;
    let request = SessionWindowRequest {
        anchor_thread_id: anchor,
        range: SessionRangeSpec {
            direction: SessionRangeDirection::Single,
            count: 1,
        },
    };
    assert_eq!(
        ipc.session_window(request.clone())
            .await
            .expect("IPC session window"),
        http.session_window(request)
            .await
            .expect("HTTP session window")
    );

    let task_id = ipc_events
        .iter()
        .find(|event| event["event_type"] == "task_created")
        .and_then(|event| event["task_id"].as_str())
        .and_then(|value| value.parse::<TaskId>().ok())
        .expect("task id");
    let trace_request = TaskTraceRequest {
        session_id,
        task_id,
        view: TraceView::Full,
        cursor: None,
        limit: 512,
        wait_for_evaluation: true,
    };
    let ipc_trace = ipc
        .task_trace(trace_request.clone())
        .await
        .expect("IPC trace");
    let http_trace = http.task_trace(trace_request).await.expect("HTTP trace");
    assert_eq!(ipc_trace.events, http_trace.events);
    assert_eq!(
        ipc_trace.integrity.event_chain_digest,
        http_trace.integrity.event_chain_digest
    );
    assert!(ipc_trace.integrity.complete);
    assert!(ipc_trace.integrity.unresolved_refs.is_empty());
    assert_eq!(ipc_trace.verification, http_trace.verification);
    assert_eq!(ipc_trace.verification_plan, http_trace.verification_plan);
    assert_eq!(ipc_trace.post_task_jobs, http_trace.post_task_jobs);
    assert_eq!(
        ipc_trace
            .artifacts
            .iter()
            .map(|artifact| artifact.artifact_id)
            .collect::<std::collections::HashSet<_>>(),
        http_trace
            .artifacts
            .iter()
            .map(|artifact| artifact.artifact_id)
            .collect::<std::collections::HashSet<_>>()
    );
}

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.start_kill();
    }
}

#[tokio::test]
async fn one_daemon_routes_multiple_cwds_and_preserves_history() {
    let cwd_a = tempdir().expect("cwd a");
    let cwd_b = tempdir().expect("cwd b");
    let home = tempdir().expect("home");
    install_mock_provider(home.path());

    let mut child = spawn_daemon(home.path());
    let transport_a = wait_for_transport(home.path(), cwd_a.path()).await;
    let transport_b = wait_for_transport(home.path(), cwd_b.path()).await;
    assert_eq!(
        transport_a.server_info().instance_id,
        transport_b.server_info().instance_id
    );
    assert_ne!(
        transport_a.info().workspace_id,
        transport_b.info().workspace_id
    );

    let session_a = transport_a.info().default_session_id;
    let session_b = transport_b.info().default_session_id;
    let command_a = prompt_command(session_a, "write result.txt with content alpha");
    let command_b = prompt_command(session_b, "write result.txt with content beta");
    let mut events_a = transport_a
        .subscribe(EventFilter {
            session_id: session_a,
            task_id: None,
            after_sequence_no: None,
        })
        .await
        .expect("SSE subscription");

    let ack_a = transport_a
        .send_command(command_a.clone())
        .await
        .expect("cwd a command");
    let ack_b = transport_b
        .send_command(command_b)
        .await
        .expect("cwd b command");
    assert!(ack_a.accepted);
    assert!(ack_b.accepted);
    wait_for_completion(&mut events_a).await;
    wait_for_terminal(&transport_b, session_b).await;
    assert_eq!(
        fs::read_to_string(cwd_a.path().join("result.txt")).expect("cwd a result"),
        "alpha"
    );
    assert_eq!(
        fs::read_to_string(cwd_b.path().join("result.txt")).expect("cwd b result"),
        "beta"
    );
    assert_eq!(
        transport_a
            .list_threads(20)
            .await
            .expect("cwd a threads")
            .len(),
        1
    );
    assert_eq!(
        transport_b
            .list_threads(20)
            .await
            .expect("cwd b threads")
            .len(),
        1
    );

    let latest_session_a = SessionId::new();
    let latest_thread_a = ThreadId::new();
    let mut latest_events_a = transport_a
        .subscribe(EventFilter {
            session_id: latest_session_a,
            task_id: None,
            after_sequence_no: None,
        })
        .await
        .expect("latest cwd a subscription");
    let mut latest_command_a = prompt_command(latest_session_a, "latest conversation");
    latest_command_a.payload = serde_json::json!({
        "prompt": "latest conversation",
        "_thread_id": latest_thread_a.to_string(),
    });
    assert!(
        transport_a
            .send_command(latest_command_a)
            .await
            .expect("latest cwd a command")
            .accepted
    );
    wait_for_completion(&mut latest_events_a).await;
    let transport_token = fs::read_to_string(home.path().join("app-server/transport.token"))
        .expect("transport token");
    let reattached_a = HttpSseTransport::connect_with_token(
        transport_a.server_info().base_url.clone(),
        cwd_a.path(),
        SecretString::from(transport_token.trim().to_owned()),
    )
    .await
    .expect("reattach cwd a");
    assert_eq!(reattached_a.info().default_session_id, latest_session_a);
    assert_eq!(reattached_a.info().default_thread_id, latest_thread_a);
    assert_eq!(
        reattached_a
            .list_threads(20)
            .await
            .expect("reattached cwd a threads")
            .len(),
        2
    );

    child.0.kill().await.expect("stop daemon");
    let mut restarted = spawn_daemon(home.path());
    let restarted_a = wait_for_transport(home.path(), cwd_a.path()).await;
    let restarted_b = wait_for_transport(home.path(), cwd_b.path()).await;
    assert_eq!(restarted_a.info().default_session_id, latest_session_a);
    assert_eq!(restarted_b.info().default_session_id, session_b);
    assert_eq!(
        restarted_a
            .send_command(command_a)
            .await
            .expect("duplicate command"),
        ack_a
    );
    assert!(
        restarted_a
            .replay_events(EventFilter {
                session_id: session_a,
                task_id: None,
                after_sequence_no: None,
            })
            .await
            .expect("cwd a replay")
            .iter()
            .any(|event| {
                event.get("event_type").and_then(serde_json::Value::as_str)
                    == Some("task_completed")
            })
    );
    assert!(
        restarted_b
            .replay_events(EventFilter {
                session_id: session_b,
                task_id: None,
                after_sequence_no: None,
            })
            .await
            .expect("cwd b replay")
            .iter()
            .any(|event| {
                event.get("event_type").and_then(serde_json::Value::as_str)
                    == Some("task_completed")
            })
    );
    restarted.0.kill().await.expect("stop restarted daemon");
}

#[tokio::test]
async fn existing_http_transport_reattaches_after_daemon_restart_on_the_same_endpoint() {
    let cwd = tempdir().expect("cwd");
    let home = tempdir().expect("home");
    install_mock_provider(home.path());
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("reserve port");
    let address = listener.local_addr().expect("address");
    drop(listener);

    let mut first_daemon = spawn_daemon_at(home.path(), address);
    let stale_transport = wait_for_transport(home.path(), cwd.path()).await;
    let stale_instance = stale_transport.server_info().instance_id.clone();
    first_daemon.0.kill().await.expect("stop first daemon");

    let mut second_daemon = spawn_daemon_at(home.path(), address);
    let fresh_transport = wait_for_transport(home.path(), cwd.path()).await;
    assert_ne!(fresh_transport.server_info().instance_id, stale_instance);

    assert!(
        stale_transport
            .list_threads(20)
            .await
            .expect("stale transport reattaches")
            .is_empty()
    );
    second_daemon.0.kill().await.expect("stop second daemon");
}

#[tokio::test]
async fn daemon_resumes_a_waiting_task_after_provider_configuration_reload() {
    let cwd = tempdir().expect("cwd");
    let home = tempdir().expect("home");
    fs::write(home.path().join("provider.json"), "{invalid-json")
        .expect("malformed provider config");
    let mut daemon = spawn_daemon(home.path());
    let transport = wait_for_transport(home.path(), cwd.path()).await;
    let session_id = transport.info().default_session_id;
    let mut events = transport
        .subscribe(EventFilter {
            session_id,
            task_id: None,
            after_sequence_no: None,
        })
        .await
        .expect("provider auth SSE subscription");

    assert!(
        transport
            .send_command(prompt_command(session_id, "hello"))
            .await
            .expect("prompt command")
            .accepted
    );
    let request_id = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let event = events
                .recv()
                .await
                .expect("stream remains open")
                .expect("runtime event");
            if event.event_type == RuntimeEventType::ProviderAuthRequired {
                return event
                    .payload
                    .get("request_id")
                    .and_then(serde_json::Value::as_str)
                    .expect("provider auth request id")
                    .to_owned();
            }
        }
    })
    .await
    .expect("provider auth request");

    install_mock_provider(home.path());
    assert!(
        transport
            .send_command(runtime_command(
                session_id,
                SessionCommandKind::ProviderConfigured,
                serde_json::json!({"request_id": request_id, "verified": true}),
            ))
            .await
            .expect("provider configured command")
            .accepted
    );

    let mut saw_configured = false;
    let mut saw_assistant = false;
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let event = events
                .recv()
                .await
                .expect("stream remains open")
                .expect("runtime event");
            saw_configured |= event.event_type == RuntimeEventType::ProviderConfigured;
            saw_assistant |= event.event_type == RuntimeEventType::AssistantMessage;
            if event.event_type == RuntimeEventType::TaskCompleted {
                return;
            }
        }
    })
    .await
    .expect("resumed task completion");
    assert!(saw_configured);
    assert!(saw_assistant);
    daemon.0.kill().await.expect("stop daemon");
}

#[tokio::test]
async fn daemon_transport_token_is_private_and_rejects_wrong_credentials() {
    let cwd = tempdir().expect("cwd");
    let home = tempdir().expect("home");
    install_mock_provider(home.path());
    let mut daemon = spawn_daemon(home.path());
    let transport = wait_for_transport(home.path(), cwd.path()).await;
    let endpoint_path = home.path().join("app-server/app-server.json");
    let token_path = home.path().join("app-server/transport.token");
    let endpoint = fs::read_to_string(&endpoint_path).expect("endpoint metadata");
    let token = fs::read_to_string(&token_path).expect("transport token");
    assert!(!endpoint.contains(token.trim()));
    assert_eq!(
        transport.server_info().protocol_versions,
        golutra_protocol::ProtocolVersionRange::runtime()
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        assert_eq!(
            fs::metadata(&token_path)
                .expect("token metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    let error = HttpSseTransport::connect_with_token(
        transport.server_info().base_url.clone(),
        cwd.path(),
        SecretString::from("wrong-transport-token-0000000000000000000000000000000000000000"),
    )
    .await
    .expect_err("wrong transport token must fail");
    assert!(error.to_string().contains("401"));
    daemon.0.kill().await.expect("stop daemon");
}

#[tokio::test]
async fn daemon_fork_rollout_and_rebind_survive_restart() {
    let old_cwd = tempdir().expect("old cwd");
    let new_cwd = tempdir().expect("new cwd");
    let home = tempdir().expect("home");
    install_mock_provider(home.path());
    let mut daemon = spawn_daemon(home.path());
    let old_transport = wait_for_transport(home.path(), old_cwd.path()).await;
    let session_id = old_transport.info().default_session_id;
    assert!(
        old_transport
            .send_command(prompt_command(session_id, "history before daemon fork"))
            .await
            .expect("parent command")
            .accepted
    );
    wait_for_terminal(&old_transport, session_id).await;
    let parent = old_transport
        .list_threads(10)
        .await
        .expect("parent threads")
        .into_iter()
        .next()
        .expect("parent thread");

    let child = old_transport
        .fork_thread(parent.thread_id, None)
        .await
        .expect("HTTP fork");
    let rollout = old_transport
        .export_thread_rollout(child.thread_id)
        .await
        .expect("HTTP rollout export");
    assert!(Path::new(&rollout.path).exists());
    assert!(rollout.event_count > 0);

    let new_transport = wait_for_transport(home.path(), new_cwd.path()).await;
    let rebound = new_transport
        .rebind_thread(child.thread_id, old_cwd.path())
        .await
        .expect("HTTP rebind");
    assert_eq!(rebound.thread.session_id, child.session_id);
    assert_eq!(
        new_transport
            .list_threads(10)
            .await
            .expect("new cwd threads")
            .len(),
        1
    );
    assert_eq!(
        old_transport
            .list_threads(10)
            .await
            .expect("old cwd threads")
            .len(),
        1
    );

    daemon.0.kill().await.expect("stop daemon");
    let mut restarted = spawn_daemon(home.path());
    let restarted_new = wait_for_transport(home.path(), new_cwd.path()).await;
    let resumed = restarted_new
        .resume_thread(child.thread_id)
        .await
        .expect("rebound child after restart");
    assert_eq!(resumed.session_id, child.session_id);
    assert!(
        restarted_new
            .replay_events(EventFilter {
                session_id: child.session_id,
                task_id: None,
                after_sequence_no: None,
            })
            .await
            .expect("fork replay")
            .iter()
            .any(|event| {
                event.get("event_type").and_then(serde_json::Value::as_str)
                    == Some("thread_rebound")
            })
    );
    restarted.0.kill().await.expect("stop restarted daemon");
}

fn prompt_command(session_id: golutra_core::SessionId, prompt: &str) -> SessionCommand {
    SessionCommand {
        command_id: CommandId::new(),
        session_id: Some(session_id),
        kind: SessionCommandKind::Prompt,
        idempotency_key: CommandId::new().to_string(),
        actor: Actor {
            kind: ActorKind::Sdk,
            id: "cross-process-test".to_owned(),
        },
        payload: serde_json::json!({"prompt": prompt}),
        timestamp: chrono::Utc::now(),
    }
}

fn runtime_command(
    session_id: SessionId,
    kind: SessionCommandKind,
    payload: serde_json::Value,
) -> SessionCommand {
    SessionCommand {
        command_id: CommandId::new(),
        session_id: Some(session_id),
        kind,
        idempotency_key: CommandId::new().to_string(),
        actor: Actor {
            kind: ActorKind::Sdk,
            id: "cross-process-test".to_owned(),
        },
        payload,
        timestamp: chrono::Utc::now(),
    }
}

fn install_mock_provider(home: &Path) {
    fs::write(
        home.join("provider.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "version": 2,
            "active_profile": "mock",
            "profiles": [{
                "name": "mock",
                "protocol": "mock",
                "model_id": "mock-model",
                "enabled": true
            }]
        }))
        .expect("provider json"),
    )
    .expect("provider config");
}

fn spawn_daemon(home: &Path) -> ChildGuard {
    spawn_daemon_at(home, "127.0.0.1:0".parse().expect("daemon address"))
}

fn spawn_daemon_at(home: &Path, address: std::net::SocketAddr) -> ChildGuard {
    ChildGuard(
        Command::new(env!("CARGO_BIN_EXE_golutra-app-server"))
            .arg("--addr")
            .arg(address.to_string())
            .env("GOLUTRA_HOME", home)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .expect("daemon process"),
    )
}

async fn wait_for_transport(home: &Path, cwd: &Path) -> HttpSseTransport {
    let endpoint = home.join("app-server/app-server.json");
    let mut last_error = None;
    for _ in 0..DAEMON_READY_ATTEMPTS {
        let attempt = async {
            let bytes = tokio::fs::read(&endpoint)
                .await
                .map_err(|error| error.to_string())?;
            let info: AppServerInfo =
                serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
            let token = tokio::fs::read_to_string(home.join("app-server/transport.token"))
                .await
                .map_err(|error| error.to_string())?;
            HttpSseTransport::connect_with_token(
                info.base_url,
                cwd,
                SecretString::from(token.trim().to_owned()),
            )
            .await
            .map_err(|error| error.to_string())
        }
        .await;
        match attempt {
            Ok(transport) => return transport,
            Err(error) => last_error = Some(error),
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!(
        "daemon endpoint did not become ready: {}",
        last_error.unwrap_or_else(|| "unknown error".to_owned())
    );
}

#[cfg(unix)]
async fn wait_for_ipc_transport(home: &Path, cwd: &Path) -> UnixIpcTransport {
    let mut last_error = None;
    for _ in 0..DAEMON_READY_ATTEMPTS {
        match UnixIpcTransport::from_home_and_cwd(home, cwd).await {
            Ok(transport) => return transport,
            Err(error) => last_error = Some(error.to_string()),
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!(
        "daemon IPC endpoint did not become ready: {}",
        last_error.unwrap_or_else(|| "unknown error".to_owned())
    );
}

async fn wait_for_completion(events: &mut golutra_client::RuntimeEventStream) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let event = events
                .recv()
                .await
                .expect("stream remains open")
                .expect("runtime event");
            if event.event_type == RuntimeEventType::TaskCompleted {
                return;
            }
        }
    })
    .await
    .expect("task completion");
}

async fn wait_for_terminal(transport: &HttpSseTransport, session_id: golutra_core::SessionId) {
    let mut events = transport
        .subscribe(EventFilter {
            session_id,
            task_id: None,
            after_sequence_no: None,
        })
        .await
        .expect("subscription");
    wait_for_completion(&mut events).await;
}
