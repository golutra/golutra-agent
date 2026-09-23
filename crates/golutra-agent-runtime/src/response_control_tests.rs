//! 通过实际 AgentLoop 验证协议续写、完成边界和截断工具的副作用隔离。

use super::*;

struct TerminalProvider {
    reasons: Vec<ProviderFinishReason>,
    requests: Mutex<Vec<ProviderRequest>>,
    tool: bool,
}

#[async_trait]
impl LlmProvider for TerminalProvider {
    async fn complete(&self, request: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        let index = {
            let mut requests = self.requests.lock().unwrap();
            let index = requests.len();
            requests.push(request.clone());
            index
        };
        let mut response = if self.tool {
            MockProvider::tool_call(
                "write_file",
                json!({"path":"must-not-exist", "content":"bad"}),
            )
            .complete(request)
            .await?
        } else {
            MockProvider::text_response(format!("part {index}"))
                .complete(request)
                .await?
        };
        response.finish_reason = self.reasons[index.min(self.reasons.len() - 1)];
        Ok(response)
    }

    fn contract(&self) -> ProviderContract {
        MockProvider::text_response("").contract()
    }
}

fn task() -> AgentTaskRequest {
    AgentTaskRequest {
        session_id: SessionId::new(),
        task_id: TaskId::new(),
        turn_id: TurnId::new(),
        objective: "Finish the original task".into(),
        completion_criteria: Vec::new(),
        output_schema: None,
        touched_code: false,
        contributors: Vec::new(),
        tools: vec!["write_file".into()],
    }
}

#[tokio::test]
async fn truncated_and_explicit_continuation_keep_history_and_only_stop_at_normal_end() {
    let workspace = tempdir().unwrap();
    let provider = TerminalProvider {
        reasons: vec![
            ProviderFinishReason::Length,
            ProviderFinishReason::Continue,
            ProviderFinishReason::Stop,
        ],
        requests: Mutex::new(Vec::new()),
        tool: false,
    };
    let agent = AgentLoop::new(
        provider,
        ContextBuilder::default(),
        BasicToolExecutor::new(WorkspacePolicy::new(workspace.path()).unwrap()),
    );
    let mut events = Vec::new();
    let outcome = agent
        .run_with_trace(task(), |event| events.push(event))
        .await
        .unwrap();
    assert_eq!(outcome.loop_decision.action, LoopAction::StopSuccess);
    assert_eq!(outcome.final_message.as_deref(), Some("part 2"));
    let requests = agent.provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    assert_eq!(
        requests[2].messages.len(),
        requests[1].messages.len() + 1,
        "explicit continuation adds only the previous assistant message, not another runtime prompt"
    );
    assert!(
        requests
            .iter()
            .all(|r| r.turn_id == requests[0].turn_id && r.task_id == requests[0].task_id)
    );
    for (index, request) in requests.iter().enumerate().skip(1) {
        for part in 0..index {
            assert!(
                request
                    .messages
                    .iter()
                    .any(|m| m.content == format!("part {part}"))
            );
        }
    }
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, AgentLoopTraceEvent::CandidateReady { .. }))
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, AgentLoopTraceEvent::TokenUsageRecorded(_)))
            .count(),
        3
    );
}

#[tokio::test]
async fn unsuccessful_terminal_responses_never_execute_tools_or_claim_completion() {
    for reason in [
        ProviderFinishReason::Length,
        ProviderFinishReason::ContentFilter,
        ProviderFinishReason::Error,
        ProviderFinishReason::Unknown,
    ] {
        let workspace = tempdir().unwrap();
        let agent = AgentLoop::new(
            TerminalProvider {
                reasons: vec![reason],
                requests: Mutex::new(Vec::new()),
                tool: true,
            },
            ContextBuilder::default(),
            BasicToolExecutor::new(WorkspacePolicy::new(workspace.path()).unwrap()),
        );
        let mut events = Vec::new();
        assert!(
            agent
                .run_with_trace(task(), |e| events.push(e))
                .await
                .is_err()
        );
        assert!(!workspace.path().join("must-not-exist").exists());
        assert!(!events.iter().any(|e| matches!(
            e,
            AgentLoopTraceEvent::ToolStarted { .. } | AgentLoopTraceEvent::CandidateReady { .. }
        )));
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentLoopTraceEvent::ProviderFailed { .. }))
        );
        assert_eq!(agent.provider.requests.lock().unwrap().len(), 1);
    }
}

#[tokio::test]
async fn differing_nonterminal_text_continues_beyond_old_limit() {
    let workspace = tempdir().unwrap();
    let agent = AgentLoop::new(
        TerminalProvider {
            reasons: [
                vec![ProviderFinishReason::Continue; 20],
                vec![ProviderFinishReason::Stop],
            ]
            .concat(),
            requests: Mutex::new(Vec::new()),
            tool: false,
        },
        ContextBuilder::default(),
        BasicToolExecutor::new(WorkspacePolicy::new(workspace.path()).unwrap()),
    );
    let mut events = Vec::new();
    let outcome = agent
        .run_with_trace(task(), |e| events.push(e))
        .await
        .unwrap();
    assert_eq!(outcome.loop_decision.action, LoopAction::StopSuccess);
    assert_eq!(agent.provider.requests.lock().unwrap().len(), 21);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentLoopTraceEvent::CandidateReady { .. }))
    );
}

#[tokio::test]
async fn incomplete_auxiliary_summary_retries_once_without_installing_partial_text() {
    struct SummaryProvider;
    #[async_trait]
    impl LlmProvider for SummaryProvider {
        async fn complete(
            &self,
            request: ProviderRequest,
        ) -> Result<ProviderResponse, ProviderError> {
            let mut response =
                MockProvider::text_response("truncated summary must not replace facts")
                    .complete(request)
                    .await?;
            response.finish_reason = ProviderFinishReason::Length;
            Ok(response)
        }
        fn contract(&self) -> ProviderContract {
            let mut contract = MockProvider::text_response("").contract();
            contract.native_protocol = "summary-test".into();
            contract
        }
    }
    let workspace = tempdir().unwrap();
    let agent = AgentLoop::new(
        SummaryProvider,
        ContextBuilder::default(),
        BasicToolExecutor::new(WorkspacePolicy::new(workspace.path()).unwrap()),
    );
    let task = task();
    let messages = (0..20)
        .map(|index| ProviderMessage {
            role: ProviderRole::User,
            content: format!("requirement {index}: {}", "fact ".repeat(100)),
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
    let (_, mut control) = agent_execution_channel(1);
    let mut trace = Vec::new();
    let result = agent
        .semantic_compaction_summary(
            &task,
            &PromptCacheScope::session(task.session_id, None),
            task.turn_id,
            &mut record,
            None,
            &mut control,
            &mut |e| trace.push(e),
            &mut None,
        )
        .await;
    assert!(result.is_none());
    assert_eq!(record.summary_attempts, 2);
    assert_eq!(
        record.summary_failure,
        Some(SummaryFailure::OutputTruncated.to_string())
    );
    assert_eq!(
        trace
            .iter()
            .filter(|e| matches!(e, AgentLoopTraceEvent::ProviderStarted { .. }))
            .count(),
        2
    );
    assert_eq!(
        trace
            .iter()
            .filter(|e| matches!(e, AgentLoopTraceEvent::TokenUsageRecorded(_)))
            .count(),
        2
    );
}
