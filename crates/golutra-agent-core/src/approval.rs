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

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalScope {
    #[default]
    Once,
    ResourcePrefix,
    Session,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ApprovalResolution {
    pub approval_id: ApprovalId,
    pub decision: ApprovalDecision,
    #[serde(default)]
    pub scope: ApprovalScope,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource_prefix: Option<String>,
    pub reason: String,
}
