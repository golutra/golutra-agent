//! 贯穿模型循环与真实文件工具的恢复验收，确认已执行的副作用不会因断网再次执行。

use super::*;

struct WriteThenOffline {
    attempts: AtomicUsize,
    request_ids: Mutex<Vec<golutra_agent_core::ProviderRequestId>>,
    recovered_tool_result: AtomicBool,
}

#[async_trait]
impl LlmProvider for WriteThenOffline {
    async fn complete(&self, request: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        self.request_ids.lock().unwrap().push(request.request_id);
        let attempt = self.attempts.fetch_add(1, Ordering::SeqCst);
        if attempt == 0 {
            return MockProvider::tool_call(
                "write_file",
                json!({"path":"result.txt", "content":"written once"}),
            )
            .complete(request)
            .await;
        }
        assert!(
            request
                .messages
                .iter()
                .any(|message| message.role == ProviderRole::Tool)
        );
        if attempt < 6 {
            return Err(ProviderError::ConnectionFailed {
                message: "network temporarily offline".into(),
            });
        }
        self.recovered_tool_result.store(true, Ordering::SeqCst);
        MockProvider::text_response("File created; existing write result retained.")
            .complete(request)
            .await
    }

    fn contract(&self) -> golutra_agent_core::ProviderContract {
        MockProvider::text_response("").contract()
    }
}

#[tokio::test(start_paused = true)]
async fn network_recovery_after_write_keeps_the_result_and_executes_the_tool_once() {
    let workspace = tempdir().unwrap();
    let provider = WriteThenOffline {
        attempts: AtomicUsize::new(0),
        request_ids: Mutex::new(Vec::new()),
        recovered_tool_result: AtomicBool::new(false),
    };
    let executor = BasicToolExecutor::new(WorkspacePolicy::new(workspace.path()).unwrap());
    let agent = AgentLoop::new(provider, ContextBuilder::default(), executor);
    let mut events = Vec::new();
    let result = agent
        .run_with_trace(
            AgentTaskRequest {
                session_id: SessionId::new(),
                task_id: TaskId::new(),
                turn_id: TurnId::new(),
                objective: "create result.txt with written once".into(),
                completion_criteria: Vec::new(),
                output_schema: None,
                touched_code: false,
                contributors: Vec::new(),
                tools: vec!["write_file".into()],
            },
            |event| events.push(event),
        )
        .await
        .unwrap();
    assert!(agent.provider.recovered_tool_result.load(Ordering::SeqCst));
    assert_eq!(
        fs::read_to_string(workspace.path().join("result.txt")).unwrap(),
        "written once"
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, AgentLoopTraceEvent::ToolStarted { .. }))
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, AgentLoopTraceEvent::ProviderStarted { .. }))
            .count(),
        2
    );
    let ids = agent.provider.request_ids.lock().unwrap();
    assert!(ids[1..].iter().all(|id| id == &ids[1]));
    assert!(
        result
            .final_message
            .unwrap()
            .contains("existing write result retained")
    );
}

struct OfflineSummary;

#[async_trait]
impl LlmProvider for OfflineSummary {
    async fn complete(&self, _: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        Err(ProviderError::ConnectionFailed {
            message: "summary endpoint offline".into(),
        })
    }
    fn contract(&self) -> golutra_agent_core::ProviderContract {
        MockProvider::text_response("").contract()
    }
}

#[tokio::test(start_paused = true)]
async fn offline_semantic_compaction_retains_the_local_summary_and_recent_context() {
    let workspace = tempdir().unwrap();
    let agent = AgentLoop::new(
        OfflineSummary,
        ContextBuilder::default(),
        BasicToolExecutor::new(WorkspacePolicy::new(workspace.path()).unwrap()),
    );
    let task = AgentTaskRequest {
        session_id: SessionId::new(),
        task_id: TaskId::new(),
        turn_id: TurnId::new(),
        objective: "continue without changing public API".into(),
        completion_criteria: Vec::new(),
        output_schema: None,
        touched_code: false,
        contributors: Vec::new(),
        tools: Vec::new(),
    };
    let messages = (0..20)
        .map(|index| ProviderMessage {
            role: ProviderRole::User,
            content: format!(
                "public API must remain stable; step {index}: {}",
                "task fact ".repeat(60)
            ),
            tool_call_id: None,
            tool_name: None,
            tool_calls: Vec::new(),
            metadata: Default::default(),
        })
        .collect::<Vec<_>>();
    let mut record = ContextWindowManager::new(512)
        .compact_if_needed(task.turn_id, 0, &messages, &[], 0)
        .unwrap()
        .unwrap();
    assert!(record.supports_model_summary());
    let local_summary = record.summary.clone();
    let (_handle, mut control) = agent_execution_channel(1);
    let mut events = Vec::new();
    let mut cost = None;
    let cache = golutra_agent_llm::PromptCacheScope::session(task.session_id, None);
    let started = tokio::time::Instant::now();
    let result = agent
        .semantic_compaction_summary(
            &task,
            &cache,
            task.turn_id,
            &mut record,
            None,
            &mut control,
            &mut |event| events.push(event),
            &mut cost,
        )
        .await;
    assert!(
        result.is_none(),
        "failed auxiliary summary uses the already-built local fallback"
    );
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(record.summary, local_summary);
    assert!(
        record
            .replacement_messages
            .iter()
            .any(|message| message.content.contains("public API"))
    );
    assert!(
        events
            .iter()
            .all(|event| !matches!(event, AgentLoopTraceEvent::ProviderStreamed { .. }))
    );
}

#[cfg(unix)]
#[tokio::test]
async fn local_background_process_finishes_during_network_wait_and_is_not_restarted() {
    struct BackgroundProvider {
        calls: AtomicUsize,
        marker: PathBuf,
    }
    #[async_trait]
    impl LlmProvider for BackgroundProvider {
        async fn complete(
            &self,
            request: ProviderRequest,
        ) -> Result<ProviderResponse, ProviderError> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            let tool = match call {
                0 => Some((
                    "shell",
                    json!({"argv":["sh", "background.sh"], "background":true, "yield_time_ms":1}),
                )),
                2 => {
                    return Err(ProviderError::ConnectionFailed {
                        message: "offline while job runs".into(),
                    });
                }
                1 | 3 => {
                    if call == 3 {
                        assert_eq!(fs::read_to_string(&self.marker).unwrap(), "finished\n");
                    }
                    let process_id = request
                        .messages
                        .iter()
                        .filter(|message| message.role == ProviderRole::Tool)
                        .filter_map(|message| {
                            serde_json::from_str::<serde_json::Value>(&message.content).ok()
                        })
                        .find_map(|value| find_process_id(&value))
                        .expect("persisted process ID in tool result");
                    Some((
                        "shell_session",
                        json!({"action":"wait", "process_id":process_id, "wait_ms":100, "wait_for_terminal":true}),
                    ))
                }
                _ => None,
            };
            let mut response =
                MockProvider::text_response("Background job finished and was observed.")
                    .complete(request)
                    .await?;
            if let Some((name, arguments)) = tool {
                response.message = None;
                response.finish_reason = ProviderFinishReason::ToolCalls;
                response.tool_calls = vec![ProviderToolCall {
                    tool_call_id: format!("background-{call}"),
                    tool_name: name.into(),
                    arguments,
                }];
            }
            Ok(response)
        }
        fn contract(&self) -> golutra_agent_core::ProviderContract {
            MockProvider::text_response("").contract()
        }
    }
    fn find_process_id(value: &serde_json::Value) -> Option<String> {
        if let Some(id) = value.get("process_id").and_then(serde_json::Value::as_str) {
            return Some(id.to_owned());
        }
        value
            .as_object()
            .and_then(|object| object.values().find_map(find_process_id))
    }
    let workspace = tempdir().unwrap();
    fs::write(
        workspace.path().join("background.sh"),
        "sleep 2\nprintf 'finished\\n' >> marker.txt\n",
    )
    .unwrap();
    let provider = BackgroundProvider {
        calls: AtomicUsize::new(0),
        marker: workspace.path().join("marker.txt"),
    };
    let executor = BasicToolExecutor::new(
        WorkspacePolicy::new(workspace.path())
            .unwrap()
            .with_unrestricted_access(true),
    );
    let agent = AgentLoop::new(provider, ContextBuilder::default(), executor);
    let result = tokio::time::timeout(
        Duration::from_secs(20),
        agent.run(AgentTaskRequest {
            session_id: SessionId::new(),
            task_id: TaskId::new(),
            turn_id: TurnId::new(),
            objective: "Run background.sh and wait for completion".into(),
            completion_criteria: Vec::new(),
            output_schema: None,
            touched_code: false,
            contributors: Vec::new(),
            tools: vec!["shell".into(), "shell_session".into()],
        }),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        fs::read_to_string(workspace.path().join("marker.txt")).unwrap(),
        "finished\n"
    );
    assert_eq!(
        result
            .tool_reports
            .iter()
            .filter(|report| report.envelope.tool_name == "shell")
            .count(),
        1
    );
    assert!(
        result
            .tool_reports
            .iter()
            .any(|report| report.envelope.tool_name == "shell_session"
                && report.envelope.status == ToolResultStatus::Ok)
    );
    let waits = result
        .tool_reports
        .iter()
        .filter(|report| report.envelope.tool_name == "shell_session")
        .map(|report| &report.envelope.structured_facts)
        .collect::<Vec<_>>();
    assert_eq!(waits.len(), 2);
    assert_eq!(waits[0]["terminal"], false);
    assert_eq!(waits[1]["terminal"], true);
    assert_eq!(waits[0]["process_id"], waits[1]["process_id"]);
    assert_eq!(waits[0]["authoritative_pid"], waits[1]["authoritative_pid"]);
}
