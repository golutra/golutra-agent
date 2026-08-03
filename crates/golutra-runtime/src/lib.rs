use std::{
    collections::{HashMap, HashSet, VecDeque},
    fs,
    path::Path,
    sync::{Arc, Mutex as StdMutex},
    time::{Duration, Instant},
};

use async_trait::async_trait;
use golutra_context::{
    ContextBuilder, ContextContributor, ContextError, ContextMessageSource, ModelInputVisibility,
    compile_model_input, estimate_message_tokens, estimate_tokens, token_usage_record,
};
use golutra_core::{
    ApprovalDecision, ApprovalId, ApprovalRequest, ApprovalResolution, BudgetState, CommandId,
    CorrectionEnvelope, LoopAction, LoopDecision, PolicyBlockDisposition, PolicyDecision,
    SessionId, SideEffectType, TaskContract, TaskId, ToolContract, ToolProgress, ToolProgressPhase,
    ToolRecoveryPolicy, ToolResultStatus, TurnId, TurnState, VerificationCheck,
    VerificationCheckKind, VerificationPlan, VerificationRecord, VerificationRequirement,
    VerificationResult, WorkspaceChangeRequirement,
};
#[cfg(test)]
use golutra_core::{
    RequiredFileContent, infer_direct_legacy_write_path, infer_legacy_write_objective,
};
use golutra_governor::{
    GoalLedger, GovernorAction, GovernorObservation, GovernorPhase, RuntimeGovernor,
};
use golutra_llm::{
    LlmProvider, ProviderError, ProviderMessage, ProviderRequest, ProviderResponse, ProviderRole,
};
use golutra_policy::parse_shell_command_with_input;
use golutra_protocol::ExternalVerificationSpec;
use golutra_tools::{
    CONTRACT_FILE_CONTENT_VERIFIER_TOOL, CONTRACT_PATH_VERIFIER_TOOL, FileBeforeImage, ToolError,
    ToolExecutionReport, ToolInvocation, ToolRequest, ToolRuntime, VerifierExecutionRequest,
    model_visible_tool_result, redact_sensitive_text, redact_tool_arguments,
};
use golutra_verify::VerificationInput;
use serde_json::Value;
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::sync::{Notify, mpsc, watch};
use tokio_util::sync::CancellationToken;

mod checkpoint;
mod completion;
mod context_guard;
mod harness;
mod lane;
mod provider_retry;
mod provider_session;
mod step_machine;
mod trace;
mod verification;

pub use checkpoint::{CheckpointError, WorkspaceCheckpointManager, checkpoint_fingerprint};
pub use golutra_protocol::UserProjection;
pub use harness::{AgentHarness, AgentRun, RunningTurn};
pub use lane::{RuntimeLaneError, RuntimeLaneManager, RuntimeTransition, is_active_status};
pub use provider_session::{ProviderSessionPolicy, ProviderTransport};
pub(crate) use step_machine::{
    CorrectionProgressLimits, StepCheckpoint, StepCompletion, StepMachine, StepSnapshot,
};
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
    #[error("invalid task contract: {0}")]
    TaskContract(String),
    #[error("agent harness worker failed: {0}")]
    Worker(String),
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
    pub defer_external_verification: bool,
    /// The provider produced a candidate without a runtime, policy, or
    /// governor failure, and final authority was deliberately delegated to an
    /// external evaluator.
    pub candidate_ready_for_external_verification: bool,
}

/// Captured provider inputs used to re-enter the ordinary AgentLoop without
/// rebuilding historical assistant/tool messages from a lossy projection.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentReplayContext {
    pub initial_messages: Vec<ProviderMessage>,
    pub tools: Vec<ToolContract>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct AgentTurnOverrides {
    pub max_elapsed_ms: Option<u64>,
    pub defer_external_verification: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingAgentTurn {
    pub command_id: CommandId,
    pub turn_id: TurnId,
    pub content: String,
    /// An appended turn carries its own completion contract so it never
    /// inherits workspace requirements from the currently active prompt.
    pub task_contract: Option<TaskContract>,
    /// Response validation belongs to the queued turn and must not inherit the
    /// active turn's schema.
    pub output_schema: Option<Value>,
    /// Verifiers belong to this turn and must not leak across queued prompts.
    pub external_verifiers: Vec<ExternalVerificationSpec>,
    /// Optional wall-clock budget for this queued turn. `None` restores the
    /// runtime default instead of inheriting the active turn's override.
    pub max_elapsed_ms: Option<u64>,
    /// Deferred evaluator closure belongs to this queued turn and must not
    /// leak from the active turn.
    pub defer_external_verification: bool,
    /// Auto-discovered repository commands are untrusted until the caller has
    /// explicitly opted in, so they require an OS-enforced sandbox.
    pub external_verifiers_require_os_sandbox: bool,
    /// A queued turn cannot change the active tool runtime's network grant.
    pub allow_network: bool,
    /// A queued turn cannot change the active tool runtime's policy mode.
    pub yolo: bool,
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

    pub async fn reserve_turn(
        &self,
        turn: PendingAgentTurn,
    ) -> Result<PendingTurnReservation, AgentLoopError> {
        self.pending_turns.reserve(turn)
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
    changed: Notify,
}

#[derive(Debug, Default)]
struct PendingTurnQueueState {
    accepting: bool,
    turns: VecDeque<PendingTurnEntry>,
}

#[derive(Debug)]
struct PendingTurnEntry {
    turn: PendingAgentTurn,
    durable: bool,
}

#[derive(Debug)]
#[must_use = "dropping an uncommitted reservation removes the pending turn"]
pub struct PendingTurnReservation {
    queue: Arc<PendingTurnQueue>,
    turn_id: TurnId,
    committed: bool,
}

impl PendingTurnReservation {
    pub fn commit(mut self) {
        self.queue.commit(self.turn_id);
        self.committed = true;
    }
}

impl Drop for PendingTurnReservation {
    fn drop(&mut self) {
        if !self.committed {
            self.queue.rollback(self.turn_id);
        }
    }
}

impl PendingTurnQueue {
    fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            state: StdMutex::new(PendingTurnQueueState {
                accepting: true,
                turns: VecDeque::new(),
            }),
            changed: Notify::new(),
        }
    }

    fn push(self: &Arc<Self>, turn: PendingAgentTurn) -> Result<(), AgentLoopError> {
        self.reserve(turn)?.commit();
        Ok(())
    }

    fn reserve(
        self: &Arc<Self>,
        turn: PendingAgentTurn,
    ) -> Result<PendingTurnReservation, AgentLoopError> {
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
        let turn_id = turn.turn_id;
        state.turns.push_back(PendingTurnEntry {
            turn,
            durable: false,
        });
        Ok(PendingTurnReservation {
            queue: self.clone(),
            turn_id,
            committed: false,
        })
    }

    fn commit(&self, turn_id: TurnId) {
        if let Some(entry) = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .turns
            .iter_mut()
            .find(|entry| entry.turn.turn_id == turn_id)
        {
            entry.durable = true;
            self.changed.notify_waiters();
        }
    }

    fn rollback(&self, turn_id: TurnId) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .turns
            .retain(|entry| entry.turn.turn_id != turn_id);
        self.changed.notify_waiters();
    }

    async fn take_or_close(&self) -> Option<PendingAgentTurn> {
        loop {
            let changed = self.changed.notified();
            let ready = {
                let mut state = self
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if !state.accepting {
                    return None;
                }
                match state.turns.front() {
                    Some(entry) if entry.durable => state.turns.pop_front().map(|entry| entry.turn),
                    Some(_) => None,
                    None => {
                        state.accepting = false;
                        return None;
                    }
                }
            };
            if ready.is_some() {
                return ready;
            }
            changed.await;
        }
    }

    fn close(&self) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .accepting = false;
        self.changed.notify_waiters();
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
pub(crate) struct AgentLoop<P> {
    provider: P,
    fallback_provider: Option<P>,
    context_builder: ContextBuilder,
    tool_executor: ToolRuntime,
    verifier: RuntimeVerificationService,
    governor: RuntimeGovernor,
    provider_session_policy: ProviderSessionPolicy,
    before_side_effect_recorder: Option<Arc<dyn BeforeSideEffectRecorder>>,
    external_verifiers: Vec<ExternalVerificationSpec>,
    external_verifiers_require_os_sandbox: bool,
    defer_external_verification: bool,
}

impl<P> AgentLoop<P>
where
    P: LlmProvider,
{
    #[must_use]
    pub(crate) fn new(
        provider: P,
        context_builder: ContextBuilder,
        tool_executor: ToolRuntime,
    ) -> Self {
        Self {
            provider,
            fallback_provider: None,
            context_builder,
            tool_executor,
            verifier: RuntimeVerificationService::default(),
            governor: RuntimeGovernor::default(),
            provider_session_policy: ProviderSessionPolicy::default(),
            before_side_effect_recorder: None,
            external_verifiers: Vec::new(),
            external_verifiers_require_os_sandbox: false,
            defer_external_verification: false,
        }
    }

    #[cfg(test)]
    #[must_use]
    pub(crate) fn with_fallback(mut self, provider: P) -> Self {
        self.fallback_provider = Some(provider);
        self
    }

    #[cfg(test)]
    #[must_use]
    pub(crate) fn with_governor(mut self, governor: RuntimeGovernor) -> Self {
        self.governor = governor;
        self
    }

    #[cfg(test)]
    #[must_use]
    pub(crate) fn with_external_verifiers(
        mut self,
        external_verifiers: Vec<ExternalVerificationSpec>,
    ) -> Self {
        self.external_verifiers = external_verifiers;
        self
    }

    #[cfg(test)]
    #[must_use]
    pub(crate) fn require_os_sandbox_for_external_verifiers(mut self, required: bool) -> Self {
        self.external_verifiers_require_os_sandbox = required;
        self
    }

    #[cfg(test)]
    pub(crate) async fn run(
        &self,
        request: AgentTaskRequest,
    ) -> Result<AgentLoopOutcome, AgentLoopError> {
        let (_handle, control) = agent_execution_channel(1);
        self.run_with_control_and_trace(request, control, |_| {})
            .await
    }

    #[cfg(test)]
    pub(crate) async fn run_with_trace<F>(
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

    #[cfg(test)]
    pub(crate) async fn run_with_task_contract_and_observation_sink<S>(
        &self,
        request: AgentTaskRequest,
        task_contract: TaskContract,
        control: AgentExecutionControl,
        mut sink: S,
    ) -> Result<AgentLoopOutcome, AgentLoopError>
    where
        S: RuntimeObservationSink,
    {
        self.run_with_control_trace_contract_and_replay_context(
            request,
            control,
            move |observation| sink.emit(observation),
            task_contract,
            None,
            AgentTurnOverrides::default(),
        )
        .await
    }

    #[cfg(test)]
    pub(crate) async fn run_with_control_and_trace<F>(
        &self,
        request: AgentTaskRequest,
        control: AgentExecutionControl,
        trace: F,
    ) -> Result<AgentLoopOutcome, AgentLoopError>
    where
        F: FnMut(AgentLoopTraceEvent) + Send,
    {
        let task_contract = legacy_task_contract(&request);
        self.run_with_control_trace_contract_and_replay_context(
            request,
            control,
            trace,
            task_contract,
            None,
            AgentTurnOverrides::default(),
        )
        .await
    }

    async fn run_with_control_trace_contract_and_replay_context<F>(
        &self,
        request: AgentTaskRequest,
        mut control: AgentExecutionControl,
        mut trace: F,
        task_contract: TaskContract,
        replay_context: Option<AgentReplayContext>,
        turn_overrides: AgentTurnOverrides,
    ) -> Result<AgentLoopOutcome, AgentLoopError>
    where
        F: FnMut(AgentLoopTraceEvent) + Send,
    {
        let mut current_task_contract = task_contract;
        let mut current_external_verifiers = self.external_verifiers.clone();
        let mut current_external_verifiers_require_os_sandbox =
            self.external_verifiers_require_os_sandbox;
        let mut current_output_schema = request.output_schema.clone();
        current_task_contract
            .validate()
            .map_err(AgentLoopError::TaskContract)?;
        let mut tool_reports = Vec::new();
        let mut last_assistant_message = None;
        let mut last_emitted_assistant_message = None;
        let mut current_turn_id = request.turn_id;
        let mut current_objective = request.objective.clone();
        let mut current_completion_criteria = current_task_contract.completion_criteria.clone();
        let mut current_turn_touched_code = current_task_contract.requires_workspace_evidence();
        let mut guard_reason = None;
        let mut repeated_failure_signature = None;
        let mut repeated_failure_count = 0_u32;
        let mut failure_families = FailureFamilyLedger::default();
        let mut empty_response_count = 0_u32;
        let default_max_elapsed_ms = self.governor.limits().max_elapsed_ms;
        let mut current_max_elapsed_ms = turn_overrides
            .max_elapsed_ms
            .unwrap_or(default_max_elapsed_ms)
            .max(1);
        let mut current_defer_external_verification = turn_overrides
            .defer_external_verification
            .unwrap_or(self.defer_external_verification);
        let mut current_governor =
            governor_with_max_elapsed_ms(&self.governor, current_max_elapsed_ms);
        let mut current_turn_started_at = Instant::now();
        let mut runtime_deadline = deadline_from_budget(current_max_elapsed_ms);
        let mut tool_call_count = 0_u32;
        let mut failed_tool_call_count = 0_u32;
        let mut consecutive_failed_tool_call_count = 0_u32;
        let mut deadline_advisory_emitted = false;
        let mut runtime_deadline_guard_emitted = false;
        let mut estimated_cost_microusd: Option<u64> = None;
        let mut governor_action = None;
        let mut goal_ledger = GoalLedger {
            task_id: request.task_id,
            original_objective: request.objective.clone(),
            success_criteria: current_completion_criteria.clone(),
            current_plan: current_completion_criteria.clone(),
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
        let mut step_machine = StepMachine::with_limits(
            step_machine::DEFAULT_NO_PROGRESS_ADVISORY_LIMIT,
            step_machine::DEFAULT_NO_PROGRESS_LIMIT,
            CorrectionProgressLimits {
                step_limit: self.governor.limits().max_correction_no_progress_steps,
                elapsed_ms_limit: self.governor.limits().max_correction_no_progress_ms,
            },
        );
        let all_provider_tools = match replay_context.as_ref() {
            Some(replay_context) => replay_context.tools.clone(),
            None => request
                .tools
                .iter()
                .map(|tool_name| {
                    self.tool_executor
                        .registry()
                        .contract(tool_name)
                        .cloned()
                        .ok_or_else(|| ToolError::UnknownTool(tool_name.clone()))
                })
                .collect::<Result<Vec<_>, _>>()?,
        };
        let mut provider_tools =
            provider_tools_for_contract(&all_provider_tools, &current_task_contract);
        let mut planned_tool_tokens = estimate_tool_contract_tokens(&provider_tools);
        let base_plan_result = match replay_context.as_ref() {
            Some(replay_context) => self.context_builder.build_from_messages(
                request.task_id,
                current_turn_id,
                replay_context.initial_messages.clone(),
            ),
            None => self.context_builder.build(
                request.task_id,
                current_turn_id,
                request.contributors.clone(),
            ),
        };
        let base_plan = match base_plan_result {
            Ok(plan) => plan,
            Err(error) => {
                return Ok(context_guard::outcome(
                    &request,
                    error,
                    &mut trace,
                    current_defer_external_verification,
                ));
            }
        };
        if !base_plan.trimmed_contributors.is_empty() {
            trace(AgentLoopTraceEvent::ContextCompacted {
                original_input_tokens: base_plan.original_planned_input_tokens,
                planned_input_tokens: base_plan.budget_snapshot.planned_input_tokens,
                trimmed_contributors: base_plan.trimmed_contributors.clone(),
            });
        }
        let mut messages = base_plan.messages.clone();
        let mut message_sources = base_plan.message_sources.clone();
        let protected_prefix_len = base_plan.messages.len();
        let context_window_manager = self.context_builder.window_manager();
        let mut turn_state = TurnState::new(current_turn_id);

        'completion_cycle: loop {
            let mut candidate_complete = false;
            'agent_loop: loop {
                let step_snapshot = step_machine.begin(current_turn_id);
                let iteration = step_snapshot.step_no;
                trace(AgentLoopTraceEvent::StepStarted(step_snapshot.clone()));
                control.wait_until_runnable().await?;
                let tool_history_before = estimate_message_tokens(&messages);
                let compacted_tool_results =
                    compact_tool_result_history(&mut messages, &mut message_sources);
                if compacted_tool_results > 0 {
                    trace(AgentLoopTraceEvent::ContextCompacted {
                        original_input_tokens: tool_history_before,
                        planned_input_tokens: estimate_message_tokens(&messages),
                        trimmed_contributors: vec!["tool_result_history".to_owned()],
                    });
                }
                let mut plan = base_plan.clone();
                plan.messages = messages.clone();
                plan.message_sources = message_sources.clone();
                plan.budget_snapshot.turn_id = current_turn_id;
                plan.budget_snapshot.planned_tool_tokens = planned_tool_tokens;
                plan.budget_snapshot.planned_input_tokens =
                    estimate_message_tokens(&messages).saturating_add(planned_tool_tokens);
                if let Some(compaction_limit) = context_window_manager.required_compaction_limit(
                    protected_prefix_len,
                    &messages,
                    planned_tool_tokens,
                ) {
                    trace(AgentLoopTraceEvent::ContextCompactionStarted {
                        original_input_tokens: plan.budget_snapshot.planned_input_tokens,
                        budget_limit: compaction_limit,
                    });
                    match context_window_manager.compact_if_needed(
                        current_turn_id,
                        protected_prefix_len,
                        &messages,
                        &message_sources,
                        planned_tool_tokens,
                    ) {
                        Ok(Some(record)) => {
                            messages = record.replacement_messages.clone();
                            message_sources = record.replacement_sources.clone();
                            plan.messages = messages.clone();
                            plan.message_sources = message_sources.clone();
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
                                budget_limit: compaction_limit,
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
                        elapsed_millis(current_turn_started_at),
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
                let provider_elapsed_ms = elapsed_millis(current_turn_started_at);
                let governance = current_governor.evaluate(
                    &goal_ledger,
                    &GovernorObservation {
                        phase: GovernorPhase::Provider,
                        iteration: iteration.saturating_add(1),
                        tool_calls: tool_call_count,
                        failed_tool_calls: failed_tool_call_count,
                        consecutive_failed_tool_calls: consecutive_failed_tool_call_count,
                        planned_input_tokens: plan.budget_snapshot.planned_input_tokens,
                        elapsed_ms: provider_elapsed_ms,
                        latest_action: current_objective.clone(),
                        estimated_cost_microusd,
                        policy_decision: None,
                        policy_block_disposition: None,
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
                    let trigger = if provider_elapsed_ms >= current_max_elapsed_ms {
                        runtime_deadline_guard_emitted = true;
                        golutra_core::LoopGuardTrigger::RuntimeDeadline
                    } else if current_governor.limits().max_iterations > 0
                        && iteration >= current_governor.limits().max_iterations
                    {
                        golutra_core::LoopGuardTrigger::MaxIteration
                    } else {
                        golutra_core::LoopGuardTrigger::ContextOverflow
                    };
                    trace(AgentLoopTraceEvent::LoopGuardTriggered {
                        trigger,
                        reason: guard_reason
                            .clone()
                            .unwrap_or_else(|| "runtime governor blocked execution".to_owned()),
                    });
                    finish_runtime_step(
                        &mut step_machine,
                        step_snapshot.clone(),
                        "governor-blocked",
                        false,
                        elapsed_millis(current_turn_started_at),
                        &mut trace,
                    );
                    break;
                }
                let provider_contract = self.provider.contract();
                let model_input = compile_model_input(
                    request.session_id,
                    &plan,
                    request.task_id,
                    current_turn_id,
                    provider_contract.provider_id.clone(),
                    provider_contract.model_id.clone(),
                    provider_tools.clone(),
                )?;
                let (provider_request, context_snapshot) = model_input.into_parts();
                trace(AgentLoopTraceEvent::ContextSnapshotCaptured {
                    snapshot: context_snapshot,
                    request: provider_request.clone(),
                });
                trace(AgentLoopTraceEvent::ProviderStarted {
                    request_id: provider_request.request_id,
                    provider_id: provider_request.provider_id.clone(),
                    model_id: provider_request.model_id.clone(),
                });
                let provider_result = self
                    .complete_with_retry(
                        provider_request.clone(),
                        runtime_deadline,
                        &mut control,
                        &mut trace,
                    )
                    .await;
                let (provider_response, completed_request) = match provider_result {
                    Ok(result) => result,
                    Err(provider_session::ProviderSessionError::DeadlineExceeded { reason }) => {
                        runtime_deadline_guard_emitted = true;
                        finish_runtime_step(
                            &mut step_machine,
                            step_snapshot.clone(),
                            "runtime-deadline",
                            false,
                            elapsed_millis(current_turn_started_at),
                            &mut trace,
                        );
                        guard_reason = Some(reason);
                        break 'agent_loop;
                    }
                    Err(provider_session::ProviderSessionError::Provider(error)) => {
                        trace(AgentLoopTraceEvent::ProviderFailed {
                            request_id: provider_request.request_id,
                            provider_id: provider_request.provider_id.clone(),
                            model_id: provider_request.model_id.clone(),
                            error: error.to_string(),
                        });
                        finish_runtime_step(
                            &mut step_machine,
                            step_snapshot.clone(),
                            format!("provider-error:{error}"),
                            false,
                            elapsed_millis(current_turn_started_at),
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
                    request_id: completed_request.request_id,
                    provider_id: completed_request.provider_id.clone(),
                    model_id: completed_request.model_id.clone(),
                    response: provider_response.clone(),
                });
                let usage_record = token_usage_record(
                    &plan,
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
                        message_sources.push(ContextMessageSource {
                            contributor: "assistant_recent".to_owned(),
                            source_refs: vec![format!(
                                "provider-response:{}",
                                provider_response.response_id
                            )],
                            origin: "provider_response".to_owned(),
                            visibility: ModelInputVisibility::ModelVisible,
                        });
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
                                elapsed_millis(current_turn_started_at),
                                &mut trace,
                            );
                            trace(AgentLoopTraceEvent::RetryScheduled {
                                attempt: empty_response_count,
                                reason: "provider returned an empty response".to_owned(),
                            });
                            messages.push(ProviderMessage {
                                role: ProviderRole::User,
                                content: "Return a concrete response or a valid tool call."
                                    .to_owned(),
                                tool_call_id: None,
                                tool_name: None,
                                tool_calls: Vec::new(),
                                metadata: Default::default(),
                            });
                            message_sources.push(ContextMessageSource {
                                contributor: "runtime_context".to_owned(),
                                source_refs: vec!["runtime:empty-response-recovery".to_owned()],
                                origin: "runtime_recovery".to_owned(),
                                visibility: ModelInputVisibility::ModelVisible,
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
                            elapsed_millis(current_turn_started_at),
                            &mut trace,
                        );
                        guard_reason = Some(reason);
                        break;
                    }

                    if let Some(pending_turn) = control.pending_turns.take_or_close().await {
                        let completed_turn_elapsed_ms = elapsed_millis(current_turn_started_at);
                        current_turn_id = pending_turn.turn_id;
                        current_objective = pending_turn.content.clone();
                        current_task_contract = pending_turn
                            .task_contract
                            .clone()
                            .unwrap_or_else(|| TaskContract::conversational(Vec::new()));
                        current_output_schema = pending_turn.output_schema.clone();
                        current_external_verifiers = pending_turn.external_verifiers.clone();
                        current_external_verifiers_require_os_sandbox =
                            pending_turn.external_verifiers_require_os_sandbox;
                        current_max_elapsed_ms = pending_turn
                            .max_elapsed_ms
                            .unwrap_or(default_max_elapsed_ms)
                            .max(1);
                        current_defer_external_verification =
                            pending_turn.defer_external_verification;
                        current_governor =
                            governor_with_max_elapsed_ms(&self.governor, current_max_elapsed_ms);
                        current_turn_started_at = Instant::now();
                        runtime_deadline = deadline_from_budget(current_max_elapsed_ms);
                        deadline_advisory_emitted = false;
                        runtime_deadline_guard_emitted = false;
                        current_task_contract
                            .validate()
                            .map_err(AgentLoopError::TaskContract)?;
                        provider_tools = provider_tools_for_contract(
                            &all_provider_tools,
                            &current_task_contract,
                        );
                        planned_tool_tokens = estimate_tool_contract_tokens(&provider_tools);
                        current_completion_criteria =
                            current_task_contract.completion_criteria.clone();
                        current_turn_touched_code =
                            current_task_contract.requires_workspace_evidence();
                        last_assistant_message = None;
                        last_emitted_assistant_message = None;
                        tool_reports.clear();
                        repeated_failure_signature = None;
                        repeated_failure_count = 0;
                        failure_families = FailureFamilyLedger::default();
                        turn_state = TurnState::new(current_turn_id);
                        step_machine.end_correction();
                        goal_ledger.original_objective = current_objective.clone();
                        goal_ledger.success_criteria = current_completion_criteria.clone();
                        goal_ledger.current_plan = current_completion_criteria.clone();
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
                        message_sources.push(ContextMessageSource {
                            contributor: "user_message".to_owned(),
                            source_refs: vec![format!("turn:{}", current_turn_id)],
                            origin: "pending_turn".to_owned(),
                            visibility: ModelInputVisibility::ModelVisible,
                        });
                        finish_runtime_step(
                            &mut step_machine,
                            step_snapshot.clone(),
                            step_fingerprint.clone(),
                            true,
                            completed_turn_elapsed_ms,
                            &mut trace,
                        );
                        continue;
                    }
                    let step_completion = finish_runtime_step_with_material_progress(
                        &mut step_machine,
                        step_snapshot.clone(),
                        step_fingerprint,
                        true,
                        false,
                        elapsed_millis(current_turn_started_at),
                        &mut trace,
                    );
                    if step_completion.should_stop {
                        let reason = step_completion.stop_reason.clone().unwrap_or_else(|| {
                            "runtime progress policy stopped execution without a reason".to_owned()
                        });
                        trace(AgentLoopTraceEvent::LoopGuardTriggered {
                            trigger: golutra_core::LoopGuardTrigger::NoProgress,
                            reason: reason.clone(),
                        });
                        guard_reason = Some(reason);
                    }
                    turn_state.candidate_ready();
                    trace(AgentLoopTraceEvent::CandidateReady {
                        turn_id: current_turn_id,
                        tool_count: tool_reports.len(),
                        has_assistant_message: last_assistant_message.is_some(),
                    });
                    candidate_complete = true;
                    break;
                }

                let tool_reports_before_step = tool_reports.len();
                let mut failed_signatures_this_step = HashSet::new();
                let mut successful_signatures_this_step = HashSet::new();
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
                message_sources.push(ContextMessageSource {
                    contributor: "assistant_recent".to_owned(),
                    source_refs: vec![format!(
                        "provider-response:{}",
                        provider_response.response_id
                    )],
                    origin: "provider_tool_request".to_owned(),
                    visibility: ModelInputVisibility::ModelVisible,
                });
                for tool_call in provider_response.tool_calls {
                    control.wait_until_runnable().await?;
                    let tool_action = format!("{} {}", tool_call.tool_name, tool_call.arguments);
                    let governance = current_governor.evaluate(
                        &goal_ledger,
                        &GovernorObservation {
                            phase: GovernorPhase::Tool,
                            iteration: iteration.saturating_add(1),
                            tool_calls: tool_call_count.saturating_add(1),
                            failed_tool_calls: failed_tool_call_count,
                            consecutive_failed_tool_calls: consecutive_failed_tool_call_count,
                            planned_input_tokens: plan.budget_snapshot.planned_input_tokens,
                            elapsed_ms: elapsed_millis(current_turn_started_at),
                            latest_action: tool_action,
                            estimated_cost_microusd,
                            policy_decision: None,
                            policy_block_disposition: None,
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
                            elapsed_millis(current_turn_started_at),
                            &mut trace,
                        );
                        break 'agent_loop;
                    }
                    tool_call_count = tool_call_count.saturating_add(1);
                    let provider_tool_call_id = tool_call.tool_call_id.clone();
                    let failure_signature =
                        format!("{}:{}", tool_call.tool_name, tool_call.arguments);
                    let failure_family =
                        semantic_failure_family(&tool_call.tool_name, &tool_call.arguments);
                    let blocked_family_failures = failure_families.failures(&failure_family);
                    let mut tool_request = ToolRequest {
                        tool_call_id: golutra_core::ToolCallId::new(),
                        provider_tool_call_id: Some(provider_tool_call_id.clone()),
                        session_id: request.session_id,
                        turn_id: Some(current_turn_id),
                        tool_name: tool_call.tool_name,
                        arguments: tool_call.arguments,
                    };
                    let prepared_objective_validation =
                        prepare_objective_validation_metadata(&tool_request);
                    let recovery_policy = self
                        .tool_executor
                        .registry()
                        .contract(&tool_request.tool_name)
                        .map(ToolRecoveryPolicy::from)
                        .unwrap_or_default();
                    trace(AgentLoopTraceEvent::ToolStarted {
                        tool_call_id: tool_request.tool_call_id,
                        provider_tool_call_id: Some(provider_tool_call_id.clone()),
                        tool_name: tool_request.tool_name.clone(),
                        display_arguments: redact_tool_arguments(&tool_request.arguments),
                        recovery_policy,
                    });
                    let contract_blocked_report = self
                        .tool_executor
                        .registry()
                        .contract(&tool_request.tool_name)
                        .filter(|contract| {
                            matches!(
                                current_task_contract.workspace_change,
                                WorkspaceChangeRequirement::Forbidden
                            ) && contract.side_effect_type != SideEffectType::None
                        })
                        .map(|_| {
                            self.tool_executor.invalid_request_report(
                                tool_request.clone(),
                                "task contract forbids side-effecting tools",
                            )
                        });
                    let strategy_blocked_report = (blocked_family_failures >= 2).then(|| {
                        self.tool_executor.invalid_request_report(
                            tool_request.clone(),
                            format!(
                                "strategy `{failure_family}` is blocked after {blocked_family_failures} failures; choose a materially different approach"
                            ),
                        )
                    });
                    let strategy_was_blocked = strategy_blocked_report.is_some();
                    let mut report = if let Some(report) = strategy_blocked_report {
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
                            detail: Some("strategy_blocked".to_owned()),
                        }));
                        report
                    } else if let Some(report) = contract_blocked_report {
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
                            detail: Some("blocked".to_owned()),
                        }));
                        report
                    } else {
                        match self.tool_executor.evaluate(&tool_request) {
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
                                    let approved =
                                        resolution.decision == ApprovalDecision::Approved;
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
                                        let checkpoint_error = if may_execute
                                            && (tool_request.tool_name == "shell"
                                                || !preparation.before_images.is_empty())
                                            && let Some(recorder) =
                                                &self.before_side_effect_recorder
                                        {
                                            recorder
                                                .persist_before_side_effect(
                                                    &tool_request,
                                                    &preparation.before_images,
                                                    preparation.complete,
                                                )
                                                .await
                                                .err()
                                        } else {
                                            None
                                        };
                                        if let Some(error) = checkpoint_error {
                                            self.tool_executor.execution_error_report(
                                                tool_request,
                                                policy,
                                                format!(
                                                    "before-side-effect checkpoint failed: {error}"
                                                ),
                                            )
                                        } else {
                                            control.wait_until_runnable().await?;
                                            let max_elapsed_ms =
                                                current_governor.limits().max_elapsed_ms;
                                            let elapsed_ms =
                                                elapsed_millis(current_turn_started_at);
                                            clamp_shell_timeout_to_budget(
                                                &mut tool_request,
                                                shell_execution_budget(
                                                    max_elapsed_ms,
                                                    elapsed_ms,
                                                    deadline_advisory_emitted,
                                                ),
                                            );
                                            let mut progress = |progress| {
                                                trace(AgentLoopTraceEvent::ToolProgress(progress));
                                            };
                                            let error_request = tool_request.clone();
                                            let error_policy = policy.clone();
                                            match self
                                                .tool_executor
                                                .invoke(
                                                    ToolInvocation::new(
                                                        tool_request,
                                                        policy,
                                                        approved,
                                                    )
                                                    .with_preparation(preparation),
                                                    control.cancellation.clone(),
                                                    Some(&mut progress),
                                                )
                                                .await
                                            {
                                                Ok(report) => report,
                                                Err(error) => {
                                                    self.tool_executor.execution_error_report(
                                                        error_request,
                                                        error_policy,
                                                        error.to_string(),
                                                    )
                                                }
                                            }
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
                        }
                    };
                    attach_prepared_objective_validation(
                        &mut report,
                        prepared_objective_validation,
                    );
                    trace(AgentLoopTraceEvent::ToolCompleted(report.clone()));
                    update_tool_failure_counts(
                        report.envelope.status,
                        &mut failed_tool_call_count,
                        &mut consecutive_failed_tool_call_count,
                    );
                    failure_families.observe(&failure_family, report.envelope.status);
                    if report.envelope.status == ToolResultStatus::Ok {
                        successful_signatures_this_step.insert(failure_signature);
                    } else {
                        failed_signatures_this_step.insert(failure_signature);
                    }
                    let tool_result_elapsed_ms = elapsed_millis(current_turn_started_at);
                    let result_governance = current_governor.evaluate(
                        &goal_ledger,
                        &GovernorObservation {
                            phase: GovernorPhase::ToolResult,
                            iteration: iteration.saturating_add(1),
                            tool_calls: tool_call_count,
                            failed_tool_calls: failed_tool_call_count,
                            consecutive_failed_tool_calls: consecutive_failed_tool_call_count,
                            planned_input_tokens: plan.budget_snapshot.planned_input_tokens,
                            elapsed_ms: tool_result_elapsed_ms,
                            latest_action: report.envelope.summary.clone(),
                            estimated_cost_microusd,
                            policy_decision: Some(report.policy_evaluation.decision),
                            policy_block_disposition: report
                                .policy_evaluation
                                .effective_block_disposition(),
                            security_risk: report.envelope.risk.clone(),
                        },
                    );
                    let permits_continuation = result_governance.permits_execution();
                    if !permits_continuation {
                        guard_reason = Some(result_governance.reason.clone());
                        governor_action = Some(result_governance.action);
                    }
                    trace(AgentLoopTraceEvent::GovernorDecided(result_governance));
                    if !permits_continuation
                        && !runtime_deadline_guard_emitted
                        && tool_result_elapsed_ms >= current_max_elapsed_ms
                    {
                        trace(AgentLoopTraceEvent::LoopGuardTriggered {
                            trigger: golutra_core::LoopGuardTrigger::RuntimeDeadline,
                            reason: guard_reason
                                .clone()
                                .unwrap_or_else(|| "runtime wall-clock budget exceeded".to_owned()),
                        });
                        runtime_deadline_guard_emitted = true;
                    }
                    messages.push(ProviderMessage {
                        role: ProviderRole::Tool,
                        content: model_visible_tool_result(&report.envelope),
                        tool_call_id: Some(provider_tool_call_id),
                        tool_name: Some(report.envelope.tool_name.clone()),
                        tool_calls: Vec::new(),
                        metadata: Default::default(),
                    });
                    message_sources.push(ContextMessageSource {
                        contributor: "tool_result_excerpt".to_owned(),
                        source_refs: vec![format!("tool-call:{}", report.envelope.tool_call_id)],
                        origin: "tool_result".to_owned(),
                        visibility: ModelInputVisibility::ModelVisible,
                    });
                    tool_reports.push(report);
                    if strategy_was_blocked && blocked_family_failures >= 3 {
                        let reason = format!(
                            "strategy `{failure_family}` remained selected after a bounded correction"
                        );
                        trace(AgentLoopTraceEvent::LoopGuardTriggered {
                            trigger: golutra_core::LoopGuardTrigger::RepeatedToolFailure,
                            reason: reason.clone(),
                        });
                        guard_reason = Some(reason);
                        finish_runtime_step(
                            &mut step_machine,
                            step_snapshot.clone(),
                            step_fingerprint.clone(),
                            false,
                            elapsed_millis(current_turn_started_at),
                            &mut trace,
                        );
                        break 'agent_loop;
                    }
                    if !permits_continuation {
                        finish_runtime_step(
                            &mut step_machine,
                            step_snapshot.clone(),
                            step_fingerprint.clone(),
                            false,
                            elapsed_millis(current_turn_started_at),
                            &mut trace,
                        );
                        break 'agent_loop;
                    }
                }
                failed_signatures_this_step
                    .retain(|signature| !successful_signatures_this_step.contains(signature));
                update_repeated_failure_streak(
                    &failed_signatures_this_step,
                    &mut repeated_failure_signature,
                    &mut repeated_failure_count,
                );
                let made_progress = tool_reports[tool_reports_before_step..]
                    .iter()
                    .enumerate()
                    .any(|(offset, report)| {
                        !report.changed_files.is_empty()
                            || objective_validation_report_in_turn(
                                report,
                                &tool_reports,
                                tool_reports_before_step.saturating_add(offset),
                                self.tool_executor.workspace_root(),
                            )
                            .is_some_and(|validation| validation.passed)
                    });
                let step_completion = finish_runtime_step(
                    &mut step_machine,
                    step_snapshot.clone(),
                    step_fingerprint,
                    made_progress,
                    elapsed_millis(current_turn_started_at),
                    &mut trace,
                );
                if !deadline_advisory_emitted
                    && !step_completion.should_stop
                    && let Some(advisory) = runtime_deadline_advisory(
                        current_max_elapsed_ms,
                        elapsed_millis(current_turn_started_at),
                    )
                {
                    messages.push(ProviderMessage {
                        role: ProviderRole::User,
                        content: advisory,
                        tool_call_id: None,
                        tool_name: None,
                        tool_calls: Vec::new(),
                        metadata: Default::default(),
                    });
                    message_sources.push(ContextMessageSource {
                        contributor: "runtime_context".to_owned(),
                        source_refs: vec![format!(
                            "runtime:deadline-advisory:{}",
                            step_completion.snapshot.step_no
                        )],
                        origin: "runtime_deadline_advisory".to_owned(),
                        visibility: ModelInputVisibility::ModelVisible,
                    });
                    deadline_advisory_emitted = true;
                }
                if let Some(advisory) = step_completion.advisory.as_deref() {
                    messages.push(ProviderMessage {
                        role: ProviderRole::User,
                        content: format!(
                            "Runtime progress advisory: {advisory}. Use the evidence already gathered and take a materially different action before repeating this strategy."
                        ),
                        tool_call_id: None,
                        tool_name: None,
                        tool_calls: Vec::new(),
                        metadata: Default::default(),
                    });
                    message_sources.push(ContextMessageSource {
                        contributor: "runtime_context".to_owned(),
                        source_refs: vec![format!(
                            "runtime:progress-advisory:{}",
                            step_completion.snapshot.step_no
                        )],
                        origin: "runtime_progress_advisory".to_owned(),
                        visibility: ModelInputVisibility::ModelVisible,
                    });
                }
                if step_completion.should_stop {
                    let reason = step_completion.stop_reason.clone().unwrap_or_else(|| {
                        "runtime progress policy stopped execution without a reason".to_owned()
                    });
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

            let candidate_tool_report_count = tool_reports.len();
            for verifier in &current_external_verifiers {
                control.wait_until_runnable().await?;
                let tool_call_id = golutra_core::ToolCallId::new();
                let execution_request = VerifierExecutionRequest {
                    tool_call_id,
                    session_id: request.session_id,
                    turn_id: Some(current_turn_id),
                    program: verifier.program.clone(),
                    args: verifier.args.clone(),
                    cwd: verifier.cwd.clone().into(),
                    timeout_ms: verifier.timeout_ms.min(
                        current_governor
                            .limits()
                            .max_elapsed_ms
                            .saturating_sub(elapsed_millis(current_turn_started_at))
                            .max(1),
                    ),
                    expected_exit_code: verifier.expected_exit_code,
                    max_output_bytes: verifier.max_output_bytes,
                };
                let tool_request = execution_request.as_tool_request();
                trace(AgentLoopTraceEvent::ToolStarted {
                    tool_call_id,
                    provider_tool_call_id: None,
                    tool_name: "external_verifier".to_owned(),
                    display_arguments: redact_tool_arguments(&tool_request.arguments),
                    recovery_policy: ToolRecoveryPolicy::for_side_effect(SideEffectType::Process),
                });
                let report = if current_external_verifiers_require_os_sandbox
                    && !self.tool_executor.sandbox_os_enforced()
                {
                    self.tool_executor.verifier_execution_error_report(
                        execution_request,
                        "auto-discovered verifier requires an OS-enforced sandbox",
                    )
                } else {
                    match self
                        .tool_executor
                        .prepare_verifier_side_effect(&execution_request)
                        .await
                    {
                        Ok(preparation) => {
                            let checkpoint_error =
                                if let Some(recorder) = &self.before_side_effect_recorder {
                                    recorder
                                        .persist_before_side_effect(
                                            &tool_request,
                                            &preparation.before_images,
                                            preparation.complete,
                                        )
                                        .await
                                        .err()
                                } else {
                                    None
                                };
                            if let Some(error) = checkpoint_error {
                                self.tool_executor.verifier_execution_error_report(
                                    execution_request,
                                    format!("before-side-effect checkpoint failed: {error}"),
                                )
                            } else {
                                control.wait_until_runnable().await?;
                                match self
                                    .tool_executor
                                    .execute_verifier_with_preparation(
                                        execution_request.clone(),
                                        control.cancellation.clone(),
                                        preparation,
                                    )
                                    .await
                                {
                                    Ok(report) => report,
                                    Err(error) => {
                                        self.tool_executor.verifier_execution_error_report(
                                            execution_request,
                                            error.to_string(),
                                        )
                                    }
                                }
                            }
                        }
                        Err(error) => self
                            .tool_executor
                            .verifier_execution_error_report(execution_request, error.to_string()),
                    }
                };
                trace(AgentLoopTraceEvent::ToolCompleted(report.clone()));
                tool_reports.push(report);
            }

            let changed_relative = tool_reports
                .iter()
                .take(candidate_tool_report_count)
                .flat_map(|report| report.changed_files.iter())
                .filter_map(|path| path.strip_prefix(self.tool_executor.workspace_root()).ok())
                .map(|path| path.to_string_lossy().replace('\\', "/"))
                .collect::<HashSet<_>>();
            let mut contract_path_checks = Vec::new();
            for required in &current_task_contract.required_paths {
                let tool_call_id = golutra_core::ToolCallId::new();
                let display_arguments = serde_json::json!({"path": required});
                trace(AgentLoopTraceEvent::ToolStarted {
                    tool_call_id,
                    provider_tool_call_id: None,
                    tool_name: CONTRACT_PATH_VERIFIER_TOOL.to_owned(),
                    display_arguments: display_arguments.clone(),
                    recovery_policy: ToolRecoveryPolicy::for_side_effect(SideEffectType::None),
                });
                let report = self
                    .tool_executor
                    .verify_workspace_path(ToolRequest {
                        tool_call_id,
                        provider_tool_call_id: None,
                        session_id: request.session_id,
                        turn_id: Some(current_turn_id),
                        tool_name: CONTRACT_PATH_VERIFIER_TOOL.to_owned(),
                        arguments: display_arguments,
                    })
                    .await;
                let exists = report
                    .envelope
                    .structured_facts
                    .get("exists")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let normalized = required.replace('\\', "/");
                let is_directory = report
                    .envelope
                    .structured_facts
                    .pointer("/metadata/file_type")
                    .and_then(Value::as_str)
                    == Some("directory");
                let changed_when_required =
                    !matches!(
                        current_task_contract.workspace_change,
                        WorkspaceChangeRequirement::Required
                    ) || delivery_path_was_changed(&normalized, is_directory, &changed_relative);
                contract_path_checks.push(VerificationCheck {
                    kind: VerificationCheckKind::ObjectiveValidation,
                    name: "objective:path:delivery".to_owned(),
                    command: Some(required.clone()),
                    passed: exists && changed_when_required,
                    evidence_refs: report.envelope.evidence_refs.clone(),
                    message: if !exists {
                        format!("task contract delivery path is missing: {normalized}")
                    } else if !changed_when_required {
                        format!("task contract delivery path was not changed: {normalized}")
                    } else {
                        format!("task contract delivery path is present: {normalized}")
                    },
                });
                trace(AgentLoopTraceEvent::ToolCompleted(report.clone()));
                tool_reports.push(report);
            }

            let mut contract_content_checks = Vec::new();
            for requirement in &current_task_contract.required_file_contents {
                let tool_call_id = golutra_core::ToolCallId::new();
                let display_arguments = serde_json::json!({
                    "path": requirement.path,
                    "expected_bytes": requirement.content.len(),
                    "expected_checksum": format!(
                        "sha256:{:x}",
                        Sha256::digest(requirement.content.as_bytes())
                    ),
                });
                trace(AgentLoopTraceEvent::ToolStarted {
                    tool_call_id,
                    provider_tool_call_id: None,
                    tool_name: CONTRACT_FILE_CONTENT_VERIFIER_TOOL.to_owned(),
                    display_arguments: display_arguments.clone(),
                    recovery_policy: ToolRecoveryPolicy::for_side_effect(SideEffectType::None),
                });
                let report = self
                    .tool_executor
                    .verify_workspace_file_content(
                        ToolRequest {
                            tool_call_id,
                            provider_tool_call_id: None,
                            session_id: request.session_id,
                            turn_id: Some(current_turn_id),
                            tool_name: CONTRACT_FILE_CONTENT_VERIFIER_TOOL.to_owned(),
                            arguments: display_arguments,
                        },
                        requirement.content.as_bytes(),
                    )
                    .await;
                let passed = report
                    .envelope
                    .structured_facts
                    .get("matches")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                contract_content_checks.push(VerificationCheck {
                    kind: VerificationCheckKind::ObjectiveValidation,
                    name: "objective:content:write_file".to_owned(),
                    command: Some(requirement.path.clone()),
                    passed,
                    evidence_refs: report.envelope.evidence_refs.clone(),
                    message: report.envelope.summary.clone(),
                });
                trace(AgentLoopTraceEvent::ToolCompleted(report.clone()));
                tool_reports.push(report);
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
            command_checks.extend(contract_path_checks);
            command_checks.extend(contract_content_checks);
            command_checks.extend(tool_reports.iter().map(|report| {
                let passed = match report.policy_evaluation.decision {
                    PolicyDecision::Allow => true,
                    PolicyDecision::Ask => report.envelope.status == ToolResultStatus::Ok,
                    PolicyDecision::Deny => false,
                    PolicyDecision::Block => {
                        report.policy_evaluation.effective_block_disposition()
                            != Some(PolicyBlockDisposition::Terminal)
                    }
                };
                VerificationCheck {
                    kind: VerificationCheckKind::Policy,
                    name: format!("policy:{}", report.envelope.tool_name),
                    command: None,
                    passed,
                    evidence_refs: report.policy_evaluation.evidence_refs.clone(),
                    message: report.policy_evaluation.reason.clone(),
                }
            }));
            if tool_reports.is_empty() {
                command_checks.push(VerificationCheck {
                    kind: VerificationCheckKind::Policy,
                    name: "policy:no_tool_calls".to_owned(),
                    command: None,
                    passed: true,
                    evidence_refs: Vec::new(),
                    message: "no side-effecting tool call was requested".to_owned(),
                });
            }
            let changed_files = tool_reports
                .iter()
                .take(candidate_tool_report_count)
                .flat_map(|report| report.changed_files.iter())
                .collect::<Vec<_>>();
            if !changed_files.is_empty() {
                command_checks.push(VerificationCheck {
                    kind: VerificationCheckKind::WorkspaceChange,
                    name: "workspace_diff".to_owned(),
                    command: None,
                    passed: true,
                    evidence_refs: tool_reports
                        .iter()
                        .take(candidate_tool_report_count)
                        .flat_map(|report| report.envelope.evidence_refs.iter().copied())
                        .collect(),
                    message: format!("{} workspace file(s) changed", changed_files.len()),
                });
            }
            let code_files_changed = changed_files.iter().any(|path| is_code_file(path));
            for (report_index, report) in tool_reports.iter().enumerate() {
                if report.envelope.tool_name == "external_verifier" {
                    command_checks.push(VerificationCheck {
                        kind: VerificationCheckKind::ObjectiveValidation,
                        name: "objective:test:external_verifier".to_owned(),
                        command: report
                            .envelope
                            .structured_facts
                            .get("command")
                            .and_then(Value::as_str)
                            .map(ToOwned::to_owned),
                        passed: report.envelope.status == ToolResultStatus::Ok,
                        evidence_refs: report.envelope.evidence_refs.clone(),
                        message: report.envelope.summary.clone(),
                    });
                    continue;
                }
                if let Some(validation) = objective_validation_report_in_turn(
                    report,
                    &tool_reports,
                    report_index,
                    self.tool_executor.workspace_root(),
                )
                .or_else(|| {
                    explicitly_requested_inspection_validation(
                        report,
                        &current_objective,
                        &current_completion_criteria,
                        &current_task_contract,
                        self.tool_executor.workspace_root(),
                    )
                }) {
                    command_checks.push(VerificationCheck {
                        kind: VerificationCheckKind::ObjectiveValidation,
                        name: format!(
                            "objective:{}:{}:identity:{}",
                            validation.kind.label(),
                            report.envelope.tool_name,
                            validation.identity
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
            }
            if last_assistant_message
                .as_deref()
                .is_some_and(|message| !message.trim().is_empty())
                && (!current_task_contract.requires_workspace_evidence()
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
            if let Some(schema) = current_output_schema
                .as_ref()
                .filter(|value| !value.is_null())
            {
                command_checks.push(output_schema_check(
                    schema,
                    last_assistant_message.as_deref(),
                ));
            }
            let verification_input = if completion::accepts_text_response_without_evidence(
                current_task_contract.requires_workspace_evidence() || requires_workspace_evidence,
                last_assistant_message.as_deref(),
                &tool_reports,
            ) && current_output_schema
                .as_ref()
                .is_none_or(serde_json::Value::is_null)
            {
                VerificationInput {
                    task_id: request.task_id,
                    objective: current_objective.clone(),
                    completion_criteria: current_completion_criteria.clone(),
                    evidence_refs: Vec::new(),
                    command_checks: vec![
                        VerificationCheck {
                            kind: VerificationCheckKind::AssistantResponse,
                            name: "assistant_response".to_owned(),
                            command: None,
                            passed: true,
                            evidence_refs: Vec::new(),
                            message: "assistant response produced".to_owned(),
                        },
                        VerificationCheck {
                            kind: VerificationCheckKind::Policy,
                            name: "policy:no_tool_calls".to_owned(),
                            command: None,
                            passed: true,
                            evidence_refs: Vec::new(),
                            message: "no side-effecting tool call was requested".to_owned(),
                        },
                    ],
                    requires_workspace_evidence: false,
                    code_files_changed: false,
                }
            } else {
                VerificationInput {
                    task_id: request.task_id,
                    objective: current_objective.clone(),
                    completion_criteria: current_completion_criteria.clone(),
                    evidence_refs,
                    command_checks,
                    requires_workspace_evidence,
                    code_files_changed,
                }
            };
            let verification_plan = self
                .verifier
                .plan_governed(&verification_input, &current_task_contract);
            turn_state.verification_ready();
            trace(AgentLoopTraceEvent::VerificationReady {
                plan_id: verification_plan.plan_id,
            });
            trace(AgentLoopTraceEvent::VerificationPlanned(
                verification_plan.clone(),
            ));
            let (mut verification, verification_plan) = self.verifier.verify_governed(
                verification_input,
                verification_plan,
                &current_task_contract,
                verification_environment_digest(
                    self.tool_executor.workspace_root(),
                    &current_task_contract,
                    &current_external_verifiers,
                ),
            );
            turn_state.begin_verification(verification.verification_id);
            for assertion in verification_plan
                .assertions
                .iter()
                .chain(verification_plan.policy_assertions.iter())
            {
                trace(AgentLoopTraceEvent::VerificationAssertionCompleted(
                    assertion.clone(),
                ));
            }
            let completion_elapsed_ms = elapsed_millis(current_turn_started_at);
            let completion_governance = current_governor.evaluate(
                &goal_ledger,
                &GovernorObservation {
                    phase: GovernorPhase::Completion,
                    iteration: step_machine.checkpoint().next_step_no,
                    tool_calls: tool_call_count,
                    failed_tool_calls: failed_tool_call_count,
                    consecutive_failed_tool_calls: consecutive_failed_tool_call_count,
                    planned_input_tokens: last_budget_state
                        .planned_input_tokens
                        .unwrap_or_default(),
                    elapsed_ms: completion_elapsed_ms,
                    latest_action: last_assistant_message
                        .clone()
                        .unwrap_or_else(|| current_objective.clone()),
                    estimated_cost_microusd,
                    policy_decision: None,
                    policy_block_disposition: None,
                    security_risk: "low".to_owned(),
                },
            );
            let permits_completion = completion_governance.permits_execution();
            if !permits_completion {
                guard_reason = Some(completion_governance.reason.clone());
                governor_action = Some(completion_governance.action);
            }
            trace(AgentLoopTraceEvent::GovernorDecided(completion_governance));
            if !permits_completion
                && !runtime_deadline_guard_emitted
                && completion_elapsed_ms >= current_max_elapsed_ms
            {
                trace(AgentLoopTraceEvent::LoopGuardTriggered {
                    trigger: golutra_core::LoopGuardTrigger::RuntimeDeadline,
                    reason: guard_reason
                        .clone()
                        .unwrap_or_else(|| "runtime wall-clock budget exceeded".to_owned()),
                });
            }
            if let Some(reason) = &guard_reason {
                if verification.result == VerificationResult::Pass {
                    verification.result = VerificationResult::Partial;
                }
                verification.residual_risks.push(reason.clone());
            }
            let independent_verifier_unavailable = current_task_contract
                .requires_independent_verification()
                && current_external_verifiers.is_empty();
            let policy_verified = verification.assertions.iter().any(|assertion| {
                assertion.blocking
                    && assertion.kind == golutra_core::VerificationAssertionKind::Policy
                    && matches!(
                        assertion.status,
                        golutra_core::VerificationAssertionStatus::Pass
                            | golutra_core::VerificationAssertionStatus::NotApplicable
                    )
            });
            let candidate_ready_for_external_verification = candidate_complete
                && guard_reason.is_none()
                && current_defer_external_verification
                && policy_verified
                && !verification.assertions.iter().any(|assertion| {
                    assertion.blocking
                        && assertion.status == golutra_core::VerificationAssertionStatus::Fail
                });
            if candidate_complete
                && guard_reason.is_none()
                && verification.result != VerificationResult::Pass
                && !independent_verifier_unavailable
                && !candidate_ready_for_external_verification
                && current_task_contract.allows_correction(turn_state.correction_attempt)
            {
                let correction = correction_envelope(
                    &verification,
                    turn_state.correction_attempt.saturating_add(1),
                    current_task_contract
                        .max_correction_rounds
                        .saturating_sub(turn_state.correction_attempt.saturating_add(1)),
                );
                trace(AgentLoopTraceEvent::VerificationCompleted {
                    record: verification.clone(),
                    terminal: false,
                });
                turn_state.issue_correction(golutra_core::ContinuationReason::VerificationFailed);
                step_machine.begin_correction(elapsed_millis(current_turn_started_at));
                trace(AgentLoopTraceEvent::CorrectionIssued(correction.clone()));
                messages.push(ProviderMessage {
                    role: ProviderRole::User,
                    content: correction.as_model_instruction(),
                    tool_call_id: None,
                    tool_name: None,
                    tool_calls: Vec::new(),
                    metadata: Default::default(),
                });
                message_sources.push(ContextMessageSource {
                    contributor: "verification_feedback".to_owned(),
                    source_refs: correction
                        .evidence_refs
                        .iter()
                        .map(|evidence| format!("evidence:{evidence}"))
                        .collect(),
                    origin: "verification_feedback".to_owned(),
                    visibility: ModelInputVisibility::ModelVisible,
                });
                tool_reports.retain(|report| {
                    !matches!(
                        report.envelope.tool_name.as_str(),
                        "external_verifier"
                            | CONTRACT_FILE_CONTENT_VERIFIER_TOOL
                            | CONTRACT_PATH_VERIFIER_TOOL
                    )
                });
                last_assistant_message = None;
                last_emitted_assistant_message = None;
                guard_reason = None;
                governor_action = None;
                continue 'completion_cycle;
            }
            trace(AgentLoopTraceEvent::VerificationCompleted {
                record: verification.clone(),
                terminal: true,
            });
            turn_state.terminal();
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
                last_emitted_assistant_message.as_ref()
                    != Some(&(current_turn_id, (*content).clone()))
            }) {
                trace(AgentLoopTraceEvent::AssistantMessage {
                    turn_id: current_turn_id,
                    content: content.clone(),
                });
            }

            break Ok(AgentLoopOutcome {
                final_message,
                verification,
                verification_plan,
                loop_decision,
                tool_reports,
                final_turn_id: current_turn_id,
                defer_external_verification: current_defer_external_verification,
                candidate_ready_for_external_verification,
            });
        }
    }

    async fn complete_with_retry<F>(
        &self,
        request: ProviderRequest,
        deadline: Option<tokio::time::Instant>,
        control: &mut AgentExecutionControl,
        trace: &mut F,
    ) -> Result<(ProviderResponse, ProviderRequest), provider_session::ProviderSessionError>
    where
        F: FnMut(AgentLoopTraceEvent) + Send,
    {
        control.wait_until_runnable().await.map_err(|_| {
            provider_session::ProviderSessionError::Provider(ProviderError::Cancelled)
        })?;
        let fallback_model_id = self
            .fallback_provider
            .as_ref()
            .map(|provider| provider.contract().model_id);
        let request_id = request.request_id;
        let mut active_provider_id = request.provider_id.clone();
        let mut active_model_id = request.model_id.clone();
        let mut on_event = |event| match event {
            provider_session::ProviderSessionEvent::Streamed {
                provider_id,
                model_id,
                event,
            } => trace(AgentLoopTraceEvent::ProviderStreamed {
                request_id,
                provider_id,
                model_id,
                event,
            }),
            provider_session::ProviderSessionEvent::RetryScheduled {
                attempt,
                max_retries,
                transport,
                reason,
            } => trace(AgentLoopTraceEvent::RetryScheduled {
                attempt,
                reason: format!(
                    "{} transport retry {attempt}/{max_retries}: {reason}",
                    transport.label()
                ),
            }),
            provider_session::ProviderSessionEvent::TransportFallback {
                provider_id,
                from,
                to,
                reason,
            } => trace(AgentLoopTraceEvent::ProviderTransportFallback {
                provider_id,
                from_transport: from.label().to_owned(),
                to_transport: to.label().to_owned(),
                reason,
            }),
            provider_session::ProviderSessionEvent::ProviderFallback {
                from_provider,
                to_provider,
                reason,
            } => {
                active_provider_id = to_provider.clone();
                active_model_id = fallback_model_id.clone().unwrap_or_default();
                trace(AgentLoopTraceEvent::ProviderFallback {
                    from_provider,
                    to_provider: to_provider.clone(),
                    reason,
                });
                trace(AgentLoopTraceEvent::ProviderStarted {
                    request_id,
                    provider_id: to_provider,
                    model_id: active_model_id.clone(),
                });
            }
        };
        let session = provider_session::ProviderSession::new(
            &self.provider,
            self.fallback_provider.as_ref(),
            self.provider_session_policy,
        )
        .with_deadline(deadline);
        let result = session
            .complete(request, &control.cancellation, &mut on_event)
            .await;
        if let Err(provider_session::ProviderSessionError::DeadlineExceeded { reason }) = &result {
            trace(AgentLoopTraceEvent::ProviderFailed {
                request_id,
                provider_id: active_provider_id,
                model_id: active_model_id,
                error: reason.clone(),
            });
            trace(AgentLoopTraceEvent::LoopGuardTriggered {
                trigger: golutra_core::LoopGuardTrigger::RuntimeDeadline,
                reason: reason.clone(),
            });
        }
        result
    }
}

fn update_tool_failure_counts(status: ToolResultStatus, total: &mut u32, consecutive: &mut u32) {
    if status == ToolResultStatus::Ok {
        *consecutive = 0;
    } else {
        *total = total.saturating_add(1);
        *consecutive = consecutive.saturating_add(1);
    }
}

#[derive(Debug, Default)]
struct FailureFamilyLedger {
    failures: HashMap<String, u32>,
}

impl FailureFamilyLedger {
    fn failures(&self, family: &str) -> u32 {
        self.failures.get(family).copied().unwrap_or_default()
    }

    fn observe(&mut self, family: &str, status: ToolResultStatus) {
        if status == ToolResultStatus::Ok {
            self.failures.remove(family);
        } else {
            let count = self.failures.entry(family.to_owned()).or_default();
            *count = count.saturating_add(1);
        }
    }
}

fn semantic_failure_family(tool_name: &str, arguments: &Value) -> String {
    golutra_core::semantic_tool_failure_family(tool_name, arguments)
        .unwrap_or_else(|| format!("{tool_name}:{}", digest_value(arguments)))
}

fn digest_value(value: &Value) -> String {
    let mut digest = Sha256::new();
    digest.update(serde_json::to_vec(value).unwrap_or_default());
    format!("{:x}", digest.finalize())
}

const MAX_MODEL_TOOL_HISTORY_TOKENS: u64 = 2_048;

fn compact_tool_result_history(
    messages: &mut [ProviderMessage],
    sources: &mut [ContextMessageSource],
) -> usize {
    let mut retained_tokens = 0_u64;
    let mut compacted = 0_usize;
    for index in (0..messages.len()).rev() {
        if messages[index].role != ProviderRole::Tool {
            continue;
        }
        let message_tokens = estimate_message_tokens(std::slice::from_ref(&messages[index]));
        if retained_tokens == 0
            || retained_tokens.saturating_add(message_tokens) <= MAX_MODEL_TOOL_HISTORY_TOKENS
        {
            retained_tokens = retained_tokens.saturating_add(message_tokens);
            continue;
        }
        if sources
            .get(index)
            .is_some_and(|source| source.origin == "tool_result_compaction")
        {
            retained_tokens = retained_tokens.saturating_add(message_tokens);
            continue;
        }
        messages[index].content = compacted_tool_result_message(&messages[index].content);
        if let Some(source) = sources.get_mut(index) {
            source.origin = "tool_result_compaction".to_owned();
        }
        retained_tokens = retained_tokens.saturating_add(estimate_message_tokens(
            std::slice::from_ref(&messages[index]),
        ));
        compacted = compacted.saturating_add(1);
    }
    compacted
}

fn compacted_tool_result_message(content: &str) -> String {
    let parsed = serde_json::from_str::<serde_json::Value>(content).unwrap_or_default();
    serde_json::to_string(&serde_json::json!({
        "tool_name": parsed.get("tool_name").and_then(serde_json::Value::as_str),
        "status": parsed.get("status").cloned().unwrap_or(serde_json::Value::Null),
        "summary": parsed.get("summary").and_then(serde_json::Value::as_str),
        "history_state": "compacted",
        "detail": "full result remains available in runtime artifacts",
    }))
    .unwrap_or_else(|_| "{\"history_state\":\"compacted\"}".to_owned())
}

fn update_repeated_failure_streak(
    failed_signatures_this_step: &HashSet<String>,
    repeated_signature: &mut Option<String>,
    repeated_count: &mut u32,
) {
    if failed_signatures_this_step.len() != 1 {
        *repeated_signature = None;
        *repeated_count = 0;
        return;
    }
    let signature = failed_signatures_this_step
        .iter()
        .next()
        .expect("one failed signature");
    if repeated_signature.as_deref() == Some(signature.as_str()) {
        *repeated_count = repeated_count.saturating_add(1);
    } else {
        *repeated_signature = Some(signature.clone());
        *repeated_count = 1;
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
                | "bash"
                | "cpp"
                | "cs"
                | "dart"
                | "erl"
                | "ex"
                | "exs"
                | "fish"
                | "fs"
                | "fsx"
                | "go"
                | "h"
                | "hpp"
                | "hrl"
                | "java"
                | "js"
                | "jsx"
                | "kt"
                | "kts"
                | "php"
                | "pl"
                | "py"
                | "r"
                | "R"
                | "rb"
                | "rs"
                | "scala"
                | "sh"
                | "sql"
                | "swift"
                | "ts"
                | "tsx"
                | "zsh"
        )
    ) || path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            matches!(
                name,
                "Dockerfile" | "Justfile" | "Makefile" | "Rakefile" | "build.gradle"
            )
        })
}

fn delivery_path_was_changed(
    normalized: &str,
    is_directory: bool,
    changed_relative: &HashSet<String>,
) -> bool {
    if changed_relative.contains(normalized) {
        return true;
    }
    if !is_directory {
        return false;
    }
    let prefix = format!("{}/", normalized.trim_end_matches('/'));
    changed_relative
        .iter()
        .any(|changed| changed.starts_with(&prefix))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ObjectiveValidationKind {
    Test,
    Diagnostic,
    FileState,
}

impl ObjectiveValidationKind {
    const fn label(self) -> &'static str {
        match self {
            Self::Test => "test",
            Self::Diagnostic => "diagnostic",
            Self::FileState => "file_state",
        }
    }

    fn from_label(value: &str) -> Option<Self> {
        match value {
            "test" => Some(Self::Test),
            "diagnostic" => Some(Self::Diagnostic),
            "file_state" => Some(Self::FileState),
            _ => None,
        }
    }
}

const PREPARED_OBJECTIVE_VALIDATION_FACT: &str = "runtime_objective_validation";

#[derive(Debug, Clone, PartialEq, Eq)]
struct ObjectiveValidationOutcome {
    kind: ObjectiveValidationKind,
    identity: String,
    passed: bool,
    message: String,
}

fn objective_validation_report(report: &ToolExecutionReport) -> Option<ObjectiveValidationOutcome> {
    if report.envelope.tool_name == "external_verifier" {
        return Some(ObjectiveValidationOutcome {
            kind: ObjectiveValidationKind::Test,
            identity: "external-verifier".to_owned(),
            passed: report.envelope.status == ToolResultStatus::Ok,
            message: report.envelope.summary.clone(),
        });
    }
    if report.envelope.tool_name == "shell" {
        if let Some(outcome) = prepared_objective_validation_report(report) {
            return Some(outcome);
        }
        let command = report
            .envelope
            .structured_facts
            .get("command")
            .and_then(serde_json::Value::as_str)?;
        let kind = objective_validation_command_kind(command)?;
        let identity = objective_validation_command_identity(command)?;
        let exited_cleanly = shell_report_exited_cleanly(report);
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
            (ObjectiveValidationKind::FileState, true, true) => {
                "file-state command passed".to_owned()
            }
            (_, true, true) => "diagnostic command passed".to_owned(),
            _ => "objective validation is unresolved".to_owned(),
        };
        return Some(ObjectiveValidationOutcome {
            kind,
            identity,
            passed,
            message,
        });
    }
    None
}

fn prepare_objective_validation_metadata(request: &ToolRequest) -> Option<Value> {
    if request.tool_name != "shell" {
        return None;
    }
    let command = request.arguments.get("command").and_then(Value::as_str)?;
    let kind = objective_validation_command_kind(command)?;
    let identity = safe_objective_validation_command_identity(command)?;
    Some(serde_json::json!({
        "kind": kind.label(),
        "identity": identity,
    }))
}

fn attach_prepared_objective_validation(report: &mut ToolExecutionReport, metadata: Option<Value>) {
    let Some(metadata) = metadata else {
        return;
    };
    if let Some(facts) = report.envelope.structured_facts.as_object_mut() {
        facts.insert(PREPARED_OBJECTIVE_VALIDATION_FACT.to_owned(), metadata);
    }
}

fn prepared_objective_validation_report(
    report: &ToolExecutionReport,
) -> Option<ObjectiveValidationOutcome> {
    let metadata = report
        .envelope
        .structured_facts
        .get(PREPARED_OBJECTIVE_VALIDATION_FACT)?;
    let kind = ObjectiveValidationKind::from_label(metadata.get("kind")?.as_str()?)?;
    let identity = metadata.get("identity")?.as_str()?.to_owned();
    let exited_cleanly = shell_report_exited_cleanly(report);
    let executed_tests =
        kind != ObjectiveValidationKind::Test || test_report_executed_tests(report);
    let passed = exited_cleanly && executed_tests;
    let message = if !exited_cleanly {
        "validation command did not exit successfully".to_owned()
    } else if kind == ObjectiveValidationKind::Test && !executed_tests {
        "test command exited successfully but no executed test was observed".to_owned()
    } else {
        match kind {
            ObjectiveValidationKind::Test => "test command passed with executed tests".to_owned(),
            ObjectiveValidationKind::FileState => "file-state command passed".to_owned(),
            ObjectiveValidationKind::Diagnostic => "diagnostic command passed".to_owned(),
        }
    };
    Some(ObjectiveValidationOutcome {
        kind,
        identity,
        passed,
        message,
    })
}

fn objective_validation_report_in_turn(
    report: &ToolExecutionReport,
    turn_reports: &[ToolExecutionReport],
    report_index: usize,
    workspace_root: &Path,
) -> Option<ObjectiveValidationOutcome> {
    if let Some(verifier) =
        turn_local_python_verifier(report, turn_reports, report_index, workspace_root)
    {
        let passed = shell_report_exited_cleanly(report);
        return Some(ObjectiveValidationOutcome {
            kind: ObjectiveValidationKind::Diagnostic,
            identity: format!(
                "python-file:{}:sha256:{}",
                verifier.relative_path, verifier.source_digest
            ),
            passed,
            message: if passed {
                "turn-local Python verifier passed".to_owned()
            } else {
                "turn-local Python verifier did not exit successfully".to_owned()
            },
        });
    }
    objective_validation_report(report)
}

fn shell_report_exited_cleanly(report: &ToolExecutionReport) -> bool {
    report.envelope.tool_name == "shell"
        && report.envelope.status == ToolResultStatus::Ok
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
            .unwrap_or(false)
}

const MAX_TURN_LOCAL_PYTHON_VERIFIER_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
struct TurnLocalPythonVerifier {
    relative_path: String,
    source_digest: String,
}

fn turn_local_python_verifier(
    report: &ToolExecutionReport,
    turn_reports: &[ToolExecutionReport],
    report_index: usize,
    workspace_root: &Path,
) -> Option<TurnLocalPythonVerifier> {
    if report.envelope.tool_name != "shell" || report_index >= turn_reports.len() {
        return None;
    }
    let command = report
        .envelope
        .structured_facts
        .get("command")
        .and_then(Value::as_str)?;
    let parsed = parse_shell_command_with_input(command)?;
    if parsed.stdin.is_some() || parsed.parts.len() != 2 {
        return None;
    }
    let program = parsed.parts.first().map(String::as_str)?;
    let program = Path::new(program)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(program);
    if !matches!(program, "python" | "python3") {
        return None;
    }
    let requested_path = Path::new(parsed.parts.get(1)?);
    if requested_path
        .extension()
        .and_then(|extension| extension.to_str())
        != Some("py")
    {
        return None;
    }

    let canonical_root = fs::canonicalize(workspace_root).ok()?;
    let candidate = if requested_path.is_absolute() {
        requested_path.to_path_buf()
    } else {
        canonical_root.join(requested_path)
    };
    let metadata = fs::symlink_metadata(&candidate).ok()?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return None;
    }
    let canonical_path = fs::canonicalize(&candidate).ok()?;
    let relative_path = canonical_path.strip_prefix(&canonical_root).ok()?;
    if relative_path.as_os_str().is_empty() {
        return None;
    }

    let changed_before_execution =
        turn_reports[..report_index]
            .iter()
            .rev()
            .find(|candidate_report| {
                candidate_report
                    .changed_files
                    .iter()
                    .any(|path| paths_resolve_to_same_file(path, &canonical_path))
            })?;
    let source_bytes = changed_before_execution
        .after_images
        .iter()
        .find(|image| paths_resolve_to_same_file(&image.path, &canonical_path))?
        .content
        .as_deref()?;
    if source_bytes.len() > MAX_TURN_LOCAL_PYTHON_VERIFIER_BYTES {
        return None;
    }
    if turn_reports[report_index..].iter().any(|candidate_report| {
        candidate_report
            .changed_files
            .iter()
            .any(|path| paths_resolve_to_same_file(path, &canonical_path))
    }) {
        return None;
    }
    let current_bytes = fs::read(&canonical_path).ok()?;
    if current_bytes != source_bytes {
        return None;
    }
    let source = std::str::from_utf8(source_bytes).ok()?.to_owned();
    if !python_source_asserts_runtime_state(&source) {
        return None;
    }

    Some(TurnLocalPythonVerifier {
        relative_path: relative_path.to_string_lossy().replace('\\', "/"),
        source_digest: format!("{:x}", Sha256::digest(source_bytes)),
    })
}

fn paths_resolve_to_same_file(path: &Path, canonical_path: &Path) -> bool {
    path == canonical_path || fs::canonicalize(path).is_ok_and(|path| path == canonical_path)
}

fn explicitly_requested_inspection_validation(
    report: &ToolExecutionReport,
    objective: &str,
    completion_criteria: &[String],
    contract: &TaskContract,
    workspace_root: &Path,
) -> Option<ObjectiveValidationOutcome> {
    if contract.requires_workspace_evidence()
        || contract.require_objective_validation
        || matches!(
            contract.verification,
            VerificationRequirement::Required | VerificationRequirement::Independent
        )
    {
        return None;
    }
    if !matches!(report.envelope.tool_name.as_str(), "read_file" | "list_dir") {
        return None;
    }
    let resource = report
        .envelope
        .structured_facts
        .get("path")
        .and_then(Value::as_str)
        .unwrap_or(&report.policy_evaluation.resource);
    let resource_path = Path::new(resource);
    let relative = resource_path
        .strip_prefix(workspace_root)
        .unwrap_or(resource_path)
        .to_string_lossy()
        .replace('\\', "/");
    if relative.is_empty() || relative == "." {
        return None;
    }
    let requested_text = std::iter::once(objective)
        .chain(completion_criteria.iter().map(String::as_str))
        .collect::<Vec<_>>()
        .join("\n")
        .to_ascii_lowercase();
    let relative_lower = relative.to_ascii_lowercase();
    let file_name = Path::new(&relative)
        .file_name()
        .and_then(|name| name.to_str())
        .map(str::to_ascii_lowercase);
    let explicitly_requested = requested_text.contains(&relative_lower)
        || file_name
            .as_deref()
            .is_some_and(|name| name.len() >= 3 && name != "." && requested_text.contains(name));
    if !explicitly_requested {
        return None;
    }
    let passed = report.envelope.status == ToolResultStatus::Ok;
    Some(ObjectiveValidationOutcome {
        kind: ObjectiveValidationKind::Diagnostic,
        identity: format!("inspection:{:x}", Sha256::digest(relative_lower.as_bytes())),
        passed,
        message: if passed {
            format!("explicitly requested workspace input was inspected: {relative}")
        } else {
            format!("explicitly requested workspace input could not be inspected: {relative}")
        },
    })
}

#[cfg(test)]
fn is_objective_validation_command(command: &str) -> bool {
    objective_validation_command_kind(command).is_some()
}

fn objective_validation_command_kind(command: &str) -> Option<ObjectiveValidationKind> {
    objective_validation_command_kind_with_depth(command, 0)
}

fn objective_validation_command_identity(command: &str) -> Option<String> {
    let atoms = objective_validation_command_atoms_with_depth(command, 0)?;
    if atoms.is_empty() {
        return None;
    }
    let mut digest = Sha256::new();
    for atom in atoms {
        digest.update((atom.len() as u64).to_le_bytes());
        digest.update(atom.as_bytes());
    }
    Some(format!("{:x}", digest.finalize()))
}

fn safe_objective_validation_command_identity(command: &str) -> Option<String> {
    let atoms = objective_validation_command_atoms_with_depth(command, 0)?;
    if atoms.is_empty() {
        return None;
    }
    let mut digest = Sha256::new();
    for atom in atoms {
        let redacted = redact_sensitive_text(&atom).0;
        digest.update((redacted.len() as u64).to_le_bytes());
        digest.update(redacted.as_bytes());
    }
    Some(format!("{:x}", digest.finalize()))
}

fn objective_validation_command_atoms_with_depth(
    command: &str,
    wrapper_depth: u8,
) -> Option<Vec<String>> {
    let parsed = parse_shell_command_with_input(command)?;
    let mut parts = parsed.parts;
    let program = parts.first().map(String::as_str)?;
    let program = Path::new(program)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(program)
        .to_owned();
    if wrapper_depth < 2
        && matches!(program.as_str(), "bash" | "sh" | "zsh")
        && parts.len() == 3
        && matches!(parts[1].as_str(), "-c" | "-lc")
    {
        return objective_validation_shell_script_atoms(
            parts[2].trim(),
            wrapper_depth.saturating_add(1),
        );
    }
    if let Some(stdin) = parsed.stdin {
        objective_validation_command_kind_with_depth(command, wrapper_depth)?;
        return Some(vec![
            serde_json::to_string(&(program, "stdin", stdin)).ok()?,
        ]);
    }
    objective_validation_command_kind_with_depth(command, wrapper_depth)?;
    parts[0] = program;
    Some(vec![serde_json::to_string(&parts).ok()?])
}

fn objective_validation_shell_script_atoms(script: &str, wrapper_depth: u8) -> Option<Vec<String>> {
    objective_validation_shell_script_kind(script, wrapper_depth)?;
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_bash::LANGUAGE.into())
        .ok()?;
    let tree = parser.parse(script, None)?;
    let root = tree.root_node();
    let mut atoms = Vec::new();
    if !collect_objective_validation_atoms(root, script.as_bytes(), wrapper_depth, &mut atoms)
        || atoms.is_empty()
    {
        return None;
    }
    Some(atoms)
}

fn collect_objective_validation_atoms(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    wrapper_depth: u8,
    atoms: &mut Vec<String>,
) -> bool {
    match node.kind() {
        "program" | "list" => {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                if !collect_objective_validation_atoms(child, source, wrapper_depth, atoms) {
                    return false;
                }
            }
            true
        }
        "command" => {
            let Ok(command) = node.utf8_text(source) else {
                return false;
            };
            if let Some(mut command_atoms) =
                objective_validation_command_atoms_with_depth(command.trim(), wrapper_depth)
            {
                atoms.append(&mut command_atoms);
            }
            true
        }
        "redirected_statement" => {
            if let Some((_, atom)) =
                objective_validation_python_heredoc(node, source, wrapper_depth)
            {
                atoms.push(atom);
            }
            true
        }
        "pipeline" | "test_command" => {
            if objective_validation_statement_kind(node, source, wrapper_depth).is_some() {
                let Ok(statement) = node.utf8_text(source) else {
                    return false;
                };
                let Ok(atom) = serde_json::to_string(&(node.kind(), statement.trim())) else {
                    return false;
                };
                atoms.push(atom);
            }
            true
        }
        "comment" | "variable_assignment" => true,
        _ => false,
    }
}

fn objective_validation_command_kind_with_depth(
    command: &str,
    wrapper_depth: u8,
) -> Option<ObjectiveValidationKind> {
    let parsed = parse_shell_command_with_input(command)?;
    let parts = parsed.parts;
    let program = parts.first().map(String::as_str)?;
    let program = Path::new(program)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(program);
    if wrapper_depth < 2
        && matches!(program, "bash" | "sh" | "zsh")
        && parts.len() == 3
        && matches!(parts[1].as_str(), "-c" | "-lc")
    {
        return objective_validation_shell_script_kind(
            parts[2].trim(),
            wrapper_depth.saturating_add(1),
        );
    }
    if let Some(stdin) = parsed.stdin {
        return (matches!(program, "python" | "python3")
            && parts.get(1).map(String::as_str) == Some("-")
            && parts.len() == 2
            && python_source_asserts_runtime_state(&stdin))
        .then_some(ObjectiveValidationKind::Diagnostic);
    }
    match program {
        "cargo" if parts.iter().any(|part| part == "test") => Some(ObjectiveValidationKind::Test),
        "cargo"
            if parts
                .iter()
                .any(|part| matches!(part.as_str(), "check" | "clippy" | "build")) =>
        {
            Some(ObjectiveValidationKind::Diagnostic)
        }
        "cargo"
            if parts.iter().any(|part| part == "fmt")
                && parts.iter().any(|part| part == "--check") =>
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
        "python" | "python3" if python_module_runs_tests(&parts) => {
            Some(ObjectiveValidationKind::Test)
        }
        "python" | "python3" if python_inline_asserts_runtime_state(&parts) => {
            Some(ObjectiveValidationKind::Diagnostic)
        }
        "curl" if curl_is_fail_fast_http_probe(&parts) => Some(ObjectiveValidationKind::Diagnostic),
        "cmp" | "diff" if comparison_has_two_operands(&parts) => {
            Some(ObjectiveValidationKind::Diagnostic)
        }
        "test" if test_command_validates_file_state(&parts) => {
            Some(ObjectiveValidationKind::FileState)
        }
        "test" if test_command_validates_runtime_comparison(&parts) => {
            Some(ObjectiveValidationKind::Diagnostic)
        }
        "grep" | "rg" if quiet_content_check(&parts, false) => {
            Some(ObjectiveValidationKind::Diagnostic)
        }
        "git" if git_command_validates_result(&parts) => Some(ObjectiveValidationKind::Diagnostic),
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

fn curl_is_fail_fast_http_probe(parts: &[String]) -> bool {
    let fail_fast = parts.iter().skip(1).any(|part| {
        matches!(part.as_str(), "--fail" | "--fail-with-body")
            || (part.starts_with('-')
                && !part.starts_with("--")
                && part.chars().skip(1).any(|flag| flag == 'f'))
    });
    fail_fast
        && parts
            .iter()
            .skip(1)
            .any(|part| part.starts_with("http://") || part.starts_with("https://"))
}

fn objective_validation_shell_script_kind(
    script: &str,
    wrapper_depth: u8,
) -> Option<ObjectiveValidationKind> {
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_bash::LANGUAGE.into())
        .ok()?;
    let tree = parser.parse(script, None)?;
    let root = tree.root_node();
    if root.has_error() {
        return None;
    }

    let source = script.as_bytes();
    if root.named_child_count() == 1 {
        let statement = root.named_child(0)?;
        if let Some((kind, _)) =
            objective_validation_python_heredoc(statement, source, wrapper_depth)
        {
            return Some(kind);
        }
        if statement.kind() == "list" {
            return objective_validation_and_chain_kind(statement, source, wrapper_depth);
        }
        if statement.kind() == "command" && !shell_script_has_unsafe_control_flow(statement) {
            let command = statement.utf8_text(source).ok()?.trim();
            let parts = shlex::split(command)?;
            if shell_command_can_change_or_skip_validation(&parts) {
                return None;
            }
            return objective_validation_command_kind_with_depth(command, wrapper_depth);
        }
    }
    let mut validation = None;
    if collect_fail_fast_validation(root, source, wrapper_depth, &mut validation)
        && validation.is_some()
    {
        return validation;
    }
    collect_terminal_statement_validation(root, source, wrapper_depth)
}

fn collect_fail_fast_validation(
    root: tree_sitter::Node<'_>,
    source: &[u8],
    wrapper_depth: u8,
    validation: &mut Option<ObjectiveValidationKind>,
) -> bool {
    let mut fail_fast = false;
    collect_fail_fast_nodes(root, source, wrapper_depth, &mut fail_fast, validation) && fail_fast
}

fn collect_fail_fast_nodes(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    wrapper_depth: u8,
    fail_fast: &mut bool,
    validation: &mut Option<ObjectiveValidationKind>,
) -> bool {
    if matches!(node.kind(), "program" | "list") {
        for index in 0..node.child_count() {
            let Some(child) = node.child(index) else {
                return false;
            };
            if child.is_named() {
                if !collect_fail_fast_nodes(child, source, wrapper_depth, fail_fast, validation) {
                    return false;
                }
                continue;
            }
            let Ok(operator) = child.utf8_text(source) else {
                return false;
            };
            if !matches!(operator.trim(), "" | ";" | "&&") {
                return false;
            }
        }
        return true;
    }

    match node.kind() {
        "comment" => true,
        "command" => {
            let Ok(command) = node.utf8_text(source) else {
                return false;
            };
            let Some(parts) = shlex::split(command.trim()) else {
                return false;
            };
            if !*fail_fast {
                if !shell_command_enables_errexit(&parts) {
                    return false;
                }
                *fail_fast = true;
                return true;
            }
            if shell_command_can_change_or_skip_validation(&parts) {
                return false;
            }
            if let Some(kind) = objective_validation_statement_kind(node, source, wrapper_depth) {
                *validation = Some(stronger_validation_kind(*validation, kind));
                true
            } else {
                validation.is_none() || shell_statement_is_read_only(node, source)
            }
        }
        "variable_assignment" => *fail_fast && shell_assignment_is_safe(node, source),
        "test_command" if *fail_fast => {
            if let Some(kind) = objective_validation_statement_kind(node, source, wrapper_depth) {
                *validation = Some(stronger_validation_kind(*validation, kind));
            }
            true
        }
        "redirected_statement" if *fail_fast => {
            if let Some(kind) = objective_validation_statement_kind(node, source, wrapper_depth) {
                *validation = Some(stronger_validation_kind(*validation, kind));
                true
            } else {
                validation.is_none() && shell_setup_statement_is_allowed(node, source)
            }
        }
        "pipeline" if *fail_fast => {
            if let Some(kind) = objective_validation_statement_kind(node, source, wrapper_depth) {
                *validation = Some(stronger_validation_kind(*validation, kind));
                true
            } else if validation.is_some() {
                shell_statement_is_read_only(node, source)
            } else {
                shell_setup_statement_is_allowed(node, source)
            }
        }
        _ => false,
    }
}

fn collect_terminal_statement_validation(
    root: tree_sitter::Node<'_>,
    source: &[u8],
    wrapper_depth: u8,
) -> Option<ObjectiveValidationKind> {
    for index in 0..root.child_count() {
        let child = root.child(index)?;
        if !child.is_named()
            && !child
                .utf8_text(source)
                .is_ok_and(|operator| matches!(operator.trim(), "" | ";"))
        {
            return None;
        }
    }
    let mut cursor = root.walk();
    let statements = root
        .named_children(&mut cursor)
        .filter(|node| node.kind() != "comment")
        .collect::<Vec<_>>();
    let (last, setup) = statements.split_last()?;
    if setup
        .iter()
        .any(|node| !shell_terminal_setup_statement_is_allowed(*node, source))
    {
        return None;
    }
    objective_validation_statement_kind(*last, source, wrapper_depth)
}

fn shell_terminal_setup_statement_is_allowed(node: tree_sitter::Node<'_>, source: &[u8]) -> bool {
    match node.kind() {
        "comment" => true,
        "variable_assignment" => shell_assignment_is_safe(node, source),
        "command" | "pipeline" => shell_statement_is_read_only(node, source),
        "redirected_statement" => node
            .child_by_field_name("body")
            .is_some_and(|body| shell_setup_statement_is_allowed(body, source)),
        _ => false,
    }
}

fn objective_validation_statement_kind(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    wrapper_depth: u8,
) -> Option<ObjectiveValidationKind> {
    match node.kind() {
        "command" if !shell_script_has_unsafe_control_flow(node) => {
            objective_validation_command_kind_with_depth(
                node.utf8_text(source).ok()?.trim(),
                wrapper_depth,
            )
        }
        "redirected_statement" => {
            objective_validation_python_heredoc(node, source, wrapper_depth).map(|(kind, _)| kind)
        }
        "pipeline" => objective_validation_pipeline_kind(node, source),
        "test_command" => objective_validation_test_node_kind(node, source),
        _ => None,
    }
}

fn objective_validation_pipeline_kind(
    pipeline: tree_sitter::Node<'_>,
    source: &[u8],
) -> Option<ObjectiveValidationKind> {
    let mut cursor = pipeline.walk();
    let commands = pipeline
        .named_children(&mut cursor)
        .filter(|child| child.kind() == "command")
        .collect::<Vec<_>>();
    let (last, inputs) = commands.split_last()?;
    if inputs.is_empty()
        || inputs
            .iter()
            .any(|command| !shell_statement_is_read_only(*command, source))
    {
        return None;
    }
    let parts = shlex::split(last.utf8_text(source).ok()?.trim())?;
    quiet_content_check(&parts, true).then_some(ObjectiveValidationKind::Diagnostic)
}

fn objective_validation_test_node_kind(
    node: tree_sitter::Node<'_>,
    source: &[u8],
) -> Option<ObjectiveValidationKind> {
    let text = node.utf8_text(source).ok()?.trim();
    let inner = text
        .strip_prefix("[[")
        .and_then(|value| value.strip_suffix("]]"))
        .or_else(|| {
            text.strip_prefix('[')
                .and_then(|value| value.strip_suffix(']'))
        })?;
    let mut parts = vec!["test".to_owned()];
    parts.extend(shlex::split(inner.trim())?);
    if test_command_validates_file_state(&parts) {
        Some(ObjectiveValidationKind::FileState)
    } else if test_command_validates_runtime_comparison(&parts) {
        Some(ObjectiveValidationKind::Diagnostic)
    } else {
        None
    }
}

fn shell_setup_statement_is_allowed(node: tree_sitter::Node<'_>, source: &[u8]) -> bool {
    match node.kind() {
        "comment" => true,
        "variable_assignment" => shell_assignment_is_safe(node, source),
        "command" => shlex::split(node.utf8_text(source).unwrap_or_default().trim())
            .is_some_and(|parts| !shell_command_can_change_or_skip_validation(&parts)),
        "redirected_statement" => node
            .child_by_field_name("body")
            .is_some_and(|body| shell_setup_statement_is_allowed(body, source)),
        "pipeline" => {
            let mut cursor = node.walk();
            let commands = node.named_children(&mut cursor).collect::<Vec<_>>();
            !commands.is_empty()
                && commands
                    .iter()
                    .all(|command| shell_setup_statement_is_allowed(*command, source))
        }
        _ => false,
    }
}

fn shell_statement_is_read_only(node: tree_sitter::Node<'_>, source: &[u8]) -> bool {
    match node.kind() {
        "command" => shlex::split(node.utf8_text(source).unwrap_or_default().trim())
            .is_some_and(|parts| shell_command_is_read_only(&parts)),
        "pipeline" => {
            let mut cursor = node.walk();
            let commands = node.named_children(&mut cursor).collect::<Vec<_>>();
            !commands.is_empty()
                && commands
                    .iter()
                    .all(|command| shell_statement_is_read_only(*command, source))
        }
        _ => false,
    }
}

fn shell_command_is_read_only(parts: &[String]) -> bool {
    let Some(program) = parts.first().and_then(|program| {
        Path::new(program)
            .file_name()
            .and_then(|name| name.to_str())
    }) else {
        return false;
    };
    match program {
        "cat" | "cut" | "du" | "file" | "grep" | "head" | "ls" | "printf" | "pwd" | "readlink"
        | "rg" | "sort" | "stat" | "strings" | "tail" | "tr" | "uniq" | "wc" => true,
        "find" => !parts.iter().any(|part| {
            matches!(
                part.as_str(),
                "-delete" | "-exec" | "-execdir" | "-ok" | "-okdir"
            )
        }),
        "git" => match parts.get(1).map(String::as_str) {
            Some("branch") => parts.iter().skip(2).all(|part| part == "--show-current"),
            Some("diff" | "log" | "merge-base" | "rev-parse" | "show" | "status") => true,
            _ => false,
        },
        "tmux" => match parts.get(1).map(String::as_str) {
            Some("capture-pane") => {
                parts.iter().any(|part| part == "-p") && !parts.iter().any(|part| part == "-b")
            }
            Some("display-message") => parts.iter().any(|part| part == "-p"),
            Some(
                "has-session"
                | "list-buffers"
                | "list-clients"
                | "list-commands"
                | "list-keys"
                | "list-panes"
                | "list-sessions"
                | "list-windows"
                | "server-info"
                | "show-environment"
                | "show-hooks"
                | "show-messages"
                | "show-options"
                | "show-window-options",
            ) => true,
            _ => false,
        },
        _ => false,
    }
}

fn objective_validation_python_heredoc(
    statement: tree_sitter::Node<'_>,
    source: &[u8],
    wrapper_depth: u8,
) -> Option<(ObjectiveValidationKind, String)> {
    let (program, python_source) =
        objective_validation_python_heredoc_source(statement, source, wrapper_depth)?;
    if !python_source_asserts_runtime_state(python_source) {
        return None;
    }
    let atom = serde_json::to_string(&(program, "stdin", python_source)).ok()?;
    Some((ObjectiveValidationKind::Diagnostic, atom))
}

fn objective_validation_python_heredoc_source<'a>(
    statement: tree_sitter::Node<'_>,
    source: &'a [u8],
    wrapper_depth: u8,
) -> Option<(String, &'a str)> {
    if statement.kind() != "redirected_statement" {
        return None;
    }
    let body = statement.child_by_field_name("body")?;
    let statement_text = statement.utf8_text(source).ok()?;
    let command_prefix = statement_text
        .lines()
        .find_map(|line| line.split_once("<<").map(|(command, _)| command.trim()))?;
    let command = match body.kind() {
        "command" => command_prefix,
        "list" => {
            let mut validation = None;
            if !collect_validation_and_chain(body, source, wrapper_depth, &mut validation)
                && !collect_fail_fast_validation(body, source, wrapper_depth, &mut validation)
            {
                return None;
            }
            command_prefix
                .rsplit_once("&&")
                .map_or(command_prefix, |(_, command)| command.trim())
        }
        _ => return None,
    };
    let parts = shlex::split(command)?;
    let program = parts.first().map(String::as_str)?;
    let program = Path::new(program)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(program)
        .to_owned();
    if !matches!(program.as_str(), "python" | "python3")
        || parts.get(1).map(String::as_str) != Some("-")
        || parts.len() != 2
    {
        return None;
    }

    let mut cursor = statement.walk();
    let redirects = statement
        .named_children(&mut cursor)
        .filter(|child| child.kind() == "heredoc_redirect")
        .collect::<Vec<_>>();
    let [redirect] = redirects.as_slice() else {
        return None;
    };
    let mut bodies = Vec::new();
    let mut nodes = vec![*redirect];
    while let Some(node) = nodes.pop() {
        if matches!(
            node.kind(),
            "command_substitution" | "expansion" | "simple_expansion"
        ) {
            return None;
        }
        if node.kind() == "heredoc_body" {
            bodies.push(node);
            continue;
        }
        let mut cursor = node.walk();
        nodes.extend(node.named_children(&mut cursor));
    }
    let [python_body] = bodies.as_slice() else {
        return None;
    };
    Some((program, python_body.utf8_text(source).ok()?))
}

fn objective_validation_and_chain_kind(
    list: tree_sitter::Node<'_>,
    source: &[u8],
    wrapper_depth: u8,
) -> Option<ObjectiveValidationKind> {
    let mut validation = None;
    if !collect_validation_and_chain(list, source, wrapper_depth, &mut validation) {
        return None;
    }
    validation
}

fn collect_validation_and_chain(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    wrapper_depth: u8,
    validation: &mut Option<ObjectiveValidationKind>,
) -> bool {
    match node.kind() {
        "list" => {
            for index in 0..node.child_count() {
                let Some(child) = node.child(index) else {
                    return false;
                };
                if child.is_named() {
                    if !collect_validation_and_chain(child, source, wrapper_depth, validation) {
                        return false;
                    }
                } else if child
                    .utf8_text(source)
                    .is_ok_and(|operator| operator.trim() != "&&")
                {
                    return false;
                }
            }
            true
        }
        "command" if !shell_script_has_unsafe_control_flow(node) => {
            let Ok(command) = node.utf8_text(source) else {
                return false;
            };
            let Some(parts) = shlex::split(command.trim()) else {
                return false;
            };
            if shell_command_can_change_or_skip_validation(&parts) {
                return false;
            }
            if let Some(kind) =
                objective_validation_command_kind_with_depth(command.trim(), wrapper_depth)
            {
                *validation = Some(stronger_validation_kind(*validation, kind));
            }
            true
        }
        "redirected_statement" => {
            let Some((kind, _)) = objective_validation_python_heredoc(node, source, wrapper_depth)
            else {
                return false;
            };
            *validation = Some(stronger_validation_kind(*validation, kind));
            true
        }
        "test_command" => true,
        _ => false,
    }
}

fn shell_script_has_unsafe_control_flow(root: tree_sitter::Node<'_>) -> bool {
    let mut nodes = vec![root];
    while let Some(node) = nodes.pop() {
        if matches!(
            node.kind(),
            "case"
                | "case_statement"
                | "compound_statement"
                | "c_style_for_statement"
                | "file_redirect"
                | "for"
                | "for_statement"
                | "function"
                | "function_definition"
                | "heredoc_redirect"
                | "herestring_redirect"
                | "if"
                | "if_statement"
                | "list"
                | "negated_command"
                | "pipeline"
                | "process_substitution"
                | "redirected_statement"
                | "subshell"
                | "until_statement"
                | "while"
                | "while_statement"
        ) {
            return true;
        }
        let mut cursor = node.walk();
        nodes.extend(node.named_children(&mut cursor));
    }
    false
}

fn shell_command_enables_errexit(parts: &[String]) -> bool {
    if parts.first().map(String::as_str) != Some("set") {
        return false;
    }
    let mut enables_errexit = false;
    let mut index = 1;
    while let Some(part) = parts.get(index) {
        if part == "+e" || (part.starts_with('+') && part[1..].contains('e')) {
            return false;
        }
        if part == "-o" && parts.get(index + 1).map(String::as_str) == Some("errexit") {
            enables_errexit = true;
            index += 2;
            continue;
        }
        if part == "-e" || (part.starts_with('-') && part[1..].contains('e')) {
            enables_errexit = true;
        }
        index += 1;
    }
    enables_errexit
}

fn shell_command_can_change_or_skip_validation(parts: &[String]) -> bool {
    let Some(program) = parts.first().map(String::as_str) else {
        return true;
    };
    matches!(
        Path::new(program)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(program),
        "." | "alias"
            | "bash"
            | "break"
            | "builtin"
            | "command"
            | "continue"
            | "enable"
            | "eval"
            | "exec"
            | "exit"
            | "hash"
            | "return"
            | "set"
            | "sh"
            | "source"
            | "trap"
            | "unalias"
            | "zsh"
    )
}

fn shell_assignment_is_safe(node: tree_sitter::Node<'_>, source: &[u8]) -> bool {
    let Some(name) = node.child_by_field_name("name") else {
        return false;
    };
    let Ok(name) = name.utf8_text(source) else {
        return false;
    };
    !matches!(
        name,
        "BASHOPTS" | "BASH_ENV" | "CDPATH" | "ENV" | "GIT_EXEC_PATH" | "IFS" | "PATH" | "SHELLOPTS"
    ) && !name.starts_with("GIT_CONFIG_")
}

const fn stronger_validation_kind(
    current: Option<ObjectiveValidationKind>,
    candidate: ObjectiveValidationKind,
) -> ObjectiveValidationKind {
    match (current, candidate) {
        (Some(ObjectiveValidationKind::Test), _) | (_, ObjectiveValidationKind::Test) => {
            ObjectiveValidationKind::Test
        }
        (Some(ObjectiveValidationKind::Diagnostic), _)
        | (_, ObjectiveValidationKind::Diagnostic) => ObjectiveValidationKind::Diagnostic,
        _ => ObjectiveValidationKind::FileState,
    }
}

fn test_command_validates_file_state(parts: &[String]) -> bool {
    parts.get(1).is_some_and(|argument| {
        matches!(
            argument.as_str(),
            "-b" | "-c"
                | "-d"
                | "-e"
                | "-f"
                | "-g"
                | "-h"
                | "-L"
                | "-p"
                | "-r"
                | "-s"
                | "-S"
                | "-u"
                | "-w"
                | "-x"
        )
    }) && parts.get(2).is_some_and(|path| !path.is_empty())
}

fn test_command_validates_runtime_comparison(parts: &[String]) -> bool {
    match parts {
        [program, operator, operand]
            if program == "test"
                && matches!(operator.as_str(), "-n" | "-z")
                && shell_operand_depends_on_runtime(operand) =>
        {
            true
        }
        [program, left, operator, right] => {
            program == "test"
                && matches!(
                    operator.as_str(),
                    "-eq" | "-ne" | "-gt" | "-ge" | "-lt" | "-le"
                )
                && left != right
                && (shell_operand_depends_on_runtime(left)
                    || shell_operand_depends_on_runtime(right))
        }
        _ => false,
    }
}

fn shell_operand_depends_on_runtime(operand: &str) -> bool {
    operand.contains('$') || operand.contains('`')
}

fn quiet_content_check(parts: &[String], piped_input: bool) -> bool {
    let Some(program) = parts.first().and_then(|program| {
        Path::new(program)
            .file_name()
            .and_then(|name| name.to_str())
    }) else {
        return false;
    };
    if !matches!(program, "grep" | "rg") {
        return false;
    }
    let quiet = parts.iter().skip(1).any(|part| {
        matches!(part.as_str(), "--quiet" | "--silent")
            || (part.starts_with('-')
                && !part.starts_with("--")
                && part.chars().skip(1).any(|option| option == 'q'))
    });
    if !quiet {
        return false;
    }
    let operands = parts
        .iter()
        .skip(1)
        .filter(|part| !part.starts_with('-'))
        .count();
    operands >= if piped_input { 1 } else { 2 }
}

fn python_module_runs_tests(parts: &[String]) -> bool {
    parts
        .windows(2)
        .any(|window| window[0] == "-m" && matches!(window[1].as_str(), "pytest" | "unittest"))
}

fn python_inline_asserts_runtime_state(parts: &[String]) -> bool {
    let Some(command_index) = parts.iter().position(|part| part == "-c") else {
        return false;
    };
    if parts
        .iter()
        .take(command_index)
        .skip(1)
        .any(|part| matches!(part.as_str(), "-O" | "-OO"))
    {
        return false;
    }
    let Some(source) = parts.get(command_index.saturating_add(1)) else {
        return false;
    };
    python_source_asserts_runtime_state(source)
}

fn python_source_asserts_runtime_state(source: &str) -> bool {
    let mut parser = tree_sitter::Parser::new();
    if parser
        .set_language(&tree_sitter_python::LANGUAGE.into())
        .is_err()
    {
        return false;
    }
    let Some(tree) = parser.parse(source, None) else {
        return false;
    };
    let root = tree.root_node();
    if root.has_error() {
        return false;
    }
    let source = source.as_bytes();
    let mut runtime_bindings = HashSet::new();
    let mut cursor = root.walk();
    for statement in root.named_children(&mut cursor) {
        if let Some((target, value, augmented)) = python_assignment_parts(statement) {
            let depends_on_runtime =
                python_expression_depends_on_runtime_state(value, source, &runtime_bindings)
                    || (augmented
                        && python_expression_depends_on_runtime_state(
                            target,
                            source,
                            &runtime_bindings,
                        ));
            update_python_runtime_bindings(
                target,
                source,
                depends_on_runtime,
                &mut runtime_bindings,
            );
            continue;
        }
        if statement.kind() == "assert_statement"
            && statement.named_child(0).is_some_and(|assertion| {
                python_assertion_is_runtime_check(assertion, source, &runtime_bindings)
            })
        {
            return true;
        }
        if statement.kind() == "if_statement"
            && python_if_has_runtime_failure(statement, source, &runtime_bindings)
        {
            return true;
        }
    }
    false
}

fn python_assignment_parts(
    statement: tree_sitter::Node<'_>,
) -> Option<(tree_sitter::Node<'_>, tree_sitter::Node<'_>, bool)> {
    let assignment = if statement.kind() == "expression_statement" {
        statement.named_child(0)?
    } else {
        statement
    };
    if !matches!(assignment.kind(), "assignment" | "augmented_assignment") {
        return None;
    }
    Some((
        assignment.child_by_field_name("left")?,
        assignment.child_by_field_name("right")?,
        assignment.kind() == "augmented_assignment",
    ))
}

fn update_python_runtime_bindings(
    target: tree_sitter::Node<'_>,
    source: &[u8],
    depends_on_runtime: bool,
    runtime_bindings: &mut HashSet<String>,
) {
    if target.kind() == "identifier" {
        if let Ok(identifier) = target.utf8_text(source) {
            if depends_on_runtime {
                runtime_bindings.insert(identifier.to_owned());
            } else {
                runtime_bindings.remove(identifier);
            }
        }
        return;
    }
    if matches!(
        target.kind(),
        "list" | "list_pattern" | "pattern_list" | "tuple" | "tuple_pattern"
    ) {
        let mut cursor = target.walk();
        for child in target.named_children(&mut cursor) {
            update_python_runtime_bindings(child, source, depends_on_runtime, runtime_bindings);
        }
    }
}

fn python_assertion_is_runtime_check(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    runtime_bindings: &HashSet<String>,
) -> bool {
    match python_constant_truthiness(node, source) {
        Some(true) => false,
        Some(false) => true,
        None => python_expression_depends_on_runtime_state(node, source, runtime_bindings),
    }
}

fn python_if_has_runtime_failure(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    runtime_bindings: &HashSet<String>,
) -> bool {
    let Some(condition) = node.child_by_field_name("condition") else {
        return false;
    };
    if python_constant_truthiness(condition, source).is_some()
        || !python_expression_depends_on_runtime_state(condition, source, runtime_bindings)
    {
        return false;
    }
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .filter(|child| child.id() != condition.id())
        .any(|child| python_suite_has_failure(child, source, runtime_bindings))
}

fn python_expression_depends_on_runtime_state(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    runtime_bindings: &HashSet<String>,
) -> bool {
    match node.kind() {
        "true" | "false" | "none" | "integer" | "float" | "string" => false,
        "identifier" => node
            .utf8_text(source)
            .is_ok_and(|identifier| runtime_bindings.contains(identifier)),
        "attribute" => {
            node.utf8_text(source)
                .is_ok_and(python_attribute_reads_runtime_state)
                || node.child_by_field_name("object").is_some_and(|object| {
                    python_expression_depends_on_runtime_state(object, source, runtime_bindings)
                })
        }
        "subscript" => {
            let mut cursor = node.walk();
            node.named_children(&mut cursor).any(|child| {
                python_expression_depends_on_runtime_state(child, source, runtime_bindings)
            })
        }
        "call" => {
            let Some(function_node) = node.child_by_field_name("function") else {
                return false;
            };
            if python_expression_depends_on_runtime_state(function_node, source, runtime_bindings) {
                return true;
            }
            let Some(arguments) = node.child_by_field_name("arguments") else {
                return false;
            };
            let mut cursor = arguments.walk();
            let arguments_depend_on_runtime =
                arguments.named_children(&mut cursor).any(|argument| {
                    python_expression_depends_on_runtime_state(argument, source, runtime_bindings)
                });
            arguments_depend_on_runtime
                || function_node.utf8_text(source).is_ok_and(|function| {
                    python_call_reads_runtime_state(function, arguments, source)
                })
        }
        "class_definition" | "function_definition" | "lambda" => false,
        _ => {
            let mut cursor = node.walk();
            node.named_children(&mut cursor).any(|child| {
                python_expression_depends_on_runtime_state(child, source, runtime_bindings)
            })
        }
    }
}

fn python_attribute_reads_runtime_state(attribute: &str) -> bool {
    matches!(attribute, "os.environ")
}

fn python_call_reads_runtime_state(
    function: &str,
    arguments: tree_sitter::Node<'_>,
    source: &[u8],
) -> bool {
    if matches!(
        function,
        "input"
            | "open"
            | "os.access"
            | "os.getcwd"
            | "os.getenv"
            | "os.listdir"
            | "os.lstat"
            | "os.readlink"
            | "os.scandir"
            | "os.stat"
            | "os.walk"
            | "os.path.exists"
            | "os.path.getsize"
            | "os.path.isdir"
            | "os.path.isfile"
            | "socket.create_connection"
            | "socket.getaddrinfo"
            | "urllib.request.urlopen"
    ) || matches!(
        function,
        "subprocess.call"
            | "subprocess.check_call"
            | "subprocess.check_output"
            | "subprocess.Popen"
            | "subprocess.run"
            | "requests.delete"
            | "requests.get"
            | "requests.head"
            | "requests.options"
            | "requests.patch"
            | "requests.post"
            | "requests.put"
            | "requests.request"
            | "httpx.delete"
            | "httpx.get"
            | "httpx.head"
            | "httpx.options"
            | "httpx.patch"
            | "httpx.post"
            | "httpx.put"
            | "httpx.request"
    ) {
        return true;
    }

    let leaf = function.rsplit('.').next().unwrap_or(function);
    if matches!(
        leaf,
        "exists"
            | "glob"
            | "is_dir"
            | "is_file"
            | "iterdir"
            | "lstat"
            | "read_bytes"
            | "read_text"
            | "readlink"
            | "recv"
            | "rglob"
            | "samefile"
            | "stat"
    ) {
        return true;
    }

    let resource_consumer = matches!(leaf, "connect" | "load" | "open")
        || leaf.starts_with("load_")
        || leaf.starts_with("open_")
        || leaf.starts_with("read_");
    resource_consumer && python_node_names_external_resource(arguments, source)
}

fn python_node_names_external_resource(node: tree_sitter::Node<'_>, source: &[u8]) -> bool {
    if node.kind() == "string"
        && node
            .utf8_text(source)
            .is_ok_and(python_string_names_external_resource)
    {
        return true;
    }
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .any(|child| python_node_names_external_resource(child, source))
}

fn python_string_names_external_resource(raw: &str) -> bool {
    let value = raw
        .trim_start_matches(|character: char| {
            matches!(character.to_ascii_lowercase(), 'b' | 'f' | 'r' | 'u')
        })
        .trim_matches(['\'', '"']);
    value.contains("://")
        || value.contains(['/', '\\'])
        || value.rsplit_once('.').is_some_and(|(stem, extension)| {
            !stem.is_empty()
                && !extension.is_empty()
                && extension.len() <= 16
                && extension
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric() || character == '_')
        })
}

fn python_suite_has_failure(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    runtime_bindings: &HashSet<String>,
) -> bool {
    match node.kind() {
        "raise_statement" => return python_raise_is_failure(node, source),
        "call" => return python_call_exits_nonzero(node, source),
        "if_statement" => {
            return python_if_has_runtime_failure(node, source, runtime_bindings);
        }
        "class_definition"
        | "for_statement"
        | "function_definition"
        | "lambda"
        | "try_statement"
        | "while_statement" => return false,
        _ => {}
    }
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .any(|child| python_suite_has_failure(child, source, runtime_bindings))
}

fn python_raise_is_failure(node: tree_sitter::Node<'_>, source: &[u8]) -> bool {
    let Some(expression) = node.named_child(0) else {
        return true;
    };
    if expression.kind() != "call" {
        return true;
    }
    let function = expression
        .child_by_field_name("function")
        .and_then(|function| function.utf8_text(source).ok())
        .unwrap_or_default();
    function != "SystemExit" || python_call_exits_nonzero(expression, source)
}

fn python_call_exits_nonzero(node: tree_sitter::Node<'_>, source: &[u8]) -> bool {
    let function = node
        .child_by_field_name("function")
        .and_then(|function| function.utf8_text(source).ok())
        .unwrap_or_default();
    if !matches!(function, "exit" | "quit" | "sys.exit" | "SystemExit") {
        return false;
    }
    let Some(arguments) = node.child_by_field_name("arguments") else {
        return false;
    };
    let mut cursor = arguments.walk();
    let values = arguments.named_children(&mut cursor).collect::<Vec<_>>();
    let [value] = values.as_slice() else {
        return false;
    };
    match python_constant_truthiness(*value, source) {
        Some(true) => true,
        Some(false) | None => false,
    }
}

fn python_constant_truthiness(node: tree_sitter::Node<'_>, source: &[u8]) -> Option<bool> {
    match node.kind() {
        "true" => Some(true),
        "false" | "none" => Some(false),
        "parenthesized_expression" => python_constant_truthiness(node.named_child(0)?, source),
        "integer" | "float" => node
            .utf8_text(source)
            .ok()?
            .replace('_', "")
            .parse::<f64>()
            .ok()
            .map(|value| value != 0.0),
        "string" => {
            let text = node.utf8_text(source).ok()?;
            Some(!matches!(text, "''" | "\"\"" | "''''''" | "\"\"\"\"\"\""))
        }
        "list" | "dictionary" | "set" | "tuple" => Some(node.named_child_count() > 0),
        "unary_operator" => node
            .utf8_text(source)
            .ok()?
            .replace('_', "")
            .parse::<f64>()
            .ok()
            .map(|value| value != 0.0),
        _ => None,
    }
}

fn comparison_has_two_operands(parts: &[String]) -> bool {
    parts
        .iter()
        .skip(1)
        .filter(|part| !part.starts_with('-'))
        .take(2)
        .count()
        == 2
}

fn git_command_validates_result(parts: &[String]) -> bool {
    if parts.get(1).is_some_and(|part| part == "merge-base")
        && parts.get(2).is_some_and(|part| part == "--is-ancestor")
    {
        return parts.len() >= 5;
    }
    if parts.get(1).is_none_or(|part| part != "diff")
        || !parts
            .iter()
            .any(|part| matches!(part.as_str(), "--exit-code" | "--quiet"))
    {
        return false;
    }
    let revisions = parts
        .iter()
        .skip(2)
        .take_while(|part| part.as_str() != "--")
        .filter(|part| !part.starts_with('-'))
        .collect::<Vec<_>>();
    let has_explicit_pathspec = parts
        .iter()
        .position(|part| part == "--")
        .is_some_and(|separator| separator + 1 < parts.len());
    revisions.len() >= 2
        || revisions.iter().any(|revision| revision.contains(".."))
        || (revisions.len() == 1 && has_explicit_pathspec)
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

fn correction_envelope(
    verification: &VerificationRecord,
    attempt: u32,
    remaining_attempts: u32,
) -> CorrectionEnvelope {
    let mut failed_requirements = verification
        .assertions
        .iter()
        .filter(|assertion| {
            assertion.blocking
                && !matches!(
                    assertion.status,
                    golutra_core::VerificationAssertionStatus::Pass
                )
        })
        .map(|assertion| {
            format!(
                "{}: {}",
                assertion.subject,
                if assertion.message.trim().is_empty() {
                    assertion.expected.as_str()
                } else {
                    assertion.message.as_str()
                }
            )
        })
        .collect::<Vec<_>>();
    failed_requirements.extend(verification.residual_risks.iter().cloned());
    failed_requirements.sort();
    failed_requirements.dedup();
    failed_requirements.truncate(8);
    failed_requirements = failed_requirements
        .into_iter()
        .map(|value| value.chars().take(512).collect())
        .collect();
    let mut evidence_refs = verification.evidence_refs.clone();
    evidence_refs.truncate(16);
    CorrectionEnvelope {
        verification_id: verification.verification_id,
        attempt,
        remaining_attempts,
        failed_requirements,
        evidence_refs,
        requested_action: "use the available tools to satisfy the failed requirements, then re-run objective validation".to_owned(),
    }
}

fn verification_environment_digest(
    workspace_root: &Path,
    contract: &TaskContract,
    external_verifiers: &[ExternalVerificationSpec],
) -> String {
    let bytes = serde_json::to_vec(&serde_json::json!({
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "workspace": workspace_root,
        "contract": contract,
        "external_verifiers": external_verifiers,
    }))
    .unwrap_or_default();
    format!("sha256:{:x}", Sha256::digest(bytes))
}

#[cfg(test)]
fn legacy_task_contract(request: &AgentTaskRequest) -> TaskContract {
    let mut contract = TaskContract::conversational(request.completion_criteria.clone());
    if request.touched_code {
        contract.workspace_change = WorkspaceChangeRequirement::Required;
        contract.require_objective_validation = true;
        contract.max_correction_rounds = 1;
    }
    if let Some(hint) = infer_legacy_write_objective(&request.objective) {
        if !contract.required_paths.contains(&hint.path) {
            contract.required_paths.push(hint.path.clone());
        }
        if let Some(content) = hint.content {
            contract.required_file_contents.push(RequiredFileContent {
                path: hint.path,
                content,
            });
        }
    }
    if let Some(path) = infer_direct_legacy_write_path(&request.objective)
        && !contract.required_paths.contains(&path)
    {
        contract.required_paths.push(path);
    }
    contract
}

fn provider_tools_for_contract(
    tools: &[ToolContract],
    contract: &TaskContract,
) -> Vec<ToolContract> {
    tools
        .iter()
        .filter(|tool| {
            !matches!(
                contract.workspace_change,
                WorkspaceChangeRequirement::Forbidden
            ) || tool.side_effect_type == SideEffectType::None
        })
        .cloned()
        .collect()
}

fn estimate_tool_contract_tokens(tools: &[ToolContract]) -> u64 {
    tools
        .iter()
        .map(|contract| {
            serde_json::to_string(contract)
                .map(|value| estimate_tokens(&value))
                .unwrap_or_default()
        })
        .sum()
}

fn elapsed_millis(started_at: Instant) -> u64 {
    u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn governor_with_max_elapsed_ms(
    governor: &RuntimeGovernor,
    max_elapsed_ms: u64,
) -> RuntimeGovernor {
    let mut limits = governor.limits().clone();
    limits.max_elapsed_ms = max_elapsed_ms.max(1);
    RuntimeGovernor::new(limits)
}

fn deadline_from_budget(max_elapsed_ms: u64) -> Option<tokio::time::Instant> {
    tokio::time::Instant::now().checked_add(Duration::from_millis(max_elapsed_ms.max(1)))
}

fn runtime_deadline_advisory(max_elapsed_ms: u64, elapsed_ms: u64) -> Option<String> {
    let remaining_ms = max_elapsed_ms.saturating_sub(elapsed_ms);
    let warning_window_ms = runtime_deadline_warning_window_ms(max_elapsed_ms);
    if remaining_ms > warning_window_ms {
        return None;
    }
    let remaining_seconds = remaining_ms.div_ceil(1_000);
    Some(format!(
        "Runtime deadline advisory: about {remaining_seconds} seconds remain. Stop broad exploration, preserve and verify the best available deliverable, and return a final response before the deadline. Do not start work that cannot finish within the remaining time."
    ))
}

fn runtime_deadline_warning_window_ms(max_elapsed_ms: u64) -> u64 {
    (max_elapsed_ms / 5).clamp(1, 120_000)
}

fn shell_execution_budget(
    max_elapsed_ms: u64,
    elapsed_ms: u64,
    deadline_advisory_emitted: bool,
) -> u64 {
    const FINAL_RESPONSE_RESERVE_MS: u64 = 30_000;

    let remaining_ms = max_elapsed_ms.saturating_sub(elapsed_ms).max(1);
    if deadline_advisory_emitted {
        return if remaining_ms > FINAL_RESPONSE_RESERVE_MS {
            remaining_ms.saturating_sub(FINAL_RESPONSE_RESERVE_MS)
        } else {
            remaining_ms.div_ceil(2)
        }
        .max(1);
    }

    let warning_window_ms = runtime_deadline_warning_window_ms(max_elapsed_ms);
    if remaining_ms > warning_window_ms {
        return remaining_ms.saturating_sub(warning_window_ms).max(1);
    }

    // A checkpoint can cross into the warning window before the model has seen
    // the advisory. Preserve a response window instead of letting that pending
    // tool call consume the entire task deadline.
    remaining_ms.div_ceil(2).max(1)
}

fn clamp_shell_timeout_to_budget(request: &mut ToolRequest, remaining_ms: u64) {
    const DEFAULT_FOREGROUND_TIMEOUT_MS: u64 = 5_000;
    const DEFAULT_BACKGROUND_TIMEOUT_MS: u64 = 60 * 60 * 1_000;

    if request.tool_name != "shell" {
        return;
    }
    let Some(arguments) = request.arguments.as_object_mut() else {
        return;
    };
    let timeout_ms = match arguments.get("timeout_ms") {
        Some(value) => {
            let Some(requested) = value.as_u64() else {
                return;
            };
            requested
        }
        None => {
            if arguments
                .get("background")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                DEFAULT_BACKGROUND_TIMEOUT_MS
            } else {
                DEFAULT_FOREGROUND_TIMEOUT_MS
            }
        }
    }
    .min(remaining_ms);
    arguments.insert("timeout_ms".to_owned(), Value::from(timeout_ms));
}

fn finish_runtime_step<F>(
    machine: &mut StepMachine,
    snapshot: StepSnapshot,
    fingerprint: impl Into<String>,
    made_progress: bool,
    elapsed_ms: u64,
    trace: &mut F,
) -> StepCompletion
where
    F: FnMut(AgentLoopTraceEvent) + Send,
{
    finish_runtime_step_with_material_progress(
        machine,
        snapshot,
        fingerprint,
        made_progress,
        made_progress,
        elapsed_ms,
        trace,
    )
}

fn finish_runtime_step_with_material_progress<F>(
    machine: &mut StepMachine,
    snapshot: StepSnapshot,
    fingerprint: impl Into<String>,
    made_progress: bool,
    made_material_progress: bool,
    elapsed_ms: u64,
    trace: &mut F,
) -> StepCompletion
where
    F: FnMut(AgentLoopTraceEvent) + Send,
{
    let completion = machine.complete_at_with_material_progress(
        snapshot,
        fingerprint,
        made_progress,
        made_material_progress,
        elapsed_ms,
    );
    trace(AgentLoopTraceEvent::StepCompleted(completion.clone()));
    trace(AgentLoopTraceEvent::StepCheckpointed(machine.checkpoint()));
    completion
}

fn provider_response_fingerprint(response: &ProviderResponse) -> String {
    let tool_calls = response
        .tool_calls
        .iter()
        .map(|call| semantic_tool_action_fingerprint(&call.tool_name, &call.arguments))
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

fn semantic_tool_action_fingerprint(tool_name: &str, arguments: &Value) -> Value {
    if let Some(family) = golutra_core::semantic_tool_failure_family(tool_name, arguments) {
        return serde_json::json!({"tool_name": tool_name, "family": family});
    }
    if matches!(tool_name, "read_file" | "list_dir")
        && let Some(path) = arguments.get("path").and_then(Value::as_str)
    {
        return serde_json::json!({
            "tool_name": "inspect",
            "resource": normalize_action_resource(path),
        });
    }
    if tool_name == "shell"
        && let Some(command) = shell_command_text(arguments)
        && let Some(resources) = shell_inspection_resources(&command)
    {
        return serde_json::json!({
            "tool_name": "inspect",
            "resources": resources,
            "command_digest": digest_value(&Value::String(command)),
        });
    }
    serde_json::json!({
        "tool_name": tool_name,
        "arguments_digest": digest_value(arguments),
    })
}

fn shell_command_text(arguments: &Value) -> Option<String> {
    match arguments.get("command")? {
        Value::String(command) => Some(command.clone()),
        Value::Array(parts) => Some(
            parts
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(" "),
        ),
        _ => None,
    }
}

fn shell_inspection_resources(command: &str) -> Option<Vec<String>> {
    let lower = command.to_ascii_lowercase();
    let inspection = [
        "cat ", "grep ", "head ", "less ", "ls ", "nl ", "rg ", "sed ", "tail ",
    ]
    .iter()
    .any(|marker| lower.contains(marker));
    if !inspection || lower.contains("sed -i") {
        return None;
    }
    let matcher =
        regex::Regex::new(r"(?:[A-Za-z0-9_.-]+/)+[A-Za-z0-9_.-]+|[A-Za-z0-9_.-]+\.[A-Za-z0-9_.-]+")
            .ok()?;
    let mut resources = matcher
        .find_iter(command)
        .map(|matched| normalize_action_resource(matched.as_str()))
        .filter(|resource| {
            !matches!(
                resource.as_str(),
                "bash" | "json.tool" | "python" | "python3"
            )
        })
        .collect::<Vec<_>>();
    resources.sort();
    resources.dedup();
    (!resources.is_empty()).then_some(resources)
}

fn normalize_action_resource(resource: &str) -> String {
    resource
        .trim_matches(|character: char| matches!(character, '\'' | '"' | ',' | ';' | ':'))
        .replace('\\', "/")
        .to_ascii_lowercase()
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
