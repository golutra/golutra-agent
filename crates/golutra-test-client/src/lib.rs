use std::fs;

use golutra_client::{EmbeddedTransport, RuntimeClient, projection_status};
use golutra_core::{Actor, ActorKind, CommandId, QueryId, TaskStatus};
use golutra_protocol::{RuntimeQuery, RuntimeQueryKind, SessionCommand, SessionCommandKind};
use serde_json::json;

pub async fn transport_smoke() -> miette::Result<bool> {
    let workspace = tempfile::tempdir().map_err(|error| miette::miette!("{error}"))?;
    let home = tempfile::tempdir().map_err(|error| miette::miette!("{error}"))?;
    fs::write(
        home.path().join("provider.json"),
        serde_json::to_vec_pretty(&json!({
            "version": 2,
            "active_profile": "mock",
            "profiles": [{
                "name": "mock",
                "protocol": "mock",
                "model_id": "mock-model",
                "enabled": true
            }]
        }))
        .map_err(|error| miette::miette!("{error}"))?,
    )
    .map_err(|error| miette::miette!("{error}"))?;
    let writer = EmbeddedTransport::from_home_and_cwd(home.path(), workspace.path())
        .await
        .map_err(|error| miette::miette!("{error}"))?;
    let session_id = writer.default_session_id();
    let command = SessionCommand {
        command_id: CommandId::new(),
        session_id: Some(session_id),
        kind: SessionCommandKind::Prompt,
        idempotency_key: "test-client-smoke".to_owned(),
        actor: Actor {
            kind: ActorKind::Sdk,
            id: "golutra-test-client".to_owned(),
        },
        payload: json!({"prompt": "smoke"}),
        timestamp: chrono::Utc::now(),
    };
    let ack = writer
        .send_command(command)
        .await
        .map_err(|error| miette::miette!("{error}"))?;
    let reader = EmbeddedTransport::from_home_and_cwd(home.path(), workspace.path())
        .await
        .map_err(|error| miette::miette!("{error}"))?;
    let query = || RuntimeQuery {
        query_id: QueryId::new(),
        session_id,
        task_id: None,
        kind: RuntimeQueryKind::SessionState,
        requester: ActorKind::Sdk,
        cursor: None,
        timestamp: chrono::Utc::now(),
    };
    for _ in 0..200 {
        let state = reader
            .query(query())
            .await
            .map_err(|error| miette::miette!("{error}"))?;
        if projection_status(&state) == Some(TaskStatus::Completed) {
            return Ok(ack.accepted);
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use golutra_client::RuntimeEventStream;
    use golutra_core::SessionId;
    use golutra_protocol::{EventFilter, RuntimeEventType};

    use super::*;

    #[tokio::test]
    async fn repeated_turns_keep_event_order_and_survive_runtime_restart() {
        let workspace = tempfile::tempdir().expect("workspace");
        let home = tempfile::tempdir().expect("home");
        install_mock_provider(home.path());
        let transport = EmbeddedTransport::from_home_and_cwd(home.path(), workspace.path())
            .await
            .expect("transport");
        let session_id = transport.default_session_id();
        let mut events = transport
            .subscribe(EventFilter {
                session_id,
                task_id: None,
                after_sequence_no: None,
            })
            .await
            .expect("event stream");
        let mut last_sequence = 0_u64;
        for turn in 0..32 {
            let command_id = CommandId::new();
            let ack = transport
                .send_command(SessionCommand {
                    command_id,
                    session_id: Some(session_id),
                    kind: SessionCommandKind::Prompt,
                    idempotency_key: format!("stability-{turn}-{command_id}"),
                    actor: Actor {
                        kind: ActorKind::Sdk,
                        id: "stability-test".to_owned(),
                    },
                    payload: json!({"prompt": format!("conversation turn {turn}")}),
                    timestamp: chrono::Utc::now(),
                })
                .await
                .expect("command");
            assert!(ack.accepted);
            last_sequence = wait_for_completed_turn(&mut events, last_sequence).await;
        }

        drop(events);
        drop(transport);
        let restarted = EmbeddedTransport::from_home_and_cwd(home.path(), workspace.path())
            .await
            .expect("restarted transport");
        assert_eq!(restarted.default_session_id(), session_id);
        assert_eq!(restarted.list_threads(10).await.expect("threads").len(), 1);
        let replay = restarted
            .replay_events(EventFilter {
                session_id,
                task_id: None,
                after_sequence_no: None,
            })
            .await
            .expect("replay");
        let sequences = replay
            .iter()
            .filter_map(|event| event.get("sequence_no").and_then(serde_json::Value::as_u64))
            .collect::<Vec<_>>();
        assert!(sequences.windows(2).all(|pair| pair[0] < pair[1]));
        assert_eq!(
            replay
                .iter()
                .filter(|event| event["event_type"] == "task_completed")
                .count(),
            32
        );
        assert_completed(&restarted, session_id).await;
    }

    async fn wait_for_completed_turn(events: &mut RuntimeEventStream, after: u64) -> u64 {
        tokio::time::timeout(Duration::from_secs(5), async {
            let mut last = after;
            loop {
                let event = events
                    .recv()
                    .await
                    .expect("event stream remains open")
                    .expect("runtime event");
                assert!(event.sequence_no > last);
                last = event.sequence_no;
                if event.event_type == RuntimeEventType::TaskCompleted {
                    return last;
                }
            }
        })
        .await
        .expect("turn completes")
    }

    async fn assert_completed(transport: &EmbeddedTransport, session_id: SessionId) {
        let state = transport
            .query(RuntimeQuery {
                query_id: QueryId::new(),
                session_id,
                task_id: None,
                kind: RuntimeQueryKind::SessionState,
                requester: ActorKind::Sdk,
                cursor: None,
                timestamp: chrono::Utc::now(),
            })
            .await
            .expect("state");
        assert_eq!(projection_status(&state), Some(TaskStatus::Completed));
    }

    fn install_mock_provider(home: &std::path::Path) {
        fs::write(
            home.join("provider.json"),
            serde_json::to_vec_pretty(&json!({
                "version": 2,
                "active_profile": "mock",
                "profiles": [{
                    "name": "mock",
                    "protocol": "mock",
                    "model_id": "mock-model",
                    "enabled": true
                }]
            }))
            .expect("provider JSON"),
        )
        .expect("provider config");
    }
}
