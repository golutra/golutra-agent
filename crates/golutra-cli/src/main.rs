use clap::{Parser, Subcommand};
use golutra_client::{InProcessTransport, RuntimeClient};
use golutra_core::{Actor, ActorKind, CommandId, SessionId, TaskStatus};
use golutra_llm::ConfiguredProvider;
use golutra_protocol::{RuntimeQuery, RuntimeQueryKind, SessionCommand, SessionCommandKind};
use tokio::time::{Duration, sleep};
use uuid::Uuid;

#[derive(Debug, Parser)]
#[command(name = "golutra")]
#[command(about = "Golutra coding agent runtime CLI")]
struct Cli {
    #[arg(long, global = true)]
    workspace: Option<std::path::PathBuf>,
    #[arg(long, global = true, value_name = "UUID")]
    session_id: Option<String>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Chat {
        #[arg(default_value = "")]
        prompt: String,
    },
    Status,
    Resume,
    Abort,
    Trace,
    Export,
    Provider {
        #[command(subcommand)]
        command: ProviderCommand,
    },
}

#[derive(Debug, Subcommand)]
enum ProviderCommand {
    Current,
    Probe,
}

#[tokio::main]
async fn main() -> miette::Result<()> {
    let cli = Cli::parse();
    let transport = match cli.workspace.as_deref() {
        Some(workspace) => InProcessTransport::for_workspace(workspace).await,
        None => InProcessTransport::for_current_workspace().await,
    }
    .map_err(|error| miette::miette!("{error}"))?;
    let session_id = resolve_session_id(cli.session_id.as_deref(), &transport)?;

    match cli.command {
        Command::Chat { prompt } => {
            let ack = transport
                .send_command(command(
                    session_id,
                    SessionCommandKind::Prompt,
                    serde_json::json!({ "prompt": prompt }),
                ))
                .await
                .map_err(|error| miette::miette!("{error}"))?;
            println!("{}", serde_json::to_string_pretty(&ack).unwrap_or_default());
            if ack.accepted {
                let state = wait_for_terminal_state(&transport, session_id).await?;
                println!(
                    "{}",
                    serde_json::to_string_pretty(&state).unwrap_or_default()
                );
            }
        }
        Command::Status => {
            let state = transport
                .query(RuntimeQuery {
                    query_id: golutra_core::QueryId::new(),
                    session_id,
                    task_id: None,
                    kind: RuntimeQueryKind::SessionState,
                    requester: ActorKind::Cli,
                    cursor: None,
                    timestamp: chrono::Utc::now(),
                })
                .await
                .map_err(|error| miette::miette!("{error}"))?;
            println!(
                "{}",
                serde_json::to_string_pretty(&state).unwrap_or_default()
            );
        }
        Command::Resume => {
            let ack = transport
                .send_command(command(
                    session_id,
                    SessionCommandKind::Resume,
                    serde_json::json!({}),
                ))
                .await
                .map_err(|error| miette::miette!("{error}"))?;
            println!("{}", serde_json::to_string_pretty(&ack).unwrap_or_default());
        }
        Command::Abort => {
            let ack = transport
                .send_command(command(
                    session_id,
                    SessionCommandKind::Abort,
                    serde_json::json!({}),
                ))
                .await
                .map_err(|error| miette::miette!("{error}"))?;
            println!("{}", serde_json::to_string_pretty(&ack).unwrap_or_default());
        }
        Command::Trace => {
            let trace = transport
                .query(RuntimeQuery {
                    query_id: golutra_core::QueryId::new(),
                    session_id,
                    task_id: None,
                    kind: RuntimeQueryKind::DebugProjection,
                    requester: ActorKind::Cli,
                    cursor: None,
                    timestamp: chrono::Utc::now(),
                })
                .await
                .map_err(|error| miette::miette!("{error}"))?;
            println!(
                "{}",
                serde_json::to_string_pretty(&trace).unwrap_or_default()
            );
        }
        Command::Export => {
            let debug = transport
                .query(RuntimeQuery {
                    query_id: golutra_core::QueryId::new(),
                    session_id,
                    task_id: None,
                    kind: RuntimeQueryKind::DebugProjection,
                    requester: ActorKind::Cli,
                    cursor: None,
                    timestamp: chrono::Utc::now(),
                })
                .await
                .map_err(|error| miette::miette!("{error}"))?;
            let artifacts = debug
                .get("artifacts")
                .cloned()
                .unwrap_or_else(|| serde_json::json!([]));
            println!(
                "{}",
                serde_json::to_string_pretty(&artifacts).unwrap_or_default()
            );
        }
        Command::Provider { command } => match command {
            ProviderCommand::Current => {
                let config = ConfiguredProvider::redacted_from_env()
                    .map_err(|error| miette::miette!("{error}"))?;
                println!(
                    "{}",
                    serde_json::to_string_pretty(&config).unwrap_or_default()
                );
            }
            ProviderCommand::Probe => {
                let result = ConfiguredProvider::probe_from_env()
                    .await
                    .map_err(|error| miette::miette!("{error}"))?;
                println!(
                    "{}",
                    serde_json::to_string_pretty(&result).unwrap_or_default()
                );
            }
        },
    }
    Ok(())
}

fn resolve_session_id(
    value: Option<&str>,
    transport: &InProcessTransport,
) -> miette::Result<SessionId> {
    value
        .map(|value| {
            Uuid::parse_str(value)
                .map(SessionId)
                .map_err(|error| miette::miette!("invalid session id: {error}"))
        })
        .transpose()
        .map(|session_id| session_id.unwrap_or_else(|| transport.default_session_id()))
}

fn command(
    session_id: golutra_core::SessionId,
    kind: SessionCommandKind,
    payload: serde_json::Value,
) -> SessionCommand {
    SessionCommand {
        command_id: CommandId::new(),
        session_id: Some(session_id),
        kind,
        idempotency_key: CommandId::new().to_string(),
        actor: Actor {
            kind: ActorKind::Cli,
            id: "golutra-cli".to_owned(),
        },
        payload,
        timestamp: chrono::Utc::now(),
    }
}

async fn wait_for_terminal_state(
    transport: &InProcessTransport,
    session_id: SessionId,
) -> miette::Result<serde_json::Value> {
    let mut last_state = serde_json::Value::Null;
    for _ in 0..200 {
        let state = transport
            .query(RuntimeQuery {
                query_id: golutra_core::QueryId::new(),
                session_id,
                task_id: None,
                kind: RuntimeQueryKind::SessionState,
                requester: ActorKind::Cli,
                cursor: None,
                timestamp: chrono::Utc::now(),
            })
            .await
            .map_err(|error| miette::miette!("{error}"))?;
        if state
            .get("task_status")
            .and_then(|value| serde_json::from_value::<TaskStatus>(value.clone()).ok())
            .is_some_and(is_terminal_status)
        {
            return Ok(state);
        }
        last_state = state;
        sleep(Duration::from_millis(50)).await;
    }
    Ok(last_state)
}

fn is_terminal_status(status: TaskStatus) -> bool {
    matches!(
        status,
        TaskStatus::Completed | TaskStatus::Partial | TaskStatus::Failed | TaskStatus::Blocked
    )
}
