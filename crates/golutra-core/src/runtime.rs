use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{CommandId, DecisionId, LaneId, SessionId, TaskId, TurnId, WorkspaceId};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ActorKind {
    User,
    Api,
    Tui,
    Cli,
    Sdk,
    Web,
    Ide,
    Runtime,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Actor {
    pub kind: ActorKind,
    pub id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Idle,
    Running,
    WaitingApproval,
    Pausing,
    Paused,
    Aborting,
    Completed,
    Partial,
    Failed,
    Blocked,
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BusyPolicy {
    Append,
    Inject,
    Interrupt,
    Reject,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RuntimeLane {
    pub lane_id: LaneId,
    pub workspace_id: WorkspaceId,
    pub session_id: SessionId,
    pub task_id: TaskId,
    pub active_turn_id: Option<TurnId>,
    pub active_controller: Actor,
    pub status: TaskStatus,
    pub pending_turns: Vec<TurnId>,
    pub injected_inputs: Vec<CommandId>,
    pub busy_policy_default: BusyPolicy,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct BusyPolicyDecision {
    pub decision_id: DecisionId,
    pub lane_id: LaneId,
    pub command_id: CommandId,
    pub requested_policy: BusyPolicy,
    pub applied_policy: BusyPolicy,
    pub reason: String,
    pub safe_to_inject: bool,
    pub affected_turn_id: Option<TurnId>,
}
