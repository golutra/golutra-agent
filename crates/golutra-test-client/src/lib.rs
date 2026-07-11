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
            "version": 1,
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
