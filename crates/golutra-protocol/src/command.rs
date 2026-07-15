use golutra_core::{Actor, CommandId, SessionId, Timestamp};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SessionCommandKind {
    Create,
    Prompt,
    Approve,
    Deny,
    Pause,
    Resume,
    Abort,
    Takeover,
    Compact,
    MemoryRollback,
    MemoryFeedback,
    RunRegression,
    ReviewCandidate,
    ApplyCandidate,
    RollbackCandidate,
    RecordBenchmark,
    CompareCounterfactual,
    PlanEvolution,
    RunEvolution,
    StageSkill,
    ReviewSkill,
    InstallSkill,
    RollbackSkill,
    ProviderConfigured,
    ProviderAuthSubmitted,
    ProviderAuthCancelled,
    RunStorageMaintenance,
    Verify,
    Replay,
    Export,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SessionCommand {
    pub command_id: CommandId,
    pub session_id: Option<SessionId>,
    pub kind: SessionCommandKind,
    pub idempotency_key: String,
    pub actor: Actor,
    pub payload: Value,
    pub timestamp: Timestamp,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CommandAck {
    pub command_id: CommandId,
    pub accepted: bool,
    pub reason: Option<String>,
}
