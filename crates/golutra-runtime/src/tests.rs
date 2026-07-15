use std::{
    fs,
    path::{Path, PathBuf},
};

use golutra_context::{ContextBudgetPolicy, ContextBuilder, ContextContributor, estimate_tokens};
use golutra_core::{
    Actor, ActorKind, BudgetOverflowAction, BusyPolicy, TaskStatus, ToolCallId, WorkspaceId,
};
use golutra_governor::GovernorLimits;
use golutra_llm::MockProvider;
use golutra_policy::WorkspacePolicy;
use golutra_protocol::RuntimeEventType;
use golutra_tools::BasicToolExecutor;
use serde_json::json;
use tempfile::tempdir;

use super::*;

#[derive(Debug, Clone)]
enum FallbackTestProvider {
    Failing(Box<golutra_core::ProviderContract>),
    Success(Box<MockProvider>),
}

#[async_trait]
impl LlmProvider for FallbackTestProvider {
    async fn complete(&self, request: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        match self {
            Self::Failing(_) => Err(ProviderError::Failed {
                message: "primary failed".to_owned(),
            }),
            Self::Success(provider) => provider.complete(request).await,
        }
    }

    fn contract(&self) -> golutra_core::ProviderContract {
        match self {
            Self::Failing(contract) => contract.as_ref().clone(),
            Self::Success(provider) => provider.contract(),
        }
    }
}

#[test]
fn prevents_second_active_task_in_same_session() {
    let mut manager = RuntimeLaneManager::new();
    let session_id = SessionId::new();
    let actor = actor("cli");

    manager
        .start_task(
            WorkspaceId::new(),
            session_id,
            TaskId::new(),
            TurnId::new(),
            actor.clone(),
            1,
        )
        .expect("first task starts");
    let result = manager.start_task(
        WorkspaceId::new(),
        session_id,
        TaskId::new(),
        TurnId::new(),
        actor,
        2,
    );

    assert_eq!(
        result.expect_err("second task rejected"),
        RuntimeLaneError::ActiveTaskExists
    );
}

#[tokio::test]
async fn pending_turn_queue_closes_atomically_when_the_loop_becomes_idle() {
    let (handle, control) = agent_execution_channel(2);
    let first = PendingAgentTurn {
        command_id: CommandId::new(),
        turn_id: TurnId::new(),
        content: "first queued turn".to_owned(),
    };

    handle
        .append_turn(first.clone())
        .await
        .expect("first turn queues");
    assert_eq!(control.pending_turns.take_or_close(), Some(first));
    assert_eq!(control.pending_turns.take_or_close(), None);
    assert!(matches!(
        handle
            .append_turn(PendingAgentTurn {
                command_id: CommandId::new(),
                turn_id: TurnId::new(),
                content: "late turn".to_owned(),
            })
            .await,
        Err(AgentLoopError::PendingTurnQueueClosed)
    ));
}

#[test]
fn completed_lane_allows_next_task_in_same_session() {
    let mut manager = RuntimeLaneManager::new();
    let session_id = SessionId::new();
    let actor = actor("cli");

    manager
        .start_task(
            WorkspaceId::new(),
            session_id,
            TaskId::new(),
            TurnId::new(),
            actor.clone(),
            1,
        )
        .expect("first task starts");
    manager
        .finish_task(session_id, TaskStatus::Completed, 2)
        .expect("first task finishes");
    let next = manager
        .start_task(
            WorkspaceId::new(),
            session_id,
            TaskId::new(),
            TurnId::new(),
            actor,
            3,
        )
        .expect("next task starts");

    assert_eq!(next.lane.status, TaskStatus::Running);
}

#[test]
fn rejects_non_active_controller_input() {
    let mut manager = RuntimeLaneManager::new();
    let session_id = SessionId::new();
    manager
        .start_task(
            WorkspaceId::new(),
            session_id,
            TaskId::new(),
            TurnId::new(),
            actor("cli"),
            1,
        )
        .expect("task starts");

    let decision = manager
        .decide_busy_policy(
            session_id,
            CommandId::new(),
            &actor("web"),
            BusyPolicy::Append,
        )
        .expect("decision exists");

    assert_eq!(decision.applied_policy, BusyPolicy::Reject);
    assert!(!decision.safe_to_inject);
}

#[test]
fn takeover_transfers_the_active_controller_and_records_both_actors() {
    let mut manager = RuntimeLaneManager::new();
    let session_id = SessionId::new();
    let original = actor("original");
    let replacement = actor("replacement");
    manager
        .start_task(
            WorkspaceId::new(),
            session_id,
            TaskId::new(),
            TurnId::new(),
            original.clone(),
            1,
        )
        .expect("task");

    let transition = manager
        .takeover(session_id, replacement.clone(), 2)
        .expect("takeover");

    assert_eq!(transition.lane.active_controller, replacement);
    assert_eq!(
        transition.event.event_type,
        RuntimeEventType::ControllerChanged
    );
    assert_eq!(
        transition.event.payload["previous_controller"],
        json!(original)
    );
}

#[test]
fn abort_moves_lane_to_aborting() {
    let mut manager = RuntimeLaneManager::new();
    let session_id = SessionId::new();
    manager
        .start_task(
            WorkspaceId::new(),
            session_id,
            TaskId::new(),
            TurnId::new(),
            actor("cli"),
            1,
        )
        .expect("task starts");

    let transition = manager.abort(session_id, 2).expect("abort works");

    assert_eq!(transition.lane.status, TaskStatus::Aborting);
    assert_eq!(
        transition.event.event_type,
        RuntimeEventType::TaskAbortRequested
    );
    assert!(is_active_status(TaskStatus::Aborting));
}

#[test]
fn terminal_lane_rejects_control_transitions() {
    let mut manager = RuntimeLaneManager::new();
    let session_id = SessionId::new();
    manager
        .start_task(
            WorkspaceId::new(),
            session_id,
            TaskId::new(),
            TurnId::new(),
            actor("cli"),
            1,
        )
        .expect("task starts");
    manager
        .finish_task(session_id, TaskStatus::Completed, 2)
        .expect("task finishes");

    assert_eq!(
        manager.abort(session_id, 3),
        Err(RuntimeLaneError::LaneNotFound)
    );
    assert_eq!(
        manager.pause(session_id, 4),
        Err(RuntimeLaneError::LaneNotFound)
    );
    assert_eq!(
        manager.resume(session_id, 5),
        Err(RuntimeLaneError::LaneNotFound)
    );
    assert_eq!(
        manager.lane(session_id).map(|lane| lane.status),
        Some(TaskStatus::Completed)
    );
}

fn actor(id: &str) -> Actor {
    Actor {
        kind: ActorKind::Cli,
        id: id.to_owned(),
    }
}

#[tokio::test]
async fn agent_loop_provider_error_includes_detail() {
    let workspace = tempdir().expect("workspace");
    let provider =
        FallbackTestProvider::Failing(Box::new(MockProvider::text_response("unused").contract()));
    let executor = BasicToolExecutor::new(WorkspacePolicy::new(workspace.path()).expect("policy"));
    let agent_loop = AgentLoop::new(provider, ContextBuilder::default(), executor);

    let error = agent_loop
        .run(AgentTaskRequest {
            session_id: SessionId::new(),
            task_id: TaskId::new(),
            turn_id: TurnId::new(),
            objective: "你好".to_owned(),
            completion_criteria: vec!["assistant response".to_owned()],
            touched_code: false,
            contributors: Vec::new(),
            tools: Vec::new(),
        })
        .await
        .expect_err("provider error");

    assert!(error.to_string().contains("provider call failed"));
    assert!(error.to_string().contains("primary failed"));
}

#[tokio::test]
async fn fallback_completion_and_usage_are_attributed_to_the_actual_provider() {
    let workspace = tempdir().expect("workspace");
    let mut primary_contract = MockProvider::text_response("unused").contract();
    primary_contract.provider_id = "primary".to_owned();
    primary_contract.model_id = "primary-model".to_owned();
    let provider = FallbackTestProvider::Failing(Box::new(primary_contract));
    let fallback = FallbackTestProvider::Success(Box::new(MockProvider::text_response("fallback")));
    let executor = BasicToolExecutor::new(WorkspacePolicy::new(workspace.path()).expect("policy"));
    let agent_loop =
        AgentLoop::new(provider, ContextBuilder::default(), executor).with_fallback(fallback);
    let mut trace = Vec::new();

    let outcome = agent_loop
        .run_with_trace(
            AgentTaskRequest {
                session_id: SessionId::new(),
                task_id: TaskId::new(),
                turn_id: TurnId::new(),
                objective: "hello".to_owned(),
                completion_criteria: vec!["assistant response".to_owned()],
                touched_code: false,
                contributors: Vec::new(),
                tools: Vec::new(),
            },
            |event| trace.push(event),
        )
        .await
        .expect("fallback outcome");

    assert_eq!(outcome.final_message.as_deref(), Some("fallback"));
    assert!(trace.iter().any(|event| matches!(
        event,
        AgentLoopTraceEvent::ProviderStarted { provider_id, model_id }
            if provider_id == "mock" && model_id == "mock-model"
    )));
    assert!(trace.iter().any(|event| matches!(
        event,
        AgentLoopTraceEvent::ProviderCompleted { provider_id, model_id, .. }
            if provider_id == "mock" && model_id == "mock-model"
    )));
    assert!(trace.iter().any(|event| matches!(
        event,
        AgentLoopTraceEvent::TokenUsageRecorded(record)
            if record.provider_id == "mock" && record.model_id == "mock-model"
    )));
}

#[tokio::test]
async fn agent_loop_governor_blocks_before_iteration_budget_is_exceeded() {
    let workspace = tempdir().expect("workspace");
    let executor = BasicToolExecutor::new(WorkspacePolicy::new(workspace.path()).expect("policy"));
    let governor = RuntimeGovernor::new(GovernorLimits {
        max_iterations: 0,
        ..GovernorLimits::default()
    });
    let agent_loop = AgentLoop::new(
        MockProvider::text_response("should not run"),
        ContextBuilder::default(),
        executor,
    )
    .with_governor(governor);
    let mut trace = Vec::new();

    let outcome = agent_loop
        .run_with_trace(
            AgentTaskRequest {
                session_id: SessionId::new(),
                task_id: TaskId::new(),
                turn_id: TurnId::new(),
                objective: "inspect runtime".to_owned(),
                completion_criteria: vec!["runtime inspected".to_owned()],
                touched_code: false,
                contributors: Vec::new(),
                tools: Vec::new(),
            },
            |event| trace.push(event),
        )
        .await
        .expect("governed outcome");

    assert_eq!(outcome.loop_decision.action, LoopAction::Blocked);
    assert!(trace.iter().any(|event| matches!(
        event,
        AgentLoopTraceEvent::GovernorDecided(decision)
            if decision.action == GovernorAction::Block
    )));
}

#[tokio::test]
async fn initial_context_overflow_returns_a_structured_blocked_outcome() {
    let workspace = tempdir().expect("workspace");
    let executor = BasicToolExecutor::new(WorkspacePolicy::new(workspace.path()).expect("policy"));
    let context_builder = ContextBuilder::new(ContextBudgetPolicy {
        context_window: 64,
        max_output: 16,
        budget_limit: 8,
        action_if_exceeded: BudgetOverflowAction::Block,
    });
    let agent_loop = AgentLoop::new(
        MockProvider::text_response("provider must not run"),
        context_builder,
        executor,
    );
    let mut trace = Vec::new();

    let outcome = agent_loop
        .run_with_trace(
            AgentTaskRequest {
                session_id: SessionId::new(),
                task_id: TaskId::new(),
                turn_id: TurnId::new(),
                objective: "inspect runtime".to_owned(),
                completion_criteria: vec!["runtime inspected".to_owned()],
                touched_code: false,
                contributors: vec![ContextContributor {
                    name: "objective".to_owned(),
                    role: ProviderRole::User,
                    content: "large context ".repeat(20),
                    token_budget_hint: 0,
                }],
                tools: Vec::new(),
            },
            |event| trace.push(event),
        )
        .await
        .expect("context guard outcome");

    assert_eq!(outcome.loop_decision.action, LoopAction::Blocked);
    assert_eq!(outcome.verification.result, VerificationResult::Unknown);
    assert!(outcome.loop_decision.budget_state.compact_recommended);
    assert!(trace.iter().any(|event| matches!(
        event,
        AgentLoopTraceEvent::LoopGuardTriggered {
            trigger: golutra_core::LoopGuardTrigger::ContextOverflow,
            ..
        }
    )));
    assert!(
        !trace
            .iter()
            .any(|event| matches!(event, AgentLoopTraceEvent::ProviderStarted { .. }))
    );
}

#[tokio::test]
async fn accumulated_tool_messages_return_an_ask_user_context_outcome() {
    let workspace = tempdir().expect("workspace");
    fs::write(workspace.path().join("large.txt"), "x".repeat(4_096)).expect("fixture");
    let executor = BasicToolExecutor::new(WorkspacePolicy::new(workspace.path()).expect("policy"));
    let contributor = ContextContributor {
        name: "objective".to_owned(),
        role: ProviderRole::User,
        content: "read large.txt".to_owned(),
        token_budget_hint: 0,
    };
    let tool_tokens = executor
        .registry()
        .contract("read_file")
        .and_then(|contract| serde_json::to_string(contract).ok())
        .map(|contract| estimate_tokens(&contract))
        .expect("read_file contract");
    let initial_tokens = estimate_tokens(&contributor.content).saturating_add(tool_tokens);
    let context_builder = ContextBuilder::new(ContextBudgetPolicy {
        context_window: initial_tokens.saturating_add(1_024),
        max_output: 64,
        budget_limit: initial_tokens.saturating_add(8),
        action_if_exceeded: BudgetOverflowAction::Trim,
    });
    let agent_loop = AgentLoop::new(
        MockProvider::tool_call("read_file", json!({"path": "large.txt"})),
        context_builder,
        executor,
    );
    let mut trace = Vec::new();

    let outcome = agent_loop
        .run_with_trace(
            AgentTaskRequest {
                session_id: SessionId::new(),
                task_id: TaskId::new(),
                turn_id: TurnId::new(),
                objective: "read large.txt".to_owned(),
                completion_criteria: vec!["file read".to_owned()],
                touched_code: false,
                contributors: vec![contributor],
                tools: vec!["read_file".to_owned()],
            },
            |event| trace.push(event),
        )
        .await
        .expect("context guard outcome");

    assert_eq!(outcome.tool_reports.len(), 1);
    assert_eq!(outcome.loop_decision.action, LoopAction::AskUser);
    assert!(outcome.loop_decision.budget_state.compact_recommended);
    assert_eq!(
        trace
            .iter()
            .filter(|event| matches!(event, AgentLoopTraceEvent::ProviderStarted { .. }))
            .count(),
        1
    );
    assert!(trace.iter().any(|event| matches!(
        event,
        AgentLoopTraceEvent::LoopGuardTriggered {
            trigger: golutra_core::LoopGuardTrigger::ContextOverflow,
            ..
        }
    )));
}

#[tokio::test]
async fn agent_loop_stops_success_when_tool_evidence_exists() {
    let workspace = tempdir().expect("workspace");
    let provider = MockProvider::tool_call(
        "write_file",
        json!({"path": "result.txt", "content": "done"}),
    );
    let executor = BasicToolExecutor::new(WorkspacePolicy::new(workspace.path()).expect("policy"));
    let agent_loop = AgentLoop::new(provider, ContextBuilder::default(), executor);
    let task_id = TaskId::new();
    let turn_id = TurnId::new();
    let session_id = SessionId::new();

    let outcome = agent_loop
        .run(AgentTaskRequest {
            session_id,
            task_id,
            turn_id,
            objective: "write result".to_owned(),
            completion_criteria: vec!["file written".to_owned()],
            touched_code: true,
            contributors: Vec::new(),
            tools: vec!["write_file".to_owned()],
        })
        .await
        .expect("loop runs");

    assert_eq!(outcome.loop_decision.action, LoopAction::StopSuccess);
    assert_eq!(
        outcome.final_message,
        Some("Completed: file written".to_owned())
    );
    assert_eq!(
        fs::read_to_string(workspace.path().join("result.txt")).unwrap(),
        "done"
    );
}

#[tokio::test]
async fn agent_loop_returns_invalid_tool_calls_to_the_provider_as_tool_results() {
    let workspace = tempdir().expect("workspace");
    let provider = MockProvider::tool_call("missing_tool", json!({"bad": true}));
    let executor = BasicToolExecutor::new(WorkspacePolicy::new(workspace.path()).expect("policy"));
    let agent_loop = AgentLoop::new(provider, ContextBuilder::default(), executor);

    let outcome = agent_loop
        .run(AgentTaskRequest {
            session_id: SessionId::new(),
            task_id: TaskId::new(),
            turn_id: TurnId::new(),
            objective: "try a tool".to_owned(),
            completion_criteria: vec!["tool result returned".to_owned()],
            touched_code: false,
            contributors: Vec::new(),
            tools: Vec::new(),
        })
        .await
        .expect("invalid tool call becomes a report");

    assert_eq!(outcome.tool_reports.len(), 1);
    assert_eq!(
        outcome.tool_reports[0].envelope.status,
        ToolResultStatus::Error
    );
    assert_eq!(
        outcome.tool_reports[0].envelope.summary,
        "tool request is invalid"
    );
}

#[tokio::test]
async fn agent_loop_blocks_without_evidence() {
    let workspace = tempdir().expect("workspace");
    let provider = MockProvider::text_response("done");
    let executor = BasicToolExecutor::new(WorkspacePolicy::new(workspace.path()).expect("policy"));
    let agent_loop = AgentLoop::new(provider, ContextBuilder::default(), executor);

    let outcome = agent_loop
        .run(AgentTaskRequest {
            session_id: SessionId::new(),
            task_id: TaskId::new(),
            turn_id: TurnId::new(),
            objective: "claim done".to_owned(),
            completion_criteria: vec!["objective evidence".to_owned()],
            touched_code: true,
            contributors: Vec::new(),
            tools: Vec::new(),
        })
        .await
        .expect("loop runs");

    assert_eq!(outcome.loop_decision.action, LoopAction::StopFailed);
    assert_eq!(
        outcome.final_message,
        Some("Task finished without enough evidence to verify completion.".to_owned())
    );
}

#[tokio::test]
async fn agent_loop_accepts_plain_conversation_response_without_tool_evidence() {
    let workspace = tempdir().expect("workspace");
    let provider = MockProvider::text_response("你好，我在。");
    let executor = BasicToolExecutor::new(WorkspacePolicy::new(workspace.path()).expect("policy"));
    let agent_loop = AgentLoop::new(provider, ContextBuilder::default(), executor);

    let outcome = agent_loop
        .run(AgentTaskRequest {
            session_id: SessionId::new(),
            task_id: TaskId::new(),
            turn_id: TurnId::new(),
            objective: "你好".to_owned(),
            completion_criteria: vec!["assistant response".to_owned()],
            touched_code: false,
            contributors: Vec::new(),
            tools: Vec::new(),
        })
        .await
        .expect("loop runs");

    assert_eq!(outcome.loop_decision.action, LoopAction::StopSuccess);
    assert_eq!(outcome.verification.result, VerificationResult::Pass);
    assert_eq!(outcome.final_message, Some("你好，我在。".to_owned()));
}

#[tokio::test]
async fn queued_plain_turn_does_not_inherit_the_previous_turn_workspace_requirement() {
    let workspace = tempdir().expect("workspace");
    let executor = BasicToolExecutor::new(WorkspacePolicy::new(workspace.path()).expect("policy"));
    let agent_loop = AgentLoop::new(
        MockProvider::text_response("plain response"),
        ContextBuilder::default(),
        executor,
    );
    let (handle, control) = agent_execution_channel(2);
    let queued_turn_id = TurnId::new();
    handle
        .append_turn(PendingAgentTurn {
            command_id: CommandId::new(),
            turn_id: queued_turn_id,
            content: "hello".to_owned(),
        })
        .await
        .expect("queued turn");

    let outcome = agent_loop
        .run_with_control_and_trace(
            AgentTaskRequest {
                session_id: SessionId::new(),
                task_id: TaskId::new(),
                turn_id: TurnId::new(),
                objective: "write a file".to_owned(),
                completion_criteria: vec!["assistant response".to_owned()],
                touched_code: true,
                contributors: Vec::new(),
                tools: Vec::new(),
            },
            control,
            |_| {},
        )
        .await
        .expect("queued turn outcome");

    assert_eq!(outcome.final_turn_id, queued_turn_id);
    assert_eq!(outcome.verification.result, VerificationResult::Pass);
    assert_eq!(outcome.final_message.as_deref(), Some("plain response"));
    assert!(outcome.tool_reports.is_empty());
}

#[tokio::test]
async fn agent_loop_still_requires_evidence_for_workspace_objectives() {
    let workspace = tempdir().expect("workspace");
    let provider = MockProvider::text_response("README looks fine.");
    let executor = BasicToolExecutor::new(WorkspacePolicy::new(workspace.path()).expect("policy"));
    let agent_loop = AgentLoop::new(provider, ContextBuilder::default(), executor);

    let outcome = agent_loop
        .run(AgentTaskRequest {
            session_id: SessionId::new(),
            task_id: TaskId::new(),
            turn_id: TurnId::new(),
            objective: "read README.md".to_owned(),
            completion_criteria: vec!["file read evidence".to_owned()],
            touched_code: false,
            contributors: Vec::new(),
            tools: vec!["read_file".to_owned()],
        })
        .await
        .expect("loop runs");

    assert_eq!(outcome.loop_decision.action, LoopAction::Blocked);
    assert_eq!(outcome.verification.result, VerificationResult::Unknown);
}

#[tokio::test]
async fn agent_loop_does_not_stop_success_when_tool_fails() {
    let workspace = tempdir().expect("workspace");
    let provider = MockProvider::tool_call("read_file", json!({"path": "missing.md"}));
    let executor = BasicToolExecutor::new(WorkspacePolicy::new(workspace.path()).expect("policy"));
    let agent_loop = AgentLoop::new(provider, ContextBuilder::default(), executor);

    let outcome = agent_loop
        .run(AgentTaskRequest {
            session_id: SessionId::new(),
            task_id: TaskId::new(),
            turn_id: TurnId::new(),
            objective: "read missing file".to_owned(),
            completion_criteria: vec!["file read evidence".to_owned()],
            touched_code: false,
            contributors: Vec::new(),
            tools: vec!["read_file".to_owned()],
        })
        .await
        .expect("loop runs");

    assert_eq!(outcome.loop_decision.action, LoopAction::StopPartial);
    assert_eq!(outcome.verification.result, VerificationResult::Partial);
}

#[tokio::test]
async fn agent_loop_waits_for_approval_before_process_execution() {
    let workspace = tempdir().expect("workspace");
    let provider = MockProvider::tool_call("shell", json!({"command": "echo approved"}));
    let executor = BasicToolExecutor::new(WorkspacePolicy::new(workspace.path()).expect("policy"));
    let agent_loop = AgentLoop::new(provider, ContextBuilder::default(), executor);
    let (handle, control) = agent_execution_channel(4);
    let (trace_tx, mut trace_rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(async move {
        agent_loop
            .run_with_control_and_trace(
                AgentTaskRequest {
                    session_id: SessionId::new(),
                    task_id: TaskId::new(),
                    turn_id: TurnId::new(),
                    objective: "run approved command".to_owned(),
                    completion_criteria: vec!["command evidence".to_owned()],
                    touched_code: false,
                    contributors: Vec::new(),
                    tools: vec!["shell".to_owned()],
                },
                control,
                move |event| {
                    let _ = trace_tx.send(event);
                },
            )
            .await
    });
    let approval = loop {
        let event = trace_rx.recv().await.expect("approval trace");
        if let AgentLoopTraceEvent::ApprovalRequested(approval) = event {
            break approval;
        }
    };

    assert!(!task.is_finished());
    handle
        .resolve_approval(ApprovalResolution {
            approval_id: approval.approval_id,
            decision: ApprovalDecision::Approved,
            reason: "approved by test".to_owned(),
        })
        .await
        .expect("approval resolves");
    let outcome = task.await.expect("task joins").expect("loop completes");

    assert_eq!(outcome.tool_reports.len(), 1);
    assert_eq!(
        outcome.tool_reports[0].envelope.status,
        ToolResultStatus::Ok
    );
}

#[cfg(unix)]
#[tokio::test]
async fn paused_approval_does_not_execute_tool_until_resume() {
    let workspace = tempdir().expect("workspace");
    let output = workspace.path().join("paused.txt");
    let provider = MockProvider::tool_call("shell", json!({"command": "touch paused.txt"}));
    let executor = BasicToolExecutor::new(WorkspacePolicy::new(workspace.path()).expect("policy"));
    let agent_loop = AgentLoop::new(provider, ContextBuilder::default(), executor);
    let (handle, control) = agent_execution_channel(4);
    let (trace_tx, mut trace_rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(async move {
        agent_loop
            .run_with_control_and_trace(
                AgentTaskRequest {
                    session_id: SessionId::new(),
                    task_id: TaskId::new(),
                    turn_id: TurnId::new(),
                    objective: "run command after resume".to_owned(),
                    completion_criteria: vec!["command evidence".to_owned()],
                    touched_code: false,
                    contributors: Vec::new(),
                    tools: vec!["shell".to_owned()],
                },
                control,
                move |event| {
                    let _ = trace_tx.send(event);
                },
            )
            .await
    });
    let approval = loop {
        let event = trace_rx.recv().await.expect("approval trace");
        if let AgentLoopTraceEvent::ApprovalRequested(approval) = event {
            break approval;
        }
    };

    handle.pause();
    handle
        .resolve_approval(ApprovalResolution {
            approval_id: approval.approval_id,
            decision: ApprovalDecision::Approved,
            reason: "approved while paused".to_owned(),
        })
        .await
        .expect("approval resolves");
    tokio::time::sleep(Duration::from_millis(50)).await;

    assert!(!output.exists());
    assert!(!task.is_finished());
    handle.resume();
    let outcome = task.await.expect("task joins").expect("loop completes");
    assert!(output.exists());
    assert_eq!(
        outcome.tool_reports[0].envelope.status,
        ToolResultStatus::Ok
    );
}

#[test]
fn checkpoint_restores_file_before_image_without_touching_git() {
    let workspace = tempdir().expect("workspace");
    let checkpoint_root = tempdir().expect("checkpoint");
    let source = workspace.path().join("src/lib.rs");
    fs::create_dir_all(source.parent().unwrap()).expect("parent");
    fs::write(&source, "pub fn value() -> u8 { 1 }").expect("source");
    let manager = WorkspaceCheckpointManager::new(workspace.path(), checkpoint_root.path());

    let checkpoint = manager
        .create_checkpoint(
            WorkspaceId::new(),
            TaskId::new(),
            TurnId::new(),
            &[FileBeforeImage {
                path: PathBuf::from("src/lib.rs"),
                content: Some(b"pub fn value() -> u8 { 1 }".to_vec()),
                unix_mode: Some(0o755),
            }],
            ToolCallId::new(),
        )
        .expect("checkpoint");

    fs::write(&source, "pub fn value() -> u8 { 2 }").expect("updated source");
    manager
        .restore_checkpoint(checkpoint.checkpoint_id)
        .expect("checkpoint restores");

    assert_eq!(checkpoint.changed_files, vec!["src/lib.rs"]);
    assert!(checkpoint_fingerprint(&checkpoint).starts_with("sha256:"));
    assert_eq!(
        fs::read_to_string(&source).expect("restored source"),
        "pub fn value() -> u8 { 1 }"
    );
    assert!(!workspace.path().join(".git").exists());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let checkpoint_dir = checkpoint_root
            .path()
            .join(checkpoint.checkpoint_id.to_string());
        let mode = |path: &Path| {
            fs::metadata(path)
                .expect("checkpoint metadata")
                .permissions()
                .mode()
                & 0o777
        };
        assert_eq!(mode(checkpoint_root.path()), 0o700);
        assert_eq!(mode(&checkpoint_dir), 0o700);
        assert_eq!(mode(&checkpoint_dir.join("manifest.json")), 0o600);
        assert_eq!(mode(&checkpoint_dir.join("files/src/lib.rs")), 0o600);
        assert_eq!(mode(&source), 0o755);
    }
}

#[test]
fn checkpoint_restore_removes_file_created_by_task() {
    let workspace = tempdir().expect("workspace");
    let checkpoint_root = tempdir().expect("checkpoint");
    let source = workspace.path().join("created.txt");
    let manager = WorkspaceCheckpointManager::new(workspace.path(), checkpoint_root.path());

    let checkpoint = manager
        .create_checkpoint(
            WorkspaceId::new(),
            TaskId::new(),
            TurnId::new(),
            &[FileBeforeImage {
                path: PathBuf::from("created.txt"),
                content: None,
                unix_mode: None,
            }],
            ToolCallId::new(),
        )
        .expect("checkpoint");

    fs::write(&source, "created by task").expect("created source");
    manager
        .restore_checkpoint(checkpoint.checkpoint_id)
        .expect("checkpoint restores");

    assert!(!source.exists());
}

#[test]
fn checkpoint_rejects_parent_directory_escape() {
    let workspace = tempdir().expect("workspace");
    let outside = tempdir().expect("outside");
    let outside_file = outside.path().join("outside.txt");
    fs::write(&outside_file, "secret").expect("outside file");
    let checkpoint_root = tempdir().expect("checkpoint");
    let manager = WorkspaceCheckpointManager::new(workspace.path(), checkpoint_root.path());

    let result = manager.create_checkpoint(
        WorkspaceId::new(),
        TaskId::new(),
        TurnId::new(),
        &[FileBeforeImage {
            path: outside_file,
            content: Some(b"secret".to_vec()),
            unix_mode: None,
        }],
        ToolCallId::new(),
    );

    assert!(matches!(result, Err(CheckpointError::OutsideWorkspace(_))));
}

#[test]
fn checkpoint_restore_rejects_traversal_in_a_tampered_manifest() {
    let root = tempdir().expect("root");
    let workspace = root.path().join("workspace");
    let checkpoint_root = root.path().join("checkpoints");
    fs::create_dir(&workspace).expect("workspace");
    let outside = root.path().join("outside.txt");
    fs::write(&outside, "keep").expect("outside file");
    let manager = WorkspaceCheckpointManager::new(&workspace, &checkpoint_root);
    let checkpoint = manager
        .create_checkpoint(
            WorkspaceId::new(),
            TaskId::new(),
            TurnId::new(),
            &[FileBeforeImage {
                path: PathBuf::from("created.txt"),
                content: None,
                unix_mode: None,
            }],
            ToolCallId::new(),
        )
        .expect("checkpoint");
    let manifest = checkpoint_root
        .join(checkpoint.checkpoint_id.to_string())
        .join("manifest.json");
    fs::write(
        manifest,
        serde_json::to_vec(&json!({
            "entries": [{
                "path": "../outside.txt",
                "existed": false,
                "checksum": null
            }]
        }))
        .expect("manifest"),
    )
    .expect("tamper manifest");

    let error = manager
        .restore_checkpoint(checkpoint.checkpoint_id)
        .expect_err("traversal must be rejected");

    assert!(matches!(error, CheckpointError::InvalidManifest(_)));
    assert_eq!(
        fs::read_to_string(outside).expect("outside remains"),
        "keep"
    );
}

#[test]
fn checkpoint_validates_every_entry_before_restoring_any_file() {
    let workspace = tempdir().expect("workspace");
    let checkpoint_root = tempdir().expect("checkpoint");
    let first = workspace.path().join("first.txt");
    let second = workspace.path().join("second.txt");
    fs::write(&first, "first before").expect("first before");
    fs::write(&second, "second before").expect("second before");
    let manager = WorkspaceCheckpointManager::new(workspace.path(), checkpoint_root.path());
    let checkpoint = manager
        .create_checkpoint(
            WorkspaceId::new(),
            TaskId::new(),
            TurnId::new(),
            &[
                FileBeforeImage {
                    path: PathBuf::from("first.txt"),
                    content: Some(b"first before".to_vec()),
                    unix_mode: None,
                },
                FileBeforeImage {
                    path: PathBuf::from("second.txt"),
                    content: Some(b"second before".to_vec()),
                    unix_mode: None,
                },
            ],
            ToolCallId::new(),
        )
        .expect("checkpoint");
    fs::write(&first, "first after").expect("first after");
    fs::write(&second, "second after").expect("second after");
    let manifest = checkpoint_root
        .path()
        .join(checkpoint.checkpoint_id.to_string())
        .join("manifest.json");
    let mut value: serde_json::Value =
        serde_json::from_slice(&fs::read(&manifest).expect("manifest")).expect("manifest JSON");
    value["entries"][1]["checksum"] = json!("sha256:tampered");
    fs::write(
        &manifest,
        serde_json::to_vec(&value).expect("manifest JSON"),
    )
    .expect("tamper manifest");

    assert!(matches!(
        manager.restore_checkpoint(checkpoint.checkpoint_id),
        Err(CheckpointError::InvalidManifest(_))
    ));
    assert_eq!(fs::read_to_string(first).unwrap(), "first after");
    assert_eq!(fs::read_to_string(second).unwrap(), "second after");
}

#[test]
fn checkpoint_rejects_gitignored_before_images() {
    let workspace = tempdir().expect("workspace");
    let checkpoint_root = tempdir().expect("checkpoint");
    fs::write(workspace.path().join(".gitignore"), "ignored/\n*.secret\n").expect("gitignore");
    fs::create_dir(workspace.path().join("ignored")).expect("ignored directory");
    let manager = WorkspaceCheckpointManager::new(workspace.path(), checkpoint_root.path());

    for path in ["ignored/new.txt", "token.secret"] {
        let result = manager.create_checkpoint(
            WorkspaceId::new(),
            TaskId::new(),
            TurnId::new(),
            &[FileBeforeImage {
                path: workspace.path().join(path),
                content: None,
                unix_mode: None,
            }],
            ToolCallId::new(),
        );

        assert!(
            matches!(result, Err(CheckpointError::Excluded(_))),
            "{path}"
        );
    }
}
