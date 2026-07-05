use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ProviderContract {
    pub provider_id: String,
    pub model_id: String,
    pub native_protocol: String,
    pub stream_event_mapping: String,
    pub tool_call_mapping: String,
    pub usage_mapping: String,
    pub reasoning_mapping: String,
    pub finish_reason_mapping: String,
    pub error_mapping: String,
    pub rate_limit_mapping: String,
    pub cost_model: String,
    pub capability_matrix_ref: Option<String>,
    pub golden_fixture_refs: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ProviderUsage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
    pub cached_input_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
    pub usage_source: UsageSource,
    pub raw: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum UsageSource {
    Provider,
    Estimated,
    Unknown,
}
