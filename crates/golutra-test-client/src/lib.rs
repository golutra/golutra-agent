use std::fs;

use golutra_client::{InProcessTransport, RuntimeClient};
use golutra_core::{Actor, ActorKind, CommandId, QueryId};
use golutra_protocol::{RuntimeQuery, RuntimeQueryKind, SessionCommand, SessionCommandKind};
use serde_json::json;

pub async fn transport_smoke() -> miette::Result<bool> {
    let workspace = std::env::temp_dir().join(format!("golutra-test-client-{}", CommandId::new()));
    fs::create_dir_all(&workspace).map_err(|error| miette::miette!("{error}"))?;
    let writer = InProcessTransport::for_workspace(&workspace)
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
    let reader = InProcessTransport::for_workspace(&workspace)
        .await
        .map_err(|error| miette::miette!("{error}"))?;
    let state = reader
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
        .map_err(|error| miette::miette!("{error}"))?;
    Ok(ack.accepted && state.get("task_status").is_some())
}
