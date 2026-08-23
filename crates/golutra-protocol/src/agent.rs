//! Agent-facing turn and stream contracts.
//!
//! RuntimeEvent remains the durable source of truth.  These types are a
//! deliberately small presentation contract for `exec`, SDKs, MCP and rich
//! clients; adapters must never create a second task state machine.

use golutra_core::{
    CommandId, SessionId, TaskContract, TaskId, TaskOutcome, TaskStatus, ThreadId, Timestamp,
    TurnId, VerificationRecord,
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
    #[serde(default)]
    pub outcome: Option<TaskOutcome>,
    pub last_sequence_no: Option<u64>,
}

/// Selects how much deterministic completion policy is allowed to shape one
/// turn.  The open path leaves planning, tool order, and stopping to the
/// provider while retaining the runtime's safety, budget, and audit gates.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AgentExecutionMode {
    #[default]
    Open,
    Strict,
}

/// Controls the model-visible tool surface without removing the underlying
/// executor capability or its policy checks.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AgentToolProfile {
    Coding,
    #[default]
    Full,
}

/// Optional execution-surface override for a steering continuation. Steering
/// cannot replace the active task contract or execution mode.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AgentSteerOptions {
    #[serde(default)]
    pub tool_profile: Option<AgentToolProfile>,
}

/// Selects the model-facing execution surface for a newly started turn.
///
/// This is separate from [`AgentTurnOptions`] so adding execution profiles does
/// not break existing Rust callers that construct that options type directly.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AgentTurnExecutionOptions {
    /// Open is the default for new clients. Strict is intended for explicit
    /// completion policy or externally verified turns.
    #[serde(default)]
    pub execution_mode: AgentExecutionMode,
    /// Full exposes every registered extension; coding is an explicit
    /// restriction for callers that need a narrower model-visible surface.
    #[serde(default)]
    pub tool_profile: AgentToolProfile,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct AgentTurnOptions {
    /// Explicit runtime completion/verification contract.  `None` keeps wire
    /// compatibility for older clients; the application adapter supplies a
    /// normalized default before execution.
    #[serde(default)]
    pub task_contract: Option<TaskContract>,
    #[serde(default)]
    pub output_schema: Option<Value>,
    #[serde(default)]
    pub completion_criteria: Vec<String>,
    /// Optional wall-clock budget for this turn. Active provider sessions and
    /// newly scheduled provider, tool, verifier, or correction work are
    /// bounded by this deadline so callers can retain a terminal candidate
    /// before an outer harness timeout.
    #[serde(default)]
    #[schemars(range(min = 1))]
    pub max_elapsed_ms: Option<u64>,
    /// Request network access for child tools. The runtime host may still
    /// reject this request when its capability is disabled.
    #[serde(default)]
    pub allow_network: bool,
    /// Disable workspace, sensitive-path, shell and OS sandbox restrictions
    /// for this turn. Network environment remains a separate host capability,
    /// but process-only execution cannot enforce OS-level network isolation.
    #[serde(default)]
    pub yolo: bool,
    /// Caller-owned commands that objectively verify the candidate workspace
    /// after the model stops. These commands are argv-based and are never
    /// interpreted by a shell.
    #[serde(default)]
    pub external_verifiers: Vec<ExternalVerificationSpec>,
    /// Discover conservative project checks when no explicit verifier list is
    /// supplied. Set this to false to send an explicit empty list.
    #[serde(default = "default_project_verifier_discovery")]
    pub discover_project_verifiers: bool,
    /// Keep the typed outcome open for a later evaluator overlay.
    #[serde(default)]
    pub defer_external_verification: bool,
}

impl Default for AgentTurnOptions {
    fn default() -> Self {
        Self {
            task_contract: None,
            output_schema: None,
            completion_criteria: Vec::new(),
            max_elapsed_ms: None,
            allow_network: false,
            yolo: false,
            external_verifiers: Vec::new(),
            discover_project_verifiers: true,
            defer_external_verification: false,
        }
    }
}

const fn default_project_verifier_discovery() -> bool {
    true
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
