use golutra_core::{ArtifactId, EventId, SessionId, TaskId, Timestamp, TurnId};
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
    User,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeEventType {
    CommandAccepted,
    CommandRejected,
    SessionCreated,
    TaskCreated,
    TurnStarted,
    TurnQueued,
    BusyPolicyDecided,
    ContextBuilt,
    ProviderStarted,
    ProviderStreamed,
    ProviderCompleted,
    TokenUsageRecorded,
    AssistantMessage,
    ToolStarted,
    ToolCompleted,
    PolicyEvaluated,
    VerificationCompleted,
    LoopDecided,
    CheckpointCreated,
    TaskCompleted,
    TaskAbortRequested,
    TaskAborted,
    TaskPaused,
    TaskResumed,
    ApprovalRequested,
    ApprovalResolved,
    RetryScheduled,
    ProviderFallback,
    LoopGuardTriggered,
    CompactionCompleted,
    MemoryRetrieved,
    MemoryPromoted,
    MemoryPromotionRejected,
    MemoryRolledBack,
    PostTaskReviewed,
    EvaluationCompleted,
    ImprovementCandidateCreated,
    AutomationCandidateCreated,
    RegressionCompleted,
    PromotionDecided,
    CandidateApplied,
    CandidateRolledBack,
    GovernorDecided,
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
