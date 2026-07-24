use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{ProviderRequestId, ProviderResponseId, TaskId, TokenBudgetSnapshotId, TurnId};

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
