use super::*;

#[tokio::test]
async fn fork_uses_child_instructions_and_tools_while_preserving_parent_tool_pairs() {
    let workspace = tempdir().unwrap();
    let executor = BasicToolExecutor::new(WorkspacePolicy::new(workspace.path()).unwrap())
        .without_tool("subagent");
    let loop_ = AgentLoop::new(
        MockProvider::text_response("analysis complete"),
        ContextBuilder::default(),
        executor,
    );
    let message = |role, content: &str| ProviderMessage {
        role,
        content: content.to_owned(),
        tool_call_id: None,
        tool_name: None,
        tool_calls: vec![],
        metadata: Default::default(),
    };
    let mut call = message(ProviderRole::Assistant, "reading");
    call.tool_calls.push(ProviderToolCall {
        tool_call_id: "parent-read".to_owned(),
        tool_name: "read_file".to_owned(),
        arguments: json!({"path":"a.rs"}),
    });
    let mut result = message(ProviderRole::Tool, "actual parent observation");
    result.tool_call_id = Some("parent-read".to_owned());
    result.tool_name = Some("read_file".to_owned());
    let inherited = vec![
        message(ProviderRole::System, "parent-only instructions"),
        message(ProviderRole::User, "parent objective"),
        call,
        result,
        message(ProviderRole::User, "inspect safely"),
    ];
    let task = AgentTaskRequest {
        session_id: SessionId::new(),
        task_id: TaskId::new(),
        turn_id: TurnId::new(),
        objective: "inspect safely".to_owned(),
        completion_criteria: vec![],
        output_schema: None,
        touched_code: false,
        contributors: vec![ContextContributor {
            name: "system".to_owned(),
            role: ProviderRole::System,
            content: "child-only instructions".to_owned(),
            token_budget_hint: 0,
            source_refs: vec![],
        }],
        tools: vec!["read_file".to_owned(), "write_file".to_owned()],
    };
    let run = AgentRun::new(task)
        .with_task_contract(TaskContract {
            workspace_change: WorkspaceChangeRequirement::Forbidden,
            ..TaskContract::default()
        })
        .with_replay_context(AgentReplayContext::for_fork(inherited.clone()));
    let (_, control) = agent_execution_channel(1);
    let mut trace = vec![];
    loop_
        .run_with_control_trace_contract_and_replay_context(
            run,
            control,
            |event| trace.push(event),
            AgentTurnOverrides::default(),
        )
        .await
        .unwrap();
    let request = trace
        .iter()
        .find_map(|event| match event {
            AgentLoopTraceEvent::ContextSnapshotCaptured { request, .. } => Some(request),
            _ => None,
        })
        .unwrap();
    assert_eq!(request.messages[0].content, "child-only instructions");
    assert_eq!(request.messages[1..], inherited[1..]);
    assert!(
        !request
            .tools
            .iter()
            .any(|tool| matches!(tool.tool_name.as_str(), "write_file" | "subagent"))
    );
    assert!(
        request
            .tools
            .iter()
            .any(|tool| tool.tool_name == "read_file")
    );
}

#[derive(Debug)]
struct ConcurrentChildren {
    barrier: tokio::sync::Barrier,
    checkpoints: Arc<AtomicUsize>,
    finished: AtomicUsize,
}

#[async_trait]
impl golutra_agent_tools::TaskDelegationBackend for ConcurrentChildren {
    async fn delegate(
        &self,
        request: &ToolRequest,
        _: CancellationToken,
    ) -> Result<golutra_agent_tools::TaskDelegationOutput, ToolError> {
        assert_eq!(self.checkpoints.load(Ordering::SeqCst), 2);
        tokio::time::timeout(Duration::from_secs(2), self.barrier.wait())
            .await
            .map_err(|_| ToolError::Execution("child launches were serialized".to_owned()))?;
        self.finished.fetch_add(1, Ordering::SeqCst);
        Ok(golutra_agent_tools::TaskDelegationOutput {
            status: ToolResultStatus::Ok,
            summary: "running".to_owned(),
            content: String::new(),
            structured_facts: json!({"child_session_id":request.provider_tool_call_id,"completed":false}),
        })
    }

    async fn notifications(
        &self,
        _: SessionId,
    ) -> Result<Vec<golutra_agent_tools::DelegationNotification>, ToolError> {
        if self.finished.load(Ordering::SeqCst) < 2 {
            return Ok(Vec::new());
        }
        Ok(vec![golutra_agent_tools::DelegationNotification {
            id: "child-event".to_owned(),
            child_session_id: "one".to_owned(),
            child_task_id: Some("task-one".to_owned()),
            content: "child-result-observation".to_owned(),
        }])
    }
}

#[derive(Debug)]
struct ChildCheckpoints(Arc<AtomicUsize>);

#[async_trait]
impl BeforeSideEffectRecorder for ChildCheckpoints {
    async fn persist_before_side_effect(
        &self,
        _: &ToolRequest,
        _: &[golutra_agent_tools::FileBeforeImage],
        _: bool,
    ) -> Result<(), AgentLoopError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[derive(Debug)]
struct ChildProvider(AtomicUsize);

#[async_trait]
impl LlmProvider for ChildProvider {
    async fn complete(&self, request: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        let turn = self.0.fetch_add(1, Ordering::SeqCst);
        assert!(turn < 2, "completion notification must not be redelivered");
        if turn == 1 {
            assert_eq!(
                request
                    .messages
                    .iter()
                    .filter(|message| message.content == "child-result-observation")
                    .count(),
                1
            );
        }
        let mut response =
            MockProvider::text_response("Children were started and the first result was received.")
                .complete(request)
                .await?;
        if turn == 0 {
            response.message = None;
            response.finish_reason = ProviderFinishReason::ToolCalls;
            response.tool_calls = ["one", "two"]
                .into_iter()
                .map(|id| ProviderToolCall {
                    tool_call_id: id.to_owned(),
                    tool_name: "subagent".to_owned(),
                    arguments: json!({"task":format!("inspect {id}"),"run_in_background":true}),
                })
                .collect();
        }
        Ok(response)
    }
    fn contract(&self) -> golutra_agent_core::ProviderContract {
        MockProvider::text_response("unused").contract()
    }
}

#[tokio::test]
async fn background_children_launch_concurrently_after_all_checkpoints_and_notify_once() {
    let root = tempdir().unwrap();
    let checkpoints = Arc::new(AtomicUsize::new(0));
    let executor = BasicToolExecutor::new(
        WorkspacePolicy::new(root.path())
            .unwrap()
            .with_unrestricted_access(true),
    )
    .with_task_delegation_backend(Arc::new(ConcurrentChildren {
        barrier: tokio::sync::Barrier::new(2),
        checkpoints: checkpoints.clone(),
        finished: AtomicUsize::new(0),
    }))
    .unwrap();
    let harness = AgentHarness::new(
        ChildProvider(AtomicUsize::new(0)),
        ContextBuilder::default(),
        executor,
    )
    .with_before_side_effect_recorder(Arc::new(ChildCheckpoints(checkpoints)));
    let (_, control) = agent_execution_channel(1);
    let outcome = harness
        .execute_configured(
            ConfiguredAgentRun::new(AgentTaskRequest {
                session_id: SessionId::new(),
                task_id: TaskId::new(),
                turn_id: TurnId::new(),
                objective: "Delegate independent investigations".to_owned(),
                completion_criteria: Vec::new(),
                output_schema: None,
                touched_code: false,
                contributors: Vec::new(),
                tools: vec!["subagent".to_owned()],
            }),
            control,
            |_| {},
        )
        .await
        .unwrap();
    let reports: Vec<_> = outcome
        .tool_reports
        .iter()
        .filter(|report| report.envelope.tool_name == "subagent")
        .collect();
    assert_eq!(reports.len(), 2);
    for report in reports {
        assert_eq!(report.envelope.status, ToolResultStatus::Ok);
        assert_eq!(
            report.envelope.structured_facts["execution_mode"],
            "parallel_tool_batch"
        );
        assert_eq!(report.envelope.structured_facts["dispatch_batch_size"], 2);
    }
}

#[test]
fn child_target_overlap_and_lifecycle_actions_are_batch_barriers() {
    let mut batch =
        ParallelBatchKind::SubagentWait(BTreeSet::from(["one".to_owned(), "two".to_owned()]));
    assert!(!extend_parallel_batch(
        &mut batch,
        ParallelBatchKind::SubagentWait(BTreeSet::from(["two".to_owned()]))
    ));
    assert!(extend_parallel_batch(
        &mut batch,
        ParallelBatchKind::SubagentWait(BTreeSet::from(["three".to_owned()]))
    ));
    assert!(!extend_parallel_batch(
        &mut batch,
        ParallelBatchKind::Exclusive
    ));
    let mut resumes = ParallelBatchKind::SubagentStart(BTreeSet::from(["one".to_owned()]));
    assert!(!extend_parallel_batch(
        &mut resumes,
        ParallelBatchKind::SubagentStart(BTreeSet::from(["one".to_owned()]))
    ));
    assert!(extend_parallel_batch(
        &mut resumes,
        ParallelBatchKind::SubagentStart(BTreeSet::new())
    ));
}
