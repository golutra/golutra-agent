use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand};
use golutra_client::{RuntimeClient, RuntimeTransport};
use golutra_core::{ActorKind, QueryId, SessionId, TaskId};
use golutra_protocol::{DebugProjection, RuntimeQuery, RuntimeQueryKind};
use golutra_vis::{audit_report, load_all_events, otel_trace};

#[derive(Debug, Parser)]
#[command(name = "golutra-vis")]
#[command(version)]
#[command(about = "Golutra runtime trace, audit, and OpenTelemetry projection")]
struct Cli {
    #[arg(long)]
    cwd: Option<PathBuf>,
    #[arg(long, conflicts_with = "connect")]
    daemon: bool,
    #[arg(long, value_name = "URL", conflicts_with = "daemon")]
    connect: Option<String>,
    #[arg(long)]
    session_id: Option<String>,
    #[arg(long)]
    task_id: Option<String>,
    #[arg(long)]
    output: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Audit,
    Events,
    Otel,
}

#[tokio::main]
async fn main() -> miette::Result<()> {
    let cli = Cli::parse();
    let cwd = cli
        .cwd
        .map_or_else(std::env::current_dir, Ok)
        .map_err(|error| miette::miette!("failed to resolve cwd: {error}"))?;
    let transport = if let Some(base_url) = cli.connect {
        RuntimeTransport::connect(base_url, &cwd).await
    } else if cli.daemon {
        RuntimeTransport::local_daemon(&cwd).await
    } else {
        RuntimeTransport::for_cwd(&cwd).await
    }
    .map_err(|error| miette::miette!("runtime connection failed: {error}"))?;
    let session_id = cli
        .session_id
        .as_deref()
        .map(str::parse::<SessionId>)
        .transpose()
        .map_err(|error| miette::miette!("invalid session id: {error}"))?
        .unwrap_or_else(|| transport.default_session_id());
    let task_id = cli
        .task_id
        .as_deref()
        .map(str::parse::<TaskId>)
        .transpose()
        .map_err(|error| miette::miette!("invalid task id: {error}"))?;

    let value = match cli.command {
        Command::Audit => {
            let projection = transport
                .query(RuntimeQuery {
                    query_id: QueryId::new(),
                    session_id,
                    task_id,
                    kind: RuntimeQueryKind::DebugProjection,
                    requester: ActorKind::Cli,
                    cursor: None,
                    timestamp: chrono::Utc::now(),
                })
                .await
                .map_err(|error| miette::miette!("debug query failed: {error}"))?;
            let projection: DebugProjection = serde_json::from_value(projection)
                .map_err(|error| miette::miette!("debug projection is invalid: {error}"))?;
            serde_json::to_value(audit_report(&projection))
        }
        Command::Events => {
            let events = load_all_events(&transport, session_id, task_id)
                .await
                .map_err(|error| miette::miette!("event replay failed: {error}"))?;
            serde_json::to_value(otel_trace(&events).spans)
        }
        Command::Otel => {
            let events = load_all_events(&transport, session_id, task_id)
                .await
                .map_err(|error| miette::miette!("event replay failed: {error}"))?;
            serde_json::to_value(otel_trace(&events))
        }
    }
    .map_err(|error| miette::miette!("visualization serialization failed: {error}"))?;
    let bytes = serde_json::to_vec_pretty(&value)
        .map_err(|error| miette::miette!("visualization serialization failed: {error}"))?;
    if let Some(path) = cli.output {
        write_owner_only(&path, &bytes)?;
    } else {
        println!("{}", String::from_utf8_lossy(&bytes));
    }
    Ok(())
}

fn write_owner_only(path: &Path, bytes: &[u8]) -> miette::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| miette::miette!("failed to create output directory: {error}"))?;
    }
    std::fs::write(path, bytes)
        .map_err(|error| miette::miette!("failed to write {}: {error}", path.display()))?;
    set_owner_only(path)
}

#[cfg(unix)]
fn set_owner_only(path: &Path) -> miette::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .map_err(|error| miette::miette!("failed to protect {}: {error}", path.display()))
}

#[cfg(not(unix))]
fn set_owner_only(_path: &Path) -> miette::Result<()> {
    Ok(())
}
