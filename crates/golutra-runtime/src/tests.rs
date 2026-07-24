use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

use golutra_context::{ContextBudgetPolicy, ContextBuilder, ContextContributor, estimate_tokens};
use golutra_core::{
    Actor, ActorKind, BudgetOverflowAction, BusyPolicy, PolicyBlockDisposition, TaskStatus,
    ToolCallId, WorkspaceId,
};
use golutra_governor::GovernorLimits;
use golutra_llm::{
    LlmProvider, MockProvider, ProviderFinishReason, ProviderMessage, ProviderRequest,
    ProviderResponse, ProviderStreamEvent, ProviderToolCall, ProviderUsage, UsageSource,
};
use golutra_policy::WorkspacePolicy;
use golutra_protocol::{ExternalVerificationSpec, RuntimeEventType};
use golutra_tools::BasicToolExecutor;
use serde_json::json;
use tempfile::tempdir;

use super::*;

#[test]
fn runtime_observation_sink_accepts_a_function_adapter() {
    let mut observed = Vec::new();
    let mut sink = |observation| observed.push(observation);

    RuntimeObservationSink::emit(
        &mut sink,
        RuntimeObservation::ToolStarted {
            tool_call_id: ToolCallId::new(),
            provider_tool_call_id: None,
            tool_name: "read_file".to_owned(),
            display_arguments: json!({"path": "README.md"}),
        },
    );

    assert!(matches!(
        observed.as_slice(),
        [RuntimeObservation::ToolStarted { tool_name, .. }] if tool_name == "read_file"
    ));
}

#[test]
fn successful_tool_result_resets_only_the_consecutive_failure_count() {
    let mut total = 7;
    let mut consecutive = 3;

    update_tool_failure_counts(ToolResultStatus::Ok, &mut total, &mut consecutive);
    assert_eq!(total, 7);
    assert_eq!(consecutive, 0);

    update_tool_failure_counts(ToolResultStatus::Error, &mut total, &mut consecutive);
    assert_eq!(total, 8);
    assert_eq!(consecutive, 1);
}

#[test]
fn duplicate_failures_in_one_provider_round_count_as_one_retry() {
    let failed = HashSet::from(["shell:{\"command\":\"git\"}".to_owned()]);
    let mut signature = None;
    let mut count = 0;

    update_repeated_failure_streak(&failed, &mut signature, &mut count);
    assert_eq!(count, 1);
    update_repeated_failure_streak(&failed, &mut signature, &mut count);
    assert_eq!(count, 2);

    update_repeated_failure_streak(&HashSet::new(), &mut signature, &mut count);
    assert_eq!(signature, None);
    assert_eq!(count, 0);
}

#[derive(Debug, Clone)]
enum FallbackTestProvider {
    Failing(Box<golutra_core::ProviderContract>),
    Success(Box<MockProvider>),
}

#[derive(Debug, Clone)]
struct SixRoundProvider {
    calls: Arc<AtomicUsize>,
    contract: golutra_core::ProviderContract,
}

#[derive(Debug, Clone)]
struct SupportThenDeliveryProvider {
    calls: Arc<AtomicUsize>,
    contract: golutra_core::ProviderContract,
}

#[derive(Debug, Clone)]
struct ValidationGateProvider {
    calls: Arc<AtomicUsize>,
    saw_nudge: Arc<AtomicBool>,
    contract: golutra_core::ProviderContract,
}

#[derive(Debug, Clone)]
struct DuplicateFailureRecoveryProvider {
    calls: Arc<AtomicUsize>,
    saw_duplicate_results: Arc<AtomicBool>,
    contract: golutra_core::ProviderContract,
}

#[async_trait]
impl LlmProvider for ValidationGateProvider {
    async fn complete(&self, request: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        let usage = ProviderUsage {
            input_tokens: Some(10),
            output_tokens: Some(5),
            reasoning_tokens: None,
            cached_input_tokens: None,
            total_tokens: Some(15),
            usage_source: UsageSource::Estimated,
            raw: json!({"round": call}),
        };
        let (message, tool_calls, finish_reason) = match call {
            0 => (
                None,
                vec![ProviderToolCall {
                    tool_call_id: "write-result".to_owned(),
                    tool_name: "write_file".to_owned(),
                    arguments: json!({"path": "recovered.txt", "content": "source bytes"}),
                }],
                ProviderFinishReason::ToolCalls,
            ),
            1 => (
                Some(ProviderMessage {
                    role: ProviderRole::Assistant,
                    content: "recovery complete".to_owned(),
                    tool_call_id: None,
                    tool_name: None,
                    tool_calls: Vec::new(),
                    metadata: Default::default(),
                }),
                Vec::new(),
                ProviderFinishReason::Stop,
            ),
            2 => {
                self.saw_nudge.store(
                    request.messages.iter().any(|message| {
                        message.role == ProviderRole::User
                            && message
                                .content
                                .contains("Runtime verification is still missing")
                    }),
                    Ordering::SeqCst,
                );
                (
                    None,
                    vec![ProviderToolCall {
                        tool_call_id: "compare-result".to_owned(),
                        tool_name: "shell".to_owned(),
                        arguments: json!({"command": "cmp source.txt recovered.txt"}),
                    }],
                    ProviderFinishReason::ToolCalls,
                )
            }
            _ => (
                Some(ProviderMessage {
                    role: ProviderRole::Assistant,
                    content: "recovery verified".to_owned(),
                    tool_call_id: None,
                    tool_name: None,
                    tool_calls: Vec::new(),
                    metadata: Default::default(),
                }),
                Vec::new(),
                ProviderFinishReason::Stop,
            ),
        };
        Ok(ProviderResponse {
            response_id: golutra_core::ProviderResponseId::new(),
            message,
            tool_calls,
            usage,
            finish_reason,
            raw_metadata: json!({"round": call}),
        })
    }

    fn contract(&self) -> golutra_core::ProviderContract {
        self.contract.clone()
    }
}

#[async_trait]
impl LlmProvider for DuplicateFailureRecoveryProvider {
    async fn complete(&self, request: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        let usage = ProviderUsage {
            input_tokens: Some(10),
            output_tokens: Some(5),
            reasoning_tokens: None,
            cached_input_tokens: None,
            total_tokens: Some(15),
            usage_source: UsageSource::Estimated,
            raw: json!({"round": call}),
        };
        let (message, tool_calls, finish_reason) = match call {
            0 => (
                None,
                vec![
                    ProviderToolCall {
                        tool_call_id: "duplicate-shell-a".to_owned(),
                        tool_name: "shell".to_owned(),
                        arguments: json!({"command": "pwd && pwd"}),
                    },
                    ProviderToolCall {
                        tool_call_id: "duplicate-shell-b".to_owned(),
                        tool_name: "shell".to_owned(),
                        arguments: json!({"command": "pwd && pwd"}),
                    },
                ],
                ProviderFinishReason::ToolCalls,
            ),
            1 => {
                let tool_result_ids = request
                    .messages
                    .iter()
                    .filter(|message| message.role == ProviderRole::Tool)
                    .filter_map(|message| message.tool_call_id.as_deref())
                    .collect::<HashSet<_>>();
                self.saw_duplicate_results.store(
                    tool_result_ids == HashSet::from(["duplicate-shell-a", "duplicate-shell-b"]),
                    Ordering::SeqCst,
                );
                (
                    None,
                    vec![ProviderToolCall {
                        tool_call_id: "recovered-write".to_owned(),
                        tool_name: "write_file".to_owned(),
                        arguments: json!({"path": "result.txt", "content": "recovered\n"}),
                    }],
                    ProviderFinishReason::ToolCalls,
                )
            }
            _ => (
                Some(ProviderMessage {
                    role: ProviderRole::Assistant,
                    content: "recovered after duplicate failures".to_owned(),
                    tool_call_id: None,
                    tool_name: None,
                    tool_calls: Vec::new(),
                    metadata: Default::default(),
                }),
                Vec::new(),
                ProviderFinishReason::Stop,
            ),
        };
        Ok(ProviderResponse {
            response_id: golutra_core::ProviderResponseId::new(),
            message,
            tool_calls,
            usage,
            finish_reason,
            raw_metadata: json!({"round": call}),
        })
    }

    fn contract(&self) -> golutra_core::ProviderContract {
        self.contract.clone()
    }
}

#[async_trait]
impl LlmProvider for SupportThenDeliveryProvider {
    async fn complete(&self, _request: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        let usage = ProviderUsage {
            input_tokens: Some(10),
            output_tokens: Some(5),
            reasoning_tokens: None,
            cached_input_tokens: None,
            total_tokens: Some(15),
            usage_source: UsageSource::Estimated,
            raw: json!({"round": call}),
        };
        let (message, tool_calls, finish_reason) = match call {
            0 => (
                None,
                vec![ProviderToolCall {
                    tool_call_id: "read-support".to_owned(),
                    tool_name: "read_file".to_owned(),
                    arguments: json!({"path": "input.txt"}),
                }],
                ProviderFinishReason::ToolCalls,
            ),
            1 => (
                None,
                vec![ProviderToolCall {
                    tool_call_id: "write-support".to_owned(),
                    tool_name: "write_file".to_owned(),
                    arguments: json!({"path": "helper.py", "content": "print('support')"}),
                }],
                ProviderFinishReason::ToolCalls,
            ),
            2 => (
                None,
                vec![ProviderToolCall {
                    tool_call_id: "write-result".to_owned(),
                    tool_name: "write_file".to_owned(),
                    arguments: json!({"path": "results.txt", "content": "done"}),
                }],
                ProviderFinishReason::ToolCalls,
            ),
            _ => (
                Some(ProviderMessage {
                    role: ProviderRole::Assistant,
                    content: "done".to_owned(),
                    tool_call_id: None,
                    tool_name: None,
                    tool_calls: Vec::new(),
                    metadata: Default::default(),
                }),
                Vec::new(),
                ProviderFinishReason::Stop,
            ),
        };
        Ok(ProviderResponse {
            response_id: golutra_core::ProviderResponseId::new(),
            message,
            tool_calls,
            usage,
            finish_reason,
            raw_metadata: json!({"round": call}),
        })
    }

    fn contract(&self) -> golutra_core::ProviderContract {
        self.contract.clone()
    }
}

#[async_trait]
impl LlmProvider for SixRoundProvider {
    async fn complete(&self, _request: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        let usage = ProviderUsage {
            input_tokens: Some(32),
            output_tokens: Some(8),
            reasoning_tokens: None,
            cached_input_tokens: None,
            total_tokens: Some(40),
            usage_source: UsageSource::Estimated,
            raw: json!({"round": call}),
        };
        if call < 6 {
            return Ok(ProviderResponse {
                response_id: golutra_core::ProviderResponseId::new(),
                message: None,
                tool_calls: vec![ProviderToolCall {
                    tool_call_id: format!("round-{call}"),
                    tool_name: "read_file".to_owned(),
                    arguments: json!({"path": format!("round-{call}.txt")}),
                }],
                usage,
                finish_reason: ProviderFinishReason::ToolCalls,
                raw_metadata: json!({"round": call}),
            });
        }
        Ok(ProviderResponse {
            response_id: golutra_core::ProviderResponseId::new(),
            message: Some(ProviderMessage {
                role: ProviderRole::Assistant,
                content: "finished six rounds".to_owned(),
                tool_call_id: None,
                tool_name: None,
                tool_calls: Vec::new(),
                metadata: Default::default(),
            }),
            tool_calls: Vec::new(),
            usage,
            finish_reason: ProviderFinishReason::Stop,
            raw_metadata: json!({"round": call}),
        })
    }

    fn contract(&self) -> golutra_core::ProviderContract {
        self.contract.clone()
    }
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
        steer: false,
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
                steer: false,
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
            output_schema: None,
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
                output_schema: None,
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
        AgentLoopTraceEvent::ProviderStarted { provider_id, model_id, .. }
            if provider_id == "mock" && model_id == "mock-model"
    )));
    assert!(trace.iter().any(|event| matches!(
        event,
        AgentLoopTraceEvent::ProviderCompleted { provider_id, model_id, .. }
            if provider_id == "mock" && model_id == "mock-model"
    )));
    assert!(trace.iter().any(|event| matches!(
        event,
        AgentLoopTraceEvent::ProviderStreamed {
            provider_id,
            model_id,
            event: ProviderStreamEvent::TextDelta { text },
            ..
        } if provider_id == "mock" && model_id == "mock-model" && text == "fallback"
    )));
    assert!(trace.iter().any(|event| matches!(
        event,
        AgentLoopTraceEvent::TokenUsageRecorded(record)
            if record.provider_id == "mock" && record.model_id == "mock-model"
    )));
}

#[tokio::test]
async fn zero_iteration_budget_disables_the_legacy_fixed_round_cap() {
    let workspace = tempdir().expect("workspace");
    let executor = BasicToolExecutor::new(WorkspacePolicy::new(workspace.path()).expect("policy"));
    let governor = RuntimeGovernor::new(GovernorLimits {
        max_iterations: 0,
        ..GovernorLimits::default()
    });
    let agent_loop = AgentLoop::new(
        MockProvider::text_response("completed without fixed cap"),
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
                output_schema: None,
                touched_code: false,
                contributors: Vec::new(),
                tools: Vec::new(),
            },
            |event| trace.push(event),
        )
        .await
        .expect("governed outcome");

    assert!(outcome.final_message.is_some());
    assert!(trace.iter().any(|event| matches!(
        event,
        AgentLoopTraceEvent::AssistantMessage { content, .. }
            if content == "completed without fixed cap"
    )));
    assert!(!trace.iter().any(|event| matches!(
        event,
        AgentLoopTraceEvent::GovernorDecided(decision)
            if decision.action == GovernorAction::Block
    )));
}

#[tokio::test]
async fn agent_loop_can_complete_more_than_four_provider_tool_rounds() {
    let workspace = tempdir().expect("workspace");
    for round in 0..6 {
        fs::write(workspace.path().join(format!("round-{round}.txt")), "ok").expect("fixture");
    }
    let calls = Arc::new(AtomicUsize::new(0));
    let provider = SixRoundProvider {
        calls: Arc::clone(&calls),
        contract: MockProvider::text_response("contract").contract(),
    };
    let executor = BasicToolExecutor::new(WorkspacePolicy::new(workspace.path()).expect("policy"));
    let agent_loop = AgentLoop::new(provider, ContextBuilder::default(), executor);
    let mut trace = Vec::new();

    let outcome = agent_loop
        .run_with_trace(
            AgentTaskRequest {
                session_id: SessionId::new(),
                task_id: TaskId::new(),
                turn_id: TurnId::new(),
                objective: "read six files and report completion".to_owned(),
                completion_criteria: vec!["all files read".to_owned()],
                output_schema: None,
                touched_code: false,
                contributors: Vec::new(),
                tools: vec!["read_file".to_owned()],
            },
            |event| trace.push(event),
        )
        .await
        .expect("long loop");

    assert_eq!(calls.load(Ordering::SeqCst), 7);
    assert!(outcome.final_message.is_some());
    assert!(trace.iter().any(|event| matches!(
        event,
        AgentLoopTraceEvent::AssistantMessage { content, .. }
            if content == "finished six rounds"
    )));
    assert_eq!(
        trace
            .iter()
            .filter(|event| matches!(event, AgentLoopTraceEvent::StepStarted(_)))
            .count(),
        7
    );
    assert_eq!(
        trace
            .iter()
            .filter(|event| matches!(event, AgentLoopTraceEvent::StepCheckpointed(_)))
            .count(),
        7
    );
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
                output_schema: None,
                touched_code: false,
                contributors: vec![ContextContributor {
                    name: "objective".to_owned(),
                    role: ProviderRole::User,
                    content: "large context ".repeat(20),
                    token_budget_hint: 0,
                    source_refs: Vec::new(),
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
async fn accumulated_tool_messages_are_compacted_and_the_turn_continues() {
    let workspace = tempdir().expect("workspace");
    fs::write(workspace.path().join("large.txt"), "x".repeat(4_096)).expect("fixture");
    let executor = BasicToolExecutor::new(WorkspacePolicy::new(workspace.path()).expect("policy"));
    let contributor = ContextContributor {
        name: "objective".to_owned(),
        role: ProviderRole::User,
        content: "read large.txt".to_owned(),
        token_budget_hint: 0,
        source_refs: Vec::new(),
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
        budget_limit: initial_tokens.saturating_add(256),
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
                completion_criteria: Vec::new(),
                output_schema: None,
                touched_code: false,
                contributors: vec![contributor],
                tools: vec!["read_file".to_owned()],
            },
            |event| trace.push(event),
        )
        .await
        .expect("compacted outcome");

    assert_eq!(outcome.tool_reports.len(), 1);
    assert_eq!(outcome.loop_decision.action, LoopAction::StopSuccess);
    assert_eq!(
        trace
            .iter()
            .filter(|event| matches!(event, AgentLoopTraceEvent::ProviderStarted { .. }))
            .count(),
        2
    );
    assert!(
        trace
            .iter()
            .any(|event| matches!(event, AgentLoopTraceEvent::ContextAutoCompacted(_)))
    );
    assert!(!trace.iter().any(|event| matches!(
        event,
        AgentLoopTraceEvent::LoopGuardTriggered {
            trigger: golutra_core::LoopGuardTrigger::ContextOverflow,
            ..
        }
    )));
}

#[tokio::test]
async fn agent_loop_does_not_treat_a_write_as_objective_validation() {
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
            output_schema: None,
            touched_code: true,
            contributors: Vec::new(),
            tools: vec!["write_file".to_owned()],
        })
        .await
        .expect("loop runs");

    assert_eq!(outcome.loop_decision.action, LoopAction::StopPartial);
    assert!(
        !outcome.verification.checks.iter().any(|check| {
            check.kind == golutra_core::VerificationCheckKind::ObjectiveValidation
        })
    );
    assert_eq!(
        fs::read_to_string(workspace.path().join("result.txt")).unwrap(),
        "done"
    );
}

#[tokio::test]
async fn workspace_change_is_returned_to_the_model_until_fresh_validation_passes() {
    let workspace = tempdir().expect("workspace");
    fs::write(workspace.path().join("source.txt"), "source bytes").expect("source");
    let calls = Arc::new(AtomicUsize::new(0));
    let saw_nudge = Arc::new(AtomicBool::new(false));
    let provider = ValidationGateProvider {
        calls: calls.clone(),
        saw_nudge: saw_nudge.clone(),
        contract: MockProvider::text_response("unused").contract(),
    };
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
                    objective: "recover source.txt into recovered.txt exactly".to_owned(),
                    completion_criteria: Vec::new(),
                    output_schema: None,
                    touched_code: true,
                    contributors: Vec::new(),
                    tools: vec!["write_file".to_owned(), "shell".to_owned()],
                },
                control,
                move |event| {
                    let _ = trace_tx.send(event);
                },
            )
            .await
    });
    let mut trace = Vec::new();
    let approval = loop {
        let event = trace_rx.recv().await.expect("approval trace");
        if let AgentLoopTraceEvent::ApprovalRequested(approval) = &event {
            trace.push(event.clone());
            break approval.clone();
        }
        trace.push(event);
    };
    handle
        .resolve_approval(ApprovalResolution {
            approval_id: approval.approval_id,
            decision: ApprovalDecision::Approved,
            reason: "approved by test".to_owned(),
        })
        .await
        .expect("approval resolves");
    let outcome = task.await.expect("task joins").expect("loop runs");
    while let Ok(event) = trace_rx.try_recv() {
        trace.push(event);
    }

    assert_eq!(calls.load(Ordering::SeqCst), 4);
    assert!(saw_nudge.load(Ordering::SeqCst));
    assert_eq!(outcome.verification.result, VerificationResult::Pass);
    assert_eq!(outcome.loop_decision.action, LoopAction::StopSuccess);
    assert!(outcome.verification.checks.iter().any(|check| {
        check.kind == golutra_core::VerificationCheckKind::ObjectiveValidation
            && check.command.as_deref() == Some("cmp source.txt recovered.txt")
            && check.passed
    }));
    assert!(trace.iter().any(|event| matches!(
        event,
        AgentLoopTraceEvent::RetryScheduled { reason, .. }
            if reason.contains("without fresh objective validation")
    )));
}

#[tokio::test]
async fn code_change_without_an_objective_validation_is_partial() {
    let workspace = tempdir().expect("workspace");
    let provider = MockProvider::tool_call(
        "write_file",
        json!({"path": "src/lib.rs", "content": "pub fn answer() -> u8 { 42 }"}),
    );
    fs::create_dir_all(workspace.path().join("src")).expect("source directory");
    let executor = BasicToolExecutor::new(WorkspacePolicy::new(workspace.path()).expect("policy"));
    let agent_loop = AgentLoop::new(provider, ContextBuilder::default(), executor);

    let outcome = agent_loop
        .run(AgentTaskRequest {
            session_id: SessionId::new(),
            task_id: TaskId::new(),
            turn_id: TurnId::new(),
            objective: "write Rust code".to_owned(),
            completion_criteria: vec!["tests pass".to_owned()],
            output_schema: None,
            touched_code: true,
            contributors: Vec::new(),
            tools: vec!["write_file".to_owned()],
        })
        .await
        .expect("loop runs");

    assert_eq!(outcome.verification.result, VerificationResult::Partial);
    assert_eq!(outcome.loop_decision.action, LoopAction::StopPartial);
    assert!(outcome.verification.checks.iter().any(|check| {
        check.kind == golutra_core::VerificationCheckKind::WorkspaceChange && check.passed
    }));
    assert!(!outcome.verification.checks.iter().any(|check| {
        check.kind == golutra_core::VerificationCheckKind::ObjectiveValidation && check.passed
    }));
}

#[tokio::test]
async fn caller_declared_verifier_controls_code_change_completion() {
    for (path, expected_result, expected_action) in [
        (
            "src/lib.rs",
            VerificationResult::Pass,
            LoopAction::StopSuccess,
        ),
        (
            "src/missing.rs",
            VerificationResult::Fail,
            LoopAction::StopFailed,
        ),
    ] {
        let workspace = tempdir().expect("workspace");
        fs::create_dir_all(workspace.path().join("src")).expect("source directory");
        let provider = MockProvider::tool_call(
            "write_file",
            json!({"path": "src/lib.rs", "content": "pub fn answer() -> u8 { 42 }"}),
        );
        let executor =
            BasicToolExecutor::new(WorkspacePolicy::new(workspace.path()).expect("policy"));
        let agent_loop = AgentLoop::new(provider, ContextBuilder::default(), executor)
            .with_external_verifiers(vec![ExternalVerificationSpec {
                program: "test".to_owned(),
                args: vec!["-f".to_owned(), path.to_owned()],
                cwd: ".".to_owned(),
                timeout_ms: 5_000,
                expected_exit_code: 0,
                max_output_bytes: 1024,
            }]);
        let mut trace = Vec::new();

        let outcome = agent_loop
            .run_with_trace(
                AgentTaskRequest {
                    session_id: SessionId::new(),
                    task_id: TaskId::new(),
                    turn_id: TurnId::new(),
                    objective: "write Rust code".to_owned(),
                    completion_criteria: vec!["tests pass".to_owned()],
                    output_schema: None,
                    touched_code: true,
                    contributors: Vec::new(),
                    tools: vec!["write_file".to_owned(), "shell".to_owned()],
                },
                |event| trace.push(event),
            )
            .await
            .expect("loop runs");

        assert_eq!(outcome.verification.result, expected_result);
        assert_eq!(outcome.loop_decision.action, expected_action);
        assert!(outcome.verification.checks.iter().any(|check| {
            check.name == "objective:test:external_verifier"
                && check.passed == (expected_result == VerificationResult::Pass)
                && !check.evidence_refs.is_empty()
        }));
        assert!(outcome.tool_reports.iter().any(|report| {
            report.envelope.tool_name == "external_verifier" && !report.artifact_contents.is_empty()
        }));
        assert!(!trace.iter().any(|event| matches!(
            event,
            AgentLoopTraceEvent::RetryScheduled { reason, .. }
                if reason.contains("without fresh objective validation")
        )));
    }
}

#[tokio::test]
async fn caller_declared_verifier_can_validate_an_unchanged_existing_delivery() {
    let workspace = tempdir().expect("workspace");
    fs::write(workspace.path().join("results.txt"), "done\n").expect("existing result");
    let provider = MockProvider::text_response("The existing result is valid.");
    let executor = BasicToolExecutor::new(WorkspacePolicy::new(workspace.path()).expect("policy"));
    let outcome = AgentLoop::new(provider, ContextBuilder::default(), executor)
        .with_external_verifiers(vec![ExternalVerificationSpec {
            program: "test".to_owned(),
            args: vec!["-f".to_owned(), "results.txt".to_owned()],
            cwd: ".".to_owned(),
            timeout_ms: 5_000,
            expected_exit_code: 0,
            max_output_bytes: 1024,
        }])
        .run(AgentTaskRequest {
            session_id: SessionId::new(),
            task_id: TaskId::new(),
            turn_id: TurnId::new(),
            objective: "verify the existing results.txt without changing it".to_owned(),
            completion_criteria: vec!["results.txt contains the expected result".to_owned()],
            output_schema: None,
            touched_code: false,
            contributors: Vec::new(),
            tools: Vec::new(),
        })
        .await
        .expect("loop runs");

    assert_eq!(outcome.verification.result, VerificationResult::Pass);
    assert_eq!(outcome.loop_decision.action, LoopAction::StopSuccess);
    assert!(
        !outcome
            .verification
            .checks
            .iter()
            .any(|check| check.name == "objective:path:delivery")
    );
}

#[test]
fn verification_command_classifier_rejects_arbitrary_shell_success() {
    assert!(is_objective_validation_command(
        "cargo test -p golutra-runtime"
    ));
    assert!(is_objective_validation_command("npm run typecheck"));
    assert!(!is_objective_validation_command("echo done"));
    assert!(!is_objective_validation_command("echo tests passed"));
    assert!(!is_objective_validation_command("git status --short"));
    assert!(!is_objective_validation_command("git log --oneline -2"));
    assert!(!is_objective_validation_command("git diff --exit-code"));
    assert!(is_objective_validation_command(
        "git diff --exit-code source HEAD -- src/lib.rs"
    ));
    assert!(is_objective_validation_command(
        "git merge-base --is-ancestor source HEAD"
    ));
    assert!(is_objective_validation_command(
        "cmp expected.txt actual.txt"
    ));
    assert!(is_objective_validation_command(
        "diff -q expected.txt actual.txt"
    ));
    assert!(!is_objective_validation_command("go version"));
}

#[test]
fn test_output_classifier_requires_an_executed_test() {
    assert!(line_reports_executed_tests(
        "test result: ok. 3 passed; 0 failed; 0 ignored"
    ));
    assert!(line_reports_executed_tests("running 1 test"));
    assert!(!line_reports_executed_tests(
        "test result: ok. 0 passed; 0 failed; 0 ignored"
    ));
    assert!(!line_reports_executed_tests("running 0 tests"));
}

#[tokio::test]
async fn agent_loop_does_not_accept_a_write_to_the_wrong_requested_path() {
    let workspace = tempdir().expect("workspace");
    let provider = MockProvider::tool_call(
        "write_file",
        json!({"path": "wrong.txt", "content": "expected"}),
    );
    let executor = BasicToolExecutor::new(WorkspacePolicy::new(workspace.path()).expect("policy"));
    let agent_loop = AgentLoop::new(provider, ContextBuilder::default(), executor);

    let outcome = agent_loop
        .run(AgentTaskRequest {
            session_id: SessionId::new(),
            task_id: TaskId::new(),
            turn_id: TurnId::new(),
            objective: "write expected.txt with content expected".to_owned(),
            completion_criteria: vec!["expected.txt contains expected".to_owned()],
            output_schema: None,
            touched_code: false,
            contributors: Vec::new(),
            tools: vec!["write_file".to_owned()],
        })
        .await
        .expect("loop runs");

    assert_ne!(outcome.loop_decision.action, LoopAction::StopSuccess);
    assert!(
        outcome
            .verification
            .checks
            .iter()
            .any(|check| { check.name == "objective:path:delivery" && !check.passed })
    );
}

#[tokio::test]
async fn supporting_read_paths_do_not_fail_a_correct_delivery_path() {
    let workspace = tempdir().expect("workspace");
    fs::write(workspace.path().join("input.txt"), "source").expect("input");
    let provider = SupportThenDeliveryProvider {
        calls: Arc::new(AtomicUsize::new(0)),
        contract: MockProvider::text_response("unused").contract(),
    };
    let executor = BasicToolExecutor::new(WorkspacePolicy::new(workspace.path()).expect("policy"));
    let outcome = AgentLoop::new(provider, ContextBuilder::default(), executor)
        .run(AgentTaskRequest {
            session_id: SessionId::new(),
            task_id: TaskId::new(),
            turn_id: TurnId::new(),
            objective: "read input.txt and write results.txt; diagnostic: /tmp/very/long/verify.py"
                .to_owned(),
            completion_criteria: vec!["results.txt is delivered".to_owned()],
            output_schema: None,
            touched_code: true,
            contributors: Vec::new(),
            tools: vec!["read_file".to_owned(), "write_file".to_owned()],
        })
        .await
        .expect("loop runs");

    assert!(
        outcome
            .verification
            .checks
            .iter()
            .any(|check| { check.name == "objective:path:delivery" && check.passed })
    );
    assert!(workspace.path().join("helper.py").is_file());
    assert!(workspace.path().join("results.txt").is_file());
}

#[tokio::test]
async fn agent_loop_does_not_accept_wrong_written_content() {
    let workspace = tempdir().expect("workspace");
    let provider = MockProvider::tool_call(
        "write_file",
        json!({"path": "expected.txt", "content": "wrong"}),
    );
    let executor = BasicToolExecutor::new(WorkspacePolicy::new(workspace.path()).expect("policy"));
    let agent_loop = AgentLoop::new(provider, ContextBuilder::default(), executor);

    let outcome = agent_loop
        .run(AgentTaskRequest {
            session_id: SessionId::new(),
            task_id: TaskId::new(),
            turn_id: TurnId::new(),
            objective: "write expected.txt with content expected".to_owned(),
            completion_criteria: vec!["expected.txt contains expected".to_owned()],
            output_schema: None,
            touched_code: false,
            contributors: Vec::new(),
            tools: vec!["write_file".to_owned()],
        })
        .await
        .expect("loop runs");

    assert_ne!(outcome.loop_decision.action, LoopAction::StopSuccess);
    assert!(
        outcome
            .verification
            .checks
            .iter()
            .any(|check| { check.name == "objective:content:write_file" && !check.passed })
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
            output_schema: None,
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
            output_schema: None,
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
            output_schema: None,
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
async fn output_schema_is_verified_by_the_runtime_before_success() {
    let workspace = tempdir().expect("workspace");
    let executor = BasicToolExecutor::new(WorkspacePolicy::new(workspace.path()).expect("policy"));
    let agent_loop = AgentLoop::new(
        MockProvider::text_response(r#"{"answer":"ok"}"#),
        ContextBuilder::default(),
        executor,
    );

    let outcome = agent_loop
        .run(AgentTaskRequest {
            session_id: SessionId::new(),
            task_id: TaskId::new(),
            turn_id: TurnId::new(),
            objective: "return a structured answer".to_owned(),
            completion_criteria: vec!["assistant response".to_owned()],
            output_schema: Some(json!({
                "type": "object",
                "required": ["answer"],
                "properties": {"answer": {"type": "string"}},
                "additionalProperties": false
            })),
            touched_code: false,
            contributors: Vec::new(),
            tools: Vec::new(),
        })
        .await
        .expect("schema-valid response");

    assert_eq!(outcome.verification.result, VerificationResult::Pass);
    assert!(
        outcome
            .verification
            .checks
            .iter()
            .any(|check| { check.kind == VerificationCheckKind::Schema && check.passed })
    );
}

#[tokio::test]
async fn output_schema_failure_is_a_runtime_turn_failure() {
    let workspace = tempdir().expect("workspace");
    let executor = BasicToolExecutor::new(WorkspacePolicy::new(workspace.path()).expect("policy"));
    let agent_loop = AgentLoop::new(
        MockProvider::text_response(r#"{"answer":42}"#),
        ContextBuilder::default(),
        executor,
    );

    let outcome = agent_loop
        .run(AgentTaskRequest {
            session_id: SessionId::new(),
            task_id: TaskId::new(),
            turn_id: TurnId::new(),
            objective: "return a structured answer".to_owned(),
            completion_criteria: vec!["assistant response".to_owned()],
            output_schema: Some(json!({
                "type": "object",
                "required": ["answer"],
                "properties": {"answer": {"type": "string"}},
                "additionalProperties": false
            })),
            touched_code: false,
            contributors: Vec::new(),
            tools: Vec::new(),
        })
        .await
        .expect("schema failure is represented in the outcome");

    assert_ne!(outcome.verification.result, VerificationResult::Pass);
    assert!(
        outcome
            .verification
            .checks
            .iter()
            .any(|check| { check.kind == VerificationCheckKind::Schema && !check.passed })
    );
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
            steer: false,
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
                output_schema: None,
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
            output_schema: None,
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
async fn agent_loop_returns_recoverable_tool_failure_to_the_provider() {
    let workspace = tempdir().expect("workspace");
    let provider = MockProvider::tool_call("read_file", json!({"path": "missing.md"}));
    let executor = BasicToolExecutor::new(WorkspacePolicy::new(workspace.path()).expect("policy"));
    let agent_loop = AgentLoop::new(provider, ContextBuilder::default(), executor);
    let mut trace = Vec::new();

    let outcome = agent_loop
        .run_with_trace(
            AgentTaskRequest {
                session_id: SessionId::new(),
                task_id: TaskId::new(),
                turn_id: TurnId::new(),
                objective: "read missing file".to_owned(),
                completion_criteria: vec!["file read evidence".to_owned()],
                output_schema: None,
                touched_code: false,
                contributors: Vec::new(),
                tools: vec!["read_file".to_owned()],
            },
            |event| trace.push(event),
        )
        .await
        .expect("loop runs");

    assert_eq!(outcome.loop_decision.action, LoopAction::StopFailed);
    assert!(
        !outcome
            .loop_decision
            .reason
            .contains("security or policy boundary rejected"),
        "{:?}",
        outcome.loop_decision
    );
    assert_eq!(
        outcome.tool_reports[0].policy_evaluation.block_disposition,
        Some(PolicyBlockDisposition::Recoverable)
    );
    assert_eq!(
        trace
            .iter()
            .filter(|event| matches!(event, AgentLoopTraceEvent::ProviderCompleted { .. }))
            .count(),
        2,
        "the blocked tool result must reach a follow-up provider turn"
    );
    assert_eq!(outcome.verification.result, VerificationResult::Fail);
}

#[tokio::test]
async fn duplicate_failures_in_one_provider_round_can_recover_on_the_next_round() {
    let workspace = tempdir().expect("workspace");
    fs::write(workspace.path().join("expected.txt"), "recovered\n").expect("expected result");
    let calls = Arc::new(AtomicUsize::new(0));
    let saw_duplicate_results = Arc::new(AtomicBool::new(false));
    let provider = DuplicateFailureRecoveryProvider {
        calls: calls.clone(),
        saw_duplicate_results: saw_duplicate_results.clone(),
        contract: MockProvider::text_response("unused").contract(),
    };
    let executor = BasicToolExecutor::new(WorkspacePolicy::new(workspace.path()).expect("policy"));
    let mut trace = Vec::new();

    let outcome = AgentLoop::new(provider, ContextBuilder::default(), executor)
        .with_external_verifiers(vec![ExternalVerificationSpec {
            program: "cmp".to_owned(),
            args: vec!["expected.txt".to_owned(), "result.txt".to_owned()],
            cwd: ".".to_owned(),
            timeout_ms: 5_000,
            expected_exit_code: 0,
            max_output_bytes: 1024,
        }])
        .run_with_trace(
            AgentTaskRequest {
                session_id: SessionId::new(),
                task_id: TaskId::new(),
                turn_id: TurnId::new(),
                objective: "write the recovered delivery to result.txt".to_owned(),
                completion_criteria: vec!["result.txt is delivered".to_owned()],
                output_schema: None,
                touched_code: false,
                contributors: Vec::new(),
                tools: vec!["shell".to_owned(), "write_file".to_owned()],
            },
            |event| trace.push(event),
        )
        .await
        .expect("loop recovers");

    assert_eq!(calls.load(Ordering::SeqCst), 3);
    assert!(saw_duplicate_results.load(Ordering::SeqCst));
    assert_eq!(
        fs::read_to_string(workspace.path().join("result.txt")).expect("result"),
        "recovered\n"
    );
    assert_eq!(
        outcome.verification.result,
        VerificationResult::Pass,
        "verification={:#?}\nplan={:#?}\nreports={:#?}",
        outcome.verification,
        outcome.verification_plan,
        outcome.tool_reports
    );
    assert_eq!(outcome.loop_decision.action, LoopAction::StopSuccess);
    assert_eq!(
        outcome.final_message.as_deref(),
        Some("recovered after duplicate failures")
    );
    assert!(!trace.iter().any(|event| matches!(
        event,
        AgentLoopTraceEvent::LoopGuardTriggered {
            trigger: golutra_core::LoopGuardTrigger::RepeatedToolFailure,
            ..
        }
    )));
}

#[tokio::test]
async fn agent_loop_stops_after_a_terminal_sensitive_path_block() {
    let workspace = tempdir().expect("workspace");
    fs::create_dir(workspace.path().join(".git")).expect("git directory");
    fs::write(workspace.path().join(".git/config"), "secret").expect("git config");
    let provider = MockProvider::tool_call("read_file", json!({"path": ".git/config"}));
    let executor = BasicToolExecutor::new(WorkspacePolicy::new(workspace.path()).expect("policy"));
    let agent_loop = AgentLoop::new(provider, ContextBuilder::default(), executor);
    let mut trace = Vec::new();

    let outcome = agent_loop
        .run_with_trace(
            AgentTaskRequest {
                session_id: SessionId::new(),
                task_id: TaskId::new(),
                turn_id: TurnId::new(),
                objective: "read internal git configuration".to_owned(),
                completion_criteria: vec!["git configuration returned".to_owned()],
                output_schema: None,
                touched_code: false,
                contributors: Vec::new(),
                tools: vec!["read_file".to_owned()],
            },
            |event| trace.push(event),
        )
        .await
        .expect("loop runs");

    assert_eq!(outcome.loop_decision.action, LoopAction::Blocked);
    assert!(
        outcome
            .loop_decision
            .reason
            .contains("security or policy boundary rejected")
    );
    assert_eq!(
        outcome.tool_reports[0].policy_evaluation.block_disposition,
        Some(PolicyBlockDisposition::Terminal)
    );
    assert_eq!(
        trace
            .iter()
            .filter(|event| matches!(event, AgentLoopTraceEvent::ProviderCompleted { .. }))
            .count(),
        1,
        "terminal policy blocks must not start another provider turn"
    );
}

#[tokio::test]
async fn hard_tool_execution_errors_still_emit_a_terminal_report() {
    let workspace = tempdir().expect("workspace");
    fs::write(workspace.path().join("binary.txt"), [0xff]).expect("binary fixture");
    let provider = MockProvider::tool_call("read_file", json!({"path": "binary.txt"}));
    let executor = BasicToolExecutor::new(WorkspacePolicy::new(workspace.path()).expect("policy"));
    let agent_loop = AgentLoop::new(provider, ContextBuilder::default(), executor);
    let mut trace = Vec::new();

    let outcome = agent_loop
        .run_with_trace(
            AgentTaskRequest {
                session_id: SessionId::new(),
                task_id: TaskId::new(),
                turn_id: TurnId::new(),
                objective: "read binary.txt".to_owned(),
                completion_criteria: vec!["file read evidence".to_owned()],
                output_schema: None,
                touched_code: false,
                contributors: Vec::new(),
                tools: vec!["read_file".to_owned()],
            },
            |event| trace.push(event),
        )
        .await
        .expect("execution error becomes a terminal report");

    assert_eq!(outcome.tool_reports.len(), 1);
    assert_eq!(
        outcome.tool_reports[0].envelope.status,
        ToolResultStatus::Error
    );
    assert_eq!(
        outcome.tool_reports[0].envelope.summary,
        "tool execution failed"
    );
    assert!(trace.iter().any(|event| matches!(
        event,
        AgentLoopTraceEvent::ToolCompleted(report)
            if report.envelope.status == ToolResultStatus::Error
    )));
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
                    output_schema: None,
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
        ToolResultStatus::Ok,
        "{:?}",
        outcome.tool_reports[0]
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
                    output_schema: None,
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
    assert!(output.exists(), "{:?}", outcome.tool_reports);
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
                metadata: None,
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

#[cfg(unix)]
#[test]
fn checkpoints_hard_link_identical_before_images() {
    use std::os::unix::fs::MetadataExt;

    let workspace = tempdir().expect("workspace");
    let checkpoint_root = tempdir().expect("checkpoint");
    fs::write(workspace.path().join("shared.txt"), "same baseline").expect("baseline");
    let manager = WorkspaceCheckpointManager::new(workspace.path(), checkpoint_root.path());
    let before_image = FileBeforeImage {
        path: PathBuf::from("shared.txt"),
        content: Some(b"same baseline".to_vec()),
        unix_mode: Some(0o644),
        metadata: None,
    };
    let first = manager
        .create_checkpoint(
            WorkspaceId::new(),
            TaskId::new(),
            TurnId::new(),
            std::slice::from_ref(&before_image),
            ToolCallId::new(),
        )
        .expect("first checkpoint");
    let second = manager
        .create_checkpoint(
            WorkspaceId::new(),
            TaskId::new(),
            TurnId::new(),
            std::slice::from_ref(&before_image),
            ToolCallId::new(),
        )
        .expect("second checkpoint");

    let first_path = checkpoint_root
        .path()
        .join(first.checkpoint_id.to_string())
        .join("files/shared.txt");
    let second_path = checkpoint_root
        .path()
        .join(second.checkpoint_id.to_string())
        .join("files/shared.txt");
    let first_metadata = fs::metadata(first_path).expect("first checkpoint file");
    let second_metadata = fs::metadata(second_path).expect("second checkpoint file");

    assert_eq!(first_metadata.ino(), second_metadata.ino());
    assert!(first_metadata.nlink() >= 3);
}

#[test]
fn checkpoint_retention_keeps_only_the_latest_bounded_set() {
    let workspace = tempdir().expect("workspace");
    let checkpoint_root = tempdir().expect("checkpoint");
    let manager = WorkspaceCheckpointManager::new(workspace.path(), checkpoint_root.path());
    for _ in 0..3 {
        manager
            .create_checkpoint(
                WorkspaceId::new(),
                TaskId::new(),
                TurnId::new(),
                &[],
                ToolCallId::new(),
            )
            .expect("checkpoint");
    }

    assert_eq!(manager.checkpoint_count().expect("count"), 3);
    assert_eq!(manager.prune_checkpoints(1).expect("prune"), 2);
    assert_eq!(manager.checkpoint_count().expect("count"), 1);
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
                metadata: None,
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
            metadata: None,
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
                metadata: None,
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
                    metadata: None,
                },
                FileBeforeImage {
                    path: PathBuf::from("second.txt"),
                    content: Some(b"second before".to_vec()),
                    unix_mode: None,
                    metadata: None,
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
                metadata: None,
            }],
            ToolCallId::new(),
        );

        assert!(
            matches!(result, Err(CheckpointError::Excluded(_))),
            "{path}"
        );
    }
}

#[test]
fn partial_checkpoint_filter_omits_ignored_images_but_keeps_safe_files() {
    let workspace = tempdir().expect("workspace");
    let checkpoint_root = tempdir().expect("checkpoint");
    fs::write(
        workspace.path().join(".gitignore"),
        ".gitignore\n*.secret\n",
    )
    .expect("gitignore");
    fs::write(workspace.path().join("safe.txt"), "safe").expect("safe file");
    fs::write(workspace.path().join("token.secret"), "secret").expect("ignored file");
    let manager = WorkspaceCheckpointManager::new(workspace.path(), checkpoint_root.path());
    let before_images = [
        FileBeforeImage {
            path: workspace.path().join(".gitignore"),
            content: Some(b".gitignore\n*.secret\n".to_vec()),
            unix_mode: None,
            metadata: None,
        },
        FileBeforeImage {
            path: workspace.path().join("safe.txt"),
            content: Some(b"safe".to_vec()),
            unix_mode: None,
            metadata: None,
        },
        FileBeforeImage {
            path: workspace.path().join("token.secret"),
            content: Some(b"secret".to_vec()),
            unix_mode: None,
            metadata: None,
        },
    ];

    let (retained, excluded_count) = manager
        .filter_checkpointable_before_images(&before_images)
        .expect("partial selection");
    let checkpoint = manager
        .create_checkpoint(
            WorkspaceId::new(),
            TaskId::new(),
            TurnId::new(),
            &retained,
            ToolCallId::new(),
        )
        .expect("partial checkpoint");

    assert_eq!(excluded_count, 2);
    assert_eq!(retained.len(), 1);
    assert_eq!(checkpoint.changed_files, vec!["safe.txt"]);
}
