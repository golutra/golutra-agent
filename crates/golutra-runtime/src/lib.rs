use std::{
    collections::VecDeque,
    path::Path,
    sync::{Arc, Mutex as StdMutex},
    time::{Duration, Instant},
};

use async_trait::async_trait;
use golutra_context::{
    ContextBuilder, ContextContributor, ContextError, context_snapshot_from_request,
    estimate_message_tokens, estimate_tokens, provider_request_from_plan, token_usage_record,
};
use golutra_core::{
    ApprovalDecision, ApprovalId, ApprovalRequest, ApprovalResolution, BudgetState, CommandId,
    LoopAction, LoopDecision, PolicyDecision, SessionId, TaskId, ToolProgress, ToolProgressPhase,
    ToolResultStatus, TurnId, VerificationCheck, VerificationCheckKind, VerificationPlan,
    VerificationRecord, VerificationResult,
};
use golutra_governor::{
    GoalLedger, GovernorAction, GovernorObservation, GovernorPhase, RuntimeGovernor,
};
use golutra_llm::{
    LlmProvider, ProviderError, ProviderMessage, ProviderRequest, ProviderResponse, ProviderRole,
};
use golutra_tools::{
    BasicToolExecutor, FileBeforeImage, ToolError, ToolExecutionReport, ToolRequest,
    redact_tool_arguments,
};
use golutra_verify::VerificationInput;
use serde_json::Value;
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

mod checkpoint;
mod completion;
mod context_guard;
mod lane;
mod provider_retry;
mod step_machine;
mod trace;
mod verification;

pub use checkpoint::{CheckpointError, WorkspaceCheckpointManager, checkpoint_fingerprint};
pub use golutra_protocol::UserProjection;
pub use lane::{RuntimeLaneError, RuntimeLaneManager, RuntimeTransition, is_active_status};
pub(crate) use step_machine::{StepCheckpoint, StepCompletion, StepMachine, StepSnapshot};
pub use trace::{AgentLoopTraceEvent, RuntimeObservation, RuntimeObservationSink};
pub use verification::RuntimeVerificationService;

#[derive(Debug, Error)]
pub enum AgentLoopError {
    #[error("context build failed")]
    Context(#[from] ContextError),
    #[error("provider call failed: {0}")]
    Provider(#[from] ProviderError),
    #[error("tool execution failed")]
    Tool(#[from] ToolError),
    #[error("checkpoint persistence failed: {0}")]
    Checkpoint(String),
    #[error("agent task was cancelled")]
    Cancelled,
    #[error("agent task no longer accepts queued turns")]
    PendingTurnQueueClosed,
    #[error("agent pending turn queue is full")]
    PendingTurnQueueFull,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AgentTaskRequest {
    pub session_id: SessionId,
    pub task_id: TaskId,
    pub turn_id: TurnId,
    pub objective: String,
    pub completion_criteria: Vec<String>,
    /// Optional machine-checkable response contract. It is verified by the
    /// runtime before the terminal task event is emitted.
    pub output_schema: Option<Value>,
    pub touched_code: bool,
    pub contributors: Vec<ContextContributor>,
    pub tools: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AgentLoopOutcome {
    pub verification: VerificationRecord,
    pub verification_plan: VerificationPlan,
    pub loop_decision: LoopDecision,
    pub tool_reports: Vec<ToolExecutionReport>,
    pub final_message: Option<String>,
    pub final_turn_id: TurnId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingAgentTurn {
    pub command_id: CommandId,
    pub turn_id: TurnId,
    pub content: String,
    /// A steer is a continuation of the active turn for stream projection;
    /// an ordinary queued prompt remains an independent turn.
    pub steer: bool,
}

#[derive(Debug, Clone)]
pub struct AgentExecutionHandle {
    cancellation: CancellationToken,
    pause: watch::Sender<bool>,
    pending_turns: Arc<PendingTurnQueue>,
    approvals: mpsc::Sender<ApprovalResolution>,
}

impl AgentExecutionHandle {
    pub fn cancel(&self) {
        self.pending_turns.close();
        self.cancellation.cancel();
    }

    pub fn pause(&self) {
        self.pause.send_replace(true);
    }

    pub fn resume(&self) {
        self.pause.send_replace(false);
    }

    pub async fn append_turn(&self, turn: PendingAgentTurn) -> Result<(), AgentLoopError> {
        self.pending_turns.push(turn)
    }

    pub async fn resolve_approval(
        &self,
        resolution: ApprovalResolution,
    ) -> Result<(), AgentLoopError> {
        self.approvals
            .send(resolution)
            .await
            .map_err(|_| AgentLoopError::Cancelled)
    }

    #[must_use]
    pub fn cancellation_token(&self) -> CancellationToken {
        self.cancellation.clone()
    }
}

#[derive(Debug)]
pub struct AgentExecutionControl {
    cancellation: CancellationToken,
    pause: watch::Receiver<bool>,
    pending_turns: Arc<PendingTurnQueue>,
    approvals: mpsc::Receiver<ApprovalResolution>,
}

#[derive(Debug)]
struct PendingTurnQueue {
    capacity: usize,
    state: StdMutex<PendingTurnQueueState>,
}

#[derive(Debug, Default)]
struct PendingTurnQueueState {
    accepting: bool,
    turns: VecDeque<PendingAgentTurn>,
}

impl PendingTurnQueue {
    fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            state: StdMutex::new(PendingTurnQueueState {
                accepting: true,
                turns: VecDeque::new(),
            }),
        }
    }

    fn push(&self, turn: PendingAgentTurn) -> Result<(), AgentLoopError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !state.accepting {
            return Err(AgentLoopError::PendingTurnQueueClosed);
        }
        if state.turns.len() >= self.capacity {
            return Err(AgentLoopError::PendingTurnQueueFull);
        }
        state.turns.push_back(turn);
        Ok(())
    }

    fn take_or_close(&self) -> Option<PendingAgentTurn> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match state.turns.pop_front() {
            Some(turn) => Some(turn),
            None => {
                state.accepting = false;
                None
            }
        }
    }

    fn close(&self) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .accepting = false;
    }
}

impl Drop for AgentExecutionControl {
    fn drop(&mut self) {
        self.pending_turns.close();
    }
}

#[async_trait]
pub trait BeforeSideEffectRecorder: std::fmt::Debug + Send + Sync {
    async fn persist_before_side_effect(
        &self,
        request: &ToolRequest,
        before_images: &[FileBeforeImage],
        complete: bool,
    ) -> Result<(), AgentLoopError>;
}

#[must_use]
pub fn agent_execution_channel(capacity: usize) -> (AgentExecutionHandle, AgentExecutionControl) {
    let cancellation = CancellationToken::new();
    let (pause_tx, pause_rx) = watch::channel(false);
    let pending_turns = Arc::new(PendingTurnQueue::new(capacity));
    let (approval_tx, approval_rx) = mpsc::channel(capacity.max(1));
    (
        AgentExecutionHandle {
            cancellation: cancellation.clone(),
            pause: pause_tx,
            pending_turns: pending_turns.clone(),
            approvals: approval_tx,
        },
        AgentExecutionControl {
            cancellation,
            pause: pause_rx,
            pending_turns,
            approvals: approval_rx,
        },
    )
}

#[derive(Debug)]
pub struct AgentLoop<P> {
    provider: P,
    fallback_provider: Option<P>,
    context_builder: ContextBuilder,
    tool_executor: BasicToolExecutor,
    verifier: RuntimeVerificationService,
    governor: RuntimeGovernor,
    before_side_effect_recorder: Option<Arc<dyn BeforeSideEffectRecorder>>,
}

impl<P> AgentLoop<P>
where
    P: LlmProvider,
{
    #[must_use]
    pub fn new(
        provider: P,
        context_builder: ContextBuilder,
        tool_executor: BasicToolExecutor,
    ) -> Self {
        Self {
            provider,
            fallback_provider: None,
            context_builder,
            tool_executor,
            verifier: RuntimeVerificationService::default(),
            governor: RuntimeGovernor::default(),
            before_side_effect_recorder: None,
        }
    }

    #[must_use]
    pub fn with_fallback(mut self, provider: P) -> Self {
        self.fallback_provider = Some(provider);
        self
    }

    #[must_use]
    pub fn with_governor(mut self, governor: RuntimeGovernor) -> Self {
        self.governor = governor;
        self
    }

    #[must_use]
    pub fn with_before_side_effect_recorder(
        mut self,
        recorder: Arc<dyn BeforeSideEffectRecorder>,
    ) -> Self {
        self.before_side_effect_recorder = Some(recorder);
        self
    }

    pub async fn run(&self, request: AgentTaskRequest) -> Result<AgentLoopOutcome, AgentLoopError> {
        let (_handle, control) = agent_execution_channel(1);
        self.run_with_control_and_trace(request, control, |_| {})
            .await
    }

    pub async fn run_with_trace<F>(
        &self,
        request: AgentTaskRequest,
        trace: F,
    ) -> Result<AgentLoopOutcome, AgentLoopError>
    where
        F: FnMut(AgentLoopTraceEvent) + Send,
    {
        let (_handle, control) = agent_execution_channel(1);
        self.run_with_control_and_trace(request, control, trace)
            .await
    }

    pub async fn run_with_observation_sink<S>(
        &self,
        request: AgentTaskRequest,
        sink: S,
    ) -> Result<AgentLoopOutcome, AgentLoopError>
    where
        S: RuntimeObservationSink,
    {
        let (_handle, control) = agent_execution_channel(1);
        self.run_with_control_and_observation_sink(request, control, sink)
            .await
    }

    pub async fn run_with_control_and_observation_sink<S>(
        &self,
        request: AgentTaskRequest,
        control: AgentExecutionControl,
        mut sink: S,
    ) -> Result<AgentLoopOutcome, AgentLoopError>
    where
        S: RuntimeObservationSink,
    {
        self.run_with_control_and_trace(request, control, move |observation| {
            sink.emit(observation);
        })
        .await
    }

    pub async fn run_with_control_and_trace<F>(
        &self,
        request: AgentTaskRequest,
        mut control: AgentExecutionControl,
        mut trace: F,
    ) -> Result<AgentLoopOutcome, AgentLoopError>
    where
        F: FnMut(AgentLoopTraceEvent) + Send,
    {
        let mut tool_reports = Vec::new();
        let mut last_assistant_message = None;
        let mut last_emitted_assistant_message = None;
        let mut current_turn_id = request.turn_id;
        let mut current_objective = request.objective.clone();
        let mut current_turn_touched_code = request.touched_code;
        let mut guard_reason = None;
        let mut repeated_failure_signature = None;
        let mut repeated_failure_count = 0_u32;
        let mut empty_response_count = 0_u32;
        let started_at = Instant::now();
        let mut tool_call_count = 0_u32;
        let mut failed_tool_call_count = 0_u32;
        let mut estimated_cost_microusd: Option<u64> = None;
        let mut governor_action = None;
        let mut goal_ledger = GoalLedger {
            task_id: request.task_id,
            original_objective: request.objective.clone(),
            success_criteria: request.completion_criteria.clone(),
            current_plan: request.completion_criteria.clone(),
            completed_steps: Vec::new(),
            open_risks: Vec::new(),
        };
        let mut last_budget_state = BudgetState {
            planned_input_tokens: None,
            actual_input_tokens: None,
            output_tokens: None,
            total_tokens: None,
            estimated_cost: None,
            budget_remaining: None,
            compact_recommended: false,
            cost_risk: "low".to_owned(),
        };
        let mut step_machine = StepMachine::default();
        let provider_tools = request
            .tools
            .iter()
            .map(|tool_name| {
                self.tool_executor
                    .registry()
                    .contract(tool_name)
                    .cloned()
                    .ok_or_else(|| ToolError::UnknownTool(tool_name.clone()))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let planned_tool_tokens = provider_tools
            .iter()
            .map(|contract| {
                serde_json::to_string(contract)
                    .map(|value| estimate_tokens(&value))
                    .unwrap_or_default()
            })
            .sum::<u64>();
        let base_plan = match self.context_builder.build(
            request.task_id,
            current_turn_id,
            request.contributors.clone(),
        ) {
            Ok(plan) => plan,
            Err(error) => return Ok(context_guard::outcome(&request, error, &mut trace)),
        };
        if !base_plan.trimmed_contributors.is_empty() {
            trace(AgentLoopTraceEvent::ContextCompacted {
                original_input_tokens: base_plan.original_planned_input_tokens,
                planned_input_tokens: base_plan.budget_snapshot.planned_input_tokens,
                trimmed_contributors: base_plan.trimmed_contributors.clone(),
            });
        }
        let mut messages = base_plan.messages.clone();
        let protected_prefix_len = base_plan.messages.len();
        let context_window_manager = self.context_builder.window_manager();

        'agent_loop: loop {
            let step_snapshot = step_machine.begin(current_turn_id);
            let iteration = step_snapshot.step_no;
            trace(AgentLoopTraceEvent::StepStarted(step_snapshot.clone()));
            control.wait_until_runnable().await?;
            let mut plan = base_plan.clone();
            plan.messages = messages.clone();
            plan.budget_snapshot.turn_id = current_turn_id;
            plan.budget_snapshot.planned_tool_tokens = planned_tool_tokens;
            plan.budget_snapshot.planned_input_tokens =
                estimate_message_tokens(&messages).saturating_add(planned_tool_tokens);
            if plan.budget_snapshot.planned_input_tokens > plan.budget_snapshot.budget_limit {
                trace(AgentLoopTraceEvent::ContextCompactionStarted {
                    original_input_tokens: plan.budget_snapshot.planned_input_tokens,
                    budget_limit: plan.budget_snapshot.budget_limit,
                });
                match context_window_manager.compact_if_needed(
                    current_turn_id,
                    protected_prefix_len,
                    &messages,
                    planned_tool_tokens,
                ) {
                    Ok(Some(record)) => {
                        messages = record.replacement_messages.clone();
                        plan.messages = messages.clone();
                        plan.budget_snapshot.planned_input_tokens =
                            record.replacement_estimated_tokens;
                        plan.budget_snapshot.planned_summary_tokens =
                            estimate_tokens(&record.summary);
                        trace(AgentLoopTraceEvent::ContextAutoCompacted(record));
                    }
                    Ok(None) => {}
                    Err(error) => {
                        trace(AgentLoopTraceEvent::ContextCompactionFailed {
                            planned_input_tokens: plan.budget_snapshot.planned_input_tokens,
                            budget_limit: plan.budget_snapshot.budget_limit,
                            reason: error.to_string(),
                        });
                    }
                }
            }
            if plan.budget_snapshot.planned_input_tokens > plan.budget_snapshot.budget_limit {
                let reason = ContextError::BudgetExceeded {
                    planned: plan.budget_snapshot.planned_input_tokens,
                    limit: plan.budget_snapshot.budget_limit,
                }
                .to_string();
                last_budget_state = BudgetState {
                    planned_input_tokens: Some(plan.budget_snapshot.planned_input_tokens),
                    actual_input_tokens: None,
                    output_tokens: None,
                    total_tokens: None,
                    estimated_cost: None,
                    budget_remaining: Some(0),
                    compact_recommended: true,
                    cost_risk: "blocked".to_owned(),
                };
                trace(AgentLoopTraceEvent::LoopGuardTriggered {
                    trigger: golutra_core::LoopGuardTrigger::ContextOverflow,
                    reason: reason.clone(),
                });
                finish_runtime_step(
                    &mut step_machine,
                    step_snapshot.clone(),
                    "context-overflow",
                    false,
                    &mut trace,
                );
                guard_reason = Some(reason);
                governor_action = Some(GovernorAction::AskUser);
                break;
            }
            trace(AgentLoopTraceEvent::ContextBuilt {
                contributors: plan.contributors.clone(),
                planned_input_tokens: plan.budget_snapshot.planned_input_tokens,
            });
            let governance = self.governor.evaluate(
                &goal_ledger,
                &GovernorObservation {
                    phase: GovernorPhase::Provider,
                    iteration: iteration.saturating_add(1),
                    tool_calls: tool_call_count,
                    failed_tool_calls: failed_tool_call_count,
                    planned_input_tokens: plan.budget_snapshot.planned_input_tokens,
                    elapsed_ms: elapsed_millis(started_at),
                    latest_action: current_objective.clone(),
                    estimated_cost_microusd,
                    policy_decision: None,
                    security_risk: "low".to_owned(),
                },
            );
            let permits_execution = governance.permits_execution();
            if !permits_execution {
                guard_reason = Some(governance.reason.clone());
                governor_action = Some(governance.action);
            }
            trace(AgentLoopTraceEvent::GovernorDecided(governance));
            if !permits_execution {
                trace(AgentLoopTraceEvent::LoopGuardTriggered {
                    trigger: if self.governor.limits().max_iterations > 0
                        && iteration >= self.governor.limits().max_iterations
                    {
                        golutra_core::LoopGuardTrigger::MaxIteration
                    } else {
                        golutra_core::LoopGuardTrigger::ContextOverflow
                    },
                    reason: guard_reason
                        .clone()
                        .unwrap_or_else(|| "runtime governor blocked execution".to_owned()),
                });
                finish_runtime_step(
                    &mut step_machine,
                    step_snapshot.clone(),
                    "governor-blocked",
                    false,
                    &mut trace,
                );
                break;
            }
            let provider_contract = self.provider.contract();
            let provider_request = provider_request_from_plan(
                &plan,
                request.task_id,
                current_turn_id,
                provider_contract.provider_id.clone(),
                provider_contract.model_id.clone(),
                provider_tools.clone(),
            );
            trace(AgentLoopTraceEvent::ContextSnapshotCaptured {
                snapshot: context_snapshot_from_request(
                    request.session_id,
                    &plan,
                    &provider_request,
                ),
                request: provider_request.clone(),
            });
            trace(AgentLoopTraceEvent::ProviderStarted {
                provider_id: provider_request.provider_id.clone(),
                model_id: provider_request.model_id.clone(),
            });
            let provider_result = self
                .complete_with_retry(provider_request.clone(), &mut control, &mut trace)
                .await;
            let (provider_response, completed_request) = match provider_result {
                Ok(result) => result,
                Err(error) => {
                    finish_runtime_step(
                        &mut step_machine,
                        step_snapshot.clone(),
                        format!("provider-error:{error}"),
                        false,
                        &mut trace,
                    );
                    return Err(if error == ProviderError::Cancelled {
                        AgentLoopError::Cancelled
                    } else {
                        AgentLoopError::Provider(error)
                    });
                }
            };
            let step_fingerprint = provider_response_fingerprint(&provider_response);
            if let Some(message) = provider_response
                .message
                .as_ref()
                .filter(|message| !message.content.trim().is_empty())
            {
                last_assistant_message = Some(message.content.trim().to_owned());
            }
            trace(AgentLoopTraceEvent::ProviderCompleted {
                provider_id: completed_request.provider_id.clone(),
                model_id: completed_request.model_id.clone(),
                finish_reason: provider_response.finish_reason,
                tool_call_count: provider_response.tool_calls.len(),
                usage: provider_response.usage.clone(),
                raw_metadata: provider_response.raw_metadata.clone(),
            });
            let usage_record = token_usage_record(
                &completed_request,
                provider_response.response_id,
                &plan.budget_snapshot,
                &provider_response.usage,
                &provider_contract.cost_model,
            );
            trace(AgentLoopTraceEvent::TokenUsageRecorded(
                usage_record.clone(),
            ));
            if let Some(cost) = usage_record.estimated_cost.and_then(cost_to_microusd) {
                estimated_cost_microusd = Some(
                    estimated_cost_microusd
                        .unwrap_or_default()
                        .saturating_add(cost),
                );
            }
            last_budget_state = BudgetState {
                planned_input_tokens: Some(plan.budget_snapshot.planned_input_tokens),
                actual_input_tokens: usage_record.input_tokens,
                output_tokens: usage_record.output_tokens,
                total_tokens: usage_record.total_tokens,
                estimated_cost: usage_record.estimated_cost.map(|cost| cost.to_string()),
                budget_remaining: plan
                    .budget_snapshot
                    .budget_limit
                    .checked_sub(plan.budget_snapshot.planned_input_tokens),
                compact_recommended: false,
                cost_risk: if usage_record.estimated_cost.is_some() {
                    "low"
                } else {
                    "unknown"
                }
                .to_owned(),
            };

            if let Some(content) = provider_response
                .message
                .as_ref()
                .map(|message| message.content.trim())
                .filter(|content| !content.is_empty())
            {
                let content = content.to_owned();
                trace(AgentLoopTraceEvent::AssistantMessage {
                    turn_id: current_turn_id,
                    content: content.clone(),
                });
                last_emitted_assistant_message = Some((current_turn_id, content));
            }

            if provider_response.tool_calls.is_empty() {
                if let Some(message) = provider_response.message {
                    messages.push(message);
                    empty_response_count = 0;
                } else {
                    empty_response_count = empty_response_count.saturating_add(1);
                    if empty_response_count < 2 {
                        finish_runtime_step(
                            &mut step_machine,
                            step_snapshot.clone(),
                            "empty-response",
                            false,
                            &mut trace,
                        );
                        trace(AgentLoopTraceEvent::RetryScheduled {
                            attempt: empty_response_count,
                            reason: "provider returned an empty response".to_owned(),
                        });
                        messages.push(ProviderMessage {
                            role: ProviderRole::User,
                            content: "Return a concrete response or a valid tool call.".to_owned(),
                            tool_call_id: None,
                            tool_name: None,
                            tool_calls: Vec::new(),
                            metadata: Default::default(),
                        });
                        continue;
                    }
                    let reason = "provider returned empty responses repeatedly".to_owned();
                    trace(AgentLoopTraceEvent::LoopGuardTriggered {
                        trigger: golutra_core::LoopGuardTrigger::EmptyResponse,
                        reason: reason.clone(),
                    });
                    finish_runtime_step(
                        &mut step_machine,
                        step_snapshot.clone(),
                        "empty-response",
                        false,
                        &mut trace,
                    );
                    guard_reason = Some(reason);
                    break;
                }

                if let Some(pending_turn) = control.pending_turns.take_or_close() {
                    current_turn_id = pending_turn.turn_id;
                    current_objective = pending_turn.content.clone();
                    current_turn_touched_code =
                        objective_requires_workspace_evidence(&current_objective);
                    last_assistant_message = None;
                    last_emitted_assistant_message = None;
                    tool_reports.clear();
                    repeated_failure_signature = None;
                    repeated_failure_count = 0;
                    goal_ledger.original_objective = current_objective.clone();
                    goal_ledger.current_plan = request.completion_criteria.clone();
                    goal_ledger.completed_steps.clear();
                    goal_ledger.open_risks.clear();
                    trace(AgentLoopTraceEvent::PendingTurnStarted(
                        pending_turn.clone(),
                    ));
                    messages.push(ProviderMessage {
                        role: ProviderRole::User,
                        content: pending_turn.content,
                        tool_call_id: None,
                        tool_name: None,
                        tool_calls: Vec::new(),
                        metadata: Default::default(),
                    });
                    finish_runtime_step(
                        &mut step_machine,
                        step_snapshot.clone(),
                        step_fingerprint.clone(),
                        true,
                        &mut trace,
                    );
                    continue;
                }
                finish_runtime_step(
                    &mut step_machine,
                    step_snapshot.clone(),
                    step_fingerprint,
                    true,
                    &mut trace,
                );
                break;
            }

            let tool_reports_before_step = tool_reports.len();
            messages.push(ProviderMessage {
                role: ProviderRole::Assistant,
                content: provider_response
                    .message
                    .as_ref()
                    .map(|message| message.content.clone())
                    .unwrap_or_default(),
                tool_call_id: None,
                tool_name: None,
                tool_calls: provider_response.tool_calls.clone(),
                metadata: provider_response
                    .message
                    .as_ref()
                    .map(|message| message.metadata.clone())
                    .unwrap_or_default(),
            });
            for tool_call in provider_response.tool_calls {
                control.wait_until_runnable().await?;
                let tool_action = format!("{} {}", tool_call.tool_name, tool_call.arguments);
                let governance = self.governor.evaluate(
                    &goal_ledger,
                    &GovernorObservation {
                        phase: GovernorPhase::Tool,
                        iteration: iteration.saturating_add(1),
                        tool_calls: tool_call_count.saturating_add(1),
                        failed_tool_calls: failed_tool_call_count,
                        planned_input_tokens: plan.budget_snapshot.planned_input_tokens,
                        elapsed_ms: elapsed_millis(started_at),
                        latest_action: tool_action,
                        estimated_cost_microusd,
                        policy_decision: None,
                        security_risk: "medium".to_owned(),
                    },
                );
                let permits_execution = governance.permits_execution();
                if !permits_execution {
                    guard_reason = Some(governance.reason.clone());
                    governor_action = Some(governance.action);
                }
                trace(AgentLoopTraceEvent::GovernorDecided(governance));
                if !permits_execution {
                    finish_runtime_step(
                        &mut step_machine,
                        step_snapshot.clone(),
                        step_fingerprint.clone(),
                        false,
                        &mut trace,
                    );
                    break 'agent_loop;
                }
                tool_call_count = tool_call_count.saturating_add(1);
                let provider_tool_call_id = tool_call.tool_call_id.clone();
                let failure_signature = format!("{}:{}", tool_call.tool_name, tool_call.arguments);
                let tool_request = ToolRequest {
                    tool_call_id: golutra_core::ToolCallId::new(),
                    session_id: request.session_id,
                    turn_id: Some(current_turn_id),
                    tool_name: tool_call.tool_name,
                    arguments: tool_call.arguments,
                };
                trace(AgentLoopTraceEvent::ToolStarted {
                    tool_call_id: tool_request.tool_call_id,
                    tool_name: tool_request.tool_name.clone(),
                    display_arguments: redact_tool_arguments(&tool_request.arguments),
                });
                let report = match self.tool_executor.evaluate(&tool_request) {
                    Ok(policy) => {
                        trace(AgentLoopTraceEvent::PolicyEvaluated(policy.clone()));
                        let approved = if policy.decision == PolicyDecision::Ask {
                            let approval = ApprovalRequest {
                                approval_id: ApprovalId::new(),
                                task_id: request.task_id,
                                turn_id: current_turn_id,
                                tool_call_id: tool_request.tool_call_id,
                                tool_name: tool_request.tool_name.clone(),
                                resource: policy.resource.clone(),
                                reason: policy.reason.clone(),
                            };
                            trace(AgentLoopTraceEvent::ApprovalRequested(approval.clone()));
                            let resolution =
                                control.wait_for_approval(approval.approval_id).await?;
                            let approved = resolution.decision == ApprovalDecision::Approved;
                            trace(AgentLoopTraceEvent::ApprovalResolved(resolution));
                            approved
                        } else {
                            false
                        };
                        control.wait_until_runnable().await?;
                        let may_execute = match policy.decision {
                            PolicyDecision::Allow => true,
                            PolicyDecision::Ask => approved,
                            PolicyDecision::Deny | PolicyDecision::Block => false,
                        };
                        let preparation = if may_execute {
                            self.tool_executor
                                .prepare_side_effect_snapshot(&tool_request)
                                .await
                        } else {
                            Ok(golutra_tools::SideEffectPreparation::default())
                        };
                        match preparation {
                            Err(error) => {
                                let report = self.tool_executor.execution_error_report(
                                    tool_request,
                                    policy,
                                    error.to_string(),
                                );
                                trace(AgentLoopTraceEvent::ToolProgress(ToolProgress {
                                    tool_call_id: report.envelope.tool_call_id,
                                    tool_name: report.envelope.tool_name.clone(),
                                    phase: ToolProgressPhase::Completed,
                                    elapsed_ms: report.metrics.duration_ms,
                                    output_bytes: report.metrics.output_bytes,
                                    output_lines: report.metrics.output_lines,
                                    detail: Some("error".to_owned()),
                                }));
                                report
                            }
                            Ok(preparation) => {
                                if may_execute
                                    && (tool_request.tool_name == "shell"
                                        || !preparation.before_images.is_empty())
                                    && let Some(recorder) = &self.before_side_effect_recorder
                                {
                                    recorder
                                        .persist_before_side_effect(
                                            &tool_request,
                                            &preparation.before_images,
                                            preparation.complete,
                                        )
                                        .await?;
                                }
                                control.wait_until_runnable().await?;
                                let mut progress = |progress| {
                                    trace(AgentLoopTraceEvent::ToolProgress(progress));
                                };
                                let error_request = tool_request.clone();
                                let error_policy = policy.clone();
                                match self
                                    .tool_executor
                                    .execute_with_policy_and_preparation_with_progress(
                                        tool_request,
                                        policy,
                                        approved,
                                        control.cancellation.clone(),
                                        preparation,
                                        Some(&mut progress),
                                    )
                                    .await
                                {
                                    Ok(report) => report,
                                    Err(error) => self.tool_executor.execution_error_report(
                                        error_request,
                                        error_policy,
                                        error.to_string(),
                                    ),
                                }
                            }
                        }
                    }
                    Err(error) => {
                        let report = self
                            .tool_executor
                            .invalid_request_report(tool_request, error.to_string());
                        trace(AgentLoopTraceEvent::PolicyEvaluated(
                            report.policy_evaluation.clone(),
                        ));
                        trace(AgentLoopTraceEvent::ToolProgress(ToolProgress {
                            tool_call_id: report.envelope.tool_call_id,
                            tool_name: report.envelope.tool_name.clone(),
                            phase: ToolProgressPhase::Completed,
                            elapsed_ms: report.metrics.duration_ms,
                            output_bytes: report.metrics.output_bytes,
                            output_lines: report.metrics.output_lines,
                            detail: Some("error".to_owned()),
                        }));
                        report
                    }
                };
                trace(AgentLoopTraceEvent::ToolCompleted(report.clone()));
                if report.envelope.status == ToolResultStatus::Ok {
                    repeated_failure_signature = None;
                    repeated_failure_count = 0;
                } else if repeated_failure_signature.as_deref() == Some(failure_signature.as_str())
                {
                    failed_tool_call_count = failed_tool_call_count.saturating_add(1);
                    repeated_failure_count = repeated_failure_count.saturating_add(1);
                } else {
                    failed_tool_call_count = failed_tool_call_count.saturating_add(1);
                    repeated_failure_signature = Some(failure_signature);
                    repeated_failure_count = 1;
                }
                let result_governance = self.governor.evaluate(
                    &goal_ledger,
                    &GovernorObservation {
                        phase: GovernorPhase::ToolResult,
                        iteration: iteration.saturating_add(1),
                        tool_calls: tool_call_count,
                        failed_tool_calls: failed_tool_call_count,
                        planned_input_tokens: plan.budget_snapshot.planned_input_tokens,
                        elapsed_ms: elapsed_millis(started_at),
                        latest_action: report.envelope.summary.clone(),
                        estimated_cost_microusd,
                        policy_decision: Some(report.policy_evaluation.decision),
                        security_risk: report.envelope.risk.clone(),
                    },
                );
                let permits_continuation = result_governance.permits_execution();
                if !permits_continuation {
                    guard_reason = Some(result_governance.reason.clone());
                    governor_action = Some(result_governance.action);
                }
                trace(AgentLoopTraceEvent::GovernorDecided(result_governance));
                messages.push(ProviderMessage {
                    role: ProviderRole::Tool,
                    content: serde_json::to_string(&report.envelope)
                        .unwrap_or_else(|_| report.envelope.summary.clone()),
                    tool_call_id: Some(provider_tool_call_id),
                    tool_name: Some(report.envelope.tool_name.clone()),
                    tool_calls: Vec::new(),
                    metadata: Default::default(),
                });
                tool_reports.push(report);
                if !permits_continuation {
                    finish_runtime_step(
                        &mut step_machine,
                        step_snapshot.clone(),
                        step_fingerprint.clone(),
                        false,
                        &mut trace,
                    );
                    break 'agent_loop;
                }
            }
            let made_progress = tool_reports[tool_reports_before_step..]
                .iter()
                .any(|report| !report.changed_files.is_empty());
            let step_completion = finish_runtime_step(
                &mut step_machine,
                step_snapshot.clone(),
                step_fingerprint,
                made_progress,
                &mut trace,
            );
            if step_completion.should_stop {
                let reason = format!(
                    "runtime made no observable progress for {} identical steps",
                    step_completion.repeated_no_progress
                );
                trace(AgentLoopTraceEvent::LoopGuardTriggered {
                    trigger: golutra_core::LoopGuardTrigger::NoProgress,
                    reason: reason.clone(),
                });
                guard_reason = Some(reason);
                break;
            }
            if repeated_failure_count >= 2 {
                let reason = "the same deterministic tool call failed repeatedly".to_owned();
                trace(AgentLoopTraceEvent::LoopGuardTriggered {
                    trigger: golutra_core::LoopGuardTrigger::RepeatedToolFailure,
                    reason: reason.clone(),
                });
                guard_reason = Some(reason);
                break;
            }
        }

        let evidence_refs = tool_reports
            .iter()
            .flat_map(|report| report.evidence.iter().map(|evidence| evidence.evidence_id))
            .collect::<Vec<_>>();
        let mut command_checks = tool_reports
            .iter()
            .map(|report| VerificationCheck {
                kind: VerificationCheckKind::ToolExecution,
                name: format!("tool:{}", report.envelope.tool_name),
                command: None,
                passed: report.envelope.status == ToolResultStatus::Ok,
                evidence_refs: report.envelope.evidence_refs.clone(),
                message: report.envelope.summary.clone(),
            })
            .collect::<Vec<_>>();
        let changed_files = tool_reports
            .iter()
            .flat_map(|report| report.changed_files.iter())
            .collect::<Vec<_>>();
        if !changed_files.is_empty() {
            let path_matches = changed_files
                .iter()
                .any(|path| objective_path_matches(&current_objective, path));
            command_checks.push(VerificationCheck {
                kind: VerificationCheckKind::WorkspaceChange,
                name: "workspace_diff".to_owned(),
                command: None,
                passed: path_matches || objective_path_hint(&current_objective).is_none(),
                evidence_refs: tool_reports
                    .iter()
                    .flat_map(|report| report.envelope.evidence_refs.iter().copied())
                    .collect(),
                message: if path_matches || objective_path_hint(&current_objective).is_none() {
                    format!("{} workspace file(s) changed", changed_files.len())
                } else {
                    "workspace changed files do not match the requested path".to_owned()
                },
            });
        }
        let code_files_changed = changed_files.iter().any(|path| is_code_file(path));
        for report in &tool_reports {
            if let Some(validation) = objective_validation_report(report, code_files_changed) {
                command_checks.push(VerificationCheck {
                    kind: VerificationCheckKind::ObjectiveValidation,
                    name: format!(
                        "objective:{}:{}",
                        validation.kind.label(),
                        report.envelope.tool_name
                    ),
                    command: report
                        .envelope
                        .structured_facts
                        .get("command")
                        .and_then(serde_json::Value::as_str)
                        .map(ToOwned::to_owned),
                    passed: validation.passed,
                    evidence_refs: report.envelope.evidence_refs.clone(),
                    message: validation.message,
                });
            }
            if let Some(observed_path) = report
                .envelope
                .structured_facts
                .get("path")
                .and_then(Value::as_str)
                && let Some(expected_path) = objective_path_hint(&current_objective)
            {
                let matches = observed_path.ends_with(&expected_path)
                    || expected_path.ends_with(observed_path);
                command_checks.push(VerificationCheck {
                    kind: VerificationCheckKind::ObjectiveValidation,
                    name: format!("objective:path:{}", report.envelope.tool_name),
                    command: None,
                    passed: matches,
                    evidence_refs: report.envelope.evidence_refs.clone(),
                    message: if matches {
                        "tool path matches the requested path".to_owned()
                    } else {
                        format!(
                            "tool path `{observed_path}` does not match requested `{expected_path}`"
                        )
                    },
                });
            }
            if report.envelope.tool_name == "write_file"
                && report.envelope.status == ToolResultStatus::Ok
                && let Some(expected_content) = objective_content_hint(&current_objective)
            {
                // 写入成功只证明发生了副作用；这里重新读取最终文件，确认交付内容确实符合用户目标。
                let mut content_matches = false;
                for path in &report.changed_files {
                    if tokio::fs::read(path)
                        .await
                        .is_ok_and(|content| content == expected_content.as_bytes())
                    {
                        content_matches = true;
                        break;
                    }
                }
                command_checks.push(VerificationCheck {
                    kind: VerificationCheckKind::ObjectiveValidation,
                    name: "objective:content:write_file".to_owned(),
                    command: None,
                    passed: content_matches,
                    evidence_refs: report.envelope.evidence_refs.clone(),
                    message: if content_matches {
                        "written content matches the requested content".to_owned()
                    } else {
                        "written content does not match the requested content".to_owned()
                    },
                });
            }
        }
        if last_assistant_message
            .as_deref()
            .is_some_and(|message| !message.trim().is_empty())
            && (!objective_requires_workspace_evidence(&current_objective)
                || !tool_reports.is_empty())
        {
            command_checks.push(VerificationCheck {
                kind: VerificationCheckKind::AssistantResponse,
                name: "assistant_response".to_owned(),
                command: None,
                passed: true,
                evidence_refs: Vec::new(),
                message: "assistant response produced after tool execution".to_owned(),
            });
        }
        let requires_workspace_evidence = current_turn_touched_code
            || tool_reports
                .iter()
                .any(|report| !report.changed_files.is_empty());
        if let Some(schema) = request
            .output_schema
            .as_ref()
            .filter(|value| !value.is_null())
        {
            command_checks.push(output_schema_check(
                schema,
                last_assistant_message.as_deref(),
            ));
        }
        let verification_input = if completion::accepts_text_response_without_evidence(
            &current_objective,
            requires_workspace_evidence,
            last_assistant_message.as_deref(),
            &tool_reports,
        ) && request
            .output_schema
            .as_ref()
            .is_none_or(serde_json::Value::is_null)
        {
            VerificationInput {
                task_id: request.task_id,
                objective: current_objective.clone(),
                completion_criteria: request.completion_criteria.clone(),
                evidence_refs: Vec::new(),
                command_checks: vec![VerificationCheck {
                    kind: VerificationCheckKind::AssistantResponse,
                    name: "assistant_response".to_owned(),
                    command: None,
                    passed: true,
                    evidence_refs: Vec::new(),
                    message: "assistant response produced".to_owned(),
                }],
                requires_workspace_evidence: false,
                code_files_changed: false,
            }
        } else {
            VerificationInput {
                task_id: request.task_id,
                objective: current_objective.clone(),
                completion_criteria: request.completion_criteria.clone(),
                evidence_refs,
                command_checks,
                requires_workspace_evidence,
                code_files_changed,
            }
        };
        let verification_plan = self.verifier.plan(&verification_input);
        trace(AgentLoopTraceEvent::VerificationPlanned(
            verification_plan.clone(),
        ));
        let (mut verification, verification_plan) =
            self.verifier.verify(verification_input, verification_plan);
        for assertion in verification_plan
            .assertions
            .iter()
            .chain(verification_plan.policy_assertions.iter())
        {
            trace(AgentLoopTraceEvent::VerificationAssertionCompleted(
                assertion.clone(),
            ));
        }
        let completion_governance = self.governor.evaluate(
            &goal_ledger,
            &GovernorObservation {
                phase: GovernorPhase::Completion,
                iteration: step_machine.checkpoint().next_step_no,
                tool_calls: tool_call_count,
                failed_tool_calls: failed_tool_call_count,
                planned_input_tokens: last_budget_state.planned_input_tokens.unwrap_or_default(),
                elapsed_ms: elapsed_millis(started_at),
                latest_action: last_assistant_message
                    .clone()
                    .unwrap_or_else(|| current_objective.clone()),
                estimated_cost_microusd,
                policy_decision: None,
                security_risk: "low".to_owned(),
            },
        );
        if !completion_governance.permits_execution() {
            guard_reason = Some(completion_governance.reason.clone());
            governor_action = Some(completion_governance.action);
        }
        trace(AgentLoopTraceEvent::GovernorDecided(completion_governance));
        if let Some(reason) = &guard_reason {
            if verification.result == VerificationResult::Pass {
                verification.result = VerificationResult::Partial;
            }
            verification.residual_risks.push(reason.clone());
        }
        let mut loop_decision = completion::loop_decision(
            request.task_id,
            current_turn_id,
            &verification,
            last_budget_state,
        );
        if let Some(action) = governor_action {
            loop_decision.action = match action {
                GovernorAction::AskUser => LoopAction::AskUser,
                GovernorAction::Block => LoopAction::Blocked,
                GovernorAction::Allow | GovernorAction::Warn => loop_decision.action,
            };
            loop_decision.reason = guard_reason
                .clone()
                .unwrap_or_else(|| "runtime governor stopped execution".to_owned());
            loop_decision.next_step =
                Some("user must revise the objective or runtime budget".to_owned());
        }

        let final_message =
            completion::final_message(last_assistant_message, &tool_reports, &verification);
        if let Some(content) = final_message.as_ref().filter(|content| {
            last_emitted_assistant_message.as_ref() != Some(&(current_turn_id, (*content).clone()))
        }) {
            trace(AgentLoopTraceEvent::AssistantMessage {
                turn_id: current_turn_id,
                content: content.clone(),
            });
        }

        Ok(AgentLoopOutcome {
            final_message,
            verification,
            verification_plan,
            loop_decision,
            tool_reports,
            final_turn_id: current_turn_id,
        })
    }

    async fn complete_with_retry<F>(
        &self,
        request: ProviderRequest,
        control: &mut AgentExecutionControl,
        trace: &mut F,
    ) -> Result<(ProviderResponse, ProviderRequest), ProviderError>
    where
        F: FnMut(AgentLoopTraceEvent) + Send,
    {
        match self
            .complete_provider(&self.provider, request.clone(), control, trace)
            .await
        {
            Ok(response) => Ok((response, request)),
            Err(primary_error) => {
                let Some(fallback) = &self.fallback_provider else {
                    return Err(primary_error);
                };
                let from_provider = self.provider.contract().provider_id;
                let to_provider = fallback.contract().provider_id;
                trace(AgentLoopTraceEvent::ProviderFallback {
                    from_provider,
                    to_provider: to_provider.clone(),
                    reason: primary_error.to_string(),
                });
                let mut fallback_request = request;
                fallback_request.provider_id = to_provider;
                fallback_request.model_id = fallback.contract().model_id;
                trace(AgentLoopTraceEvent::ProviderStarted {
                    provider_id: fallback_request.provider_id.clone(),
                    model_id: fallback_request.model_id.clone(),
                });
                self.complete_provider(fallback, fallback_request.clone(), control, trace)
                    .await
                    .map(|response| (response, fallback_request))
            }
        }
    }

    async fn complete_provider<F>(
        &self,
        provider: &P,
        request: ProviderRequest,
        control: &mut AgentExecutionControl,
        trace: &mut F,
    ) -> Result<ProviderResponse, ProviderError>
    where
        F: FnMut(AgentLoopTraceEvent) + Send,
    {
        const MAX_ATTEMPTS: u32 = 3;
        for attempt in 1..=MAX_ATTEMPTS {
            control
                .wait_until_runnable()
                .await
                .map_err(|_| ProviderError::Cancelled)?;
            let contract = provider.contract();
            let result = {
                let provider_id = contract.provider_id;
                let model_id = contract.model_id;
                let mut on_event = |event| {
                    trace(AgentLoopTraceEvent::ProviderStreamed {
                        provider_id: provider_id.clone(),
                        model_id: model_id.clone(),
                        event,
                    });
                };
                tokio::select! {
                    biased;
                    _ = control.cancellation.cancelled() => Err(ProviderError::Cancelled),
                    result = provider.complete_stream(request.clone(), &mut on_event) => result,
                }
            };
            match result {
                Ok(response) => return Ok(response),
                Err(error) if provider_retry::is_retryable(&error) && attempt < MAX_ATTEMPTS => {
                    trace(AgentLoopTraceEvent::RetryScheduled {
                        attempt,
                        reason: error.to_string(),
                    });
                    let delay = tokio::time::sleep(Duration::from_millis(100 * u64::from(attempt)));
                    tokio::pin!(delay);
                    tokio::select! {
                        _ = control.cancellation.cancelled() => return Err(ProviderError::Cancelled),
                        _ = &mut delay => {}
                    }
                }
                Err(error) => return Err(error),
            }
        }
        unreachable!("provider retry loop always returns")
    }
}

impl AgentExecutionControl {
    #[must_use]
    pub fn cancellation_token(&self) -> CancellationToken {
        self.cancellation.clone()
    }

    async fn wait_until_runnable(&mut self) -> Result<(), AgentLoopError> {
        loop {
            if self.cancellation.is_cancelled() {
                return Err(AgentLoopError::Cancelled);
            }
            if !*self.pause.borrow() {
                return Ok(());
            }
            tokio::select! {
                _ = self.cancellation.cancelled() => return Err(AgentLoopError::Cancelled),
                changed = self.pause.changed() => {
                    if changed.is_err() {
                        return Err(AgentLoopError::Cancelled);
                    }
                }
            }
        }
    }

    async fn wait_for_approval(
        &mut self,
        approval_id: ApprovalId,
    ) -> Result<ApprovalResolution, AgentLoopError> {
        loop {
            tokio::select! {
                _ = self.cancellation.cancelled() => return Err(AgentLoopError::Cancelled),
                resolution = self.approvals.recv() => {
                    let resolution = resolution.ok_or(AgentLoopError::Cancelled)?;
                    if resolution.approval_id == approval_id {
                        return Ok(resolution);
                    }
                }
            }
        }
    }
}

#[must_use]
pub fn runtime_boundary() -> &'static str {
    "SessionCommand -> RuntimeEvent -> StateProjection -> LoopDecision"
}

fn is_code_file(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|extension| extension.to_str()),
        Some(
            "c" | "cc"
                | "cpp"
                | "cs"
                | "go"
                | "h"
                | "hpp"
                | "java"
                | "js"
                | "jsx"
                | "kt"
                | "kts"
                | "php"
                | "py"
                | "rb"
                | "rs"
                | "swift"
                | "ts"
                | "tsx"
        )
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ObjectiveValidationKind {
    Test,
    Diagnostic,
    Delivery,
}

impl ObjectiveValidationKind {
    const fn label(self) -> &'static str {
        match self {
            Self::Test => "test",
            Self::Diagnostic => "diagnostic",
            Self::Delivery => "delivery",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ObjectiveValidationOutcome {
    kind: ObjectiveValidationKind,
    passed: bool,
    message: String,
}

fn objective_validation_report(
    report: &ToolExecutionReport,
    code_files_changed: bool,
) -> Option<ObjectiveValidationOutcome> {
    if report.envelope.tool_name == "shell" {
        let command = report
            .envelope
            .structured_facts
            .get("command")
            .and_then(serde_json::Value::as_str)?;
        let kind = objective_validation_command_kind(command)?;
        let exited_cleanly = report.envelope.status == ToolResultStatus::Ok
            && report
                .envelope
                .structured_facts
                .get("exit_code")
                .and_then(Value::as_i64)
                == Some(0)
            && !report
                .envelope
                .structured_facts
                .get("timed_out")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            && !report
                .envelope
                .structured_facts
                .get("cancelled")
                .and_then(Value::as_bool)
                .unwrap_or(false);
        let passed = exited_cleanly
            && (kind != ObjectiveValidationKind::Test || test_report_executed_tests(report));
        let message = match (kind, exited_cleanly, passed) {
            (_, false, _) => "validation command did not exit successfully".to_owned(),
            (ObjectiveValidationKind::Test, true, false) => {
                "test command exited successfully but no executed test was observed".to_owned()
            }
            (ObjectiveValidationKind::Test, true, true) => {
                "test command passed with executed tests".to_owned()
            }
            (_, true, true) => "diagnostic command passed".to_owned(),
            _ => "objective validation is unresolved".to_owned(),
        };
        return Some(ObjectiveValidationOutcome {
            kind,
            passed,
            message,
        });
    }
    let is_delivery = !code_files_changed
        && report.changed_files.iter().any(|path| !is_code_file(path))
        && matches!(
            report.envelope.tool_name.as_str(),
            "write_file" | "edit_file"
        );
    is_delivery.then(|| ObjectiveValidationOutcome {
        kind: ObjectiveValidationKind::Delivery,
        passed: report.envelope.status == ToolResultStatus::Ok,
        message: report.envelope.summary.clone(),
    })
}

#[cfg(test)]
fn is_objective_validation_command(command: &str) -> bool {
    objective_validation_command_kind(command).is_some()
}

fn objective_validation_command_kind(command: &str) -> Option<ObjectiveValidationKind> {
    let parts = shlex::split(command)?;
    let program = parts.first().map(String::as_str)?;
    match program {
        "cargo" if parts.iter().any(|part| part == "test") => Some(ObjectiveValidationKind::Test),
        "cargo"
            if parts
                .iter()
                .any(|part| matches!(part.as_str(), "check" | "clippy" | "build" | "fmt")) =>
        {
            Some(ObjectiveValidationKind::Diagnostic)
        }
        "npm" | "pnpm" | "yarn" | "bun" if parts.iter().any(|part| part == "test") => {
            Some(ObjectiveValidationKind::Test)
        }
        "npm" | "pnpm" | "yarn" | "bun"
            if parts
                .iter()
                .any(|part| matches!(part.as_str(), "check" | "typecheck" | "build" | "lint")) =>
        {
            Some(ObjectiveValidationKind::Diagnostic)
        }
        "pytest" => Some(ObjectiveValidationKind::Test),
        "go" if parts.get(1).is_some_and(|part| part == "test") => {
            Some(ObjectiveValidationKind::Test)
        }
        "make" | "mvn" | "gradle" | "swift"
            if parts.iter().skip(1).any(|part| {
                let part = part.to_ascii_lowercase();
                ["test", "check", "build", "verify", "lint"]
                    .iter()
                    .any(|marker| part.contains(marker))
            }) =>
        {
            if parts
                .iter()
                .any(|part| part.to_ascii_lowercase().contains("test"))
            {
                Some(ObjectiveValidationKind::Test)
            } else {
                Some(ObjectiveValidationKind::Diagnostic)
            }
        }
        _ => None,
    }
}

fn test_report_executed_tests(report: &ToolExecutionReport) -> bool {
    report.artifact_contents.iter().any(|artifact| {
        let output = String::from_utf8_lossy(&artifact.bytes).to_ascii_lowercase();
        output.lines().any(line_reports_executed_tests)
    })
}

fn line_reports_executed_tests(line: &str) -> bool {
    let line = line.trim();
    if line.starts_with("ok ") || line.starts_with("ok\t") {
        return true;
    }
    [" passed", " tests", " test", " tests run:"]
        .iter()
        .filter_map(|marker| line.find(marker).map(|index| &line[..index]))
        .any(|prefix| {
            prefix
                .split(|character: char| !character.is_ascii_digit())
                .rfind(|value| !value.is_empty())
                .and_then(|value| value.parse::<u64>().ok())
                .is_some_and(|count| count > 0)
        })
}

fn output_schema_check(schema: &Value, message: Option<&str>) -> VerificationCheck {
    let (passed, detail) = match message {
        None => (false, "assistant response is empty".to_owned()),
        Some(message) => match serde_json::from_str::<Value>(message) {
            Err(error) => (
                false,
                format!("assistant response is not valid JSON: {error}"),
            ),
            Ok(value) => match jsonschema::validator_for(schema) {
                Err(error) => (false, format!("output schema is invalid: {error}")),
                Ok(validator) => match validator.validate(&value) {
                    Ok(()) => (
                        true,
                        "assistant response validates against output schema".to_owned(),
                    ),
                    Err(error) => (
                        false,
                        format!("assistant response failed output schema: {error}"),
                    ),
                },
            },
        },
    };
    VerificationCheck {
        kind: VerificationCheckKind::Schema,
        name: "output_schema".to_owned(),
        command: None,
        passed,
        evidence_refs: Vec::new(),
        message: detail.chars().take(512).collect(),
    }
}

fn objective_requires_workspace_evidence(objective: &str) -> bool {
    let lower = objective.to_ascii_lowercase();
    const ENGLISH_MARKERS: &[&str] = &[
        "write",
        "create",
        "edit",
        "modify",
        "update",
        "delete",
        "read",
        "list",
        "search",
        "find",
        "inspect",
        "run",
        "test",
        "build",
        "fix",
        "debug",
        "refactor",
        "file",
        "code",
        "workspace",
        "diff",
        "commit",
    ];
    const CJK_MARKERS: &[&str] = &[
        "写",
        "创建",
        "修改",
        "更新",
        "删除",
        "读取",
        "查看",
        "列出",
        "搜索",
        "查找",
        "检查",
        "运行",
        "测试",
        "构建",
        "修复",
        "重构",
        "文件",
        "代码",
        "项目",
        "工作区",
        "提交",
    ];

    ENGLISH_MARKERS.iter().any(|marker| lower.contains(marker))
        || CJK_MARKERS.iter().any(|marker| objective.contains(marker))
}

fn objective_path_hint(objective: &str) -> Option<String> {
    objective
        .split_whitespace()
        .map(|token| {
            token.trim_matches(|character: char| {
                matches!(
                    character,
                    '`' | '\'' | '"' | ',' | '.' | ':' | ';' | '(' | ')' | '[' | ']'
                )
            })
        })
        .filter(|token| {
            !token.is_empty()
                && (token.contains('/')
                    || token.contains('\\')
                    || token.rsplit_once('.').is_some_and(|(_, extension)| {
                        !extension.is_empty()
                            && extension
                                .chars()
                                .all(|character| character.is_ascii_alphanumeric())
                    }))
        })
        .max_by_key(|token| token.len())
        .map(ToOwned::to_owned)
}

fn objective_path_matches(objective: &str, path: &Path) -> bool {
    let Some(expected) = objective_path_hint(objective) else {
        return false;
    };
    let observed = path.to_string_lossy();
    observed.ends_with(&expected) || expected.ends_with(observed.as_ref())
}

fn objective_content_hint(objective: &str) -> Option<String> {
    let lower = objective.to_ascii_lowercase();
    let english_markers = [" with content ", " content is "];
    let english = english_markers.iter().find_map(|marker| {
        lower
            .find(marker)
            .map(|start| &objective[start.saturating_add(marker.len())..])
    });
    let chinese_markers = ["内容为", "内容是", "内容：", "内容:"];
    let chinese = chinese_markers.iter().find_map(|marker| {
        objective
            .find(marker)
            .map(|start| &objective[start.saturating_add(marker.len())..])
    });
    english
        .or(chinese)
        .map(|value| {
            value
                .trim()
                .trim_matches(|character: char| {
                    matches!(
                        character,
                        '`' | '\'' | '"' | ',' | '.' | ':' | ';' | '，' | '。' | '：'
                    )
                })
                .to_owned()
        })
        .filter(|value| !value.is_empty())
}

fn elapsed_millis(started_at: Instant) -> u64 {
    u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn finish_runtime_step<F>(
    machine: &mut StepMachine,
    snapshot: StepSnapshot,
    fingerprint: impl Into<String>,
    made_progress: bool,
    trace: &mut F,
) -> StepCompletion
where
    F: FnMut(AgentLoopTraceEvent) + Send,
{
    let completion = machine.complete(snapshot, fingerprint, made_progress);
    trace(AgentLoopTraceEvent::StepCompleted(completion.clone()));
    trace(AgentLoopTraceEvent::StepCheckpointed(machine.checkpoint()));
    completion
}

fn provider_response_fingerprint(response: &ProviderResponse) -> String {
    let tool_calls = response
        .tool_calls
        .iter()
        .map(|call| {
            serde_json::json!({
                "tool_name": &call.tool_name,
                "arguments": &call.arguments,
            })
        })
        .collect::<Vec<_>>();
    let canonical = serde_json::json!({
        "message": response.message.as_ref().map(|message| &message.content),
        "tool_calls": tool_calls,
        "finish_reason": response.finish_reason,
    });
    format!(
        "sha256:{:x}",
        Sha256::digest(canonical.to_string().as_bytes())
    )
}

fn cost_to_microusd(cost_usd: f64) -> Option<u64> {
    if !cost_usd.is_finite() || cost_usd.is_sign_negative() {
        return None;
    }
    let microusd = cost_usd * 1_000_000.0;
    if microusd >= u64::MAX as f64 {
        Some(u64::MAX)
    } else {
        Some(microusd.round() as u64)
    }
}

#[cfg(test)]
mod tests;
