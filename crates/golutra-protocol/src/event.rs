use golutra_core::{
    ArtifactId, CausalContext, CausalLink, EventId, RUNTIME_EVENT_SCHEMA_VERSION, SessionId,
    TaskId, TaskStatus, Timestamp, TurnId, UserQuestionRequest,
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
    ThreadRenamed,
    ThreadArchived,
    ThreadDeleted,
    TaskCreated,
    TurnStarted,
    StepStarted,
    StepCompleted,
    StepCheckpointed,
    TurnQueued,
    TurnUpdated,
    TurnCancelled,
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
    UserQuestionRequested,
    UserQuestionResolved,
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
    CandidateReady,
    VerificationReady,
    ExternalVerificationRequested,
    ExternalVerificationFeedback,
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
            | Self::ThreadRenamed
            | Self::ThreadArchived
            | Self::ThreadDeleted
            | Self::TaskCreated
            | Self::TurnStarted
            | Self::TurnQueued
            | Self::TurnUpdated
            | Self::TurnCancelled
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
            | Self::UserQuestionRequested
            | Self::UserQuestionResolved
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
            | Self::ExternalVerificationFeedback
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
            Self::CandidateReady
            | Self::VerificationReady
            | Self::ExternalVerificationRequested => RuntimeEventClass::Execution,
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
                | Self::TurnUpdated
                | Self::AssistantMessage
                | Self::ToolCompleted
                | Self::TaskCompleted
                | Self::TaskAborted
                | Self::TaskInterrupted
                | Self::TaskUncertain
                | Self::CandidateReady
                | Self::VerificationReady
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
                | Self::TurnUpdated
                | Self::TurnCancelled
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
                | Self::UserQuestionRequested
                | Self::UserQuestionResolved
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
                | Self::CandidateReady
                | Self::VerificationReady
                | Self::ExternalVerificationRequested
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

/// Returns the latest structured question that is still owned by an active task.
///
/// Question events are durable, so merely finding the latest request is not
/// enough: a later terminal task event also closes the in-memory response
/// channel even when no explicit resolution event was written.
#[must_use]
pub fn pending_user_question(
    events: &[RuntimeEvent],
    active_task_id: Option<TaskId>,
) -> Option<UserQuestionRequest> {
    let request_event = events.iter().rev().find(|event| {
        event.event_type == RuntimeEventType::UserQuestionRequested
            && active_task_id.is_none_or(|task_id| event.task_id == Some(task_id))
    })?;
    let request = request_event
        .payload
        .get("request")
        .cloned()
        .and_then(|value| serde_json::from_value::<UserQuestionRequest>(value).ok())?;
    if request_event.task_id != Some(request.task_id) {
        return None;
    }
    let question_id = request.question_id.to_string();
    let closed = events.iter().any(|event| {
        event.sequence_no > request_event.sequence_no
            && ((event.task_id == Some(request.task_id) && event.event_type.is_task_terminal())
                || (event.event_type == RuntimeEventType::UserQuestionResolved
                    && event.payload.get("question_id").and_then(Value::as_str)
                        == Some(question_id.as_str())))
    });
    (!closed).then_some(request)
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
    use golutra_core::{
        QuestionId, ToolCallId, UserQuestionMode, UserQuestionOption, UserQuestionPrompt,
    };
    use serde_json::json;

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

    #[test]
    fn pending_question_is_closed_by_resolution_or_a_terminal_task_event() {
        let session_id = SessionId::new();
        let task_id = TaskId::new();
        let request = UserQuestionRequest {
            question_id: QuestionId::new(),
            task_id,
            turn_id: TurnId::new(),
            tool_call_id: ToolCallId::new(),
            questions: vec![UserQuestionPrompt {
                id: "format".to_owned(),
                header: "Output".to_owned(),
                question: "Choose the output format".to_owned(),
                mode: UserQuestionMode::Single,
                options: vec![
                    UserQuestionOption {
                        id: "json".to_owned(),
                        label: "JSON".to_owned(),
                        description: None,
                    },
                    UserQuestionOption {
                        id: "text".to_owned(),
                        label: "Text".to_owned(),
                        description: None,
                    },
                ],
            }],
        };
        let requested = test_event(
            1,
            session_id,
            task_id,
            RuntimeEventType::UserQuestionRequested,
            json!({"request": request}),
        );

        assert_eq!(
            pending_user_question(std::slice::from_ref(&requested), Some(task_id))
                .map(|pending| pending.question_id),
            Some(request.question_id)
        );
        assert!(
            pending_user_question(std::slice::from_ref(&requested), Some(TaskId::new())).is_none()
        );

        let unrelated_terminal = test_event(
            2,
            session_id,
            TaskId::new(),
            RuntimeEventType::TaskCompleted,
            json!({}),
        );
        assert!(
            pending_user_question(&[requested.clone(), unrelated_terminal], Some(task_id))
                .is_some()
        );

        let terminal = test_event(
            3,
            session_id,
            task_id,
            RuntimeEventType::TaskCompleted,
            json!({}),
        );
        assert!(pending_user_question(&[requested.clone(), terminal], Some(task_id)).is_none());

        let resolved = test_event(
            2,
            session_id,
            task_id,
            RuntimeEventType::UserQuestionResolved,
            json!({"question_id": request.question_id}),
        );
        assert!(pending_user_question(&[requested, resolved], Some(task_id)).is_none());
    }

    fn test_event(
        sequence_no: u64,
        session_id: SessionId,
        task_id: TaskId,
        event_type: RuntimeEventType,
        payload: Value,
    ) -> RuntimeEvent {
        RuntimeEvent {
            schema_version: RUNTIME_EVENT_SCHEMA_VERSION,
            id: EventId::new(),
            sequence_no,
            session_id,
            turn_id: None,
            task_id: Some(task_id),
            parent_event_id: None,
            causal_context: CausalContext::default(),
            causal_links: Vec::new(),
            event_type,
            timestamp: chrono::Utc::now(),
            source: RuntimeEventSource::Runtime,
            payload,
            payload_ref: None,
            durable: true,
        }
    }
}
