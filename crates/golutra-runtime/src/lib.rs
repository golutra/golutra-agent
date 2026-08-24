use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::Path,
    sync::{Arc, Mutex as StdMutex},
    time::{Duration, Instant},
};

use async_trait::async_trait;
use futures_util::{StreamExt, stream};
use golutra_context::{
    ContextBuilder, ContextContributor, ContextError, ContextMessageSource, ModelInputVisibility,
    compile_model_input, estimate_message_tokens, estimate_tokens, token_usage_record,
};
use golutra_core::{
    ApprovalDecision, ApprovalId, ApprovalRequest, ApprovalResolution, ApprovalScope, BudgetState,
    CommandId, CorrectionEnvelope, LoopAction, LoopDecision, PolicyBlockDisposition,
    PolicyDecision, PolicyEvaluation, PolicyId, SessionId, SideEffectType, TaskContract, TaskId,
    ToolContract, ToolExecutionMetrics, ToolProgress, ToolProgressPhase, ToolRecoveryPolicy,
    ToolResultEnvelope, ToolResultStatus, TurnId, TurnState, UserQuestionPrompt,
    UserQuestionRequest, UserQuestionResolution, VerificationCheck, VerificationCheckKind,
    VerificationPlan, VerificationRecord, VerificationResult, WorkspaceChangeRequirement,
};
#[cfg(test)]
use golutra_core::{
    RequiredFileContent, UserQuestionAnswer, VerificationRequirement,
    infer_direct_legacy_write_path, infer_legacy_write_objective,
};
use golutra_governor::{
    GoalLedger, GovernorAction, GovernorObservation, GovernorPhase, RuntimeGovernor,
    RuntimeGovernorDecision,
};
use golutra_llm::{
    LlmProvider, ProviderError, ProviderMessage, ProviderRequest, ProviderResponse, ProviderRole,
    ProviderToolCall,
};
use golutra_policy::approval_resource_matches;
use golutra_protocol::{AgentExecutionMode, AgentToolProfile, ExternalVerificationSpec};
use golutra_tools::{
    CONTRACT_FILE_CONTENT_VERIFIER_TOOL, CONTRACT_PATH_VERIFIER_TOOL, FileBeforeImage, ToolError,
    ToolExecutionReport, ToolInvocation, ToolRegistry, ToolRequest, ToolRuntime,
    VerifierExecutionRequest, model_visible_tool_result, redact_tool_arguments,
};
use golutra_verify::VerificationInput;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::sync::{Notify, mpsc, watch};
use tokio_util::sync::CancellationToken;

mod checkpoint;
mod completion;
mod context_guard;
mod harness;
mod lane;
mod objective_evidence;
mod provider_retry;
mod provider_session;
mod step_machine;
mod trace;
mod verification;

pub use checkpoint::{CheckpointError, WorkspaceCheckpointManager, checkpoint_fingerprint};
pub use golutra_protocol::UserProjection;
pub use harness::{AgentHarness, AgentRun, ConfiguredAgentRun, RunningTurn};
pub use lane::{RuntimeLaneError, RuntimeLaneManager, RuntimeTransition, is_active_status};
pub use provider_session::{ProviderSessionPolicy, ProviderTransport};
pub(crate) use step_machine::{
    CorrectionProgressLimits, StepCheckpoint, StepCompletion, StepMachine, StepSnapshot,
};
pub use trace::{AgentLoopTraceEvent, RuntimeObservation, RuntimeObservationSink};
pub use verification::RuntimeVerificationService;

const PARALLEL_READ_CONCURRENCY_LIMIT: usize = 8;

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
    #[error("queued agent turn was not found")]
    PendingTurnNotFound,
    #[error("queued agent turn is already being changed")]
    PendingTurnMutationInProgress,
    #[error("invalid task contract: {0}")]
    TaskContract(String),
    #[error("invalid user question response: {0}")]
    UserQuestion(String),
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

/// The execution surface currently active at a turn boundary.
///
/// `None` for `execution_mode` is the compatibility marker for callers that
/// predate the explicit open/strict protocol field.  This state is shared by
/// the producer and consumer sides of the execution channel so host-side
/// capabilities such as delegation do not depend on an asynchronously
/// persisted observation arriving first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActiveExecutionSurface {
    pub execution_mode: Option<AgentExecutionMode>,
    pub tool_profile: AgentToolProfile,
}

impl Default for ActiveExecutionSurface {
    fn default() -> Self {
        Self {
            execution_mode: None,
            tool_profile: AgentToolProfile::Full,
        }
    }
}

/// Cumulative governor consumption carried across a durable runtime recovery.
///
/// Ordinary queued and steering turns already share these counters because they
/// execute in one [`AgentLoop`]. A recovered task starts a new loop process, so
/// the host supplies the last durable totals through this value instead of
/// silently resetting hard limits.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AgentGovernorUsage {
    pub iterations: u32,
    pub tool_calls: u32,
    pub failed_tool_calls: u32,
    pub consecutive_failed_tool_calls: u32,
    pub estimated_cost_microusd: Option<u64>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct AgentTurnOverrides {
    pub max_elapsed_ms: Option<u64>,
    pub defer_external_verification: Option<bool>,
    pub execution_mode: Option<AgentExecutionMode>,
    pub tool_profile: Option<AgentToolProfile>,
    pub governor_usage: AgentGovernorUsage,
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

/// Optional execution-surface override carried alongside a queued turn.
///
/// This is intentionally separate from [`PendingAgentTurn`]. The latter is a
/// long-lived public struct that downstream Rust callers may construct with a
/// struct literal, so adding fields to it would be a source-incompatible API
/// change.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PendingTurnExecutionOptions {
    /// An ordinary queued turn may select an explicit mode. `None` preserves
    /// the pre-mode wire contract and therefore starts a legacy turn; only a
    /// steering turn inherits the active mode.
    pub execution_mode: Option<AgentExecutionMode>,
    /// `None` selects the legacy full profile when execution_mode is absent,
    /// otherwise it inherits the active profile. `Some` selects a profile for
    /// the queued turn.
    pub tool_profile: Option<AgentToolProfile>,
}

/// A queued turn plus its optional model-facing execution surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfiguredPendingAgentTurn {
    pub turn: PendingAgentTurn,
    pub execution: PendingTurnExecutionOptions,
}

impl ConfiguredPendingAgentTurn {
    #[must_use]
    pub fn new(turn: PendingAgentTurn) -> Self {
        Self {
            turn,
            execution: PendingTurnExecutionOptions::default(),
        }
    }

    #[must_use]
    pub fn with_execution_options(mut self, execution: PendingTurnExecutionOptions) -> Self {
        self.execution = execution;
        self
    }
}

impl From<PendingAgentTurn> for ConfiguredPendingAgentTurn {
    fn from(turn: PendingAgentTurn) -> Self {
        Self::new(turn)
    }
}

#[derive(Debug)]
struct PreparedParallelReadCall {
    provider_tool_call_id: String,
    failure_signature: String,
    failure_family: String,
    blocked_family_failures: u32,
    request: ToolRequest,
    policy: PolicyEvaluation,
    governance: RuntimeGovernorDecision,
    tool_call_count: u32,
}

#[derive(Debug)]
struct ParallelReadOutcome {
    provider_tool_call_id: String,
    failure_signature: String,
    failure_family: String,
    blocked_family_failures: u32,
    report: ToolExecutionReport,
    progress: Vec<ToolProgress>,
    tool_call_count: u32,
}

#[derive(Debug, Clone)]
pub struct AgentExecutionHandle {
    cancellation: CancellationToken,
    pause: watch::Sender<bool>,
    pending_turns: Arc<PendingTurnQueue>,
    active_execution_surface: Arc<StdMutex<ActiveExecutionSurface>>,
    approvals: mpsc::Sender<ApprovalResolution>,
    questions: mpsc::Sender<UserQuestionResolution>,
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

    /// Publish the surface selected for the next active turn.  This is a
    /// memory-only control-plane update; durable `TurnStarted` observations
    /// remain the source of record for replay and recovery.
    pub fn set_active_execution_surface(
        &self,
        execution_mode: Option<AgentExecutionMode>,
        tool_profile: AgentToolProfile,
    ) {
        *self
            .active_execution_surface
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = ActiveExecutionSurface {
            execution_mode,
            tool_profile,
        };
    }

    #[must_use]
    pub fn active_execution_surface(&self) -> ActiveExecutionSurface {
        *self
            .active_execution_surface
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub async fn append_turn(&self, turn: PendingAgentTurn) -> Result<(), AgentLoopError> {
        self.pending_turns.push(turn)
    }

    /// Queue a turn with an explicit model-facing execution surface.
    pub async fn append_configured_turn(
        &self,
        turn: ConfiguredPendingAgentTurn,
    ) -> Result<(), AgentLoopError> {
        self.pending_turns.push_configured(turn)
    }

    pub async fn reserve_turn(
        &self,
        turn: PendingAgentTurn,
    ) -> Result<PendingTurnReservation, AgentLoopError> {
        self.pending_turns.reserve(turn)
    }

    /// Reserve a turn with an explicit model-facing execution surface.
    pub fn reserve_configured_turn(
        &self,
        turn: ConfiguredPendingAgentTurn,
    ) -> Result<PendingTurnReservation, AgentLoopError> {
        self.pending_turns.reserve_configured(turn)
    }

    pub fn reserve_turn_update(
        &self,
        turn_id: TurnId,
        replacement: PendingAgentTurn,
    ) -> Result<PendingTurnMutation, AgentLoopError> {
        self.pending_turns.reserve_update(turn_id, replacement)
    }

    /// Reserve an update while retaining its explicit execution surface.
    pub fn reserve_configured_turn_update(
        &self,
        turn_id: TurnId,
        replacement: ConfiguredPendingAgentTurn,
    ) -> Result<PendingTurnMutation, AgentLoopError> {
        self.pending_turns
            .reserve_configured_update(turn_id, replacement)
    }

    pub fn reserve_turn_cancellation(
        &self,
        turn_id: TurnId,
    ) -> Result<PendingTurnMutation, AgentLoopError> {
        self.pending_turns.reserve_cancellation(turn_id)
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

    pub async fn resolve_question(
        &self,
        resolution: UserQuestionResolution,
    ) -> Result<(), AgentLoopError> {
        self.questions
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
    active_execution_surface: Arc<StdMutex<ActiveExecutionSurface>>,
    approvals: mpsc::Receiver<ApprovalResolution>,
    questions: mpsc::Receiver<UserQuestionResolution>,
    approval_grants: Vec<ApprovalGrant>,
}

#[derive(Debug, Clone)]
struct ApprovalGrant {
    scope: ApprovalScope,
    tool_name: String,
    resource_prefix: Option<String>,
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
    turn: ConfiguredPendingAgentTurn,
    execution_origin: PendingTurnExecutionOrigin,
    durable: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingTurnExecutionOrigin {
    Legacy,
    Configured,
}

#[derive(Debug)]
struct TakenPendingTurn {
    turn: ConfiguredPendingAgentTurn,
    execution_origin: PendingTurnExecutionOrigin,
}

#[derive(Debug)]
#[must_use = "dropping an uncommitted reservation removes the pending turn"]
pub struct PendingTurnReservation {
    queue: Arc<PendingTurnQueue>,
    turn_id: TurnId,
    committed: bool,
}

#[derive(Debug)]
enum PendingTurnMutationKind {
    Update {
        original: Box<ConfiguredPendingAgentTurn>,
        original_execution_origin: PendingTurnExecutionOrigin,
    },
    Cancel,
}

#[derive(Debug)]
#[must_use = "dropping an uncommitted mutation restores the pending turn"]
pub struct PendingTurnMutation {
    queue: Arc<PendingTurnQueue>,
    turn_id: TurnId,
    kind: Option<PendingTurnMutationKind>,
}

impl PendingTurnMutation {
    pub fn commit(mut self) {
        let kind = self.kind.take().expect("pending turn mutation kind");
        self.queue.commit_mutation(self.turn_id, kind);
    }
}

impl Drop for PendingTurnMutation {
    fn drop(&mut self) {
        if let Some(kind) = self.kind.take() {
            self.queue.rollback_mutation(self.turn_id, kind);
        }
    }
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
        self.reserve_with_origin(turn.into(), PendingTurnExecutionOrigin::Legacy)?
            .commit();
        Ok(())
    }

    fn push_configured(
        self: &Arc<Self>,
        turn: ConfiguredPendingAgentTurn,
    ) -> Result<(), AgentLoopError> {
        self.reserve_with_origin(turn, PendingTurnExecutionOrigin::Configured)?
            .commit();
        Ok(())
    }

    fn reserve(
        self: &Arc<Self>,
        turn: PendingAgentTurn,
    ) -> Result<PendingTurnReservation, AgentLoopError> {
        self.reserve_with_origin(turn.into(), PendingTurnExecutionOrigin::Legacy)
    }

    fn reserve_configured(
        self: &Arc<Self>,
        turn: ConfiguredPendingAgentTurn,
    ) -> Result<PendingTurnReservation, AgentLoopError> {
        self.reserve_with_origin(turn, PendingTurnExecutionOrigin::Configured)
    }

    fn reserve_with_origin(
        self: &Arc<Self>,
        turn: ConfiguredPendingAgentTurn,
        execution_origin: PendingTurnExecutionOrigin,
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
        let turn_id = turn.turn.turn_id;
        state.turns.push_back(PendingTurnEntry {
            turn,
            execution_origin,
            durable: false,
        });
        Ok(PendingTurnReservation {
            queue: self.clone(),
            turn_id,
            committed: false,
        })
    }

    fn reserve_update(
        self: &Arc<Self>,
        turn_id: TurnId,
        replacement: PendingAgentTurn,
    ) -> Result<PendingTurnMutation, AgentLoopError> {
        self.reserve_update_with_origin(
            turn_id,
            replacement.into(),
            PendingTurnExecutionOrigin::Legacy,
        )
    }

    fn reserve_configured_update(
        self: &Arc<Self>,
        turn_id: TurnId,
        replacement: ConfiguredPendingAgentTurn,
    ) -> Result<PendingTurnMutation, AgentLoopError> {
        self.reserve_update_with_origin(
            turn_id,
            replacement,
            PendingTurnExecutionOrigin::Configured,
        )
    }

    fn reserve_update_with_origin(
        self: &Arc<Self>,
        turn_id: TurnId,
        replacement: ConfiguredPendingAgentTurn,
        replacement_execution_origin: PendingTurnExecutionOrigin,
    ) -> Result<PendingTurnMutation, AgentLoopError> {
        if replacement.turn.turn_id != turn_id {
            return Err(AgentLoopError::PendingTurnNotFound);
        }
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = state
            .turns
            .iter_mut()
            .find(|entry| entry.turn.turn.turn_id == turn_id)
            .ok_or(AgentLoopError::PendingTurnNotFound)?;
        if !entry.durable {
            return Err(AgentLoopError::PendingTurnMutationInProgress);
        }
        let original = std::mem::replace(&mut entry.turn, replacement);
        let original_execution_origin =
            std::mem::replace(&mut entry.execution_origin, replacement_execution_origin);
        entry.durable = false;
        Ok(PendingTurnMutation {
            queue: self.clone(),
            turn_id,
            kind: Some(PendingTurnMutationKind::Update {
                original: Box::new(original),
                original_execution_origin,
            }),
        })
    }

    fn reserve_cancellation(
        self: &Arc<Self>,
        turn_id: TurnId,
    ) -> Result<PendingTurnMutation, AgentLoopError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = state
            .turns
            .iter_mut()
            .find(|entry| entry.turn.turn.turn_id == turn_id)
            .ok_or(AgentLoopError::PendingTurnNotFound)?;
        if !entry.durable {
            return Err(AgentLoopError::PendingTurnMutationInProgress);
        }
        entry.durable = false;
        Ok(PendingTurnMutation {
            queue: self.clone(),
            turn_id,
            kind: Some(PendingTurnMutationKind::Cancel),
        })
    }

    fn commit(&self, turn_id: TurnId) {
        if let Some(entry) = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .turns
            .iter_mut()
            .find(|entry| entry.turn.turn.turn_id == turn_id)
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
            .retain(|entry| entry.turn.turn.turn_id != turn_id);
        self.changed.notify_waiters();
    }

    fn commit_mutation(&self, turn_id: TurnId, kind: PendingTurnMutationKind) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match kind {
            PendingTurnMutationKind::Update { .. } => {
                if let Some(entry) = state
                    .turns
                    .iter_mut()
                    .find(|entry| entry.turn.turn.turn_id == turn_id)
                {
                    entry.durable = true;
                }
            }
            PendingTurnMutationKind::Cancel => {
                state
                    .turns
                    .retain(|entry| entry.turn.turn.turn_id != turn_id);
            }
        }
        drop(state);
        self.changed.notify_waiters();
    }

    fn rollback_mutation(&self, turn_id: TurnId, kind: PendingTurnMutationKind) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(entry) = state
            .turns
            .iter_mut()
            .find(|entry| entry.turn.turn.turn_id == turn_id)
        {
            if let PendingTurnMutationKind::Update {
                original,
                original_execution_origin,
            } = kind
            {
                entry.turn = *original;
                entry.execution_origin = original_execution_origin;
            }
            entry.durable = true;
        }
        drop(state);
        self.changed.notify_waiters();
    }

    async fn take_or_close(&self) -> Option<TakenPendingTurn> {
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
                    Some(entry) if entry.durable => {
                        state.turns.pop_front().map(|entry| TakenPendingTurn {
                            turn: entry.turn,
                            execution_origin: entry.execution_origin,
                        })
                    }
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

    fn try_take_steer(&self) -> Option<TakenPendingTurn> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match state.turns.front() {
            Some(entry) if entry.durable && entry.turn.turn.steer => {
                state.turns.pop_front().map(|entry| TakenPendingTurn {
                    turn: entry.turn,
                    execution_origin: entry.execution_origin,
                })
            }
            _ => None,
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
    agent_execution_channel_with_cancellation(capacity, CancellationToken::new())
}

/// Create an execution channel whose cancellation is owned by the caller.
///
/// A delegated task passes a child token here so cancellation flows from its
/// parent without allowing a child abort to cancel the parent budget.
#[must_use]
pub fn agent_execution_channel_with_cancellation(
    capacity: usize,
    cancellation: CancellationToken,
) -> (AgentExecutionHandle, AgentExecutionControl) {
    let (pause_tx, pause_rx) = watch::channel(false);
    let pending_turns = Arc::new(PendingTurnQueue::new(capacity));
    let active_execution_surface = Arc::new(StdMutex::new(ActiveExecutionSurface::default()));
    let (approval_tx, approval_rx) = mpsc::channel(capacity.max(1));
    let (question_tx, question_rx) = mpsc::channel(capacity.max(1));
    (
        AgentExecutionHandle {
            cancellation: cancellation.clone(),
            pause: pause_tx,
            pending_turns: pending_turns.clone(),
            active_execution_surface: active_execution_surface.clone(),
            approvals: approval_tx,
            questions: question_tx,
        },
        AgentExecutionControl {
            cancellation,
            pause: pause_rx,
            pending_turns,
            active_execution_surface,
            approvals: approval_rx,
            questions: question_rx,
            approval_grants: Vec::new(),
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
        let mut current_execution_mode = turn_overrides.execution_mode;
        let mut current_tool_profile = turn_overrides
            .tool_profile
            .unwrap_or(AgentToolProfile::Full);
        control.set_active_execution_surface(current_execution_mode, current_tool_profile);
        let mut current_governor =
            governor_with_max_elapsed_ms(&self.governor, current_max_elapsed_ms);
        let mut current_turn_started_at = Instant::now();
        let mut runtime_deadline = deadline_from_budget(current_max_elapsed_ms);
        let governor_usage = turn_overrides.governor_usage;
        let mut tool_call_count = governor_usage.tool_calls;
        let mut failed_tool_call_count = governor_usage.failed_tool_calls;
        let mut consecutive_failed_tool_call_count = governor_usage.consecutive_failed_tool_calls;
        let mut deadline_advisory_emitted = false;
        let mut runtime_deadline_guard_emitted = false;
        let mut estimated_cost_microusd = governor_usage.estimated_cost_microusd;
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
        let mut provider_tools = provider_tools_for_turn(
            &all_provider_tools,
            &current_task_contract,
            current_tool_profile,
            self.tool_executor.registry(),
        );
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
        let mut pending_turn_at_boundary: Option<TakenPendingTurn> = None;

        'completion_cycle: loop {
            let mut candidate_complete = false;
            'agent_loop: loop {
                if let Some(taken_turn) = pending_turn_at_boundary.take() {
                    let execution_origin = taken_turn.execution_origin;
                    let configured_turn = taken_turn.turn;
                    let pending_execution = configured_turn.execution;
                    let pending_turn = configured_turn.turn;
                    current_turn_id = pending_turn.turn_id;
                    if pending_turn.steer {
                        if let Some(tool_profile) = pending_execution.tool_profile {
                            current_tool_profile = tool_profile;
                            provider_tools = provider_tools_for_turn(
                                &all_provider_tools,
                                &current_task_contract,
                                current_tool_profile,
                                self.tool_executor.registry(),
                            );
                            planned_tool_tokens = estimate_tool_contract_tokens(&provider_tools);
                        }
                        turn_state.continue_after_steer(current_turn_id);
                    } else {
                        if matches!(execution_origin, PendingTurnExecutionOrigin::Legacy)
                            || pending_execution.execution_mode.is_none()
                        {
                            current_execution_mode = None;
                            current_tool_profile = pending_execution
                                .tool_profile
                                .unwrap_or(AgentToolProfile::Full);
                        } else {
                            current_execution_mode = pending_execution.execution_mode;
                            if let Some(tool_profile) = pending_execution.tool_profile {
                                current_tool_profile = tool_profile;
                            }
                        }
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
                        provider_tools = provider_tools_for_turn(
                            &all_provider_tools,
                            &current_task_contract,
                            current_tool_profile,
                            self.tool_executor.registry(),
                        );
                        planned_tool_tokens = estimate_tool_contract_tokens(&provider_tools);
                        current_completion_criteria =
                            current_task_contract.completion_criteria.clone();
                        current_turn_touched_code =
                            current_task_contract.requires_workspace_evidence();
                        tool_reports.clear();
                        turn_state = TurnState::new(current_turn_id);
                        goal_ledger.original_objective = current_objective.clone();
                        goal_ledger.success_criteria = current_completion_criteria.clone();
                        goal_ledger.current_plan = current_completion_criteria.clone();
                        goal_ledger.completed_steps.clear();
                        goal_ledger.open_risks.clear();
                    }
                    control
                        .set_active_execution_surface(current_execution_mode, current_tool_profile);
                    last_assistant_message = None;
                    last_emitted_assistant_message = None;
                    repeated_failure_signature = None;
                    repeated_failure_count = 0;
                    failure_families = FailureFamilyLedger::default();
                    step_machine.end_correction();
                    let pending_started =
                        if pending_execution == PendingTurnExecutionOptions::default() {
                            AgentLoopTraceEvent::PendingTurnStarted(pending_turn.clone())
                        } else {
                            AgentLoopTraceEvent::PendingTurnStartedWithExecution(
                                ConfiguredPendingAgentTurn {
                                    turn: pending_turn.clone(),
                                    execution: pending_execution,
                                },
                            )
                        };
                    trace(pending_started);
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
                }
                let step_snapshot = step_machine.begin(current_turn_id);
                let iteration = governor_usage
                    .iterations
                    .saturating_add(step_snapshot.step_no);
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
                let usage_record = token_usage_record(
                    &plan,
                    &completed_request,
                    provider_response.response_id,
                    &plan.budget_snapshot,
                    &provider_response.usage,
                    &provider_contract.cost_model,
                );
                // Persist accounting before the completion boundary. A crash
                // after the provider returned but before the derived usage
                // event must not make a recovered governor forget the cost.
                trace(AgentLoopTraceEvent::TokenUsageRecorded(
                    usage_record.clone(),
                ));
                trace(AgentLoopTraceEvent::ProviderCompleted {
                    request_id: completed_request.request_id,
                    provider_id: completed_request.provider_id.clone(),
                    model_id: completed_request.model_id.clone(),
                    response: provider_response.clone(),
                });
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
                    total_tokens: usage_record.provider_total_tokens,
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
                        finish_runtime_step(
                            &mut step_machine,
                            step_snapshot.clone(),
                            step_fingerprint.clone(),
                            true,
                            elapsed_millis(current_turn_started_at),
                            &mut trace,
                        );
                        pending_turn_at_boundary = Some(pending_turn);
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
                let parallel_read_candidate = provider_batch_is_parallel_read_candidate(
                    &provider_response.tool_calls,
                    replay_context.is_some(),
                    current_governor
                        .limits()
                        .max_failed_tool_calls
                        .saturating_sub(consecutive_failed_tool_call_count),
                    current_tool_profile,
                    self.tool_executor.registry(),
                );
                let mut parallel_read_outcomes = VecDeque::new();
                if parallel_read_candidate {
                    control.wait_until_runnable().await?;
                    let mut prepared = Vec::with_capacity(provider_response.tool_calls.len());
                    let mut parallel_failure_signatures = HashSet::new();
                    for (offset, tool_call) in provider_response.tool_calls.iter().enumerate() {
                        let provider_tool_call_id = tool_call.tool_call_id.clone();
                        let failure_signature =
                            format!("{}:{}", tool_call.tool_name, tool_call.arguments);
                        let failure_family =
                            semantic_failure_family(&tool_call.tool_name, &tool_call.arguments);
                        let blocked_family_failures = failure_families.failures(&failure_family);
                        if blocked_family_failures > 0
                            || !parallel_failure_signatures.insert(failure_signature.clone())
                        {
                            prepared.clear();
                            break;
                        }
                        let request = ToolRequest {
                            tool_call_id: golutra_core::ToolCallId::new(),
                            provider_tool_call_id: Some(provider_tool_call_id.clone()),
                            session_id: request.session_id,
                            turn_id: Some(current_turn_id),
                            tool_name: tool_call.tool_name.clone(),
                            arguments: tool_call.arguments.clone(),
                        };
                        if tool_profile_rejection_reason(
                            &request,
                            current_tool_profile,
                            self.tool_executor.registry(),
                        )
                        .is_some()
                        {
                            prepared.clear();
                            break;
                        }
                        let Ok(policy) = self.tool_executor.evaluate(&request) else {
                            prepared.clear();
                            break;
                        };
                        if policy.decision != PolicyDecision::Allow {
                            prepared.clear();
                            break;
                        }
                        let offset = u32::try_from(offset).unwrap_or(u32::MAX).saturating_add(1);
                        let batch_tool_call_count = tool_call_count.saturating_add(offset);
                        let governance = current_governor.evaluate(
                            &goal_ledger,
                            &GovernorObservation {
                                phase: GovernorPhase::Tool,
                                iteration: iteration.saturating_add(1),
                                tool_calls: batch_tool_call_count,
                                failed_tool_calls: failed_tool_call_count,
                                consecutive_failed_tool_calls: consecutive_failed_tool_call_count,
                                planned_input_tokens: plan.budget_snapshot.planned_input_tokens,
                                elapsed_ms: elapsed_millis(current_turn_started_at),
                                latest_action: format!(
                                    "{} {}",
                                    tool_call.tool_name, tool_call.arguments
                                ),
                                estimated_cost_microusd,
                                policy_decision: None,
                                policy_block_disposition: None,
                                security_risk: "medium".to_owned(),
                            },
                        );
                        if !governance.permits_execution() {
                            prepared.clear();
                            break;
                        }
                        prepared.push(PreparedParallelReadCall {
                            provider_tool_call_id,
                            failure_signature,
                            failure_family,
                            blocked_family_failures,
                            request,
                            policy,
                            governance,
                            tool_call_count: batch_tool_call_count,
                        });
                    }
                    if prepared.len() == provider_response.tool_calls.len() {
                        for prepared_call in &prepared {
                            trace(AgentLoopTraceEvent::GovernorDecided(
                                prepared_call.governance.clone(),
                            ));
                            let recovery_policy = self
                                .tool_executor
                                .registry()
                                .contract(&prepared_call.request.tool_name)
                                .map(ToolRecoveryPolicy::from)
                                .unwrap_or_default();
                            trace(AgentLoopTraceEvent::ToolStarted {
                                tool_call_id: prepared_call.request.tool_call_id,
                                provider_tool_call_id: Some(
                                    prepared_call.provider_tool_call_id.clone(),
                                ),
                                tool_name: prepared_call.request.tool_name.clone(),
                                display_arguments: redact_tool_arguments(
                                    &prepared_call.request.arguments,
                                ),
                                recovery_policy,
                            });
                            trace(AgentLoopTraceEvent::PolicyEvaluated(
                                prepared_call.policy.clone(),
                            ));
                        }
                        tool_call_count = prepared
                            .last()
                            .map_or(tool_call_count, |call| call.tool_call_count);
                        parallel_read_outcomes = invoke_parallel_read_calls(
                            &self.tool_executor,
                            prepared,
                            control.cancellation.clone(),
                            runtime_deadline,
                        )
                        .await;
                    }
                }
                let parallel_read_batch = !parallel_read_outcomes.is_empty();
                let mut stop_after_parallel_read_batch = false;
                for tool_call in provider_response.tool_calls {
                    let (
                        provider_tool_call_id,
                        failure_signature,
                        failure_family,
                        blocked_family_failures,
                        strategy_was_blocked,
                        prepared_objective_validation,
                        result_tool_call_count,
                        mut report,
                    ) = if let Some(outcome) = parallel_read_outcomes.pop_front() {
                        debug_assert_eq!(
                            outcome.provider_tool_call_id, tool_call.tool_call_id,
                            "parallel read outcomes retain provider source order"
                        );
                        debug_assert_eq!(
                            outcome.report.envelope.tool_name, tool_call.tool_name,
                            "parallel read outcome matches its source call"
                        );
                        for progress in outcome.progress {
                            trace(AgentLoopTraceEvent::ToolProgress(progress));
                        }
                        (
                            outcome.provider_tool_call_id,
                            outcome.failure_signature,
                            outcome.failure_family,
                            outcome.blocked_family_failures,
                            false,
                            None,
                            outcome.tool_call_count,
                            outcome.report,
                        )
                    } else {
                        control.wait_until_runnable().await?;
                        let tool_action =
                            format!("{} {}", tool_call.tool_name, tool_call.arguments);
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
                        let question_report = if tool_request.tool_name == "ask_user" {
                            match tool_request
                                .arguments
                                .get("questions")
                                .cloned()
                                .map(serde_json::from_value::<Vec<UserQuestionPrompt>>)
                                .transpose()
                            {
                                Ok(Some(questions)) => {
                                    let question = UserQuestionRequest {
                                        question_id: golutra_core::QuestionId::new(),
                                        task_id: request.task_id,
                                        turn_id: current_turn_id,
                                        tool_call_id: tool_request.tool_call_id,
                                        questions,
                                    };
                                    match question.validate() {
                                        Ok(()) => {
                                            trace(AgentLoopTraceEvent::UserQuestionRequested(
                                                question.clone(),
                                            ));
                                            let resolution =
                                                control.wait_for_question(&question).await?;
                                            trace(AgentLoopTraceEvent::UserQuestionResolved(
                                                resolution.clone(),
                                            ));
                                            Some(user_question_report(
                                                tool_request.clone(),
                                                resolution,
                                            ))
                                        }
                                        Err(error) => {
                                            Some(self.tool_executor.invalid_request_report(
                                                tool_request.clone(),
                                                error,
                                            ))
                                        }
                                    }
                                }
                                Ok(None) => Some(self.tool_executor.invalid_request_report(
                                    tool_request.clone(),
                                    "ask_user requires questions",
                                )),
                                Err(error) => Some(self.tool_executor.invalid_request_report(
                                    tool_request.clone(),
                                    format!("invalid ask_user questions: {error}"),
                                )),
                            }
                        } else {
                            None
                        };
                        let profile_blocked_report = tool_profile_rejection_reason(
                            &tool_request,
                            current_tool_profile,
                            self.tool_executor.registry(),
                        )
                        .map(|reason| {
                            self.tool_executor
                                .invalid_request_report(tool_request.clone(), reason)
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
                        let report = if let Some(report) = question_report {
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
                                detail: Some("answered".to_owned()),
                                output_excerpt: report.envelope.model_visible_excerpt.clone(),
                            }));
                            report
                        } else if let Some(report) = profile_blocked_report {
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
                                detail: Some("profile_blocked".to_owned()),
                                output_excerpt: None,
                            }));
                            report
                        } else if let Some(report) = strategy_blocked_report {
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
                                output_excerpt: None,
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
                                output_excerpt: None,
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
                                        trace(AgentLoopTraceEvent::ApprovalRequested(
                                            approval.clone(),
                                        ));
                                        let resolution = match control.scoped_approval(&approval) {
                                            Some(resolution) => resolution,
                                            None => control.wait_for_approval(&approval).await?,
                                        };
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
                                        await_runtime_operation(
                                            self.tool_executor
                                                .prepare_side_effect_snapshot(&tool_request),
                                            &control.cancellation,
                                            runtime_deadline,
                                        )
                                        .await
                                    } else {
                                        RuntimeOperationOutcome::Completed(Ok(
                                            golutra_tools::SideEffectPreparation::default(),
                                        ))
                                    };
                                    match preparation {
                                        RuntimeOperationOutcome::Cancelled => self
                                            .tool_executor
                                            .cancelled_execution_report(
                                                tool_request,
                                                policy,
                                                "tool call cancelled during side-effect preparation",
                                            ),
                                        RuntimeOperationOutcome::TimedOut => self
                                            .tool_executor
                                            .deadline_exceeded_report(
                                                tool_request,
                                                policy,
                                                "side-effect preparation",
                                            ),
                                        RuntimeOperationOutcome::Completed(Err(error)) => {
                                            let report = self.tool_executor.execution_error_report(
                                                tool_request,
                                                policy,
                                                error.to_string(),
                                            );
                                            trace(AgentLoopTraceEvent::ToolProgress(
                                                ToolProgress {
                                                    tool_call_id: report.envelope.tool_call_id,
                                                    tool_name: report.envelope.tool_name.clone(),
                                                    phase: ToolProgressPhase::Completed,
                                                    elapsed_ms: report.metrics.duration_ms,
                                                    output_bytes: report.metrics.output_bytes,
                                                    output_lines: report.metrics.output_lines,
                                                    detail: Some("error".to_owned()),
                                                    output_excerpt: None,
                                                },
                                            ));
                                            report
                                        }
                                        RuntimeOperationOutcome::Completed(Ok(preparation)) => {
                                            let checkpoint = if may_execute
                                                && (matches!(
                                                    tool_request.tool_name.as_str(),
                                                    "shell" | "delegate_task"
                                                ) || !preparation.before_images.is_empty())
                                                && let Some(recorder) =
                                                    &self.before_side_effect_recorder
                                            {
                                                await_runtime_operation(
                                                    recorder.persist_before_side_effect(
                                                        &tool_request,
                                                        &preparation.before_images,
                                                        preparation.complete,
                                                    ),
                                                    &control.cancellation,
                                                    runtime_deadline,
                                                )
                                                .await
                                            } else {
                                                RuntimeOperationOutcome::Completed(Ok(()))
                                            };
                                            match checkpoint {
                                                RuntimeOperationOutcome::Cancelled => self
                                                    .tool_executor
                                                    .cancelled_execution_report(
                                                        tool_request,
                                                        policy,
                                                        "tool call cancelled while persisting its side-effect checkpoint",
                                                    ),
                                                RuntimeOperationOutcome::TimedOut => self
                                                    .tool_executor
                                                    .deadline_exceeded_report(
                                                        tool_request,
                                                        policy,
                                                        "side-effect checkpoint",
                                                    ),
                                                RuntimeOperationOutcome::Completed(Err(error)) => self
                                                    .tool_executor
                                                    .execution_error_report(
                                                        tool_request,
                                                        policy,
                                                        format!(
                                                            "before-side-effect checkpoint failed: {error}"
                                                        ),
                                                    ),
                                                RuntimeOperationOutcome::Completed(Ok(())) => {
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
                                                    trace(AgentLoopTraceEvent::ToolProgress(
                                                        progress,
                                                    ));
                                                };
                                                let error_request = tool_request.clone();
                                                let error_policy = policy.clone();
                                                let invocation = ToolInvocation::new(
                                                    tool_request,
                                                    policy,
                                                    approved,
                                                )
                                                .with_preparation(preparation);
                                                let invocation =
                                                    if let Some(deadline) = runtime_deadline {
                                                        invocation.with_deadline(deadline)
                                                    } else {
                                                        invocation
                                                    };
                                                match self
                                                    .tool_executor
                                                    .invoke(
                                                        invocation,
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
                                        output_excerpt: None,
                                    }));
                                    report
                                }
                            }
                        };
                        (
                            provider_tool_call_id,
                            failure_signature,
                            failure_family,
                            blocked_family_failures,
                            strategy_was_blocked,
                            prepared_objective_validation,
                            tool_call_count,
                            report,
                        )
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
                            tool_calls: result_tool_call_count,
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
                        if parallel_read_batch {
                            stop_after_parallel_read_batch = true;
                        } else {
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
                    if !permits_continuation {
                        if parallel_read_batch {
                            stop_after_parallel_read_batch = true;
                        } else {
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
                }
                if stop_after_parallel_read_batch {
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
                failed_signatures_this_step
                    .retain(|signature| !successful_signatures_this_step.contains(signature));
                update_repeated_failure_streak(
                    &failed_signatures_this_step,
                    &mut repeated_failure_signature,
                    &mut repeated_failure_count,
                );
                let made_progress = tool_reports[tool_reports_before_step..]
                    .iter()
                    .any(|report| {
                        !report.changed_files.is_empty()
                            || objective_validation_report(report)
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
                if let Some(pending_turn) = control.pending_turns.try_take_steer() {
                    pending_turn_at_boundary = Some(pending_turn);
                    continue;
                }
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
            // Only clearly non-behavioral documents can skip objective validation. Unknown
            // files remain conservative because manifests, CI, templates and configuration
            // can change the delivered program without using a source-code extension.
            let behavior_files_changed = changed_files
                .iter()
                .any(|path| !is_documentation_only_file(path));
            for report in &tool_reports {
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
                if let Some(validation) = objective_validation_report(report).or_else(|| {
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
                    code_files_changed: behavior_files_changed,
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
                    iteration: governor_usage
                        .iterations
                        .saturating_add(step_machine.checkpoint().next_step_no),
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

    fn set_active_execution_surface(
        &self,
        execution_mode: Option<AgentExecutionMode>,
        tool_profile: AgentToolProfile,
    ) {
        *self
            .active_execution_surface
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = ActiveExecutionSurface {
            execution_mode,
            tool_profile,
        };
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

    fn scoped_approval(&self, request: &ApprovalRequest) -> Option<ApprovalResolution> {
        self.approval_grants.iter().find_map(|grant| {
            let matches = match grant.scope {
                ApprovalScope::Session => true,
                ApprovalScope::ResourcePrefix => {
                    grant.tool_name == request.tool_name
                        && grant.resource_prefix.as_deref().is_some_and(|prefix| {
                            approval_resource_matches(&request.tool_name, prefix, &request.resource)
                        })
                }
                ApprovalScope::Once => false,
            };
            matches.then(|| ApprovalResolution {
                approval_id: request.approval_id,
                decision: ApprovalDecision::Approved,
                scope: grant.scope,
                resource_prefix: grant.resource_prefix.clone(),
                reason: "matched an explicit scoped approval from this execution".to_owned(),
            })
        })
    }

    async fn wait_for_approval(
        &mut self,
        request: &ApprovalRequest,
    ) -> Result<ApprovalResolution, AgentLoopError> {
        loop {
            tokio::select! {
                _ = self.cancellation.cancelled() => return Err(AgentLoopError::Cancelled),
                resolution = self.approvals.recv() => {
                    let mut resolution = resolution.ok_or(AgentLoopError::Cancelled)?;
                    if resolution.approval_id == request.approval_id {
                        if resolution.decision != ApprovalDecision::Approved {
                            resolution.scope = ApprovalScope::Once;
                            resolution.resource_prefix = None;
                        } else if resolution.scope == ApprovalScope::ResourcePrefix {
                            let valid_prefix = resolution
                                .resource_prefix
                                .as_deref()
                                .filter(|prefix| !prefix.is_empty())
                                .filter(|prefix| {
                                    approval_resource_matches(
                                        &request.tool_name,
                                        prefix,
                                        &request.resource,
                                    )
                                });
                            if valid_prefix.is_none() {
                                resolution.scope = ApprovalScope::Once;
                                resolution.resource_prefix = None;
                            }
                        } else {
                            resolution.resource_prefix = None;
                        }
                        if resolution.decision == ApprovalDecision::Approved
                            && resolution.scope != ApprovalScope::Once
                        {
                            self.approval_grants.push(ApprovalGrant {
                                scope: resolution.scope,
                                tool_name: request.tool_name.clone(),
                                resource_prefix: resolution.resource_prefix.clone(),
                            });
                        }
                        return Ok(resolution);
                    }
                }
            }
        }
    }

    async fn wait_for_question(
        &mut self,
        request: &UserQuestionRequest,
    ) -> Result<UserQuestionResolution, AgentLoopError> {
        loop {
            tokio::select! {
                _ = self.cancellation.cancelled() => return Err(AgentLoopError::Cancelled),
                resolution = self.questions.recv() => {
                    let resolution = resolution.ok_or(AgentLoopError::Cancelled)?;
                    if resolution.question_id == request.question_id {
                        request
                            .validate_resolution(&resolution)
                            .map_err(AgentLoopError::UserQuestion)?;
                        return Ok(resolution);
                    }
                }
            }
        }
    }
}

fn user_question_report(
    request: ToolRequest,
    resolution: UserQuestionResolution,
) -> ToolExecutionReport {
    let content = serde_json::to_string(&resolution.answers).unwrap_or_else(|_| "[]".to_owned());
    let output_bytes = u64::try_from(content.len()).unwrap_or(u64::MAX);
    let output_lines = u64::try_from(resolution.answers.len()).unwrap_or(u64::MAX);
    ToolExecutionReport {
        envelope: ToolResultEnvelope {
            tool_call_id: request.tool_call_id,
            tool_name: request.tool_name.clone(),
            status: ToolResultStatus::Ok,
            summary: "user answered structured questions".to_owned(),
            structured_facts: json!({"answers": resolution.answers}),
            model_visible_excerpt: Some(content),
            raw_artifact_ref: None,
            evidence_refs: Vec::new(),
            risk: "p0_user_input".to_owned(),
            verification_hint: None,
        },
        artifacts: Vec::new(),
        evidence: Vec::new(),
        changed_files: Vec::new(),
        policy_evaluation: PolicyEvaluation {
            policy_ref: PolicyId::new(),
            subject: "tool".to_owned(),
            action: request.tool_name,
            resource: "interactive_user_input".to_owned(),
            decision: PolicyDecision::Allow,
            block_disposition: None,
            reason: "structured user input is mediated by the active controller".to_owned(),
            evidence_refs: Vec::new(),
        },
        artifact_contents: Vec::new(),
        before_images: Vec::new(),
        after_images: Vec::new(),
        metrics: ToolExecutionMetrics {
            output_bytes,
            output_lines,
            ..ToolExecutionMetrics::default()
        },
    }
}

#[must_use]
pub fn runtime_boundary() -> &'static str {
    "SessionCommand -> RuntimeEvent -> StateProjection -> LoopDecision"
}

#[must_use]
pub fn default_agent_max_elapsed_ms() -> u64 {
    RuntimeGovernor::default().limits().max_elapsed_ms
}

fn is_documentation_only_file(path: &Path) -> bool {
    if matches!(
        path.extension().and_then(|extension| extension.to_str()),
        Some("md" | "mdown" | "markdown" | "rst")
    ) {
        return true;
    }

    let extension = path.extension().and_then(|extension| extension.to_str());
    (extension.is_none() || extension == Some("txt"))
        && path
            .file_stem()
            .and_then(|name| name.to_str())
            .map(str::to_ascii_lowercase)
            .is_some_and(|name| {
                [
                    "authors",
                    "changelog",
                    "changes",
                    "contributors",
                    "copying",
                    "license",
                    "notice",
                    "readme",
                ]
                .iter()
                .any(|prefix| name == *prefix || name.starts_with(&format!("{prefix}.")))
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

#[cfg(test)]
use objective_evidence::{
    ObjectiveValidationKind, is_objective_validation_command, line_reports_executed_tests,
    objective_validation_command_identity, objective_validation_command_kind,
    shell_command_is_read_only,
};
use objective_evidence::{
    attach_prepared_objective_validation, explicitly_requested_inspection_validation,
    objective_validation_report, prepare_objective_validation_metadata,
};
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

fn provider_tools_for_turn(
    tools: &[ToolContract],
    contract: &TaskContract,
    profile: AgentToolProfile,
    registry: &ToolRegistry,
) -> Vec<ToolContract> {
    tools
        .iter()
        .filter(|tool| {
            tool_allowed_for_profile(&tool.tool_name, profile, registry)
                && (!matches!(
                    contract.workspace_change,
                    WorkspaceChangeRequirement::Forbidden
                ) || tool.side_effect_type == SideEffectType::None)
        })
        .cloned()
        .map(|mut tool| {
            if matches!(profile, AgentToolProfile::Coding)
                && let Some(capabilities) = registry.capabilities(&tool.tool_name)
            {
                if let Some(properties) = tool
                    .input_schema
                    .get_mut("properties")
                    .and_then(Value::as_object_mut)
                {
                    for argument in &capabilities.coding_profile_hidden_arguments {
                        properties.remove(argument);
                    }
                }
                if let Some(required) = tool
                    .input_schema
                    .get_mut("required")
                    .and_then(Value::as_array_mut)
                {
                    required.retain(|required| {
                        required.as_str().is_none_or(|required| {
                            !capabilities
                                .coding_profile_hidden_arguments
                                .iter()
                                .any(|hidden| hidden == required)
                        })
                    });
                }
            }
            tool
        })
        .collect()
}

fn tool_allowed_for_profile(
    tool_name: &str,
    profile: AgentToolProfile,
    registry: &ToolRegistry,
) -> bool {
    matches!(profile, AgentToolProfile::Full)
        || registry
            .capabilities(tool_name)
            .is_some_and(|capabilities| capabilities.available_in_coding_profile)
}

fn tool_profile_rejection_reason(
    request: &ToolRequest,
    profile: AgentToolProfile,
    registry: &ToolRegistry,
) -> Option<&'static str> {
    if !tool_allowed_for_profile(&request.tool_name, profile, registry) {
        return Some(
            "tool is not available in the active coding profile; select the full tool profile explicitly",
        );
    }
    if matches!(profile, AgentToolProfile::Coding)
        && registry
            .capabilities(&request.tool_name)
            .is_some_and(|capabilities| {
                capabilities
                    .coding_profile_hidden_arguments
                    .iter()
                    .any(|argument| request.arguments.get(argument).is_some())
            })
    {
        return Some(
            "managed shell controls require the full tool profile so their process controls remain available",
        );
    }
    None
}

fn provider_batch_is_parallel_read_candidate(
    tool_calls: &[ProviderToolCall],
    replay_context_active: bool,
    remaining_failure_budget: u32,
    profile: AgentToolProfile,
    registry: &ToolRegistry,
) -> bool {
    !replay_context_active
        && tool_calls.len() > 1
        && u32::try_from(tool_calls.len()).is_ok_and(|count| count <= remaining_failure_budget)
        && tool_calls.iter().all(|tool_call| {
            tool_allowed_for_profile(&tool_call.tool_name, profile, registry)
                && registry
                    .capabilities(&tool_call.tool_name)
                    .is_some_and(|capabilities| {
                        capabilities.parallel_read_safe
                            && (matches!(profile, AgentToolProfile::Full)
                                || capabilities
                                    .coding_profile_hidden_arguments
                                    .iter()
                                    .all(|argument| tool_call.arguments.get(argument).is_none()))
                    })
                && registry
                    .contract(&tool_call.tool_name)
                    .is_some_and(|contract| contract.side_effect_type == SideEffectType::None)
        })
}

async fn invoke_parallel_read_calls(
    tool_executor: &ToolRuntime,
    prepared: Vec<PreparedParallelReadCall>,
    cancellation: CancellationToken,
    runtime_deadline: Option<tokio::time::Instant>,
) -> VecDeque<ParallelReadOutcome> {
    let futures = prepared.into_iter().map(|prepared| {
        let executor = tool_executor.clone();
        let cancellation = cancellation.clone();
        async move {
            let PreparedParallelReadCall {
                provider_tool_call_id,
                failure_signature,
                failure_family,
                blocked_family_failures,
                request,
                policy,
                governance: _,
                tool_call_count,
            } = prepared;
            let error_request = request.clone();
            let error_policy = policy.clone();
            let mut invocation = ToolInvocation::new(request, policy, false);
            if let Some(deadline) = runtime_deadline {
                invocation = invocation.with_deadline(deadline);
            }
            let mut progress = Vec::new();
            let mut collect_progress = |event| progress.push(event);
            let report = match executor
                .invoke(invocation, cancellation, Some(&mut collect_progress))
                .await
            {
                Ok(report) => report,
                Err(error) => {
                    executor.execution_error_report(error_request, error_policy, error.to_string())
                }
            };
            ParallelReadOutcome {
                provider_tool_call_id,
                failure_signature,
                failure_family,
                blocked_family_failures,
                report,
                progress,
                tool_call_count,
            }
        }
    });
    stream::iter(futures)
        .buffered(PARALLEL_READ_CONCURRENCY_LIMIT)
        .collect::<Vec<_>>()
        .await
        .into()
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

enum RuntimeOperationOutcome<T> {
    Completed(T),
    Cancelled,
    TimedOut,
}

async fn await_runtime_operation<F>(
    operation: F,
    cancellation: &CancellationToken,
    deadline: Option<tokio::time::Instant>,
) -> RuntimeOperationOutcome<F::Output>
where
    F: std::future::Future,
{
    tokio::pin!(operation);
    match deadline {
        Some(deadline) => {
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => RuntimeOperationOutcome::Cancelled,
                _ = tokio::time::sleep_until(deadline) => RuntimeOperationOutcome::TimedOut,
                output = &mut operation => RuntimeOperationOutcome::Completed(output),
            }
        }
        None => {
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => RuntimeOperationOutcome::Cancelled,
                output = &mut operation => RuntimeOperationOutcome::Completed(output),
            }
        }
    }
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
