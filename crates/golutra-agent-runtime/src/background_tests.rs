//! Passive results attach to existing work; only input or a sealed batch wakes it.

use super::*;
use base64::Engine;
use golutra_agent_core::{
    BackgroundBatchId, BackgroundResult, BackgroundResultId, BackgroundScope, BackgroundTaskId,
    BackgroundTaskStatus,
};

fn request() -> AgentTaskRequest {
    AgentTaskRequest {
        session_id: SessionId::new(),
        task_id: TaskId::new(),
        turn_id: TurnId::new(),
        objective: "Inspect the background result".into(),
        completion_criteria: Vec::new(),
        output_schema: None,
        touched_code: false,
        contributors: Vec::new(),
        tools: Vec::new(),
    }
}

#[test]
fn subagent_background_ack_does_not_repeat_findings_already_returned_by_wait() {
    let root = tempdir().unwrap();
    let executor = BasicToolExecutor::new(WorkspacePolicy::new(root.path()).unwrap());
    let task = request();
    let mut observation = result(&task);
    observation.tool_name = "subagent".into();
    observation.output.tool_name = "subagent".into();
    observation.output.structured_facts =
        json!({"child_session_id":"child","child_task_id":"execution","child_terminal":true});
    observation.output.model_visible_excerpt = Some("unique child findings".into());
    let mut plan = ContextBuilder::default()
        .build(task.task_id, task.turn_id, Vec::new())
        .unwrap();
    let mut tokens = plan.estimated_message_tokens();
    append_plan_message(&mut plan, ProviderMessage {role:ProviderRole::Tool,content:json!({"structured_facts":observation.output.structured_facts,"output":"unique child findings"}).to_string(),
        tool_call_id:Some("wait".into()),tool_name:Some("subagent".into()),tool_calls:Vec::new(),metadata:Default::default()},
        ContextMessageSource {contributor:"tool_result_excerpt".into(),source_refs:Vec::new(),origin:"tool_result".into(),visibility:ModelInputVisibility::ModelVisible},&mut tokens);
    let mut pending = Vec::new();
    background::append_results(
        &mut plan,
        vec![observation],
        &[],
        &executor,
        &mut HashSet::new(),
        &mut pending,
        &mut tokens,
    );
    assert_eq!(pending.len(), 1);
    assert_eq!(
        plan.messages
            .iter()
            .filter(|message| message.content.contains("unique child findings"))
            .count(),
        1
    );
    assert!(
        plan.messages
            .last()
            .unwrap()
            .content
            .contains("output_already_observed")
    );
}

fn result(request: &AgentTaskRequest) -> BackgroundResult {
    let report = objective_test_report("browser_run", None);
    BackgroundResult {
        result_id: BackgroundResultId::new(),
        background_task_id: BackgroundTaskId::new(),
        batch_id: BackgroundBatchId::new(),
        scope: BackgroundScope {
            workspace_id: WorkspaceId::new(),
            session_id: request.session_id,
            task_id: request.task_id,
            turn_id: request.turn_id,
        },
        tool_call_id: report.envelope.tool_call_id,
        tool_name: "browser_run".into(),
        status: BackgroundTaskStatus::Succeeded,
        summary: "Observed page title".into(),
        output: report.envelope,
        artifact_refs: Vec::new(),
        created_at: chrono::Utc::now(),
    }
}

fn follow_up() -> PendingAgentTurn {
    PendingAgentTurn {
        command_id: CommandId::new(),
        turn_id: TurnId::new(),
        content: "Use my newer instructions".into(),
        task_contract: None,
        output_schema: None,
        external_verifiers: Vec::new(),
        max_elapsed_ms: None,
        defer_external_verification: false,
        external_verifiers_require_os_sandbox: false,
        allow_network: false,
        yolo: false,
        steer: false,
    }
}

struct DeliveryProvider {
    handle: AgentExecutionHandle,
    result: BackgroundResult,
    mode: BackgroundDeliveryMode,
    follow_up: Option<PendingAgentTurn>,
    requests: Arc<Mutex<Vec<ProviderRequest>>>,
}

#[tokio::test]
async fn captured_child_results_ack_late_delivery_without_an_extra_model_turn() {
    // Completion arrives after request capture, as it can after a synchronous
    // wait. Only complete findings for the same execution may avoid sampling.
    for (batch, truncated, same_task, expected_requests) in [
        (false, false, true, 1),
        (true, false, true, 1),
        (true, true, true, 2),
        (true, false, false, 2),
    ] {
        let root = tempdir().unwrap();
        let task = request();
        let mut observation = result(&task);
        observation.tool_name = "subagent".into();
        observation.output.tool_name = "subagent".into();
        observation.output.structured_facts = json!({
            "child_session_id":"child", "child_task_id":"current", "child_terminal":true,
        });
        observation.output.model_visible_excerpt = Some("actual terminal findings".into());
        let facts = json!({"child_session_id":"child", "child_task_id":if same_task {"current"} else {"previous"},
            "child_terminal":true, "model_visible_truncated":truncated});
        let output = if batch {
            json!({"structured_facts":{"child_results":[{"child_session_id":"child", "facts":facts, "content":"actual terminal findings"}]}})
        } else {
            json!({"structured_facts":facts, "model_visible_excerpt":"actual terminal findings"})
        };
        let messages = vec![
            ProviderMessage {
                role: ProviderRole::Assistant,
                content: String::new(),
                tool_call_id: None,
                tool_name: None,
                tool_calls: vec![ProviderToolCall {
                    tool_call_id: "wait".into(),
                    tool_name: "subagent".into(),
                    arguments: json!({"action":"wait"}),
                }],
                metadata: Default::default(),
            },
            ProviderMessage {
                role: ProviderRole::Tool,
                content: output.to_string(),
                tool_call_id: Some("wait".into()),
                tool_name: Some("subagent".into()),
                tool_calls: vec![],
                metadata: Default::default(),
            },
        ];
        let (handle, control) = agent_execution_channel(4);
        let requests = Arc::new(Mutex::new(Vec::new()));
        let provider = DeliveryProvider {
            handle,
            result: observation.clone(),
            mode: BackgroundDeliveryMode::ContinueAfterBatch,
            follow_up: None,
            requests: requests.clone(),
        };
        let harness = AgentHarness::new(
            provider,
            ContextBuilder::default(),
            BasicToolExecutor::new(WorkspacePolicy::new(root.path()).unwrap()),
        );
        let mut trace = Vec::new();
        harness
            .execute_configured(
                ConfiguredAgentRun::new(task)
                    .with_replay_context(AgentReplayContext::for_fork(messages)),
                control,
                |event| trace.push(event),
            )
            .await
            .unwrap();
        assert_eq!(
            requests.lock().unwrap().len(),
            expected_requests,
            "batch={batch}, truncated={truncated}, same_task={same_task}"
        );
        let consumed = trace.iter().position(|event| matches!(event,
            AgentLoopTraceEvent::BackgroundResultsConsumed { result_ids, .. } if result_ids == &vec![observation.result_id])).unwrap();
        let ack_request = match &trace[consumed] {
            AgentLoopTraceEvent::BackgroundResultsConsumed { request_id, .. } => *request_id,
            _ => unreachable!(),
        };
        assert!(trace[..consumed].iter().any(|event| matches!(event,
            AgentLoopTraceEvent::ContextSnapshotCaptured {request, ..} if request.request_id == ack_request)));
    }
}

#[async_trait]
impl LlmProvider for DeliveryProvider {
    fn contract(&self) -> ProviderContract {
        MockProvider::text_response("").contract()
    }

    async fn complete(&self, request: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        let index = {
            let mut requests = self.requests.lock().unwrap();
            requests.push(request.clone());
            requests.len()
        };
        assert!(index <= 2, "duplicate result must not keep the loop alive");
        self.handle
            .enqueue_background_results(vec![self.result.clone()], self.mode)
            .unwrap();
        if index == 1
            && let Some(turn) = &self.follow_up
        {
            self.handle.append_turn(turn.clone()).await.unwrap();
        }
        MockProvider::text_response(if index == 1 {
            "Candidate completion."
        } else {
            "Done."
        })
        .complete(request)
        .await
    }
}

async fn run_delivery(
    mode: BackgroundDeliveryMode,
    follow: bool,
    max_iterations: u32,
) -> (
    AgentTaskRequest,
    BackgroundResult,
    AgentLoopOutcome,
    Vec<ProviderRequest>,
    Vec<AgentLoopTraceEvent>,
) {
    let root = tempdir().unwrap();
    let request = request();
    let result = result(&request);
    let (handle, control) = agent_execution_channel(8);
    let requests = Arc::new(Mutex::new(Vec::new()));
    let provider = DeliveryProvider {
        handle,
        result: result.clone(),
        mode,
        follow_up: follow.then(follow_up),
        requests: requests.clone(),
    };
    let agent = AgentLoop::new(
        provider,
        ContextBuilder::default(),
        BasicToolExecutor::new(WorkspacePolicy::new(root.path()).unwrap()),
    )
    .with_governor(RuntimeGovernor::new(GovernorLimits {
        max_iterations,
        ..Default::default()
    }));
    let mut trace = Vec::new();
    let outcome = agent
        .run_with_control_and_trace(request.clone(), control, |event| trace.push(event))
        .await
        .unwrap();
    let captured = requests.lock().unwrap().clone();
    (request, result, outcome, captured, trace)
}

#[tokio::test]
async fn passive_result_never_requests_an_extra_model_step() {
    let (_, _, _, requests, trace) = run_delivery(BackgroundDeliveryMode::Passive, false, 10).await;
    assert_eq!(requests.len(), 1);
    assert!(
        !trace
            .iter()
            .any(|event| matches!(event, AgentLoopTraceEvent::BackgroundResultsConsumed { .. }))
    );
}

#[test]
fn active_request_attaches_current_passive_results_in_bounded_pages() {
    let task = request();
    let (_, control) = agent_execution_channel(4);
    let queue = &control.pending_turns;
    queue
        .set_background_turn(task.session_id, task.task_id, task.turn_id)
        .unwrap();
    let mut old = result(&task);
    old.scope.turn_id = TurnId::new();
    let current: Vec<_> = (0..40).map(|_| result(&task)).collect();
    queue
        .enqueue_background_results(vec![old.clone()], BackgroundDeliveryMode::Passive)
        .unwrap();
    queue
        .enqueue_background_results(current.clone(), BackgroundDeliveryMode::Passive)
        .unwrap();
    let mut collected = Vec::new();
    for expected in [16, 16, 8] {
        let page = queue.take_background_results(false);
        assert_eq!(page.len(), expected);
        collected.extend(page.iter().map(|r| r.result_id));
        queue.acknowledge_captured_background(&collected);
    }
    assert_eq!(
        collected,
        current.iter().map(|r| r.result_id).collect::<Vec<_>>()
    );
    assert!(queue.take_background_results(false).is_empty());
    assert_eq!(
        queue.take_background_results(true)[0].result_id,
        old.result_id
    );
}

struct ActiveDeliveryProvider {
    handle: AgentExecutionHandle,
    result: BackgroundResult,
    requests: Arc<Mutex<Vec<ProviderRequest>>>,
}

#[async_trait]
impl LlmProvider for ActiveDeliveryProvider {
    fn contract(&self) -> ProviderContract {
        MockProvider::text_response("").contract()
    }
    async fn complete(&self, request: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        let count = {
            let mut requests = self.requests.lock().unwrap();
            requests.push(request.clone());
            requests.len()
        };
        if count == 1 {
            self.handle
                .enqueue_background_results(
                    vec![self.result.clone()],
                    BackgroundDeliveryMode::Passive,
                )
                .unwrap();
            MockProvider::tool_call("read_file", json!({"path":"input.txt"}))
                .complete(request)
                .await
        } else {
            assert_eq!(count, 2);
            MockProvider::text_response("Combined findings.")
                .complete(request)
                .await
        }
    }
}

#[tokio::test]
async fn passive_result_piggybacks_next_tool_step_without_extra_inference_or_boilerplate() {
    let root = tempdir().unwrap();
    std::fs::write(root.path().join("input.txt"), "local input").unwrap();
    let task = request();
    let observation = result(&task);
    let (handle, control) = agent_execution_channel(4);
    let requests = Arc::new(Mutex::new(Vec::new()));
    let provider = ActiveDeliveryProvider {
        handle,
        result: observation.clone(),
        requests: requests.clone(),
    };
    let mut trace = Vec::new();
    AgentLoop::new(
        provider,
        ContextBuilder::default(),
        BasicToolExecutor::new(WorkspacePolicy::new(root.path()).unwrap()),
    )
    .run_with_control_and_trace(task, control, |event| trace.push(event))
    .await
    .unwrap();
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    let output = requests[1]
        .messages
        .iter()
        .position(|message| message.tool_name.as_deref() == Some("background_result"))
        .unwrap();
    assert!(
        requests[1].messages[output]
            .content
            .contains(&observation.result_id.to_string())
    );
    assert!(requests[1].messages[output - 1].content.is_empty());
    assert_eq!(trace.iter().filter(|event| matches!(event, AgentLoopTraceEvent::AssistantMessage {content, ..} if content == "Combined findings.")).count(), 1);
}

#[tokio::test]
async fn user_input_takes_precedence_and_receives_passive_results_as_tool_data() {
    let (_, result, outcome, requests, trace) =
        run_delivery(BackgroundDeliveryMode::Passive, true, 10).await;
    assert_eq!(requests.len(), 2);
    let messages = &requests[1].messages;
    assert!(
        messages
            .iter()
            .any(|message| message.role == ProviderRole::User
                && message.content == "Use my newer instructions")
    );
    let output = messages
        .iter()
        .position(|message| message.tool_name.as_deref() == Some("background_result"))
        .unwrap();
    assert_eq!(messages[output].role, ProviderRole::Tool);
    assert_eq!(messages[output - 1].role, ProviderRole::Assistant);
    assert_eq!(
        messages[output - 1].tool_calls[0].tool_call_id,
        messages[output].tool_call_id.clone().unwrap()
    );
    assert!(
        messages[output]
            .content
            .contains(&result.result_id.to_string())
    );
    assert!(
        messages
            .iter()
            .filter(|message| matches!(message.role, ProviderRole::User | ProviderRole::System))
            .all(|message| !message.content.contains(&result.result_id.to_string()))
    );
    let consumed = trace
        .iter()
        .position(|event| matches!(event, AgentLoopTraceEvent::BackgroundResultsConsumed { .. }))
        .unwrap();
    assert!(matches!(
        trace[consumed - 1],
        AgentLoopTraceEvent::ContextSnapshotCaptured { .. }
    ));
    assert!(
        matches!(&trace[consumed], AgentLoopTraceEvent::BackgroundResultsConsumed { result_ids, turn_id, .. }
        if result_ids == &vec![result.result_id] && *turn_id == outcome.final_turn_id)
    );
}

#[tokio::test]
async fn completed_batch_continues_current_turn_once_without_fabricating_user_input() {
    let (request, _, outcome, requests, trace) =
        run_delivery(BackgroundDeliveryMode::ContinueAfterBatch, false, 10).await;
    assert_eq!(requests.len(), 2);
    assert_eq!(outcome.final_turn_id, request.turn_id);
    assert!(!trace.iter().any(|event| matches!(
        event,
        AgentLoopTraceEvent::PendingTurnStarted(_)
            | AgentLoopTraceEvent::PendingTurnStartedWithExecution(_)
    )));
    let users = |request: &ProviderRequest| {
        request
            .messages
            .iter()
            .filter(|message| message.role == ProviderRole::User)
            .count()
    };
    assert_eq!(users(&requests[0]), users(&requests[1]));
    assert!(!trace.iter().any(|event| matches!(event, AgentLoopTraceEvent::AssistantMessage {content, ..} if content == "Candidate completion.")));
    assert_eq!(trace.iter().filter(|event| matches!(event, AgentLoopTraceEvent::AssistantMessage {content, ..} if content == "Done.")).count(), 1);
    assert_eq!(
        trace
            .iter()
            .filter(|event| matches!(event, AgentLoopTraceEvent::BackgroundResultsConsumed { .. }))
            .count(),
        1
    );
}

#[tokio::test]
async fn background_continuation_does_not_reset_iteration_budget() {
    let (_, _, _, requests, trace) =
        run_delivery(BackgroundDeliveryMode::ContinueAfterBatch, false, 1).await;
    assert_eq!(requests.len(), 1);
    assert!(trace.iter().any(
        |event| matches!(event, AgentLoopTraceEvent::GovernorDecided(decision)
        if decision.action != GovernorAction::Allow && decision.iteration > 1)
    ));
    assert!(
        !trace
            .iter()
            .any(|event| matches!(event, AgentLoopTraceEvent::BackgroundResultsConsumed { .. }))
    );
}

#[test]
fn stale_turn_and_pause_cannot_promote_a_passive_result() {
    let request = request();
    let prior_result = result(&request);
    let (handle, control) = agent_execution_channel(4);
    control
        .pending_turns
        .set_background_turn(request.session_id, request.task_id, TurnId::new())
        .unwrap();
    handle
        .enqueue_background_results(
            vec![prior_result],
            BackgroundDeliveryMode::ContinueAfterBatch,
        )
        .unwrap();
    assert!(
        control
            .pending_turns
            .take_background_results(false)
            .is_empty()
    );
    assert_eq!(control.pending_turns.take_background_results(true).len(), 1);

    let next = result(&request);
    control
        .pending_turns
        .set_background_turn(request.session_id, request.task_id, request.turn_id)
        .unwrap();
    handle.pause();
    handle
        .enqueue_background_results(vec![next], BackgroundDeliveryMode::ContinueAfterBatch)
        .unwrap();
    handle.resume();
    assert!(
        control
            .pending_turns
            .take_background_results(false)
            .is_empty()
    );
    assert_eq!(control.pending_turns.take_background_results(true).len(), 1);
}

#[test]
fn cancellation_and_closed_admission_leave_results_for_the_durable_host() {
    let request = request();
    let (handle, _control) = agent_execution_channel(1);
    handle.cancel();
    assert!(matches!(
        handle.enqueue_background_results(
            vec![result(&request)],
            BackgroundDeliveryMode::ContinueAfterBatch
        ),
        Err(AgentLoopError::Cancelled)
    ));
    let (handle, control) = agent_execution_channel(1);
    drop(control);
    assert!(matches!(
        handle.enqueue_background_results(vec![result(&request)], BackgroundDeliveryMode::Passive),
        Err(AgentLoopError::PendingTurnQueueClosed)
    ));
}

#[test]
fn handoff_revokes_ready_results_without_losing_observations() {
    let request = request();
    let (handle, control) = agent_execution_channel(2);
    handle
        .enqueue_background_results(
            vec![result(&request)],
            BackgroundDeliveryMode::ContinueAfterBatch,
        )
        .unwrap();
    handle.suppress_background_continuations();
    assert!(
        control
            .pending_turns
            .take_background_results(false)
            .is_empty()
    );
    assert_eq!(control.pending_turns.take_background_results(true).len(), 1);
}

#[test]
fn cancelling_one_batch_preserves_other_wakes_and_all_passive_observations() {
    let request = request();
    let (handle, control) = agent_execution_channel(4);
    let old = result(&request);
    let mut sibling = result(&request);
    sibling.batch_id = old.batch_id;
    let newer = result(&request);
    let passive = result(&request);
    handle
        .enqueue_background_results(
            vec![old.clone(), sibling.clone(), newer.clone()],
            BackgroundDeliveryMode::ContinueAfterBatch,
        )
        .unwrap();
    handle
        .enqueue_background_results(vec![passive.clone()], BackgroundDeliveryMode::Passive)
        .unwrap();

    // Cancellation can arrive after the batch's wake has been admitted but
    // before the runtime reaches its next sampling boundary.
    handle.suppress_background_batch(old.batch_id);
    assert_eq!(
        control.pending_turns.take_background_results(false),
        vec![newer]
    );
    assert_eq!(
        control.pending_turns.take_background_results(true),
        vec![old, sibling, passive]
    );

    // Suppressing an old batch must not disable future batches in the session.
    let future = result(&request);
    handle
        .enqueue_background_results(
            vec![future.clone()],
            BackgroundDeliveryMode::ContinueAfterBatch,
        )
        .unwrap();
    assert_eq!(
        control.pending_turns.take_background_results(false),
        vec![future]
    );
}

#[test]
fn terminal_unknown_replaces_pending_verification_without_claiming_success() {
    let root = tempdir().unwrap();
    let executor = BasicToolExecutor::new(WorkspacePolicy::new(root.path()).unwrap());
    let request = request();
    let mut terminal = result(&request);
    let mut initial = objective_test_report("shell", Some("python3 -m unittest -v"));
    initial.envelope.structured_facts["result_pending"] = json!(true);
    initial.envelope.structured_facts["background_task_id"] = json!(terminal.background_task_id);
    terminal.tool_call_id = initial.envelope.tool_call_id;
    terminal.output = initial.envelope.clone();
    terminal.status = BackgroundTaskStatus::OutcomeUnknown;
    let mut reports = vec![initial.clone()];
    let statuses =
        background::reconcile_completed_results(&executor, &[terminal], &mut reports, &mut []);
    assert_eq!(statuses, vec![ToolResultStatus::Error]);
    assert_eq!(reports.len(), 1, "a terminal result is the same invocation");
    let resolved = process_validation::resolve_validation_report(&reports[0], &reports);
    assert_eq!(resolved.envelope.status, ToolResultStatus::Error);
    assert!(!objective_validation_report(&resolved).unwrap().passed);
    assert!(!tool_execution_check_status(reports.last().unwrap(), &[]).0);
}

#[test]
fn fast_background_failure_replaces_already_settled_admission_receipt() {
    let root = tempdir().unwrap();
    let executor = BasicToolExecutor::new(WorkspacePolicy::new(root.path()).unwrap());
    let mut observation = result(&request());
    let mut receipt = objective_test_report("browser_run", None);
    receipt.envelope.structured_facts = json!({
        "background":true, "background_task_id":observation.background_task_id,
        "background_status":"failed", "result_pending":false, "terminal":true,
    });
    observation.tool_call_id = receipt.envelope.tool_call_id;
    observation.status = BackgroundTaskStatus::Failed;
    observation.output = receipt.envelope.clone();
    observation.output.status = ToolResultStatus::Error;
    observation.output.summary = "browser disconnected before result delivery".into();
    observation.output.structured_facts = json!({"code":"chrome_disconnected"});
    let mut reports = vec![receipt];
    let statuses =
        background::reconcile_completed_results(&executor, &[observation], &mut reports, &mut []);
    assert_eq!(statuses, vec![ToolResultStatus::Error]);
    assert_eq!(
        reports[0].envelope.structured_facts["code"],
        "chrome_disconnected"
    );
    assert!(!tool_execution_check_status(&reports[0], &[]).0);
}

#[test]
fn user_handoff_replaces_pending_receipt_without_success_or_automatic_wake() {
    let root = tempdir().unwrap();
    let executor = BasicToolExecutor::new(WorkspacePolicy::new(root.path()).unwrap());
    let request = request();
    for status in [ToolResultStatus::Ok, ToolResultStatus::Blocked] {
        let mut observation = result(&request);
        let mut initial = objective_test_report("browser_run", None);
        initial.envelope.structured_facts = json!({"result_pending":true});
        observation.tool_call_id = initial.envelope.tool_call_id;
        observation.output = initial.envelope.clone();
        observation.output.status = status;
        observation.output.structured_facts =
            json!({"phase":"waiting_for_user", "code":"user_in_control"});
        observation.status = BackgroundTaskStatus::WaitingForUser;

        let (handle, control) = agent_execution_channel(4);
        handle
            .enqueue_background_results(
                vec![observation.clone()],
                BackgroundDeliveryMode::ContinueAfterBatch,
            )
            .unwrap();
        assert!(
            control
                .pending_turns
                .take_background_results(false)
                .is_empty()
        );
        let results = control.pending_turns.take_background_results(true);
        let mut reports = vec![initial];
        let mut attempts = vec![ToolAttemptMetadata {
            tool_call_id: observation.tool_call_id,
            step_no: 1,
            signature: "browser_run:fixture".into(),
            status: ToolResultStatus::Ok,
            recoverable_failure: false,
        }];
        let statuses = background::reconcile_completed_results(
            &executor,
            &results,
            &mut reports,
            &mut attempts,
        );
        assert_eq!(statuses, vec![ToolResultStatus::Blocked]);
        assert_eq!(attempts[0].status, ToolResultStatus::Blocked);
        assert_eq!(
            reports[0].envelope.structured_facts["result_pending"],
            false
        );
        assert_eq!(reports[0].envelope.structured_facts["terminal"], false);
        assert_eq!(
            reports[0].envelope.structured_facts["waiting_for_user"],
            true
        );
        assert_eq!(
            reports[0].envelope.structured_facts["background_status"],
            "waiting_for_user"
        );
        assert_eq!(
            reports[0].envelope.structured_facts["code"],
            "user_in_control"
        );
        assert!(!tool_execution_check_status(&reports[0], &attempts).0);
        assert!(
            background::reconcile_completed_results(
                &executor,
                &results,
                &mut reports,
                &mut attempts
            )
            .is_empty()
        );
    }
}

#[test]
fn incomplete_background_test_output_cannot_validate_executed_tests() {
    let mut report = objective_test_report("shell", Some("python3 -m unittest -v"));
    report.envelope.structured_facts["exit_code"] = json!(0);
    report.envelope.structured_facts["background_verification_output_incomplete"] = json!(true);
    report
        .artifact_contents
        .push(golutra_agent_tools::ArtifactContent {
            artifact_id: golutra_agent_core::ArtifactId::new(),
            bytes: b"Ran 3 tests in 0.01s\nOK\n".to_vec(),
        });
    assert!(!objective_validation_report(&report).unwrap().passed);
}

#[tokio::test]
async fn initial_results_are_attached_without_an_extra_user_turn() {
    let root = tempdir().unwrap();
    let request = request();
    let result = result(&request);
    let harness = AgentHarness::new(
        MockProvider::text_response("Done."),
        ContextBuilder::default(),
        BasicToolExecutor::new(WorkspacePolicy::new(root.path()).unwrap()),
    );
    let (_, control) = agent_execution_channel(4);
    let mut trace = Vec::new();
    let outcome = harness
        .execute_configured(
            ConfiguredAgentRun::new(request.clone()).with_background_results(vec![result.clone()]),
            control,
            |event| trace.push(event),
        )
        .await
        .unwrap();
    assert_eq!(outcome.final_turn_id, request.turn_id);
    assert!(trace.iter().any(|event| matches!(event, AgentLoopTraceEvent::BackgroundResultsConsumed { result_ids, .. } if result_ids == &vec![result.result_id])));
    assert_eq!(
        trace
            .iter()
            .filter(|event| matches!(event, AgentLoopTraceEvent::ProviderStarted { .. }))
            .count(),
        1
    );
}

#[tokio::test]
async fn recovered_screenshot_without_live_bytes_reports_omission_and_keeps_artifact_reference() {
    let root = tempdir().unwrap();
    let request = request();
    let mut result = result(&request);
    let artifact_id = golutra_agent_core::ArtifactId::new();
    result.artifact_refs = vec![format!("artifact:{artifact_id}")];
    result.output.structured_facts["background_image_artifacts"] =
        json!([{"artifact_id":artifact_id,"mime_type":"image/png"}]);
    let harness = AgentHarness::new(
        MockProvider::text_response("Done."),
        ContextBuilder::default(),
        BasicToolExecutor::new(WorkspacePolicy::new(root.path()).unwrap()),
    );
    let (_, control) = agent_execution_channel(4);
    let mut trace = Vec::new();
    harness
        .execute_configured(
            ConfiguredAgentRun::new(request).with_background_results(vec![result]),
            control,
            |event| trace.push(event),
        )
        .await
        .unwrap();
    let output = trace
        .iter()
        .find_map(|event| match event {
            AgentLoopTraceEvent::ContextSnapshotCaptured { request, .. } => request
                .messages
                .iter()
                .find(|message| message.tool_name.as_deref() == Some("background_result")),
            _ => None,
        })
        .unwrap();
    let output: Value = serde_json::from_str(&output.content).unwrap();
    assert!(
        output["image_omission"]
            .as_str()
            .unwrap()
            .contains("not included")
    );
    assert_eq!(
        output["artifact_refs"][0],
        format!("artifact:{artifact_id}")
    );
}

#[tokio::test]
async fn replay_after_snapshot_before_ack_deduplicates_and_acknowledges_again() {
    let root = tempdir().unwrap();
    let request = request();
    let result = result(&request);
    let harness = AgentHarness::new(
        MockProvider::text_response("Done."),
        ContextBuilder::default(),
        BasicToolExecutor::new(WorkspacePolicy::new(root.path()).unwrap()),
    );
    let (_, control) = agent_execution_channel(4);
    let mut first = Vec::new();
    harness
        .execute_configured(
            ConfiguredAgentRun::new(request.clone()).with_background_results(vec![result.clone()]),
            control,
            |event| first.push(event),
        )
        .await
        .unwrap();
    let captured = first
        .iter()
        .find_map(|event| match event {
            AgentLoopTraceEvent::ContextSnapshotCaptured { request, .. } => {
                Some(request.messages.clone())
            }
            _ => None,
        })
        .unwrap();
    let (_, control) = agent_execution_channel(4);
    let mut replay = Vec::new();
    harness
        .execute_configured(
            ConfiguredAgentRun::new(request)
                .with_replay_context(AgentReplayContext::for_resume(captured, Vec::new()))
                .with_background_results(vec![result.clone()]),
            control,
            |event| replay.push(event),
        )
        .await
        .unwrap();
    let captured = replay
        .iter()
        .find_map(|event| match event {
            AgentLoopTraceEvent::ContextSnapshotCaptured { request, .. } => Some(&request.messages),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        captured
            .iter()
            .filter(|message| message.tool_name.as_deref() == Some("background_result"))
            .count(),
        1
    );
    assert!(replay.iter().any(|event| matches!(event, AgentLoopTraceEvent::BackgroundResultsConsumed { result_ids, .. } if result_ids == &vec![result.result_id])));
}

#[tokio::test]
async fn result_compacted_before_capture_returns_passively_with_next_user_input() {
    let root = tempdir().unwrap();
    let executor = BasicToolExecutor::new(WorkspacePolicy::new(root.path()).unwrap());
    let request = request();
    let result = result(&request);
    let (handle, control) = agent_execution_channel(4);
    control
        .pending_turns
        .set_background_turn(request.session_id, request.task_id, request.turn_id)
        .unwrap();
    handle
        .enqueue_background_results(
            vec![result.clone()],
            BackgroundDeliveryMode::ContinueAfterBatch,
        )
        .unwrap();
    let mut plan = ContextBuilder::default()
        .build(request.task_id, request.turn_id, Vec::new())
        .unwrap();
    let original_messages = plan.messages.clone();
    let original_sources = plan.message_sources.clone();
    let mut seen = HashSet::new();
    let mut pending_ack = Vec::new();
    let mut tokens = plan.estimated_message_tokens();
    background::append_results(
        &mut plan,
        control.pending_turns.take_background_results(false),
        &[],
        &executor,
        &mut seen,
        &mut pending_ack,
        &mut tokens,
    );
    // Exercise the same history replacement boundary as context compaction.
    tokens = plan.replace_messages(original_messages, original_sources);
    background::restore_compacted_results(
        &plan,
        &mut seen,
        &mut pending_ack,
        &control.pending_turns,
    );
    assert!(
        pending_ack.is_empty(),
        "unseen result cannot be acknowledged"
    );
    assert!(
        control
            .pending_turns
            .take_background_results(false)
            .is_empty(),
        "overflow cannot cause an automatic retry loop"
    );
    let next = follow_up();
    handle.append_turn(next.clone()).await.unwrap();
    assert_eq!(
        control
            .pending_turns
            .take_ready_turn()
            .unwrap()
            .turn
            .turn
            .turn_id,
        next.turn_id
    );
    // The host can redeliver its pending records; the in-flight copy deduplicates it.
    handle
        .enqueue_background_results(vec![result.clone()], BackgroundDeliveryMode::Passive)
        .unwrap();
    background::append_results(
        &mut plan,
        control.pending_turns.take_background_results(true),
        &[],
        &executor,
        &mut seen,
        &mut pending_ack,
        &mut tokens,
    );
    assert_eq!(pending_ack, vec![result.result_id]);
    assert_eq!(
        plan.messages
            .iter()
            .filter(|message| message.tool_name.as_deref() == Some("background_result"))
            .count(),
        1
    );
}

#[derive(Debug)]
struct GatedBrowser {
    release: Arc<tokio::sync::Semaphore>,
    calls: Arc<AtomicUsize>,
    screenshot: bool,
}

#[async_trait]
impl ExternalToolBackend for GatedBrowser {
    fn contracts(&self) -> Vec<ToolContract> {
        let mut contract = ToolRegistry::p0_default()
            .contract("read_file")
            .unwrap()
            .clone();
        contract.tool_name = "browser_run".into();
        contract.side_effect_type = SideEffectType::ExternalSystem;
        contract.input_schema =
            json!({"type":"object","properties":{},"additionalProperties":false});
        vec![contract]
    }
    fn capabilities(&self) -> HashMap<String, ToolCapabilities> {
        HashMap::from([(
            "browser_run".into(),
            ToolCapabilities {
                available_in_coding_profile: true,
                ..Default::default()
            },
        )])
    }
    async fn call(
        &self,
        _: &ToolRequest,
        _: CancellationToken,
    ) -> Result<ExternalToolOutput, ToolError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.release.acquire().await.unwrap().forget();
        Ok(ExternalToolOutput {
            summary: "Page observed".into(),
            content: "Title: Example".into(),
            structured_facts: json!({"title":"Example"}),
            is_error: false,
            images: if self.screenshot {
                vec![golutra_agent_tools::ExternalToolImage {
                    mime_type: "image/png".into(),
                    bytes: base64::engine::general_purpose::STANDARD
                        .decode(TEST_PNG)
                        .unwrap(),
                }]
            } else {
                Vec::new()
            },
        })
    }
}

#[derive(Debug)]
struct MemoryBackgroundObserver {
    handle: AgentExecutionHandle,
    scope: BackgroundScope,
    batch_id: BackgroundBatchId,
    state: Mutex<(bool, Vec<BackgroundResult>)>,
    sealed: tokio::sync::Semaphore,
}

impl MemoryBackgroundObserver {
    fn deliver(&self) {
        let state = self.state.lock().unwrap();
        let mode = if state.0 {
            BackgroundDeliveryMode::ContinueAfterBatch
        } else {
            BackgroundDeliveryMode::Passive
        };
        // Our in-memory journal keeps rejected results just as the host store does.
        let _ = self
            .handle
            .enqueue_background_results(state.1.clone(), mode);
    }
}

#[async_trait]
impl golutra_agent_tools::BackgroundToolObserver for MemoryBackgroundObserver {
    async fn admit(&self, _: &ToolRequest) -> Result<(), ToolError> {
        Ok(())
    }
    async fn complete(
        &self,
        request: &ToolRequest,
        report: &ToolExecutionReport,
    ) -> Result<(), ToolError> {
        self.state.lock().unwrap().1.push(BackgroundResult {
            result_id: BackgroundResultId::new(),
            background_task_id: BackgroundTaskId(request.tool_call_id.0),
            batch_id: self.batch_id,
            scope: self.scope,
            tool_call_id: request.tool_call_id,
            tool_name: request.tool_name.clone(),
            status: if report.envelope.status == ToolResultStatus::Ok {
                BackgroundTaskStatus::Succeeded
            } else {
                BackgroundTaskStatus::Failed
            },
            summary: report.envelope.summary.clone(),
            output: report.envelope.clone(),
            artifact_refs: Vec::new(),
            created_at: chrono::Utc::now(),
        });
        self.deliver();
        Ok(())
    }
    async fn seal(&self, _: SessionId, _: Option<TurnId>) -> Result<(), ToolError> {
        self.state.lock().unwrap().0 = true;
        self.deliver();
        self.sealed.add_permits(1);
        Ok(())
    }
}

struct CountingProvider {
    inner: MockProvider,
    requests: Arc<Mutex<Vec<ProviderRequest>>>,
}

#[async_trait]
impl LlmProvider for CountingProvider {
    fn contract(&self) -> ProviderContract {
        self.inner.contract()
    }
    async fn complete(&self, request: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        self.requests.lock().unwrap().push(request.clone());
        self.inner.complete(request).await
    }
}

#[tokio::test]
async fn sealed_background_wait_is_quiet_and_terminal_result_resumes_same_loop() {
    gated_wait(false, false, false).await;
}

#[tokio::test]
async fn user_input_preempts_background_wait_without_waiting_for_old_task() {
    gated_wait(true, false, false).await;
}

#[tokio::test]
async fn cancellation_preempts_background_wait_without_another_model_request() {
    gated_wait(false, true, false).await;
}

const TEST_PNG: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+a8l8AAAAASUVORK5CYII=";

#[tokio::test]
async fn async_browser_screenshot_reaches_the_resumed_provider_with_observation_provenance() {
    gated_wait(false, false, true).await;
}

async fn gated_wait(preempt: bool, cancel: bool, screenshot: bool) {
    let root = tempdir().unwrap();
    let mut request = request();
    request.tools = vec!["browser_run".into()];
    let initial_turn = request.turn_id;
    let (handle, control) = agent_execution_channel(8);
    let supervisor = golutra_agent_tools::BackgroundToolSupervisor::new();
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let calls = Arc::new(AtomicUsize::new(0));
    let observer = Arc::new(MemoryBackgroundObserver {
        handle: handle.clone(),
        scope: BackgroundScope {
            workspace_id: WorkspaceId::new(),
            session_id: request.session_id,
            task_id: request.task_id,
            turn_id: request.turn_id,
        },
        batch_id: BackgroundBatchId::new(),
        state: Mutex::new((false, Vec::new())),
        sealed: tokio::sync::Semaphore::new(0),
    });
    let executor = BasicToolExecutor::new(
        WorkspacePolicy::new(root.path())
            .unwrap()
            .with_unrestricted_access(true),
    )
    .with_unrestricted_access(true)
    .with_external_backend(Arc::new(GatedBrowser {
        release: release.clone(),
        calls: calls.clone(),
        screenshot,
    }))
    .unwrap()
    .with_background_supervisor(supervisor.clone(), observer.clone())
    .unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let agent = AgentLoop::new(
        CountingProvider {
            inner: MockProvider::tool_call("browser_run", json!({"background":true})),
            requests: requests.clone(),
        },
        ContextBuilder::default(),
        executor,
    );
    let running = tokio::spawn(async move {
        let mut trace = Vec::new();
        let outcome = agent
            .run_with_control_and_trace(request, control, |event| trace.push(event))
            .await;
        (outcome, trace)
    });
    tokio::time::timeout(Duration::from_secs(5), observer.sealed.acquire())
        .await
        .unwrap()
        .unwrap()
        .forget();
    assert!(
        !running.is_finished(),
        "pending background work keeps admission open"
    );
    assert_eq!(
        requests.lock().unwrap().len(),
        2,
        "idle wait never polls the model"
    );
    let next_turn = if preempt {
        let next = follow_up();
        let id = next.turn_id;
        handle.append_turn(next).await.unwrap();
        Some(id)
    } else {
        None
    };
    if cancel {
        handle.cancel();
    }
    if !preempt && !cancel {
        release.add_permits(1);
    }
    let (outcome, trace) = tokio::time::timeout(Duration::from_secs(5), running)
        .await
        .unwrap()
        .unwrap();
    if cancel {
        assert!(matches!(outcome, Err(AgentLoopError::Cancelled)));
        assert_eq!(requests.lock().unwrap().len(), 2);
    } else {
        let outcome = outcome.unwrap();
        assert_eq!(outcome.final_turn_id, next_turn.unwrap_or(initial_turn));
        assert_eq!(requests.lock().unwrap().len(), 3);
        if screenshot {
            let requests = requests.lock().unwrap();
            let messages = &requests[2].messages;
            let image_index = messages
                .iter()
                .position(|message| !message.metadata.images.is_empty())
                .expect("async screenshot must deliver real image bytes");
            assert_eq!(messages[image_index].metadata.images[0].base64, TEST_PNG);
            assert_eq!(
                messages[image_index - 1].tool_name.as_deref(),
                Some("background_result")
            );
            assert!(
                messages[image_index]
                    .content
                    .contains("untrusted page content")
            );
            assert!(trace.iter().any(|event| matches!(event, AgentLoopTraceEvent::ContextSnapshotCaptured { snapshot, .. }
                if snapshot.message_manifest.iter().any(|message| message.origin == "background_result" && message.contributor == "tool_result_image"))));
        }
        if !preempt {
            assert_eq!(
                outcome.tool_reports.len(),
                1,
                "terminal delivery does not count as a second invocation"
            );
            assert!(
                trace.iter().any(
                    |event| matches!(event, AgentLoopTraceEvent::ToolCompleted(report)
                if report.envelope.structured_facts["result_pending"] == true)
                ),
                "original receipt remains immutable in audit"
            );
            assert!(
                outcome
                    .tool_reports
                    .last()
                    .unwrap()
                    .envelope
                    .structured_facts["background_result_id"]
                    .is_string()
            );
            assert_eq!(
                trace
                    .iter()
                    .filter(|event| matches!(
                        event,
                        AgentLoopTraceEvent::BackgroundResultsConsumed { .. }
                    ))
                    .count(),
                1
            );
        }
    }
    assert!(
        calls.load(Ordering::SeqCst) <= 1,
        "observation never replays execution"
    );
    release.add_permits(1);
    supervisor.shutdown_and_wait().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn finalization_and_completion_admission_are_atomic() {
    let root = tempdir().unwrap();
    let executor = BasicToolExecutor::new(WorkspacePolicy::new(root.path()).unwrap());
    for _ in 0..32 {
        let request = request();
        let result = result(&request);
        let (handle, mut control) = agent_execution_channel(1);
        let start = Arc::new(tokio::sync::Barrier::new(2));
        let sender_start = start.clone();
        let sender = tokio::spawn(async move {
            sender_start.wait().await;
            handle.enqueue_background_results(
                vec![result],
                BackgroundDeliveryMode::ContinueAfterBatch,
            )
        });
        start.wait().await;
        let received = control
            .next_input_or_wait_for_background(&executor, request.session_id, request.turn_id, None)
            .await
            .unwrap();
        match (sender.await.unwrap(), received) {
            (Ok(()), Some(background::PendingRuntimeInput::Background(results))) => {
                assert_eq!(results.len(), 1)
            }
            (Err(AgentLoopError::PendingTurnQueueClosed), None) => {}
            _ => panic!("admitted result cannot disappear during finalization"),
        }
    }
}
