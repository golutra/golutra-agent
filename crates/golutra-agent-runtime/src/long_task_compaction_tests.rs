//! 多次真实循环压缩、用户追加要求和连接恢复的组合验收；摘要夹具只继承实际输入中的要求。

use super::*;

const ORIGINAL: &str = "Inspect every input file; preserve the public API.";
const STEER: &str = "Also report the checksum; do not modify configuration.";
const ROUNDS: usize = 12;
const FIRST_FACT: &str = "STAGE_ONE_OBSERVED_73X";
const FRESH_FACT: &str = "EXTERNAL_CHANGE_AFTER_OUTAGE_73X";

struct CompactionProvider {
    steps: AtomicUsize,
    summaries: AtomicUsize,
    disconnected: AtomicBool,
    requests: Mutex<Vec<ProviderRequest>>,
    handle: AgentExecutionHandle,
    workspace: PathBuf,
    outages: AtomicUsize,
}

#[async_trait]
impl LlmProvider for CompactionProvider {
    async fn complete(&self, request: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        if request.messages.first().is_some_and(|m| {
            m.content
                .starts_with("You are a context summarization assistant")
        }) {
            self.summaries.fetch_add(1, Ordering::SeqCst);
            let source: Value =
                serde_json::from_str(&request.messages.last().unwrap().content).unwrap();
            let mut retained = source["previous_summary"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
            for message in source["history"].as_array().unwrap() {
                let text = message["content"].as_str().unwrap_or_default();
                // 故意不复述用户要求，验证宿主保留的原文不会依赖摘要质量。

                // 只从真实工具观察提取里程碑；已压缩事实必须通过 previous_summary 继续传递。
                if message["role"] == "tool" {
                    for fact in [FIRST_FACT, FRESH_FACT] {
                        if text.contains(fact) && !retained.contains(fact) {
                            retained.push('\n');
                            retained.push_str(fact);
                        }
                    }
                }
            }
            // 夹具不替模型补造已丢失的原始要求，缺失会在下一次主请求断言中暴露。
            return MockProvider::text_response(retained)
                .complete(request)
                .await;
        }
        let index = self.steps.load(Ordering::SeqCst);
        if index == 5 && self.outages.fetch_add(1, Ordering::SeqCst) < 5 {
            self.disconnected.store(true, Ordering::SeqCst);
            fs::write(self.workspace.join("input-0.txt"), FRESH_FACT).unwrap();
            return Err(ProviderError::ConnectionFailed {
                message: "offline between compacted steps".into(),
            });
        }
        self.steps.fetch_add(1, Ordering::SeqCst);
        if index == ROUNDS + 1 {
            // 断网阶段使用虚拟时钟；独立验收启动真实进程前恢复实时时钟，避免虚假超时。
            tokio::time::resume();
        }
        if index > 0 {
            assert!(
                request
                    .messages
                    .iter()
                    .any(|m| m.content.contains(FIRST_FACT)),
                "completed observation lost at step {index}"
            );
        }
        if index > 6 {
            assert!(
                request
                    .messages
                    .iter()
                    .any(|m| m.content.contains(FRESH_FACT)),
                "fresh fact lost at step {index}"
            );
        }
        assert!(
            request
                .messages
                .iter()
                .any(|m| m.content.contains(ORIGINAL)),
            "original objective lost at step {index}"
        );
        if index > 2 {
            assert!(
                request.messages.iter().any(|m| m.content.contains(STEER)),
                "steering lost at step {index}"
            );
        }
        let calls = request
            .messages
            .iter()
            .flat_map(|m| &m.tool_calls)
            .map(|call| call.tool_call_id.as_str())
            .collect::<HashSet<_>>();
        let results = request
            .messages
            .iter()
            .filter(|m| m.role == ProviderRole::Tool)
            .filter_map(|m| m.tool_call_id.as_deref())
            .collect::<HashSet<_>>();
        assert_eq!(calls, results, "tool pairs diverged after compaction");
        self.requests.lock().unwrap().push(request.clone());
        if index == 2 {
            self.handle
                .append_turn(PendingAgentTurn {
                    command_id: CommandId::new(),
                    turn_id: TurnId::new(),
                    content: STEER.into(),
                    task_contract: None,
                    output_schema: None,
                    external_verifiers: Vec::new(),
                    max_elapsed_ms: None,
                    defer_external_verification: false,
                    external_verifiers_require_os_sandbox: false,
                    allow_network: false,
                    yolo: false,
                    steer: true,
                })
                .await
                .unwrap();
        }
        let mut response = MockProvider::text_response(
            "All inputs inspected; public API and configuration unchanged; checksum reported.",
        )
        .complete(request)
        .await?;
        // 可控的长观察保证跨越多个上下文窗口，不伪造很低的 provider 用量覆盖本地估计。
        response.usage.input_tokens = None;
        response.usage.usage_source = UsageSource::Unknown;
        if index <= ROUNDS {
            response.finish_reason = ProviderFinishReason::ToolCalls;
            response.tool_calls = vec![ProviderToolCall {
                tool_call_id: format!("read-{index}"),
                tool_name: if index == ROUNDS {
                    "write_file"
                } else {
                    "read_file"
                }
                .into(),
                arguments: if index == ROUNDS {
                    json!({"path":"result.txt", "content":FRESH_FACT})
                } else {
                    json!({"path":format!("input-{}.txt", if index == 6 { 0 } else { index })})
                },
            }];
            response.message = Some(ProviderMessage {
                role: ProviderRole::Assistant,
                content: format!(
                    "Observation {index}: {}",
                    "bounded context detail ".repeat(350)
                ),
                tool_call_id: None,
                tool_name: None,
                tool_calls: Vec::new(),
                metadata: Default::default(),
            });
        }
        Ok(response)
    }

    fn contract(&self) -> ProviderContract {
        let mut contract = MockProvider::text_response("").contract();
        contract.native_protocol = "compaction-test".into();
        contract
    }
}

#[tokio::test(start_paused = true)]
async fn multiple_compactions_preserve_objective_steering_tool_pairs_and_network_continuation() {
    let workspace = tempdir().unwrap();
    for index in 0..ROUNDS {
        fs::write(
            workspace.path().join(format!("input-{index}.txt")),
            if index == 0 {
                FIRST_FACT.to_owned()
            } else {
                format!("fact {index}")
            },
        )
        .unwrap();
    }
    let (handle, control) = agent_execution_channel(4);
    let provider = CompactionProvider {
        steps: AtomicUsize::new(0),
        summaries: AtomicUsize::new(0),
        disconnected: AtomicBool::new(false),
        requests: Mutex::new(Vec::new()),
        handle,
        workspace: workspace.path().into(),
        outages: AtomicUsize::new(0),
    };
    let agent = AgentLoop::new(
        provider,
        ContextBuilder::new(ContextBudgetPolicy {
            context_window: 6_000,
            max_output: 1_000,
            budget_limit: 4_500,
            action_if_exceeded: BudgetOverflowAction::Compact,
        }),
        BasicToolExecutor::new(WorkspacePolicy::new(workspace.path()).unwrap()),
    )
    .with_external_verifiers(vec![ExternalVerificationSpec {
        program: "cmp".into(),
        args: vec!["input-0.txt".into(), "result.txt".into()],
        cwd: ".".into(),
        timeout_ms: 5_000,
        expected_exit_code: 0,
        max_output_bytes: 1024,
    }]);
    let mut trace = Vec::new();
    let outcome = agent
        .run_with_control_and_trace(
            AgentTaskRequest {
                session_id: SessionId::new(),
                task_id: TaskId::new(),
                turn_id: TurnId::new(),
                objective: ORIGINAL.into(),
                completion_criteria: Vec::new(),
                output_schema: None,
                touched_code: false,
                contributors: vec![ContextContributor {
                    name: "objective".into(),
                    role: ProviderRole::User,
                    content: ORIGINAL.into(),
                    token_budget_hint: 0,
                    source_refs: Vec::new(),
                }],
                tools: vec!["read_file".into(), "write_file".into()],
            },
            control,
            |e| trace.push(e),
        )
        .await
        .unwrap();
    assert_eq!(
        outcome.loop_decision.action,
        LoopAction::StopSuccess,
        "{:?} {:?}",
        outcome.loop_decision,
        outcome.verification.checks
    );
    assert_eq!(agent.provider.steps.load(Ordering::SeqCst), ROUNDS + 2);
    assert_eq!(
        fs::read_to_string(workspace.path().join("result.txt")).unwrap(),
        FRESH_FACT
    );
    assert!(agent.provider.summaries.load(Ordering::SeqCst) >= 3);
    assert!(
        trace
            .iter()
            .filter(|e| matches!(e, AgentLoopTraceEvent::ContextAutoCompacted(_)))
            .count()
            >= 3
    );
    assert_eq!(
        trace
            .iter()
            .filter(|e| matches!(e, AgentLoopTraceEvent::ToolStarted { .. }))
            .count(),
        ROUNDS + 2
    );
    assert!(agent.provider.disconnected.load(Ordering::SeqCst));
    assert_eq!(
        trace
            .iter()
            .filter(|e| matches!(e, AgentLoopTraceEvent::CandidateReady { .. }))
            .count(),
        1
    );
}
