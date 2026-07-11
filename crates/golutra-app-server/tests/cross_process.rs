use std::{fs, path::Path, process::Stdio, time::Duration};

use golutra_client::{AppServerInfo, HttpSseTransport, RuntimeClient};
use golutra_core::{Actor, ActorKind, CommandId, SessionId, ThreadId};
use golutra_protocol::{EventFilter, RuntimeEventType, SessionCommand, SessionCommandKind};
use tempfile::tempdir;
use tokio::process::{Child, Command};

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
    let reattached_a =
        HttpSseTransport::connect(transport_a.server_info().base_url.clone(), cwd_a.path())
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

fn install_mock_provider(home: &Path) {
    fs::write(
        home.join("provider.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "version": 1,
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
    for _ in 0..200 {
        let attempt = async {
            let bytes = tokio::fs::read(&endpoint)
                .await
                .map_err(|error| error.to_string())?;
            let info: AppServerInfo =
                serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
            HttpSseTransport::connect(info.base_url, cwd)
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
