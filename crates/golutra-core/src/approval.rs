use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{ApprovalId, TaskId, ToolCallId, TurnId};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ApprovalRequest {
    pub approval_id: ApprovalId,
    pub task_id: TaskId,
    pub turn_id: TurnId,
    pub tool_call_id: ToolCallId,
    pub tool_name: String,
    pub resource: String,
    pub reason: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecision {
    Approved,
    Denied,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ApprovalResolution {
    pub approval_id: ApprovalId,
    pub decision: ApprovalDecision,
    pub reason: String,
}
