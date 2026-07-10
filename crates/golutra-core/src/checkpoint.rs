use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{ArtifactId, CheckpointId, TaskId, ToolCallId, TurnId, WorkspaceId};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CheckpointType {
    ShadowGit,
    Snapshot,
    External,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WorkspaceCheckpoint {
    pub checkpoint_id: CheckpointId,
    pub workspace_id: WorkspaceId,
    pub task_id: TaskId,
    pub turn_id: TurnId,
    pub checkpoint_type: CheckpointType,
    pub changed_files: Vec<String>,
    pub artifact_refs: Vec<ArtifactId>,
    pub created_before_tool_call_id: ToolCallId,
    pub restore_hint: String,
    pub retention_policy: String,
}
