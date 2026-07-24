//! Stable trace adapter emitted by `AgentLoop`.
//!
//! The loop reports execution facts through this enum and does not know how
//! they are persisted or projected.  RuntimeHost is the adapter that turns
//! these facts into canonical `RuntimeEvent` and artifact records.

use golutra_context::ContextCompactionRecord;
use golutra_core::{
    ApprovalRequest, ApprovalResolution, ContextSnapshot, ProviderRequestId, ToolCallId,
    ToolProgress, TurnId, VerificationAssertion, VerificationPlan,
};
use golutra_governor::RuntimeGovernorDecision;
use golutra_llm::{ProviderRequest, ProviderResponse, ProviderStreamEvent};
use golutra_tools::ToolExecutionReport;

use super::PendingAgentTurn;
use super::{StepCheckpoint, StepCompletion, StepSnapshot};

#[derive(Debug, Clone, PartialEq)]
pub enum AgentLoopTraceEvent {
    StepStarted(StepSnapshot),
    StepCompleted(StepCompletion),
    StepCheckpointed(StepCheckpoint),
    ContextBuilt {
        contributors: Vec<String>,
        planned_input_tokens: u64,
    },
    ContextCompacted {
        original_input_tokens: u64,
        planned_input_tokens: u64,
        trimmed_contributors: Vec<String>,
    },
    ContextCompactionStarted {
        original_input_tokens: u64,
        budget_limit: u64,
    },
    ContextAutoCompacted(ContextCompactionRecord),
    ContextCompactionFailed {
        planned_input_tokens: u64,
        budget_limit: u64,
        reason: String,
    },
    ContextSnapshot(ContextSnapshot),
    ContextSnapshotCaptured {
        snapshot: ContextSnapshot,
        request: ProviderRequest,
    },
    VerificationPlanned(VerificationPlan),
    VerificationAssertionCompleted(VerificationAssertion),
    ProviderStarted {
        request_id: ProviderRequestId,
        provider_id: String,
        model_id: String,
    },
    ProviderStreamed {
        request_id: ProviderRequestId,
        provider_id: String,
        model_id: String,
        event: ProviderStreamEvent,
    },
    ProviderCompleted {
        request_id: ProviderRequestId,
        provider_id: String,
        model_id: String,
        response: ProviderResponse,
    },
    ProviderFailed {
        request_id: ProviderRequestId,
        provider_id: String,
        model_id: String,
        error: String,
    },
    TokenUsageRecorded(golutra_core::TokenUsageRecord),
    ToolStarted {
        tool_call_id: ToolCallId,
        provider_tool_call_id: Option<String>,
        tool_name: String,
        display_arguments: serde_json::Value,
    },
    ToolProgress(ToolProgress),
    ToolCompleted(ToolExecutionReport),
    PolicyEvaluated(golutra_core::PolicyEvaluation),
    ApprovalRequested(ApprovalRequest),
    ApprovalResolved(ApprovalResolution),
    RetryScheduled {
        attempt: u32,
        reason: String,
    },
    ProviderFallback {
        from_provider: String,
        to_provider: String,
        reason: String,
    },
    ProviderTransportFallback {
        provider_id: String,
        from_transport: String,
        to_transport: String,
        reason: String,
    },
    LoopGuardTriggered {
        trigger: golutra_core::LoopGuardTrigger,
        reason: String,
    },
    GovernorDecided(RuntimeGovernorDecision),
    PendingTurnStarted(PendingAgentTurn),
    AssistantMessage {
        turn_id: TurnId,
        content: String,
    },
}

/// A typed execution fact emitted by `AgentLoop` before it is translated into
/// a canonical runtime event. The alias keeps the execution layer independent
/// from persistence and projection concerns.
pub type RuntimeObservation = AgentLoopTraceEvent;

/// The observation seam used by the loop and by deterministic test adapters.
///
/// Runtime hosts normally provide a channel-backed adapter while tests may use
/// a closure or an in-memory collector. Implementations must preserve emission
/// order and must not perform blocking IO in `emit`.
pub trait RuntimeObservationSink: Send {
    fn emit(&mut self, observation: RuntimeObservation);
}

impl<F> RuntimeObservationSink for F
where
    F: FnMut(RuntimeObservation) + Send,
{
    fn emit(&mut self, observation: RuntimeObservation) {
        self(observation);
    }
}
