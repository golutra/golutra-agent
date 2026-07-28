use golutra_core::{
    ArtifactId, CausalContext, CausalLink, EventId, RUNTIME_EVENT_SCHEMA_VERSION, SessionId,
    TaskId, TaskStatus, Timestamp, TurnId,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeEventSource {
    Runtime,
    Provider,
    Tool,
    Policy,
    Verifier,
    Memory,
    Evaluator,
    Governor,
    Evolution,
    User,
}

/// Stable semantic class for a durable runtime fact.
///
/// This is a routing aid for projections, not a disclosure permission. A
/// control or execution fact may be useful to a user projection, while an
/// evaluation or governance fact must remain outside ordinary turn context.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeEventClass {
    Control,
    Execution,
    Memory,
    Evaluation,
    Governance,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeEventType {
    CommandReceived,
    CommandCompleted,
    CommandAccepted,
    CommandRejected,
    SessionCreated,
    ThreadForked,
    ThreadRebound,
    TaskCreated,
    TurnStarted,
    StepStarted,
    StepCompleted,
    StepCheckpointed,
    TurnQueued,
    BusyPolicyDecided,
    ControllerChanged,
    ContextBuilt,
    ProviderStarted,
    ProviderStreamed,
    ProviderCompleted,
    ProviderFailed,
    TokenUsageRecorded,
    AssistantMessage,
    ToolStarted,
    ToolProgress,
    ToolCompleted,
    PolicyEvaluated,
    VerificationCompleted,
    LoopDecided,
    CheckpointCreated,
    TaskCompleted,
    TaskAbortRequested,
    TaskAborted,
    TaskInterrupted,
    TaskUncertain,
    TaskReconciled,
    TaskPaused,
    TaskResumed,
    ApprovalRequested,
    ApprovalResolved,
    RetryScheduled,
    ProviderFallback,
    ProviderTransportFallback,
    ProviderAuthRequired,
    ProviderAuthSubmitted,
    ProviderAuthCancelled,
    ProviderConfigured,
    ProviderProbeStarted,
    ProviderProbeCompleted,
    ProviderAuthFailed,
    ProviderRateLimited,
    ProviderCredentialRefreshed,
    LoopGuardTriggered,
    CompactionStarted,
    CompactionCompleted,
    CompactionFailed,
    MemoryRetrieved,
    MemoryPromoted,
    MemoryPromotionRejected,
    MemoryRolledBack,
    MemoryFeedbackRecorded,
    PostTaskReviewed,
    EvaluationCompleted,
    ImprovementCandidateCreated,
    AutomationCandidateCreated,
    CandidatePatchFrozen,
    RegressionBlocked,
    RegressionCompleted,
    PromotionDecided,
    CandidateApplied,
    CandidateRolledBack,
    BenchmarkRecorded,
    CounterfactualCompared,
    EvolutionPlanned,
    EvolutionTaskStarted,
    EvolutionTaskCompleted,
    EvolutionCompleted,
    SkillStaged,
    SkillReviewed,
    SkillInstalled,
    SkillRolledBack,
    GovernorDecided,
    StorageMaintenanceCompleted,
    ContextSnapshotCreated,
    PostTaskJobQueued,
    PostTaskJobStarted,
    PostTaskJobCompleted,
    PostTaskJobFailed,
    PostTaskStageFailed,
    VerificationPlanned,
    VerificationAssertionCompleted,
    ContinuationDecided,
    RegressionCampaignStarted,
    RegressionExecutionCompleted,
    MemoryCandidateQuarantined,
    MemoryActivated,
    MemoryInvalidated,
    FailureDiagnosed,
    FailureEpisodeRecorded,
    DiagnosticSliceCreated,
    ReplayCapsuleCreated,
    ReplayExecuted,
    ExternalEvaluationIngested,
    ExternalEvaluationCompared,
}

impl RuntimeEventType {
    #[must_use]
    pub const fn class(self) -> RuntimeEventClass {
        match self {
            Self::CommandReceived
            | Self::CommandCompleted
            | Self::CommandAccepted
            | Self::CommandRejected
            | Self::SessionCreated
            | Self::ThreadForked
            | Self::ThreadRebound
            | Self::TaskCreated
            | Self::TurnStarted
            | Self::TurnQueued
            | Self::BusyPolicyDecided
            | Self::ControllerChanged
            | Self::CheckpointCreated
            | Self::TaskCompleted
            | Self::TaskAbortRequested
            | Self::TaskAborted
            | Self::TaskInterrupted
            | Self::TaskUncertain
            | Self::TaskReconciled
            | Self::TaskPaused
            | Self::TaskResumed
            | Self::ApprovalRequested
            | Self::ApprovalResolved
            | Self::ProviderAuthRequired
            | Self::ProviderAuthSubmitted
            | Self::ProviderAuthCancelled
            | Self::ProviderConfigured
            | Self::ProviderProbeStarted
            | Self::ProviderProbeCompleted
            | Self::ProviderAuthFailed
            | Self::ProviderRateLimited
            | Self::ProviderCredentialRefreshed
            | Self::StorageMaintenanceCompleted => RuntimeEventClass::Control,
            Self::MemoryRetrieved
            | Self::MemoryPromoted
            | Self::MemoryPromotionRejected
            | Self::MemoryRolledBack
            | Self::MemoryFeedbackRecorded
            | Self::MemoryCandidateQuarantined
            | Self::MemoryActivated
            | Self::MemoryInvalidated => RuntimeEventClass::Memory,
            Self::PostTaskReviewed
            | Self::EvaluationCompleted
            | Self::BenchmarkRecorded
            | Self::CounterfactualCompared
            | Self::PostTaskJobQueued
            | Self::PostTaskJobStarted
            | Self::PostTaskJobCompleted
            | Self::PostTaskJobFailed
            | Self::PostTaskStageFailed
            | Self::FailureDiagnosed
            | Self::FailureEpisodeRecorded
            | Self::DiagnosticSliceCreated
            | Self::ReplayCapsuleCreated
            | Self::ReplayExecuted
            | Self::ExternalEvaluationIngested
            | Self::ExternalEvaluationCompared
            | Self::RegressionCampaignStarted
            | Self::RegressionExecutionCompleted => RuntimeEventClass::Evaluation,
            Self::ImprovementCandidateCreated
            | Self::AutomationCandidateCreated
            | Self::CandidatePatchFrozen
            | Self::RegressionBlocked
            | Self::RegressionCompleted
            | Self::PromotionDecided
            | Self::CandidateApplied
            | Self::CandidateRolledBack
            | Self::EvolutionPlanned
            | Self::EvolutionTaskStarted
            | Self::EvolutionTaskCompleted
            | Self::EvolutionCompleted
            | Self::SkillStaged
            | Self::SkillReviewed
            | Self::SkillInstalled
            | Self::SkillRolledBack => RuntimeEventClass::Governance,
            Self::StepStarted
            | Self::StepCompleted
            | Self::StepCheckpointed
            | Self::ContextBuilt
            | Self::ProviderStarted
            | Self::ProviderStreamed
            | Self::ProviderCompleted
            | Self::ProviderFailed
            | Self::TokenUsageRecorded
            | Self::AssistantMessage
            | Self::ToolStarted
            | Self::ToolProgress
            | Self::ToolCompleted
            | Self::PolicyEvaluated
            | Self::VerificationCompleted
            | Self::LoopDecided
            | Self::RetryScheduled
            | Self::ProviderFallback
            | Self::ProviderTransportFallback
            | Self::LoopGuardTriggered
            | Self::CompactionStarted
            | Self::CompactionCompleted
            | Self::CompactionFailed
            | Self::GovernorDecided
            | Self::ContextSnapshotCreated
            | Self::VerificationPlanned
            | Self::VerificationAssertionCompleted
            | Self::ContinuationDecided => RuntimeEventClass::Execution,
        }
    }

    /// Facts deliberately allowed into bounded historical model context.
    /// This is narrower than the execution class and excludes all projections.
    #[must_use]
    pub const fn is_model_history_fact(self) -> bool {
        matches!(
            self,
            Self::TaskCreated
                | Self::TurnQueued
                | Self::AssistantMessage
                | Self::ToolCompleted
                | Self::TaskCompleted
                | Self::TaskAborted
                | Self::TaskInterrupted
                | Self::TaskUncertain
        )
    }

    /// Facts suitable for ordinary user-facing progress, excluding debug and
    /// offline evaluation/governance noise.
    #[must_use]
    pub const fn is_user_projection_fact(self) -> bool {
        matches!(
            self,
            Self::TaskCreated
                | Self::TurnStarted
                | Self::TurnQueued
                | Self::TaskCompleted
                | Self::TaskAbortRequested
                | Self::TaskAborted
                | Self::TaskInterrupted
                | Self::TaskUncertain
                | Self::TaskReconciled
                | Self::TaskPaused
                | Self::TaskResumed
                | Self::ApprovalRequested
                | Self::ApprovalResolved
                | Self::ProviderStarted
                | Self::ProviderCompleted
                | Self::ProviderFailed
                | Self::ProviderFallback
                | Self::ProviderTransportFallback
                | Self::ProviderAuthRequired
                | Self::ProviderAuthSubmitted
                | Self::ProviderAuthCancelled
                | Self::ProviderRateLimited
                | Self::AssistantMessage
                | Self::ToolStarted
                | Self::ToolProgress
                | Self::ToolCompleted
                | Self::VerificationCompleted
                | Self::LoopDecided
                | Self::RetryScheduled
                | Self::CompactionStarted
                | Self::CompactionCompleted
                | Self::CompactionFailed
                | Self::ContinuationDecided
        )
    }

    #[must_use]
    pub const fn is_task_terminal(self) -> bool {
        matches!(
            self,
            Self::TaskCompleted | Self::TaskAborted | Self::TaskInterrupted | Self::TaskUncertain
        )
    }

    #[must_use]
    pub const fn for_terminal_status(status: TaskStatus) -> Self {
        match status {
            TaskStatus::Cancelled => Self::TaskAborted,
            TaskStatus::Interrupted => Self::TaskInterrupted,
            TaskStatus::Uncertain => Self::TaskUncertain,
            _ => Self::TaskCompleted,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RuntimeEvent {
    #[serde(default = "legacy_runtime_event_schema_version")]
    pub schema_version: u32,
    pub id: EventId,
    pub sequence_no: u64,
    pub session_id: SessionId,
    pub turn_id: Option<TurnId>,
    pub task_id: Option<TaskId>,
    pub parent_event_id: Option<EventId>,
    #[serde(default)]
    pub causal_context: CausalContext,
    #[serde(default)]
    pub causal_links: Vec<CausalLink>,
    pub event_type: RuntimeEventType,
    pub timestamp: Timestamp,
    pub source: RuntimeEventSource,
    pub payload: Value,
    pub payload_ref: Option<ArtifactId>,
    pub durable: bool,
}

impl RuntimeEvent {
    #[must_use]
    pub const fn class(&self) -> RuntimeEventClass {
        self.event_type.class()
    }
}

#[must_use]
pub const fn new_runtime_event_schema_version() -> u32 {
    RUNTIME_EVENT_SCHEMA_VERSION
}

const fn legacy_runtime_event_schema_version() -> u32 {
    1
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct EventFilter {
    pub session_id: SessionId,
    pub task_id: Option<TaskId>,
    pub after_sequence_no: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EventPageDirection {
    Forward,
    Backward,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct EventPageRequest {
    pub session_id: SessionId,
    pub task_id: Option<TaskId>,
    pub cursor: Option<u64>,
    pub direction: EventPageDirection,
    pub limit: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct EventPage {
    pub direction: EventPageDirection,
    pub events: Vec<RuntimeEvent>,
    pub start_cursor: Option<u64>,
    pub end_cursor: Option<u64>,
    pub has_more: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_classes_keep_offline_governance_out_of_execution_routing() {
        assert_eq!(
            RuntimeEventType::ToolCompleted.class(),
            RuntimeEventClass::Execution
        );
        assert_eq!(
            RuntimeEventType::EvaluationCompleted.class(),
            RuntimeEventClass::Evaluation
        );
        assert_eq!(
            RuntimeEventType::PromotionDecided.class(),
            RuntimeEventClass::Governance
        );
        assert!(RuntimeEventType::AssistantMessage.is_model_history_fact());
        assert!(!RuntimeEventType::EvaluationCompleted.is_model_history_fact());
        assert!(!RuntimeEventType::PromotionDecided.is_user_projection_fact());
    }
}
