use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{
    CommandId, DecisionId, EventId, LaneId, SessionId, TaskId, Timestamp, ToolCallId, TurnId,
    WorkspaceId,
};

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
    WaitingAuthentication,
    Pausing,
    Paused,
    Aborting,
    Completed,
    Partial,
    Failed,
    Blocked,
    Cancelled,
    Interrupted,
    Uncertain,
}

impl TaskStatus {
    #[must_use]
    pub const fn is_active(self) -> bool {
        matches!(
            self,
            Self::Running
                | Self::WaitingApproval
                | Self::WaitingAuthentication
                | Self::Pausing
                | Self::Paused
                | Self::Aborting
        )
    }

    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed
                | Self::Partial
                | Self::Failed
                | Self::Blocked
                | Self::Cancelled
                | Self::Interrupted
                | Self::Uncertain
        )
    }

    #[must_use]
    pub const fn requires_reconciliation(self) -> bool {
        matches!(self, Self::Uncertain)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TaskRecoveryDisposition {
    Interrupted,
    Uncertain,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct IncompleteToolCall {
    pub tool_call_id: ToolCallId,
    pub tool_name: String,
    pub side_effect_possible: bool,
    pub started_event_ref: EventId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TaskRecoveryRecord {
    pub task_id: TaskId,
    pub disposition: TaskRecoveryDisposition,
    pub interrupted_turn_ids: Vec<TurnId>,
    pub incomplete_tool_calls: Vec<IncompleteToolCall>,
    pub running_process_ids: Vec<String>,
    pub checkpoint_event_refs: Vec<EventId>,
    pub last_event_ref: Option<EventId>,
    pub previous_runtime_identity: Option<String>,
    pub recovering_runtime_identity: String,
    pub safe_to_replay: bool,
    pub reconciliation_required: bool,
    pub reason: String,
    pub detected_at: Timestamp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TaskReconciliationDecision {
    NoSideEffectObserved,
    SideEffectObserved,
    Abandon,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TaskReconciliationRecord {
    pub task_id: TaskId,
    pub recovery_event_ref: EventId,
    pub decision: TaskReconciliationDecision,
    pub resulting_status: TaskStatus,
    pub note: Option<String>,
    pub reconciled_by: Actor,
    pub reconciled_at: Timestamp,
    pub resumed_pending_turns: bool,
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

#[cfg(test)]
mod tests {
    use super::TaskStatus;

    #[test]
    fn task_status_predicates_keep_recovery_states_terminal_and_inactive() {
        assert!(TaskStatus::Running.is_active());
        assert!(!TaskStatus::Running.is_terminal());
        assert!(!TaskStatus::Interrupted.is_active());
        assert!(TaskStatus::Interrupted.is_terminal());
        assert!(TaskStatus::Uncertain.requires_reconciliation());
        assert!(!TaskStatus::Interrupted.requires_reconciliation());
    }
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
