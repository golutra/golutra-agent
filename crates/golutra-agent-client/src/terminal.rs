//! 将进程状态及终态的脱敏完整保留输出持久化，详情查看不需要追加工具调用。
use super::*;
use sha2::{Digest, Sha256};

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn terminal_exit_is_published_and_persisted_without_a_wait_tool_call() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("job.sh"),
            "sleep 0.05\nprintf 'FIRST-RETAINED-LINE\\n'\ni=0; while [ \"$i\" -lt 1000 ]; do printf 'background-output-%s\\n' \"$i\"; i=$((i + 1)); done\nprintf 'background-finished\\n'\n",
        )
        .unwrap();
        let host = RuntimeHost::in_memory().await.unwrap();
        let session = host.default_session_id();
        let mut events = host.execution.event_bus.subscribe();
        let executor = host
            .build_tool_executor(
                WorkspacePolicy::new(root.path()).unwrap(),
                root.path().to_path_buf(),
                false,
                false,
            )
            .await
            .unwrap();
        let request = ToolRequest {
            tool_call_id: golutra_agent_core::ToolCallId::new(),
            provider_tool_call_id: None,
            session_id: session,
            turn_id: None,
            tool_name: "shell".to_owned(),
            arguments: json!({"command":"sh job.sh","background":true}),
        };
        let policy = executor.evaluate(&request).unwrap();
        let start = executor
            .execute_with_policy(request, policy, true, CancellationToken::new())
            .await
            .unwrap();
        let process = start.envelope.structured_facts["process_id"].clone();
        let event = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let event = events.recv().await.unwrap();
                if event.event_type == RuntimeEventType::ProcessUpdated
                    && event.payload["process_id"] == process
                    && event.payload["terminal"] == true
                {
                    break event;
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(event.payload["exit_code"], 0);
        assert!(
            event.payload["output_excerpt"]
                .as_str()
                .unwrap()
                .contains("background-finished")
        );
        let stored = host
            .storage
            .repositories
            .events
            .load(session, None, None)
            .await
            .unwrap();
        assert!(stored.iter().any(|stored| stored.id == event.id));
        let artifact_id =
            serde_json::from_value(event.payload["output_artifact_ref"].clone()).unwrap();
        let bytes = host
            .storage
            .repositories
            .artifacts
            .bytes(artifact_id)
            .await
            .unwrap()
            .unwrap();
        let output = String::from_utf8(bytes).unwrap();
        assert!(output.starts_with("FIRST-RETAINED-LINE\n"));
        assert!(output.ends_with("background-finished\n"));
        assert!(output.len() > 4096);
        assert_eq!(event.payload["output_artifact_truncated"], false);
        flush_process_events(&host).await.unwrap();
        let refreshed = host
            .storage
            .repositories
            .events
            .load(session, None, None)
            .await
            .unwrap();
        let last = refreshed
            .iter()
            .rev()
            .find(|event| event.payload["process_id"] == process)
            .unwrap();
        assert_eq!(
            last.payload["output_artifact_ref"],
            event.payload["output_artifact_ref"]
        );
        host.close().await.unwrap();
    }

    #[tokio::test]
    async fn closing_host_persists_terminal_state_for_a_running_process() {
        let root = tempfile::tempdir().unwrap();
        let host = RuntimeHost::in_memory().await.unwrap();
        let session = host.default_session_id();
        let executor = host
            .build_tool_executor(
                WorkspacePolicy::new(root.path()).unwrap(),
                root.path().to_path_buf(),
                false,
                false,
            )
            .await
            .unwrap();
        let request = ToolRequest {
            tool_call_id: golutra_agent_core::ToolCallId::new(),
            provider_tool_call_id: None,
            session_id: session,
            turn_id: None,
            tool_name: "shell".to_owned(),
            arguments: json!({"command":"sleep 30","background":true}),
        };
        let policy = executor.evaluate(&request).unwrap();
        executor
            .execute_with_policy(request, policy, true, CancellationToken::new())
            .await
            .unwrap();
        host.close().await.unwrap();
        let events = host
            .storage
            .repositories
            .events
            .load(session, None, None)
            .await
            .unwrap();
        let terminal = events
            .iter()
            .rev()
            .find(|event| event.event_type == RuntimeEventType::ProcessUpdated)
            .unwrap();
        assert_eq!(terminal.payload["process_state"], "cancelled");
        assert_eq!(terminal.payload["workspace_scan_pending"], false);
    }
}

type ProcessOrigins = HashMap<
    (SessionId, String),
    Option<(Option<TaskId>, Option<TurnId>, golutra_agent_core::EventId)>,
>;

pub(super) fn start_process_events(host: &Arc<RuntimeHost>) -> tokio::task::JoinHandle<()> {
    let mut updates = host.execution.process_supervisor.subscribe_updates();
    let shutdown = host.execution.shutdown.clone();
    let weak = Arc::downgrade(host);
    tokio::spawn(async move {
        let mut origins = HashMap::new();
        loop {
            let update = tokio::select! {
                biased;
                _ = shutdown.cancelled() => break,
                update = updates.recv() => update,
            };
            let Some(host) = weak.upgrade() else {
                break;
            };
            let batch = match update {
                Ok(update) => vec![update],
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    host.execution.process_supervisor.current_updates().await
                }
                Err(broadcast::error::RecvError::Closed) => break,
            };
            for update in batch {
                if let Err(error) = persist_process_update(&host, &mut origins, update).await {
                    tracing::warn!(%error, "could not persist terminal update");
                }
            }
        }
    })
}

pub(super) async fn flush_process_events(host: &RuntimeHost) -> Result<(), ClientError> {
    let mut origins = HashMap::new();
    for update in host.execution.process_supervisor.current_updates().await {
        persist_process_update(host, &mut origins, update).await?;
    }
    Ok(())
}

async fn persist_process_update(
    host: &RuntimeHost,
    origins: &mut ProcessOrigins,
    mut update: golutra_agent_tools::ProcessUpdate,
) -> Result<(), ClientError> {
    let key = (update.session_id, update.process_id.clone());
    if origins.get(&key).is_none_or(Option::is_none) {
        let events = host
            .storage
            .repositories
            .events
            .load(update.session_id, None, None)
            .await?;
        let call_id = update
            .process_id
            .strip_prefix("proc-")
            .unwrap_or(&update.process_id);
        let origin = events.iter().rev().find(|event| {
            event.event_type == RuntimeEventType::ToolStarted
                && event.payload.get("tool_call_id").and_then(Value::as_str) == Some(call_id)
        });
        origins.insert(
            key.clone(),
            origin.map(|event| (event.task_id, event.turn_id, event.id)),
        );
    }
    let origin = origins.get(&key).copied().flatten();
    if update.payload["terminal"] == true {
        // 输出保存失败不应吞掉真实的进程终态；保留诊断供 UI 明确展示。
        if let Err(error) =
            persist_terminal_output(host, &mut update, origin.and_then(|value| value.1)).await
        {
            tracing::warn!(%error, "could not persist terminal output");
            update.payload["output_artifact_error"] = json!(error.to_string());
        }
    }
    if update.payload["terminal"] == true && update.payload["workspace_scan_pending"] == false {
        origins.remove(&key);
    }
    host.record_event(RuntimeEvent {
        schema_version: golutra_agent_core::RUNTIME_EVENT_SCHEMA_VERSION,
        id: golutra_agent_core::EventId::new(),
        sequence_no: 0,
        session_id: update.session_id,
        task_id: origin.and_then(|origin| origin.0),
        turn_id: origin.and_then(|origin| origin.1),
        parent_event_id: origin.map(|origin| origin.2),
        causal_context: Default::default(),
        causal_links: Vec::new(),
        event_type: RuntimeEventType::ProcessUpdated,
        timestamp: chrono::Utc::now(),
        source: RuntimeEventSource::Tool,
        payload: update.payload,
        payload_ref: None,
        durable: true,
    })
    .await
}

async fn persist_terminal_output(
    host: &RuntimeHost,
    update: &mut golutra_agent_tools::ProcessUpdate,
    turn_id: Option<TurnId>,
) -> Result<(), ClientError> {
    let Some((output, truncated)) = host
        .execution
        .process_supervisor
        .retained_terminal_output(update.session_id, &update.process_id)
        .await
        .map_err(|error| ClientError::TaskExecution(error.to_string()))?
    else {
        return Ok(());
    };
    let (output, redaction_status) = golutra_agent_tools::redact_sensitive_text(&output);
    let bytes = output.as_bytes();
    let checksum = format!("sha256:{:x}", Sha256::digest(bytes));
    let repository = &host.storage.repositories.artifacts;
    // 结束通知、工作区扫描通知和退出 flush 可能重复到达；相同内容复用 artifact。
    let artifact = if let Some(existing) = repository
        .find_by_content(
            update.session_id,
            "terminal_output",
            &checksum,
            bytes.len() as u64,
        )
        .await?
    {
        existing
    } else {
        let artifact_id = ArtifactId::new();
        let artifact = ArtifactRecord {
            artifact_id,
            session_id: update.session_id,
            turn_id,
            tool_call_id: update
                .process_id
                .strip_prefix("proc-")
                .and_then(|id| id.parse::<Uuid>().ok())
                .map(golutra_agent_core::ToolCallId),
            artifact_type: "terminal_output".to_owned(),
            uri: format!("artifact://terminal/{}/{artifact_id}", update.process_id),
            checksum,
            size_bytes: bytes.len() as u64,
            created_at: chrono::Utc::now(),
            producer: "process-supervisor".to_owned(),
            redaction_status,
            retention_policy: "debug_default".to_owned(),
            provenance_refs: Vec::new(),
        };
        repository.store(&artifact, bytes).await?;
        artifact
    };
    update.payload["output_artifact_ref"] = json!(artifact.artifact_id);
    update.payload["output_artifact_truncated"] = json!(truncated);
    Ok(())
}
