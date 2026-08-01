//! Stable lifecycle facade around the provider/tool loop.
//!
//! Callers submit one [`AgentRun`] and either execute it with an existing
//! runtime control channel or start an owned turn. Trace, replay, task
//! contracts, and control remain implementation details of this module.

use std::sync::Arc;

use golutra_context::ContextBuilder;
use golutra_core::{ApprovalResolution, TaskContract, WorkspaceChangeRequirement};
use golutra_llm::LlmProvider;
use golutra_tools::ToolRuntime;
use tokio::task::JoinHandle;

use super::{
    AgentExecutionControl, AgentExecutionHandle, AgentLoop, AgentLoopError, AgentLoopOutcome,
    AgentReplayContext, AgentTaskRequest, AgentTurnOverrides, PendingAgentTurn,
    RuntimeObservationSink, agent_execution_channel,
};

const DEFAULT_PENDING_TURN_CAPACITY: usize = 32;

#[derive(Debug, Clone, PartialEq)]
pub struct AgentRun {
    pub request: AgentTaskRequest,
    pub task_contract: TaskContract,
    pub replay_context: Option<AgentReplayContext>,
    pub max_elapsed_ms: Option<u64>,
    pub defer_external_verification: Option<bool>,
}

impl AgentRun {
    #[must_use]
    pub fn new(request: AgentTaskRequest) -> Self {
        let mut task_contract = TaskContract::conversational(request.completion_criteria.clone());
        if request.touched_code {
            task_contract.workspace_change = WorkspaceChangeRequirement::Required;
            task_contract.require_objective_validation = true;
            task_contract.max_correction_rounds = 1;
        }
        Self {
            request,
            task_contract,
            replay_context: None,
            max_elapsed_ms: None,
            defer_external_verification: None,
        }
    }

    #[must_use]
    pub fn with_task_contract(mut self, task_contract: TaskContract) -> Self {
        self.task_contract = task_contract;
        self
    }

    #[must_use]
    pub fn with_replay_context(mut self, replay_context: AgentReplayContext) -> Self {
        self.replay_context = Some(replay_context);
        self
    }

    /// Override the wall-clock budget for this run's initial turn. Queued
    /// turns carry and reset their own budget independently.
    #[must_use]
    pub fn with_max_elapsed_ms(mut self, max_elapsed_ms: u64) -> Self {
        self.max_elapsed_ms = Some(max_elapsed_ms.max(1));
        self
    }

    /// Override deferred external verification for this run's initial turn.
    /// Queued turns carry their own setting independently.
    #[must_use]
    pub fn with_deferred_external_verification(mut self, deferred: bool) -> Self {
        self.defer_external_verification = Some(deferred);
        self
    }
}

#[derive(Debug)]
pub struct AgentHarness<P> {
    loop_core: AgentLoop<P>,
    pending_turn_capacity: usize,
}

impl<P> AgentHarness<P>
where
    P: LlmProvider,
{
    #[must_use]
    pub fn new(provider: P, context_builder: ContextBuilder, tool_runtime: ToolRuntime) -> Self {
        Self {
            loop_core: AgentLoop::new(provider, context_builder, tool_runtime),
            pending_turn_capacity: DEFAULT_PENDING_TURN_CAPACITY,
        }
    }

    fn loop_core_mut(&mut self) -> &mut AgentLoop<P> {
        &mut self.loop_core
    }

    #[must_use]
    pub fn with_fallback(mut self, provider: P) -> Self {
        self.loop_core_mut().fallback_provider = Some(provider);
        self
    }

    #[must_use]
    pub fn with_governor(mut self, governor: golutra_governor::RuntimeGovernor) -> Self {
        self.loop_core_mut().governor = governor;
        self
    }

    #[must_use]
    pub fn with_provider_session_policy(mut self, policy: super::ProviderSessionPolicy) -> Self {
        self.loop_core_mut().provider_session_policy = policy;
        self
    }

    #[must_use]
    pub fn with_before_side_effect_recorder(
        mut self,
        recorder: Arc<dyn super::BeforeSideEffectRecorder>,
    ) -> Self {
        self.loop_core_mut().before_side_effect_recorder = Some(recorder);
        self
    }

    #[must_use]
    pub fn with_external_verifiers(
        mut self,
        external_verifiers: Vec<golutra_protocol::ExternalVerificationSpec>,
    ) -> Self {
        self.loop_core_mut().external_verifiers = external_verifiers;
        self
    }

    #[must_use]
    pub fn with_max_elapsed_ms(mut self, max_elapsed_ms: u64) -> Self {
        let mut limits = self.loop_core.governor.limits().clone();
        limits.max_elapsed_ms = max_elapsed_ms.max(1);
        self.loop_core_mut().governor = golutra_governor::RuntimeGovernor::new(limits);
        self
    }

    #[must_use]
    pub fn with_deferred_external_verification(mut self, deferred: bool) -> Self {
        self.loop_core_mut().defer_external_verification = deferred;
        self
    }

    #[must_use]
    pub fn require_os_sandbox_for_external_verifiers(mut self, required: bool) -> Self {
        self.loop_core_mut().external_verifiers_require_os_sandbox = required;
        self
    }

    #[must_use]
    pub fn with_pending_turn_capacity(mut self, capacity: usize) -> Self {
        self.pending_turn_capacity = capacity.max(1);
        self
    }

    pub async fn execute<S>(
        &self,
        run: AgentRun,
        control: AgentExecutionControl,
        mut sink: S,
    ) -> Result<AgentLoopOutcome, AgentLoopError>
    where
        S: RuntimeObservationSink,
    {
        self.loop_core
            .run_with_control_trace_contract_and_replay_context(
                run.request,
                control,
                move |observation| sink.emit(observation),
                run.task_contract,
                run.replay_context,
                AgentTurnOverrides {
                    max_elapsed_ms: run.max_elapsed_ms,
                    defer_external_verification: run.defer_external_verification,
                },
            )
            .await
    }

    /// Start an owned turn on the current Tokio runtime.
    ///
    /// Use [`Self::execute`] when the host already owns the runtime control
    /// channel, as the durable `RuntimeHost` does. Starting consumes the
    /// configured harness so configuration cannot be mutated after execution.
    #[must_use]
    pub fn start<S>(self, run: AgentRun, sink: S) -> RunningTurn
    where
        P: 'static,
        S: RuntimeObservationSink + 'static,
    {
        let (control_handle, execution_control) =
            agent_execution_channel(self.pending_turn_capacity);
        let completion =
            tokio::spawn(async move { self.execute(run, execution_control, sink).await });
        RunningTurn {
            control: control_handle,
            completion,
        }
    }
}

#[derive(Debug)]
pub struct RunningTurn {
    control: AgentExecutionHandle,
    completion: JoinHandle<Result<AgentLoopOutcome, AgentLoopError>>,
}

impl RunningTurn {
    pub async fn steer(&self, mut turn: PendingAgentTurn) -> Result<(), AgentLoopError> {
        turn.steer = true;
        self.control.append_turn(turn).await
    }

    pub async fn follow_up(&self, mut turn: PendingAgentTurn) -> Result<(), AgentLoopError> {
        turn.steer = false;
        self.control.append_turn(turn).await
    }

    pub fn pause(&self) {
        self.control.pause();
    }

    pub fn resume(&self) {
        self.control.resume();
    }

    pub fn interrupt(&self) {
        self.control.cancel();
    }

    pub async fn resolve_approval(
        &self,
        resolution: ApprovalResolution,
    ) -> Result<(), AgentLoopError> {
        self.control.resolve_approval(resolution).await
    }

    pub async fn wait(self) -> Result<AgentLoopOutcome, AgentLoopError> {
        self.completion.await.map_err(|error| {
            AgentLoopError::Worker(format!(
                "agent harness worker stopped unexpectedly: {error}"
            ))
        })?
    }
}
