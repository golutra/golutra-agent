//! Stable lifecycle facade around the provider/tool loop.
//!
//! Callers submit one [`AgentRun`] and either execute it with an existing
//! runtime control channel or start an owned turn. Trace, replay, task
//! contracts, and control remain implementation details of this module.

use std::{ops::Deref, sync::Arc};

use golutra_context::ContextBuilder;
use golutra_core::{
    ApprovalResolution, TaskContract, VerificationRequirement, WorkspaceChangeRequirement,
};
use golutra_llm::{LlmProvider, PromptCacheScope};
use golutra_protocol::{AgentExecutionMode, AgentToolProfile};
use golutra_tools::ToolRuntime;
use tokio::task::JoinHandle;

use super::{
    AgentExecutionControl, AgentExecutionHandle, AgentGovernorUsage, AgentLoop, AgentLoopError,
    AgentLoopOutcome, AgentReplayContext, AgentTaskRequest, AgentTurnOverrides, PendingAgentTurn,
    RuntimeObservationSink, agent_execution_channel,
};

const DEFAULT_PENDING_TURN_CAPACITY: usize = 32;

#[derive(Debug, Clone, PartialEq)]
pub struct AgentRun {
    pub request: AgentTaskRequest,
    pub task_contract: TaskContract,
    pub replay_context: Option<AgentReplayContext>,
    pub cache_scope: PromptCacheScope,
    pub max_elapsed_ms: Option<u64>,
    pub defer_external_verification: Option<bool>,
}

impl AgentRun {
    #[must_use]
    pub fn new(request: AgentTaskRequest) -> Self {
        let task_contract = default_task_contract(&request);
        let cache_scope = PromptCacheScope::session(request.session_id, None);
        Self {
            request,
            task_contract,
            replay_context: None,
            cache_scope,
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

    #[must_use]
    pub fn with_cache_scope(mut self, cache_scope: PromptCacheScope) -> Self {
        self.cache_scope = cache_scope;
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

/// Opt-in execution-surface configuration for a run.
///
/// Keeping this state outside [`AgentRun`] preserves the original public run
/// shape for Rust callers while allowing new clients to select a mode/profile
/// and resume durable governor accounting.
#[derive(Debug, Clone, PartialEq)]
pub struct ConfiguredAgentRun {
    run: AgentRun,
    execution_mode: Option<AgentExecutionMode>,
    tool_profile: Option<AgentToolProfile>,
    tool_profile_is_explicit: bool,
    governor_usage: AgentGovernorUsage,
    task_contract_is_explicit: bool,
}

impl ConfiguredAgentRun {
    #[must_use]
    pub fn new(request: AgentTaskRequest) -> Self {
        Self {
            run: AgentRun::new(request),
            execution_mode: None,
            tool_profile: None,
            tool_profile_is_explicit: false,
            governor_usage: AgentGovernorUsage::default(),
            task_contract_is_explicit: false,
        }
    }

    #[must_use]
    pub fn with_task_contract(mut self, task_contract: TaskContract) -> Self {
        self.run.task_contract = task_contract;
        self.task_contract_is_explicit = true;
        self
    }

    #[must_use]
    pub fn with_replay_context(mut self, replay_context: AgentReplayContext) -> Self {
        self.run.replay_context = Some(replay_context);
        self
    }

    #[must_use]
    pub fn with_cache_scope(mut self, cache_scope: PromptCacheScope) -> Self {
        self.run.cache_scope = cache_scope;
        self
    }

    #[must_use]
    pub fn with_max_elapsed_ms(mut self, max_elapsed_ms: u64) -> Self {
        self.run.max_elapsed_ms = Some(max_elapsed_ms.max(1));
        self
    }

    #[must_use]
    pub fn with_deferred_external_verification(mut self, deferred: bool) -> Self {
        self.run.defer_external_verification = Some(deferred);
        self
    }

    /// Select the model-visible tool surface for the initial turn. Queued
    /// turns carry their own optional override.
    #[must_use]
    pub fn with_tool_profile(mut self, profile: AgentToolProfile) -> Self {
        self.tool_profile = Some(profile);
        self.tool_profile_is_explicit = true;
        self
    }

    /// Select a complete explicit execution surface for the initial turn.
    #[must_use]
    pub fn with_execution_surface(
        mut self,
        mode: AgentExecutionMode,
        profile: AgentToolProfile,
    ) -> Self {
        self.execution_mode = Some(mode);
        self.tool_profile = Some(profile);
        self.tool_profile_is_explicit = true;
        self.apply_execution_mode_contract(Some(mode));
        self
    }

    /// Select the execution contract for the initial turn. `None` retains the
    /// legacy contract for direct runtime callers; `open` keeps the model loop
    /// conversational even when the request records a workspace mutation.
    #[must_use]
    pub fn with_execution_mode(mut self, mode: Option<AgentExecutionMode>) -> Self {
        self.execution_mode = mode;
        if !self.tool_profile_is_explicit {
            self.tool_profile = mode.map(|_| AgentToolProfile::Coding);
        }
        self.apply_execution_mode_contract(mode);
        self
    }

    fn apply_execution_mode_contract(&mut self, mode: Option<AgentExecutionMode>) {
        if !self.task_contract_is_explicit {
            self.run.task_contract = match mode {
                Some(AgentExecutionMode::Strict) => strict_task_contract(&self.run.request),
                Some(AgentExecutionMode::Open) => open_task_contract(&self.run.request),
                None => default_task_contract(&self.run.request),
            };
        }
    }

    /// Resume cumulative hard-budget accounting after a durable host recovery.
    #[must_use]
    pub fn with_governor_usage(mut self, usage: AgentGovernorUsage) -> Self {
        self.governor_usage = usage;
        self
    }
}

impl Deref for ConfiguredAgentRun {
    type Target = AgentRun;

    fn deref(&self) -> &Self::Target {
        &self.run
    }
}

fn default_task_contract(request: &AgentTaskRequest) -> TaskContract {
    let mut contract = TaskContract::conversational(request.completion_criteria.clone());
    if request.touched_code {
        contract.workspace_change = WorkspaceChangeRequirement::Required;
        contract.require_objective_validation = true;
        contract.max_correction_rounds = 1;
    }
    contract
}

fn open_task_contract(request: &AgentTaskRequest) -> TaskContract {
    let mut contract = TaskContract::conversational(request.completion_criteria.clone());
    if request.touched_code {
        contract.workspace_change = WorkspaceChangeRequirement::Required;
        contract.require_objective_validation = true;
    }
    contract
}

fn strict_task_contract(request: &AgentTaskRequest) -> TaskContract {
    let mut contract = default_task_contract(request);
    contract.require_objective_validation = true;
    contract.verification = VerificationRequirement::Required;
    contract.max_correction_rounds = contract.max_correction_rounds.max(1);
    contract
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
        sink: S,
    ) -> Result<AgentLoopOutcome, AgentLoopError>
    where
        S: RuntimeObservationSink,
    {
        let turn_overrides = AgentTurnOverrides {
            max_elapsed_ms: run.max_elapsed_ms,
            defer_external_verification: run.defer_external_verification,
            ..AgentTurnOverrides::default()
        };
        self.execute_inner(run, turn_overrides, control, sink).await
    }

    pub async fn execute_configured<S>(
        &self,
        run: ConfiguredAgentRun,
        control: AgentExecutionControl,
        sink: S,
    ) -> Result<AgentLoopOutcome, AgentLoopError>
    where
        S: RuntimeObservationSink,
    {
        let ConfiguredAgentRun {
            run,
            execution_mode,
            tool_profile,
            tool_profile_is_explicit: _,
            governor_usage,
            task_contract_is_explicit: _,
        } = run;
        let turn_overrides = AgentTurnOverrides {
            max_elapsed_ms: run.max_elapsed_ms,
            defer_external_verification: run.defer_external_verification,
            execution_mode,
            tool_profile,
            governor_usage,
        };
        self.execute_inner(run, turn_overrides, control, sink).await
    }

    async fn execute_inner<S>(
        &self,
        run: AgentRun,
        turn_overrides: AgentTurnOverrides,
        control: AgentExecutionControl,
        mut sink: S,
    ) -> Result<AgentLoopOutcome, AgentLoopError>
    where
        S: RuntimeObservationSink,
    {
        self.loop_core
            .run_with_control_trace_contract_and_replay_context(
                run,
                control,
                move |observation| sink.emit(observation),
                turn_overrides,
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

    /// Start an owned turn with an explicit model-facing execution surface.
    #[must_use]
    pub fn start_configured<S>(self, run: ConfiguredAgentRun, sink: S) -> RunningTurn
    where
        P: 'static,
        S: RuntimeObservationSink + 'static,
    {
        let (control_handle, execution_control) =
            agent_execution_channel(self.pending_turn_capacity);
        let completion =
            tokio::spawn(
                async move { self.execute_configured(run, execution_control, sink).await },
            );
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

#[cfg(test)]
mod tests {
    use super::*;
    use golutra_core::SessionId;
    use golutra_core::TaskId;
    use golutra_core::TurnId;

    fn request() -> AgentTaskRequest {
        AgentTaskRequest {
            session_id: SessionId::new(),
            task_id: TaskId::new(),
            turn_id: TurnId::new(),
            objective: "inspect the workspace".to_owned(),
            completion_criteria: vec!["return a useful result".to_owned()],
            output_schema: None,
            touched_code: false,
            contributors: Vec::new(),
            tools: Vec::new(),
        }
    }

    #[test]
    fn strict_mode_builds_a_completion_contract_for_direct_harness_callers() {
        let run = ConfiguredAgentRun::new(request())
            .with_execution_mode(Some(AgentExecutionMode::Strict));

        assert!(run.task_contract.require_objective_validation);
        assert_eq!(
            run.task_contract.verification,
            VerificationRequirement::Required
        );
        assert_eq!(run.task_contract.max_correction_rounds, 1);
    }

    #[test]
    fn open_mode_does_not_add_a_correction_round_for_workspace_changes() {
        let mut request = request();
        request.touched_code = true;
        let run =
            ConfiguredAgentRun::new(request).with_execution_mode(Some(AgentExecutionMode::Open));

        assert_eq!(
            run.task_contract.workspace_change,
            WorkspaceChangeRequirement::Required
        );
        assert!(run.task_contract.require_objective_validation);
        assert_eq!(run.task_contract.max_correction_rounds, 0);
    }

    #[test]
    fn explicit_contract_remains_authoritative_in_strict_mode() {
        let contract = TaskContract::conversational(vec!["answer plainly".to_owned()]);
        let run = ConfiguredAgentRun::new(request())
            .with_task_contract(contract.clone())
            .with_execution_mode(Some(AgentExecutionMode::Strict));

        assert_eq!(run.task_contract, contract);
    }

    #[test]
    fn explicit_default_contract_remains_authoritative_in_strict_mode() {
        let request = request();
        let contract = default_task_contract(&request);
        let run = ConfiguredAgentRun::new(request)
            .with_task_contract(contract.clone())
            .with_execution_mode(Some(AgentExecutionMode::Strict));

        assert_eq!(run.task_contract, contract);
        assert_eq!(
            run.task_contract.verification,
            VerificationRequirement::BestEffort
        );
    }

    #[test]
    fn configured_run_execution_surface_defaults_are_unambiguous() {
        let legacy = ConfiguredAgentRun::new(request());
        assert_eq!(legacy.execution_mode, None);
        assert_eq!(legacy.tool_profile, None);

        let open =
            ConfiguredAgentRun::new(request()).with_execution_mode(Some(AgentExecutionMode::Open));
        assert_eq!(open.execution_mode, Some(AgentExecutionMode::Open));
        assert_eq!(open.tool_profile, Some(AgentToolProfile::Coding));

        let explicit = ConfiguredAgentRun::new(request())
            .with_execution_surface(AgentExecutionMode::Open, AgentToolProfile::Full);
        assert_eq!(explicit.execution_mode, Some(AgentExecutionMode::Open));
        assert_eq!(explicit.tool_profile, Some(AgentToolProfile::Full));

        let profile_first = ConfiguredAgentRun::new(request())
            .with_tool_profile(AgentToolProfile::Full)
            .with_execution_mode(Some(AgentExecutionMode::Strict));
        assert_eq!(
            profile_first.execution_mode,
            Some(AgentExecutionMode::Strict)
        );
        assert_eq!(profile_first.tool_profile, Some(AgentToolProfile::Full));

        let restored_legacy = ConfiguredAgentRun::new(request())
            .with_execution_mode(Some(AgentExecutionMode::Open))
            .with_execution_mode(None);
        assert_eq!(restored_legacy.execution_mode, None);
        assert_eq!(restored_legacy.tool_profile, None);
    }
}
