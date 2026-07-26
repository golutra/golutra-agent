//! Agent-facing turn and stream contracts.
//!
//! RuntimeEvent remains the durable source of truth.  These types are a
//! deliberately small presentation contract for `exec`, SDKs, MCP and rich
//! clients; adapters must never create a second task state machine.

use golutra_core::{
    CommandId, SessionId, TaskId, TaskStatus, ThreadId, Timestamp, TurnId, VerificationRecord,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct AgentThreadRef {
    pub thread_id: ThreadId,
    pub session_id: SessionId,
    pub workspace_root: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct AgentTurnStart {
    pub thread_id: ThreadId,
    pub session_id: SessionId,
    pub command_id: CommandId,
    pub task_id: Option<TaskId>,
    pub turn_id: Option<TurnId>,
    pub accepted: bool,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct AgentTurnStartResponse {
    pub attachment_id: String,
    pub thread: AgentThreadRef,
    pub command_id: CommandId,
    pub accepted: bool,
    pub reason: Option<String>,
    pub cursor: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AgentItemKind {
    UserMessage,
    AssistantMessage,
    Model,
    Tool,
    Approval,
    Verification,
    Runtime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AgentItemStatus {
    InProgress,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct AgentItem {
    pub id: String,
    pub kind: AgentItemKind,
    pub status: AgentItemStatus,
    pub title: String,
    pub content: Option<String>,
    pub data: Value,
    pub runtime_event_id: Option<String>,
    pub sequence_no: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type")]
pub enum AgentStreamEvent {
    #[serde(rename = "thread.started")]
    ThreadStarted {
        thread_id: ThreadId,
        session_id: SessionId,
        workspace_root: Option<String>,
        timestamp: Timestamp,
    },
    #[serde(rename = "turn.started")]
    TurnStarted {
        thread_id: ThreadId,
        session_id: SessionId,
        task_id: Option<TaskId>,
        turn_id: Option<TurnId>,
        timestamp: Timestamp,
    },
    #[serde(rename = "item.started")]
    ItemStarted { item: AgentItem },
    #[serde(rename = "item.updated")]
    ItemUpdated { item: AgentItem },
    #[serde(rename = "item.completed")]
    ItemCompleted { item: AgentItem },
    #[serde(rename = "runtime.event")]
    RuntimeEvent { event: crate::RuntimeEvent },
    #[serde(rename = "turn.completed")]
    TurnCompleted {
        thread_id: ThreadId,
        session_id: SessionId,
        task_id: Option<TaskId>,
        turn_id: Option<TurnId>,
        status: TaskStatus,
        final_message: Option<String>,
        verification: Option<VerificationRecord>,
        last_sequence_no: Option<u64>,
        timestamp: Timestamp,
    },
    #[serde(rename = "turn.failed")]
    TurnFailed {
        thread_id: ThreadId,
        session_id: SessionId,
        task_id: Option<TaskId>,
        turn_id: Option<TurnId>,
        status: TaskStatus,
        error: String,
        final_message: Option<String>,
        verification: Option<VerificationRecord>,
        last_sequence_no: Option<u64>,
        timestamp: Timestamp,
    },
}

impl AgentStreamEvent {
    #[must_use]
    pub fn event_type(&self) -> &'static str {
        match self {
            Self::ThreadStarted { .. } => "thread.started",
            Self::TurnStarted { .. } => "turn.started",
            Self::ItemStarted { .. } => "item.started",
            Self::ItemUpdated { .. } => "item.updated",
            Self::ItemCompleted { .. } => "item.completed",
            Self::RuntimeEvent { .. } => "runtime.event",
            Self::TurnCompleted { .. } => "turn.completed",
            Self::TurnFailed { .. } => "turn.failed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct AgentTurnResult {
    pub thread_id: ThreadId,
    pub session_id: SessionId,
    pub task_id: Option<TaskId>,
    pub turn_id: Option<TurnId>,
    pub status: TaskStatus,
    pub final_message: Option<String>,
    pub verification: Option<VerificationRecord>,
    pub last_sequence_no: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct AgentTurnOptions {
    #[serde(default)]
    pub output_schema: Option<Value>,
    #[serde(default)]
    pub completion_criteria: Vec<String>,
    /// Request network access for child tools. The runtime host may still
    /// reject this request when its capability is disabled.
    #[serde(default)]
    pub allow_network: bool,
    /// Caller-owned commands that objectively verify the candidate workspace
    /// after the model stops. These commands are argv-based and are never
    /// interpreted by a shell.
    #[serde(default)]
    pub external_verifiers: Vec<ExternalVerificationSpec>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ExternalVerificationSpec {
    pub program: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default = "default_verifier_cwd")]
    pub cwd: String,
    #[serde(default = "default_verifier_timeout_ms")]
    pub timeout_ms: u64,
    #[serde(default)]
    pub expected_exit_code: i32,
    #[serde(default = "default_verifier_output_bytes")]
    pub max_output_bytes: usize,
}

fn default_verifier_cwd() -> String {
    ".".to_owned()
}

const fn default_verifier_timeout_ms() -> u64 {
    120_000
}

const fn default_verifier_output_bytes() -> usize {
    256 * 1024
}
