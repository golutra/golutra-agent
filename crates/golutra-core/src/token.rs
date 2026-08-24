use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{
    CacheIdentity, NormalizedUsage, ProviderRequestId, ProviderResponseId, SessionId, TaskId,
    TokenBudgetSnapshotId, TurnId,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BudgetOverflowAction {
    Trim,
    Compact,
    AskUser,
    Block,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TokenBudgetSnapshot {
    pub snapshot_id: TokenBudgetSnapshotId,
    pub task_id: TaskId,
    pub turn_id: TurnId,
    pub context_window: u64,
    pub max_output: u64,
    pub reserved_output_tokens: u64,
    pub planned_input_tokens: u64,
    pub planned_tool_tokens: u64,
    pub planned_summary_tokens: u64,
    pub budget_limit: u64,
    pub budget_policy: String,
    pub action_if_exceeded: BudgetOverflowAction,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct TokenUsageRecord {
    #[serde(default)]
    pub session_id: Option<SessionId>,
    pub task_id: TaskId,
    pub turn_id: TurnId,
    pub provider_id: String,
    pub model_id: String,
    pub request_event_id: ProviderRequestId,
    pub response_event_id: ProviderResponseId,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
    pub cached_input_tokens: Option<u64>,
    pub tool_result_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
    pub estimated_cost: Option<f64>,
    pub budget_snapshot_ref: TokenBudgetSnapshotId,
    pub attribution_ref: Option<TokenAttribution>,
    pub usage_source: String,
    #[serde(default)]
    pub cache_read_tokens: Option<u64>,
    #[serde(default)]
    pub cache_write_tokens: Option<u64>,
    #[serde(default)]
    pub non_cached_input_tokens: Option<u64>,
    #[serde(default)]
    pub tool_schema_tokens_estimated: Option<u64>,
    #[serde(default)]
    pub tool_result_tokens_estimated: Option<u64>,
    #[serde(default)]
    pub tool_estimated_tokens: Option<u64>,
    #[serde(default)]
    pub provider_total_tokens: Option<u64>,
    #[serde(default)]
    pub usage_complete: bool,
    #[serde(default)]
    pub cache_identity: Option<CacheIdentity>,
}

impl TokenUsageRecord {
    #[must_use]
    pub fn usage(&self) -> NormalizedUsage {
        NormalizedUsage {
            input_tokens_total: self.input_tokens,
            input_tokens_non_cached: self.non_cached_input_tokens.or_else(|| {
                self.input_tokens
                    .zip(self.cached_input_tokens)
                    .and_then(|(input, cached)| (cached <= input).then(|| input - cached))
            }),
            cache_read_tokens: self.cache_read_tokens.or(self.cached_input_tokens),
            cache_write_tokens: self.cache_write_tokens,
            output_tokens: self.output_tokens,
            reasoning_tokens: self.reasoning_tokens,
            provider_total_tokens: self.provider_total_tokens.or(self.total_tokens),
            estimated_cost: self.estimated_cost,
            tool_schema_tokens_estimated: self.tool_schema_tokens_estimated,
            tool_result_tokens_estimated: self
                .tool_result_tokens_estimated
                .or(self.tool_estimated_tokens)
                .or(self.tool_result_tokens),
            usage_source: match self.usage_source.as_str() {
                "provider" => crate::UsageSource::Provider,
                "estimated" => crate::UsageSource::Estimated,
                _ => crate::UsageSource::Unknown,
            },
            // 旧记录没有该字段时，用已知输入/输出恢复完整性；缺失的
            // cache/total 明细仍由对应 Option 保持未知。
            usage_complete: self.usage_complete
                || (self.input_tokens.is_some() && self.output_tokens.is_some()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TokenAttribution {
    pub system_prompt_tokens: Option<u64>,
    pub developer_instruction_tokens: Option<u64>,
    pub runtime_context_tokens: Option<u64>,
    pub policy_tokens: Option<u64>,
    pub user_message_tokens: Option<u64>,
    pub assistant_recent_tokens: Option<u64>,
    pub working_summary_tokens: Option<u64>,
    pub memory_tokens: Option<u64>,
    pub evidence_tokens: Option<u64>,
    pub tool_instruction_tokens: Option<u64>,
    pub tool_result_excerpt_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
    pub cached_input_tokens: Option<u64>,
    #[serde(default)]
    pub contributors: Vec<TokenContributorAttribution>,
    #[serde(default)]
    pub unattributed_input_tokens: Option<u64>,
    pub source: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TokenContributorAttribution {
    pub contributor: String,
    pub source_refs: Vec<String>,
    pub message_indexes: Vec<u32>,
    pub estimated_input_tokens: u64,
    pub attributed_input_tokens: Option<u64>,
    pub attribution_method: String,
}
