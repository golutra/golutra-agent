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
#[serde(deny_unknown_fields)]
pub struct TokenUsageRecord {
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
    pub estimated_cost: Option<f64>,
    pub budget_snapshot_ref: TokenBudgetSnapshotId,
    pub attribution_ref: Option<TokenAttribution>,
    pub usage_source: String,
    pub cache_read_tokens: Option<u64>,
    pub cache_write_tokens: Option<u64>,
    pub non_cached_input_tokens: Option<u64>,
    pub tool_schema_tokens_estimated: Option<u64>,
    pub tool_result_tokens_estimated: Option<u64>,
    pub tool_estimated_tokens: Option<u64>,
    pub provider_total_tokens: Option<u64>,
    pub usage_complete: bool,
    pub cache_identity: Option<CacheIdentity>,
}

impl TokenUsageRecord {
    #[must_use]
    pub fn usage(&self) -> NormalizedUsage {
        NormalizedUsage {
            input_tokens_total: self.input_tokens,
            input_tokens_non_cached: self.non_cached_input_tokens,
            cache_read_tokens: self.cache_read_tokens,
            cache_write_tokens: self.cache_write_tokens,
            output_tokens: self.output_tokens,
            reasoning_tokens: self.reasoning_tokens,
            provider_total_tokens: self.provider_total_tokens,
            estimated_cost: self.estimated_cost,
            tool_schema_tokens_estimated: self.tool_schema_tokens_estimated,
            tool_result_tokens_estimated: self.tool_result_tokens_estimated,
            usage_source: match self.usage_source.as_str() {
                "provider" => crate::UsageSource::Provider,
                "estimated" => crate::UsageSource::Estimated,
                _ => crate::UsageSource::Unknown,
            },
            usage_complete: self.usage_complete,
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
    pub contributors: Vec<TokenContributorAttribution>,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn record() -> TokenUsageRecord {
        TokenUsageRecord {
            session_id: Some(SessionId::new()),
            task_id: TaskId::new(),
            turn_id: TurnId::new(),
            provider_id: "provider".to_owned(),
            model_id: "model".to_owned(),
            request_event_id: ProviderRequestId::new(),
            response_event_id: ProviderResponseId::new(),
            input_tokens: Some(12),
            output_tokens: Some(3),
            reasoning_tokens: Some(1),
            estimated_cost: Some(0.1),
            budget_snapshot_ref: TokenBudgetSnapshotId::new(),
            attribution_ref: None,
            usage_source: "provider".to_owned(),
            cache_read_tokens: Some(4),
            cache_write_tokens: Some(2),
            non_cached_input_tokens: Some(8),
            tool_schema_tokens_estimated: Some(5),
            tool_result_tokens_estimated: Some(6),
            tool_estimated_tokens: Some(11),
            provider_total_tokens: Some(15),
            usage_complete: true,
            cache_identity: None,
        }
    }

    #[test]
    fn usage_maps_the_canonical_record_fields_without_derivation() {
        let record = record();
        let usage = record.usage();

        assert_eq!(usage.input_tokens_non_cached, Some(8));
        assert_eq!(usage.cache_read_tokens, Some(4));
        assert_eq!(usage.provider_total_tokens, Some(15));
        assert_eq!(usage.tool_result_tokens_estimated, Some(6));
        assert!(usage.usage_complete);
    }

    #[test]
    fn old_record_shape_is_rejected() {
        let mut value = serde_json::to_value(record()).expect("record serializes");
        let object = value.as_object_mut().expect("record object");
        object.remove("cache_read_tokens");
        object.remove("cache_write_tokens");
        object.remove("non_cached_input_tokens");
        object.remove("tool_schema_tokens_estimated");
        object.remove("tool_result_tokens_estimated");
        object.remove("tool_estimated_tokens");
        object.remove("provider_total_tokens");
        object.remove("usage_complete");
        object.remove("cache_identity");
        object.insert("cached_input_tokens".to_owned(), serde_json::json!(4));
        object.insert("tool_result_tokens".to_owned(), serde_json::json!(6));
        object.insert("total_tokens".to_owned(), serde_json::json!(15));

        assert!(serde_json::from_value::<TokenUsageRecord>(value).is_err());
    }

    #[test]
    fn unknown_record_fields_are_rejected() {
        let mut value = serde_json::to_value(record()).expect("record serializes");
        value
            .as_object_mut()
            .expect("record object")
            .insert("legacy_total_tokens".to_owned(), serde_json::json!(15));

        assert!(serde_json::from_value::<TokenUsageRecord>(value).is_err());
    }
}
