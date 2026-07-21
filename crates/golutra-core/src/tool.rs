use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{ArtifactId, EvidenceId, PolicyId, ToolCallId};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SideEffectType {
    None,
    File,
    Process,
    Network,
    ExternalSystem,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ToolResultStatus {
    Ok,
    Error,
    Blocked,
    Cancelled,
    Timeout,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ToolProgressPhase {
    Started,
    Output,
    Completed,
}

/// Bounded, presentation-safe progress for one tool call.
///
/// Progress is diagnostic and may be sampled. Durable completion facts live in
/// [`ToolExecutionMetrics`], so consumers must not infer success from the last
/// progress event they happened to receive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ToolProgress {
    pub tool_call_id: ToolCallId,
    pub tool_name: String,
    pub phase: ToolProgressPhase,
    pub elapsed_ms: u64,
    pub output_bytes: u64,
    pub output_lines: u64,
    pub detail: Option<String>,
}

/// Stable execution metrics attached to every terminal tool report.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ToolExecutionMetrics {
    pub duration_ms: u64,
    pub output_bytes: u64,
    pub output_lines: u64,
    pub output_truncated: bool,
    pub exit_code: Option<i32>,
    pub item_count: Option<u64>,
    pub match_count: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ToolContract {
    pub tool_name: String,
    pub input_schema: Value,
    pub output_schema: Value,
    pub error_schema: Value,
    pub side_effect_type: SideEffectType,
    pub idempotency_key_policy: String,
    pub timeout_policy: String,
    pub cancellation_policy: String,
    pub retry_policy: String,
    pub artifact_policy: String,
    pub permission_policy_ref: Option<PolicyId>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ToolResultEnvelope {
    pub tool_call_id: ToolCallId,
    pub tool_name: String,
    pub status: ToolResultStatus,
    pub summary: String,
    pub structured_facts: Value,
    pub model_visible_excerpt: Option<String>,
    pub raw_artifact_ref: Option<ArtifactId>,
    pub evidence_refs: Vec<EvidenceId>,
    pub risk: String,
    pub verification_hint: Option<String>,
}
