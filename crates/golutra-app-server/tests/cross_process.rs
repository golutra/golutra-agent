use std::{path::Path, process::Stdio, time::Duration};

use golutra_client::{HttpSseTransport, RuntimeClient};
use golutra_core::{Actor, ActorKind, CommandId, QueryId};
use golutra_protocol::{
    EventFilter, RuntimeEventType, RuntimeQuery, RuntimeQueryKind, SessionCommand,
    SessionCommandKind,
};
use tempfile::tempdir;
use tokio::process::{Child, Command};

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.start_kill();
    }
}

#[tokio::test]
async fn daemon_process_shares_commands_queries_and_sse() {
    let workspace = tempdir().expect("workspace");
    let home = tempdir().expect("home");
    let mut child = spawn_daemon(workspace.path(), home.path());
    let transport = wait_for_transport(workspace.path()).await;
    let session_id = transport.info().default_session_id;
    let mut events = transport
        .subscribe(EventFilter {
            session_id,
            task_id: None,
            after_sequence_no: None,
        })
        .await
        .expect("SSE subscription");

    let command = SessionCommand {
        command_id: CommandId::new(),
        session_id: Some(session_id),
        kind: SessionCommandKind::Prompt,
        idempotency_key: CommandId::new().to_string(),
        actor: Actor {
            kind: ActorKind::Sdk,
            id: "cross-process-test".to_owned(),
        },
        payload: serde_json::json!({"prompt": "list workspace files"}),
        timestamp: chrono::Utc::now(),
    };
    let ack = transport
        .send_command(command.clone())
        .await
        .expect("command");
    let (terminal, governor_recorded) = tokio::time::timeout(Duration::from_secs(5), async {
        let mut governor_recorded = false;
        loop {
            let event = events
                .recv()
                .await
                .expect("stream remains open")
                .expect("event");
            governor_recorded |= event.event_type == RuntimeEventType::GovernorDecided;
            if event.event_type == RuntimeEventType::TaskCompleted {
                return (event, governor_recorded);
            }
        }
    })
    .await
    .expect("terminal event");

    assert!(ack.accepted);
    assert_eq!(terminal.session_id, session_id);
    assert!(governor_recorded);
    assert!(
        !query(&transport, session_id, RuntimeQueryKind::MemoryList)
            .await
            .as_array()
            .expect("memory list")
            .is_empty()
    );
    assert!(
        !query(&transport, session_id, RuntimeQueryKind::EvaluationResults,)
            .await
            .get("results")
            .and_then(serde_json::Value::as_array)
            .expect("evaluation results")
            .is_empty()
    );
    child.0.kill().await.expect("stop daemon");

    let mut restarted = spawn_daemon(workspace.path(), home.path());
    let restarted_transport = wait_for_transport(workspace.path()).await;
    let duplicate_ack = restarted_transport
        .send_command(command)
        .await
        .expect("duplicate command ack");
    let replay = restarted_transport
        .replay_events(EventFilter {
            session_id,
            task_id: None,
            after_sequence_no: None,
        })
        .await
        .expect("replay after restart");
    assert_eq!(duplicate_ack, ack);
    assert_eq!(
        replay
            .iter()
            .filter(|event| {
                event.get("event_type").and_then(serde_json::Value::as_str) == Some("task_created")
            })
            .count(),
        1
    );
    assert!(
        !query(
            &restarted_transport,
            session_id,
            RuntimeQueryKind::MemoryList,
        )
        .await
        .as_array()
        .expect("restarted memory list")
        .is_empty()
    );
    assert!(
        !query(
            &restarted_transport,
            session_id,
            RuntimeQueryKind::EvaluationResults,
        )
        .await
        .get("results")
        .and_then(serde_json::Value::as_array)
        .expect("restarted evaluation results")
        .is_empty()
    );
    restarted.0.kill().await.expect("stop restarted daemon");
}

fn spawn_daemon(workspace: &Path, home: &Path) -> ChildGuard {
    ChildGuard(
        Command::new(env!("CARGO_BIN_EXE_golutra-app-server"))
            .args([
                "--addr",
                "127.0.0.1:0",
                "--workspace",
                workspace.to_str().expect("workspace path"),
            ])
            .env("GOLUTRA_HOME", home)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .expect("daemon process"),
    )
}

async fn query(
    transport: &HttpSseTransport,
    session_id: golutra_core::SessionId,
    kind: RuntimeQueryKind,
) -> serde_json::Value {
    transport
        .query(RuntimeQuery {
            query_id: QueryId::new(),
            session_id,
            task_id: None,
            kind,
            requester: ActorKind::Sdk,
            cursor: None,
            timestamp: chrono::Utc::now(),
        })
        .await
        .expect("runtime query")
}

async fn wait_for_transport(workspace: &Path) -> HttpSseTransport {
    let mut last_error = None;
    for _ in 0..100 {
        match HttpSseTransport::connect_workspace(workspace).await {
            Ok(transport) => return transport,
            Err(error) => last_error = Some(error),
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!(
        "daemon endpoint did not become ready: {}",
        last_error
            .map(|error| error.to_string())
            .unwrap_or_else(|| "unknown error".to_owned())
    );
}
