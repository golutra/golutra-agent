use async_trait::async_trait;
use golutra_core::{ProviderContract, ProviderRequestId, ProviderResponseId, TaskId, TurnId};
pub use golutra_core::{ProviderUsage, UsageSource};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ProviderError {
    #[error("provider failed: {message}")]
    Failed { message: String },
    #[error("provider rate limited: {message}")]
    RateLimited { message: String },
    #[error("provider is not configured: {message}")]
    NotConfigured { message: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderRequest {
    pub request_id: ProviderRequestId,
    pub task_id: TaskId,
    pub turn_id: TurnId,
    pub provider_id: String,
    pub model_id: String,
    pub messages: Vec<ProviderMessage>,
    pub tools: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderMessage {
    pub role: ProviderRole,
    pub content: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderRole {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderResponse {
    pub response_id: ProviderResponseId,
    pub message: Option<ProviderMessage>,
    pub tool_calls: Vec<ProviderToolCall>,
    pub usage: ProviderUsage,
    pub finish_reason: ProviderFinishReason,
    pub raw_metadata: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderToolCall {
    pub tool_call_id: String,
    pub tool_name: String,
    pub arguments: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderFinishReason {
    Stop,
    ToolCalls,
    Length,
    ContentFilter,
    Error,
    Unknown,
}

#[async_trait]
pub trait LlmProvider: Send + Sync {
    async fn complete(&self, request: ProviderRequest) -> Result<ProviderResponse, ProviderError>;
    fn contract(&self) -> ProviderContract;
}

#[derive(Debug, Clone)]
pub struct MockProvider {
    contract: ProviderContract,
    response: ProviderResponse,
}

impl MockProvider {
    #[must_use]
    pub fn text_response(content: impl Into<String>) -> Self {
        Self {
            contract: mock_contract(),
            response: ProviderResponse {
                response_id: ProviderResponseId::new(),
                message: Some(ProviderMessage {
                    role: ProviderRole::Assistant,
                    content: content.into(),
                }),
                tool_calls: Vec::new(),
                usage: usage(128, 32),
                finish_reason: ProviderFinishReason::Stop,
                raw_metadata: serde_json::json!({"provider": "mock"}),
            },
        }
    }

    #[must_use]
    pub fn tool_call(tool_name: impl Into<String>, arguments: Value) -> Self {
        let tool_name = tool_name.into();
        Self {
            contract: mock_contract(),
            response: ProviderResponse {
                response_id: ProviderResponseId::new(),
                message: None,
                tool_calls: vec![ProviderToolCall {
                    tool_call_id: "mock-tool-call".to_owned(),
                    tool_name,
                    arguments,
                }],
                usage: usage(96, 16),
                finish_reason: ProviderFinishReason::ToolCalls,
                raw_metadata: serde_json::json!({"provider": "mock"}),
            },
        }
    }
}

#[async_trait]
impl LlmProvider for MockProvider {
    async fn complete(&self, _request: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        Ok(self.response.clone())
    }

    fn contract(&self) -> ProviderContract {
        self.contract.clone()
    }
}

#[derive(Debug, Clone, Default)]
pub struct GenaiProviderAdapter {
    configured: bool,
}

impl GenaiProviderAdapter {
    #[must_use]
    pub fn unconfigured() -> Self {
        Self { configured: false }
    }
}

#[async_trait]
impl LlmProvider for GenaiProviderAdapter {
    async fn complete(&self, _request: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        if self.configured {
            Err(ProviderError::Failed {
                message: "genai adapter transport is not implemented in P0 scaffold".to_owned(),
            })
        } else {
            Err(ProviderError::NotConfigured {
                message: "genai adapter requires provider configuration".to_owned(),
            })
        }
    }

    fn contract(&self) -> ProviderContract {
        ProviderContract {
            provider_id: "genai".to_owned(),
            model_id: "configured-at-runtime".to_owned(),
            native_protocol: "genai".to_owned(),
            stream_event_mapping: "adapter_owned".to_owned(),
            tool_call_mapping: "adapter_owned".to_owned(),
            usage_mapping: "adapter_owned".to_owned(),
            reasoning_mapping: "adapter_owned".to_owned(),
            finish_reason_mapping: "adapter_owned".to_owned(),
            error_mapping: "adapter_owned".to_owned(),
            rate_limit_mapping: "adapter_owned".to_owned(),
            cost_model: "configured-at-runtime".to_owned(),
            capability_matrix_ref: None,
            golden_fixture_refs: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelRouteDecision {
    pub provider_id: String,
    pub model_id: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderConfig {
    pub provider_id: String,
    pub model_id: String,
    pub auth_env: Option<String>,
    pub base_url: Option<String>,
    pub enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelCapability {
    pub provider_id: String,
    pub model_id: String,
    pub supports_tools: bool,
    pub context_window: u64,
    pub max_output: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ModelCatalog {
    pub providers: Vec<ProviderConfig>,
    pub capabilities: Vec<ModelCapability>,
}

impl ModelCatalog {
    #[must_use]
    pub fn p1_default() -> Self {
        Self {
            providers: vec![
                ProviderConfig {
                    provider_id: "mock".to_owned(),
                    model_id: "mock-model".to_owned(),
                    auth_env: None,
                    base_url: None,
                    enabled: true,
                },
                ProviderConfig {
                    provider_id: "genai".to_owned(),
                    model_id: "configured-at-runtime".to_owned(),
                    auth_env: Some("GOLUTRA_PROVIDER_API_KEY".to_owned()),
                    base_url: None,
                    enabled: false,
                },
            ],
            capabilities: vec![
                ModelCapability {
                    provider_id: "mock".to_owned(),
                    model_id: "mock-model".to_owned(),
                    supports_tools: true,
                    context_window: 8_192,
                    max_output: 1_024,
                },
                ModelCapability {
                    provider_id: "genai".to_owned(),
                    model_id: "configured-at-runtime".to_owned(),
                    supports_tools: true,
                    context_window: 128_000,
                    max_output: 8_192,
                },
            ],
        }
    }

    #[must_use]
    pub fn capability(&self, provider_id: &str, model_id: &str) -> Option<&ModelCapability> {
        self.capabilities.iter().find(|capability| {
            capability.provider_id == provider_id && capability.model_id == model_id
        })
    }

    #[must_use]
    pub fn route_default(&self) -> Option<ModelRouteDecision> {
        self.providers
            .iter()
            .find(|provider| provider.enabled)
            .map(|provider| ModelRouteDecision {
                provider_id: provider.provider_id.clone(),
                model_id: provider.model_id.clone(),
                reason: "first enabled provider in catalog".to_owned(),
            })
    }
}

fn mock_contract() -> ProviderContract {
    ProviderContract {
        provider_id: "mock".to_owned(),
        model_id: "mock-model".to_owned(),
        native_protocol: "in_memory".to_owned(),
        stream_event_mapping: "none".to_owned(),
        tool_call_mapping: "normalized".to_owned(),
        usage_mapping: "known".to_owned(),
        reasoning_mapping: "none".to_owned(),
        finish_reason_mapping: "normalized".to_owned(),
        error_mapping: "structured".to_owned(),
        rate_limit_mapping: "none".to_owned(),
        cost_model: "zero".to_owned(),
        capability_matrix_ref: None,
        golden_fixture_refs: Vec::new(),
    }
}

fn usage(input_tokens: u64, output_tokens: u64) -> ProviderUsage {
    ProviderUsage {
        input_tokens: Some(input_tokens),
        output_tokens: Some(output_tokens),
        reasoning_tokens: Some(0),
        cached_input_tokens: Some(0),
        total_tokens: Some(input_tokens + output_tokens),
        usage_source: UsageSource::Provider,
        raw: serde_json::json!({"source": "mock"}),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[tokio::test]
    async fn mock_provider_returns_text_response() {
        let provider = MockProvider::text_response("done");
        let response = provider.complete(request()).await.expect("response");

        assert_eq!(response.finish_reason, ProviderFinishReason::Stop);
        assert_eq!(response.message.expect("message").content, "done");
        assert_eq!(response.usage.total_tokens, Some(160));
    }

    #[tokio::test]
    async fn mock_provider_returns_tool_call() {
        let provider = MockProvider::tool_call("read_file", json!({"path": "README.md"}));
        let response = provider.complete(request()).await.expect("response");

        assert_eq!(response.finish_reason, ProviderFinishReason::ToolCalls);
        assert_eq!(response.tool_calls[0].tool_name, "read_file");
    }

    #[tokio::test]
    async fn genai_adapter_does_not_leak_native_types() {
        let provider = GenaiProviderAdapter::unconfigured();
        let error = provider
            .complete(request())
            .await
            .expect_err("not configured");

        assert!(matches!(error, ProviderError::NotConfigured { .. }));
        assert_eq!(provider.contract().provider_id, "genai");
    }

    #[test]
    fn model_catalog_routes_to_first_enabled_provider() {
        let catalog = ModelCatalog::p1_default();
        let route = catalog.route_default().expect("route");

        assert_eq!(route.provider_id, "mock");
        assert!(catalog.capability("mock", "mock-model").is_some());
    }

    fn request() -> ProviderRequest {
        ProviderRequest {
            request_id: ProviderRequestId::new(),
            task_id: TaskId::new(),
            turn_id: TurnId::new(),
            provider_id: "mock".to_owned(),
            model_id: "mock-model".to_owned(),
            messages: vec![ProviderMessage {
                role: ProviderRole::User,
                content: "hello".to_owned(),
            }],
            tools: Vec::new(),
        }
    }
}
