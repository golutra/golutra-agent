use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{SessionId, ThreadId};

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

/// Provider usage 在边界处统一后的可消费快照。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct NormalizedUsage {
    pub input_tokens_total: Option<u64>,
    pub input_tokens_non_cached: Option<u64>,
    pub cache_read_tokens: Option<u64>,
    pub cache_write_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
    pub provider_total_tokens: Option<u64>,
    pub estimated_cost: Option<f64>,
    pub tool_schema_tokens_estimated: Option<u64>,
    pub tool_result_tokens_estimated: Option<u64>,
    pub usage_source: UsageSource,
    pub usage_complete: bool,
}

impl ProviderUsage {
    #[must_use]
    pub fn normalize(&self) -> NormalizedUsage {
        let provider_total_tokens = self.total_tokens;
        let cache_read_tokens = self.cached_input_tokens.or_else(|| {
            first_raw_u64(
                &self.raw,
                &[
                    "/prompt_tokens_details/cached_tokens",
                    "/prompt_tokens_details/cache_read_tokens",
                    "/prompt_tokens_details/cache_read_input_tokens",
                    "/input_tokens_details/cached_tokens",
                    "/input_tokens_details/cache_read_tokens",
                    "/input_tokens_details/cache_read_input_tokens",
                    "/usage/prompt_tokens_details/cached_tokens",
                    "/usage/prompt_tokens_details/cache_read_tokens",
                    "/usage/prompt_tokens_details/cache_read_input_tokens",
                    "/cache_read_tokens",
                    "/cache_read_input_tokens",
                    "/usage/cache_read_tokens",
                    "/usage/cache_read_input_tokens",
                ],
            )
        });
        let input_tokens_non_cached = self
            .input_tokens
            .zip(cache_read_tokens)
            .and_then(|(input, cached)| (cached <= input).then(|| input - cached));
        let cache_write_tokens = first_raw_u64(
            &self.raw,
            &[
                "/prompt_tokens_details/cache_creation_tokens",
                "/prompt_tokens_details/cache_write_tokens",
                "/prompt_tokens_details/cache_creation_input_tokens",
                "/input_tokens_details/cache_creation_tokens",
                "/input_tokens_details/cache_write_tokens",
                "/input_tokens_details/cache_creation_input_tokens",
                "/usage/prompt_tokens_details/cache_creation_tokens",
                "/usage/prompt_tokens_details/cache_write_tokens",
                "/usage/prompt_tokens_details/cache_creation_input_tokens",
                "/cache_write_tokens",
                "/cache_creation_tokens",
                "/cache_creation_input_tokens",
                "/usage/cache_write_tokens",
                "/usage/cache_creation_tokens",
                "/usage/cache_creation_input_tokens",
            ],
        );
        let tool_schema_tokens_estimated = self
            .raw
            .get("tool_schema_tokens")
            .and_then(Value::as_u64)
            .or_else(|| {
                self.raw
                    .pointer("/tool/schema_tokens")
                    .and_then(Value::as_u64)
            });
        let tool_result_tokens_estimated = self
            .raw
            .get("tool_result_tokens")
            .and_then(Value::as_u64)
            .or_else(|| {
                self.raw
                    .pointer("/tool/result_tokens")
                    .and_then(Value::as_u64)
            });
        NormalizedUsage {
            input_tokens_total: self.input_tokens,
            input_tokens_non_cached,
            cache_read_tokens,
            cache_write_tokens,
            output_tokens: self.output_tokens,
            reasoning_tokens: self.reasoning_tokens,
            provider_total_tokens,
            estimated_cost: None,
            tool_schema_tokens_estimated,
            tool_result_tokens_estimated,
            usage_source: self.usage_source,
            // 输入和输出已知时可以安全计算展示总量；provider 未提供的
            // total/cache 明细仍保持 None，不把估算值伪装成 provider 事实。
            usage_complete: self.input_tokens.is_some() && self.output_tokens.is_some(),
        }
    }
}

fn first_raw_u64(raw: &Value, paths: &[&str]) -> Option<u64> {
    paths
        .iter()
        .find_map(|path| raw.pointer(path).and_then(Value::as_u64))
}

impl NormalizedUsage {
    /// 为旧展示调用方保留聚合总量回退；不会再次叠加 reasoning 或工具估算。
    #[must_use]
    pub fn aggregate_total(&self) -> Option<u64> {
        self.provider_total_tokens.or_else(|| {
            self.input_tokens_total
                .zip(self.output_tokens)
                .map(|(input, output)| input.saturating_add(output))
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "snake_case")]
pub enum PromptCachePolicy {
    #[default]
    Auto,
    None,
    Short,
    Long,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CacheIdentity {
    pub session_id: SessionId,
    /// 为未来 projection 预留的持久 thread id；请求编译时仍以 session id 作为稳定 wire 身份。
    #[serde(default)]
    pub thread_id: Option<ThreadId>,
    pub provider_id: String,
    pub model_id: String,
    pub key: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum UsageSource {
    Provider,
    Estimated,
    Unknown,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_fills_total_and_cache_breakdown_without_faking_missing_usage() {
        let usage = ProviderUsage {
            input_tokens: Some(12),
            output_tokens: Some(3),
            reasoning_tokens: Some(1),
            cached_input_tokens: Some(4),
            total_tokens: None,
            usage_source: UsageSource::Provider,
            raw: serde_json::json!({"prompt_tokens_details": {"cache_creation_tokens": 2}}),
        };
        let normalized = usage.normalize();
        assert_eq!(normalized.aggregate_total(), Some(15));
        assert_eq!(normalized.cache_read_tokens, Some(4));
        assert_eq!(normalized.cache_write_tokens, Some(2));
        assert_eq!(normalized.input_tokens_non_cached, Some(8));
        assert!(normalized.usage_complete);
    }

    #[test]
    fn normalize_keeps_non_cached_unknown_when_cache_breakdown_is_missing_or_invalid() {
        for cached in [None, Some(13)] {
            let usage = ProviderUsage {
                input_tokens: Some(12),
                output_tokens: Some(1),
                reasoning_tokens: None,
                cached_input_tokens: cached,
                total_tokens: Some(13),
                usage_source: UsageSource::Provider,
                raw: Value::Null,
            };
            assert_eq!(usage.normalize().input_tokens_non_cached, None);
        }
    }

    #[test]
    fn normalize_accepts_provider_cache_aliases() {
        let usage = ProviderUsage {
            input_tokens: Some(20),
            output_tokens: Some(4),
            reasoning_tokens: None,
            cached_input_tokens: None,
            total_tokens: Some(24),
            usage_source: UsageSource::Provider,
            raw: serde_json::json!({
                "prompt_tokens_details": {
                    "cache_read_input_tokens": 7,
                    "cache_write_tokens": 5
                }
            }),
        };

        let normalized = usage.normalize();
        assert_eq!(normalized.cache_read_tokens, Some(7));
        assert_eq!(normalized.cache_write_tokens, Some(5));
        assert_eq!(normalized.input_tokens_non_cached, Some(13));
    }
}
