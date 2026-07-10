use golutra_core::{ActorKind, QueryId, SessionId, TaskId, Timestamp};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeQueryKind {
    SessionState,
    TaskState,
    UserProjection,
    DebugProjection,
    ReplayCursor,
    MemoryList,
    EvaluationResults,
    ImprovementCandidates,
    AutomationCandidates,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RuntimeQuery {
    pub query_id: QueryId,
    pub session_id: SessionId,
    pub task_id: Option<TaskId>,
    pub kind: RuntimeQueryKind,
    pub requester: ActorKind,
    pub cursor: Option<u64>,
    pub timestamp: Timestamp,
}
