use super::*;

fn pending(content: &str, steer: bool) -> PendingAgentTurn {
    PendingAgentTurn {
        command_id: CommandId::new(),
        turn_id: TurnId::new(),
        content: content.into(),
        task_contract: None,
        output_schema: None,
        external_verifiers: Vec::new(),
        max_elapsed_ms: None,
        defer_external_verification: false,
        external_verifiers_require_os_sandbox: false,
        allow_network: false,
        yolo: false,
        steer,
    }
}

#[tokio::test]
async fn steering_batch_respects_durability_and_preserves_both_queue_orders() {
    let (handle, control) = agent_execution_channel(8);
    let follow1 = pending("follow one", false);
    let follow2 = pending("follow two", false);
    let steer1 = pending("steer one", true);
    let steer2 = pending("steer two", true);
    handle.append_turn(follow1.clone()).await.unwrap();
    let reserved = handle.reserve_turn(steer1.clone()).await.unwrap();
    handle.append_turn(follow2.clone()).await.unwrap();
    handle.append_turn(steer2.clone()).await.unwrap();
    assert!(control.pending_turns.take_ready_steers().is_empty());
    reserved.commit();
    let mut batch = control.pending_turns.take_ready_steers();
    assert_eq!(batch.len(), 2);
    assert_legacy_taken_turn(batch.pop_front(), steer1);
    assert_legacy_taken_turn(batch.pop_front(), steer2);
    assert_legacy_taken_turn(control.pending_turns.take_or_close().await, follow1);
    assert_legacy_taken_turn(control.pending_turns.take_or_close().await, follow2);
    assert!(control.pending_turns.take_or_close().await.is_none());
}

#[tokio::test]
async fn completed_response_prioritizes_steers_before_followups() {
    let (handle, control) = agent_execution_channel(4);
    let follow = pending("later task", false);
    let steer = pending("current correction", true);
    handle.append_turn(follow.clone()).await.unwrap();
    handle.append_turn(steer.clone()).await.unwrap();
    assert_legacy_taken_turn(control.pending_turns.take_or_close().await, steer);
    assert_legacy_taken_turn(control.pending_turns.take_or_close().await, follow);
}

#[tokio::test]
async fn steering_batch_shares_provider_request_while_followups_get_separate_requests() {
    let workspace = tempdir().unwrap();
    fs::write(workspace.path().join("README.md"), "fixture").unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let requests = Arc::new(Mutex::new(Vec::new()));
    let provider = PrefixContractProvider {
        calls: calls.clone(),
        requests: requests.clone(),
        contract: MockProvider::text_response("unused").contract(),
    };
    let executor = BasicToolExecutor::new(WorkspacePolicy::new(workspace.path()).unwrap());
    let agent = AgentLoop::new(provider, ContextBuilder::default(), executor);
    let (handle, control) = agent_execution_channel(8);
    let follow1 = pending("FOLLOW_ONE", false);
    let follow2 = pending("FOLLOW_TWO", false);
    let steer1 = pending("STEER_ONE", true);
    let steer2 = pending("STEER_TWO", true);
    for turn in [&follow1, &follow2, &steer1, &steer2] {
        handle.append_turn(turn.clone()).await.unwrap();
    }
    let mut trace = Vec::new();
    let outcome = agent
        .run_with_control_and_trace(
            AgentTaskRequest {
                session_id: SessionId::new(),
                task_id: TaskId::new(),
                turn_id: TurnId::new(),
                objective: "inspect README.md".into(),
                completion_criteria: Vec::new(),
                output_schema: None,
                touched_code: false,
                contributors: Vec::new(),
                tools: vec!["read_file".into()],
            },
            control,
            |event| trace.push(event),
        )
        .await
        .unwrap();
    assert_eq!(outcome.final_turn_id, follow2.turn_id);
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 4);
    let users = |index: usize| {
        requests[index]
            .messages
            .iter()
            .filter(|message| message.role == ProviderRole::User)
            .map(|message| message.content.as_str())
            .filter(|content| content.starts_with("STEER_") || content.starts_with("FOLLOW_"))
            .collect::<Vec<_>>()
    };
    assert_eq!(users(1), vec!["STEER_ONE", "STEER_TWO"]);
    assert_eq!(users(2), vec!["STEER_ONE", "STEER_TWO", "FOLLOW_ONE"]);
    assert_eq!(
        users(3),
        vec!["STEER_ONE", "STEER_TWO", "FOLLOW_ONE", "FOLLOW_TWO"]
    );
    let starts = trace
        .iter()
        .filter_map(|event| match event {
            AgentLoopTraceEvent::PendingTurnStarted(turn) => Some(turn.turn_id),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        starts,
        vec![
            steer1.turn_id,
            steer2.turn_id,
            follow1.turn_id,
            follow2.turn_id
        ]
    );
}
