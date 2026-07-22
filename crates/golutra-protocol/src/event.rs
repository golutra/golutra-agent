use golutra_core::{ArtifactId, EventId, SessionId, TaskId, TaskStatus, Timestamp, TurnId};
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
    VerificationPlanned,
    VerificationAssertionCompleted,
    RegressionCampaignStarted,
    RegressionExecutionCompleted,
    MemoryCandidateQuarantined,
    MemoryActivated,
    MemoryInvalidated,
}

impl RuntimeEventType {
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
    pub id: EventId,
    pub sequence_no: u64,
    pub session_id: SessionId,
    pub turn_id: Option<TurnId>,
    pub task_id: Option<TaskId>,
    pub parent_event_id: Option<EventId>,
    pub event_type: RuntimeEventType,
    pub timestamp: Timestamp,
    pub source: RuntimeEventSource,
    pub payload: Value,
    pub payload_ref: Option<ArtifactId>,
    pub durable: bool,
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
