//! Opt-in live benchmark driver. Uses configured credentials, isolated state,
//! and explicit per-turn generation settings without changing user preferences.
use std::{io::Write, path::PathBuf, time::Duration};

use golutra_agent_client::{AgentClient, RuntimeClient, RuntimeExecutionOptions, RuntimeTransport};
use golutra_agent_core::{Actor, ActorKind, CommandId, SessionId};
use golutra_agent_protocol::{EventFilter, RuntimeEventType, SessionCommand, SessionCommandKind};
use serde_json::{Value, json};

fn command(session: SessionId, kind: SessionCommandKind, payload: Value) -> SessionCommand {
    let id = CommandId::new();
    SessionCommand {
        command_id: id,
        session_id: Some(session),
        kind,
        idempotency_key: id.to_string(),
        actor: Actor {
            kind: ActorKind::Sdk,
            id: "live-game-benchmark".into(),
        },
        payload,
        timestamp: chrono::Utc::now(),
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.len() != 6 {
        return Err(
            "usage: live_subagent_benchmark WORKSPACE RUN_DIR PROMPT_FILE EFFORT NODE VERIFIER"
                .into(),
        );
    }
    let workspace = PathBuf::from(&args[0]).canonicalize()?;
    let prompt = std::fs::read_to_string(&args[2])?;
    let transport = RuntimeTransport::ephemeral_persistent_for_cwd_with_options(
        &workspace,
        &args[1],
        RuntimeExecutionOptions::with_network_access(true),
    )
    .await?;
    let run_result: Result<(), Box<dyn std::error::Error>> = async {
    let client = AgentClient::with_actor(
        transport.clone(),
        Actor {
            kind: ActorKind::Sdk,
            id: "live-game-benchmark".into(),
        },
    );
    let thread = client.start_thread().await?;
    let session = thread.session_id();
    let mut stream = transport
        .subscribe(EventFilter {
            session_id: session,
            task_id: None,
            after_sequence_no: None,
        })
        .await?;
    let payload = json!({
        "prompt": prompt, "_thread_id": thread.thread_id(),
        "provider_generation_config": {"reasoning_effort": args[3]},
        "execution_mode": "open", "_execution_mode": "open", "tool_profile": "coding",
        "allow_network": true, "yolo": true, "max_elapsed_ms": 1_200_000,
        "verify_on_change": "off", "_task_contract_origin": "open",
        "task_contract": {"schema_version":1,"workspace_change":"optional",
            "required_paths":[],"required_file_contents":[],"completion_criteria":[],
            "require_objective_validation":false,"verification":"best_effort","max_correction_rounds":null},
        "external_verifiers": [{"program":args[4],"args":[args[5],workspace],
            "cwd":".","timeout_ms":60000,"expected_exit_code":0,"max_output_bytes":262144}],
        "_external_verifiers_require_os_sandbox": false
    });
    println!(
        "{}",
        json!({"type":"benchmark.started","session_id":session,
        "thread_id":thread.thread_id(),"reasoning_effort":args[3]})
    );
    let ack = transport
        .send_command(command(session, SessionCommandKind::Prompt, payload))
        .await?;
    if !ack.accepted {
        return Err(format!("prompt rejected: {:?}", ack.reason).into());
    }
    let deadline = tokio::time::sleep(Duration::from_secs(1260));
    tokio::pin!(deadline);
    let mut interrupted = false;
    let result = loop {
        let event = tokio::select! {
            event = stream.recv() => event,
            _ = tokio::signal::ctrl_c(), if !interrupted => {
                interrupted = true;
                transport.send_command(command(session, SessionCommandKind::Abort,
                    json!({"cancel_children":true}))).await?;
                continue;
            }
            _ = &mut deadline => break Err("benchmark driver deadline elapsed"),
        };
        let Some(event) = event else {
            break Err("event stream ended before terminal result");
        };
        let event = event?;
        println!("{}", serde_json::to_string(&event)?);
        std::io::stdout().flush()?;
        if matches!(
            event.event_type,
            RuntimeEventType::TaskCompleted
                | RuntimeEventType::TaskAborted
                | RuntimeEventType::TaskInterrupted
                | RuntimeEventType::TaskUncertain
        ) {
            break if event.payload.get("status").and_then(Value::as_str) == Some("completed") {
                Ok(())
            } else {
                Err("benchmark task did not complete")
            };
        }
    };
    result.map_err(Into::into)
    }.await;
    // Also drain owned children/processes on stream, serialization, and I/O errors.
    let close_result = transport.close().await;
    run_result?;
    close_result?;
    Ok(())
}
