//! 穿越旧工具次数与无进展阈值，验证完成仍由实际响应及显式控制决定。

use super::*;

struct LongReadProvider(AtomicUsize);

#[async_trait]
impl LlmProvider for LongReadProvider {
    async fn complete(&self, request: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        if request.messages.first().is_some_and(|message| {
            message
                .content
                .starts_with("You are a context summarization assistant")
        }) {
            return MockProvider::text_response(
                "Repeatedly inspected input.txt. Continue the same task.",
            )
            .complete(request)
            .await;
        }
        let index = self.0.fetch_add(1, Ordering::SeqCst);
        let mut response = MockProvider::text_response("Inspection completed.")
            .complete(request)
            .await?;
        if index < 300 {
            response.finish_reason = ProviderFinishReason::ToolCalls;
            response.message = None;
            response.tool_calls = vec![ProviderToolCall {
                tool_call_id: format!("read-{index}"),
                tool_name: "read_file".into(),
                arguments: json!({"path":"input.txt"}),
            }];
        }
        Ok(response)
    }
    fn contract(&self) -> ProviderContract {
        MockProvider::text_response("").contract()
    }
}

#[tokio::test]
async fn three_hundred_repeated_tools_finish_without_an_implicit_cap() {
    let workspace = tempdir().unwrap();
    fs::write(workspace.path().join("input.txt"), "data").unwrap();
    let harness = AgentHarness::new(
        LongReadProvider(AtomicUsize::new(0)),
        ContextBuilder::default(),
        BasicToolExecutor::new(WorkspacePolicy::new(workspace.path()).unwrap()),
    );
    let request = AgentTaskRequest {
        session_id: SessionId::new(),
        task_id: TaskId::new(),
        turn_id: TurnId::new(),
        objective: "Inspect input.txt".into(),
        completion_criteria: Vec::new(),
        output_schema: None,
        touched_code: false,
        contributors: Vec::new(),
        tools: vec!["read_file".into()],
    };
    let (_handle, control) = agent_execution_channel(1);
    let outcome = harness
        .execute_configured(
            ConfiguredAgentRun::new(request).with_execution_mode(Some(AgentExecutionMode::Open)),
            control,
            |_| {},
        )
        .await
        .unwrap();
    assert_eq!(outcome.tool_reports.len(), 300);
    assert_eq!(outcome.loop_decision.action, LoopAction::StopSuccess);
}
