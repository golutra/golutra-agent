use golutra_core::{
    ArtifactRecord, BusyPolicyDecision, EvidenceRecord, LoopDecision, RuntimeLane, SessionId,
    TaskId, TaskStatus, ToolResultEnvelope, VerificationRecord,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::RuntimeEvent;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct StateProjection {
    pub session_id: SessionId,
    pub active_task_id: Option<TaskId>,
    pub task_status: TaskStatus,
    pub runtime_lane: Option<RuntimeLane>,
    pub last_sequence_no: u64,
    pub visible_steps: Vec<VisibleStep>,
    pub pending_approval: Option<String>,
    pub last_loop_decision: Option<LoopDecision>,
    pub last_verification: Option<VerificationRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct VisibleStep {
    pub label: String,
    pub status: String,
    pub summary: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct UserProjection {
    pub session_id: SessionId,
    pub task_id: Option<TaskId>,
    pub status: TaskStatus,
    pub visible_steps: Vec<VisibleStep>,
    pub pending_approval: Option<String>,
    pub final_message: Option<String>,
    pub residual_risks: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct DebugProjection {
    pub session_id: SessionId,
    pub task_id: Option<TaskId>,
    pub events: Vec<RuntimeEvent>,
    pub busy_policy_decisions: Vec<BusyPolicyDecision>,
    pub tool_results: Vec<ToolResultEnvelope>,
    pub artifacts: Vec<ArtifactRecord>,
    pub evidence: Vec<EvidenceRecord>,
    pub verification: Option<VerificationRecord>,
    pub loop_decisions: Vec<LoopDecision>,
}
