//! Background execution facts are independent of Agent turn and delivery lifetimes.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{
    BackgroundBatchId, BackgroundResultId, BackgroundTaskId, SessionId, TaskId, Timestamp,
    ToolCallId, ToolResultEnvelope, TurnId, WorkspaceId,
};

pub const MAX_BACKGROUND_BATCH_TASKS: usize = 64;
pub const MAX_BACKGROUND_PENDING_RESULTS: usize = 128;
pub const MAX_BACKGROUND_RESULT_BYTES: usize = 128 * 1024;
pub const MAX_BACKGROUND_SUMMARY_BYTES: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub struct BackgroundScope {
    pub workspace_id: WorkspaceId,
    pub session_id: SessionId,
    pub task_id: TaskId,
    pub turn_id: TurnId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BackgroundTaskStatus {
    Queued,
    Running,
    Succeeded,
    Failed,
    Cancelled,
    TimedOut,
    WaitingForUser,
    OutcomeUnknown,
}

impl BackgroundTaskStatus {
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Succeeded
                | Self::Failed
                | Self::Cancelled
                | Self::TimedOut
                | Self::OutcomeUnknown
        )
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::TimedOut => "timed_out",
            Self::WaitingForUser => "waiting_for_user",
            Self::OutcomeUnknown => "outcome_unknown",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct BackgroundBatch {
    pub batch_id: BackgroundBatchId,
    pub scope: BackgroundScope,
    /// Host-owned continuation epoch; user stop/replacement invalidates an old epoch.
    pub generation: u64,
    pub created_at: Timestamp,
    pub deadline_at: Timestamp,
    pub sealed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct BackgroundTaskRecord {
    pub background_task_id: BackgroundTaskId,
    pub batch_id: BackgroundBatchId,
    pub scope: BackgroundScope,
    pub tool_call_id: ToolCallId,
    pub tool_name: String,
    pub backend: String,
    pub status: BackgroundTaskStatus,
    pub created_at: Timestamp,
    pub deadline_at: Timestamp,
}

/// Model-visible observation, never a user instruction. Large bodies belong in artifacts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct BackgroundResult {
    pub result_id: BackgroundResultId,
    pub background_task_id: BackgroundTaskId,
    pub batch_id: BackgroundBatchId,
    pub scope: BackgroundScope,
    pub tool_call_id: ToolCallId,
    pub tool_name: String,
    pub status: BackgroundTaskStatus,
    pub summary: String,
    pub output: ToolResultEnvelope,
    pub artifact_refs: Vec<String>,
    pub created_at: Timestamp,
}

/// A durable admission claim. Only its owner can commit or release it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackgroundWakeClaim {
    pub batch: BackgroundBatch,
    pub owner: String,
    pub lease_expires_at: Timestamp,
}
