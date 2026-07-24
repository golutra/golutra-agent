use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{
    EventId, ProviderRequestId, ProviderResponseId, RegressionCampaignId, RunId, SessionId, TaskId,
    Timestamp, ToolCallId, TurnId, VerificationId, WorkspaceId,
};

pub const RUNTIME_EVENT_SCHEMA_VERSION: u32 = 2;
pub const BUILD_PROVENANCE_SCHEMA_VERSION: u32 = 1;
pub const RUN_PROVENANCE_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CausalRelation {
    Parent,
    TriggeredBy,
    RespondsTo,
    DerivedFrom,
    Verifies,
    Compares,
    Supersedes,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CausalLink {
    pub event_id: EventId,
    pub relation: CausalRelation,
}

/// Correlation identifiers propagated through one governed runtime execution.
///
/// The event envelope remains authoritative for session/task/turn ownership.
/// Repeating those identifiers here makes detached facts self-describing and
/// lets integrity validation reject mismatched context rather than guessing.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CausalContext {
    pub run_id: Option<RunId>,
    pub workspace_id: Option<WorkspaceId>,
    pub session_id: Option<SessionId>,
    pub task_id: Option<TaskId>,
    pub turn_id: Option<TurnId>,
    pub step_id: Option<String>,
    pub step_no: Option<u32>,
    pub provider_round_id: Option<String>,
    pub provider_request_id: Option<ProviderRequestId>,
    pub provider_response_id: Option<ProviderResponseId>,
    pub provider_tool_call_id: Option<String>,
    pub tool_call_id: Option<ToolCallId>,
    pub verification_id: Option<VerificationId>,
    pub candidate_id: Option<String>,
    pub regression_campaign_id: Option<RegressionCampaignId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct BuildProvenance {
    pub schema_version: u32,
    pub package_version: String,
    pub git_commit: Option<String>,
    pub dirty: bool,
    pub source_digest: Option<String>,
    pub cargo_lock_digest: Option<String>,
    pub target: String,
    pub profile: String,
    pub features: Vec<String>,
    pub rustc_version: String,
    pub binary_checksum: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RunProvenance {
    pub schema_version: u32,
    pub run_id: RunId,
    pub runtime_identity: String,
    pub build: BuildProvenance,
    pub runtime_config_digest: Option<String>,
    pub provider_config_digest: Option<String>,
    pub tool_manifest_digest: Option<String>,
    pub policy_digest: Option<String>,
    pub verifier_digest: Option<String>,
    pub workspace_initial_digest: Option<String>,
    pub captured_at: Timestamp,
}
