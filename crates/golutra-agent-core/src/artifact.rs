use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{ArtifactId, EventId, EvidenceId, SessionId, Timestamp, ToolCallId, TurnId};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RedactionStatus {
    Raw,
    Redacted,
    NotRequired,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ArtifactRecord {
    pub artifact_id: ArtifactId,
    pub session_id: SessionId,
    pub turn_id: Option<TurnId>,
    pub tool_call_id: Option<ToolCallId>,
    pub artifact_type: String,
    pub uri: String,
    pub checksum: String,
    pub size_bytes: u64,
    pub created_at: Timestamp,
    pub producer: String,
    pub redaction_status: RedactionStatus,
    pub retention_policy: String,
    pub provenance_refs: Vec<EventId>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct EvidenceRecord {
    pub evidence_id: EvidenceId,
    pub claim: String,
    pub artifact_refs: Vec<ArtifactId>,
    pub source_event_refs: Vec<EventId>,
    pub evidence_strength: EvidenceStrength,
    pub verifier: String,
    pub confidence: f32,
    pub limitations: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceStrength {
    Weak,
    Medium,
    Strong,
}
