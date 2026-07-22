use std::{
    io::{Read, Write},
    path::PathBuf,
    time::{Duration, Instant},
};

use clap::Parser;
use golutra_client::RuntimeApplication;
use golutra_core::{Actor, ActorKind, CommandId, TraceView};
use golutra_protocol::{
    EventFilter, RuntimeEvaluationWorkerRequest, RuntimeEvaluationWorkerResponse, RuntimeEventType,
    RuntimeQuery, RuntimeQueryKind, SessionCommand, SessionCommandKind, TaskTraceRequest,
};

const MAX_REQUEST_BYTES: u64 = 1024 * 1024;
const TASK_TIMEOUT: Duration = Duration::from_secs(90);

#[derive(Debug, Parser)]
#[command(name = "golutra-eval-worker", hide = true)]
struct Cli {
    #[arg(long)]
    home: PathBuf,
    #[arg(long)]
    workspace: PathBuf,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    let request = read_request()?;
    if request.objective.trim().is_empty() {
        return Err("evaluation objective is required".into());
    }
    let application = RuntimeApplication::from_home_and_cwd(&cli.home, &cli.workspace).await?;
    let session_id = application.session_service().default_session_id();
    let cursor = application
        .query(RuntimeQuery {
            query_id: golutra_core::QueryId::new(),
            session_id,
            task_id: None,
            kind: RuntimeQueryKind::SessionState,
            requester: ActorKind::Runtime,
            cursor: None,
            timestamp: chrono::Utc::now(),
        })
        .await?
        .get("last_sequence_no")
        .and_then(serde_json::Value::as_u64);
    let mut events = application
        .subscribe(EventFilter {
            session_id,
            task_id: None,
            after_sequence_no: cursor,
        })
        .await?;
    let mut payload = request.payload.clone();
    let payload = payload
        .as_object_mut()
        .ok_or("evaluation payload must be an object")?;
    payload.insert(
        "prompt".to_owned(),
        serde_json::Value::String(request.objective.clone()),
    );
    let started = Instant::now();
    let command_id = CommandId::new();
    let ack = application
        .send_command(SessionCommand {
            command_id,
            session_id: Some(session_id),
            kind: SessionCommandKind::Prompt,
            idempotency_key: format!("runtime-command:{command_id}"),
            actor: Actor {
                kind: ActorKind::Runtime,
                id: "runtime-controller".to_owned(),
            },
            payload: serde_json::Value::Object(payload.clone()),
            timestamp: chrono::Utc::now(),
        })
        .await?;
    if !ack.accepted {
        return Err(format!(
            "runtime rejected evaluation case: {}",
            ack.reason.unwrap_or_else(|| "unknown reason".to_owned())
        )
        .into());
    }

    let command_id_text = command_id.to_string();
    let task_id = tokio::time::timeout(TASK_TIMEOUT, async {
        let mut task_id = None;
        loop {
            let event = match events.recv().await {
                Some(Ok(event)) => event,
                Some(Err(error)) => return Err(error.to_string()),
                None => return Err("runtime event stream ended".to_owned()),
            };
            if task_id.is_none()
                && matches!(
                    event.event_type,
                    RuntimeEventType::TaskCreated | RuntimeEventType::TurnStarted
                )
                && event
                    .payload
                    .get("command_id")
                    .and_then(serde_json::Value::as_str)
                    == Some(command_id_text.as_str())
            {
                task_id = event.task_id;
            }
            if event.event_type.is_task_terminal() && event.task_id == task_id {
                return task_id.ok_or_else(|| "terminal event has no task id".to_owned());
            }
        }
    })
    .await
    .map_err(|_| "evaluation task timed out")??;
    application
        .governance_service()
        .wait_for_evaluation(task_id)
        .await?;
    let trace = application
        .complete_task_trace(TaskTraceRequest {
            session_id,
            task_id,
            view: TraceView::Full,
            cursor: None,
            limit: 512,
            wait_for_evaluation: true,
        })
        .await?;
    let response = RuntimeEvaluationWorkerResponse {
        elapsed_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        trace,
    };
    let bytes = serde_json::to_vec(&response)?;
    std::io::stdout().lock().write_all(&bytes)?;
    Ok(())
}

fn read_request() -> Result<RuntimeEvaluationWorkerRequest, Box<dyn std::error::Error>> {
    let mut bytes = Vec::new();
    std::io::stdin()
        .lock()
        .take(MAX_REQUEST_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_REQUEST_BYTES {
        return Err("evaluation worker request exceeds its size limit".into());
    }
    Ok(serde_json::from_slice(&bytes)?)
}
