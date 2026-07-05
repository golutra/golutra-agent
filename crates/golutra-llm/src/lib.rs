use async_trait::async_trait;
use golutra_core::{ProviderContract, ProviderRequestId, ProviderResponseId, TaskId, TurnId};
pub use golutra_core::{ProviderUsage, UsageSource};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
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

#[derive(Debug, Clone)]
pub struct OpenAiCompatibleProvider {
    api_key: String,
    base_url: String,
    model_id: String,
    client: reqwest::Client,
}

impl OpenAiCompatibleProvider {
    #[must_use]
    pub fn new(
        api_key: impl Into<String>,
        base_url: impl Into<String>,
        model_id: impl Into<String>,
    ) -> Self {
        Self {
            api_key: api_key.into(),
            base_url: base_url.into(),
            model_id: model_id.into(),
            client: reqwest::Client::new(),
        }
    }

    pub fn from_env() -> Result<Self, ProviderError> {
        let api_key = std::env::var("GOLUTRA_PROVIDER_API_KEY").map_err(|_| {
            ProviderError::NotConfigured {
                message: "GOLUTRA_PROVIDER_API_KEY is not set".to_owned(),
            }
        })?;
        let model_id =
            std::env::var("GOLUTRA_PROVIDER_MODEL").map_err(|_| ProviderError::NotConfigured {
                message: "GOLUTRA_PROVIDER_MODEL is not set".to_owned(),
            })?;
        let base_url = std::env::var("GOLUTRA_PROVIDER_BASE_URL")
            .unwrap_or_else(|_| "https://api.openai.com/v1".to_owned());
        Ok(Self::new(api_key, base_url, model_id))
    }
}

#[async_trait]
impl LlmProvider for OpenAiCompatibleProvider {
    async fn complete(&self, request: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        let mut body = json!({
            "model": self.model_id,
            "messages": request.messages.iter().map(openai_message).collect::<Vec<_>>(),
        });
        if !request.tools.is_empty() {
            body["tools"] = Value::Array(request.tools.iter().map(openai_tool_schema).collect());
            body["tool_choice"] = Value::String("auto".to_owned());
        }

        let response = self
            .client
            .post(url)
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|error| ProviderError::Failed {
                message: error.to_string(),
            })?;
        let status = response.status();
        let value = response
            .json::<Value>()
            .await
            .map_err(|error| ProviderError::Failed {
                message: error.to_string(),
            })?;
        if status.as_u16() == 429 {
            return Err(ProviderError::RateLimited {
                message: provider_error_message(&value),
            });
        }
        if !status.is_success() {
            return Err(ProviderError::Failed {
                message: provider_error_message(&value),
            });
        }

        Ok(provider_response_from_openai(
            value,
            request.task_id,
            request.turn_id,
        ))
    }

    fn contract(&self) -> ProviderContract {
        ProviderContract {
            provider_id: "openai_compatible".to_owned(),
            model_id: self.model_id.clone(),
            native_protocol: "openai_chat_completions".to_owned(),
            stream_event_mapping: "non_streaming_p0".to_owned(),
            tool_call_mapping: "function_tool_calls".to_owned(),
            usage_mapping: "chat_completion_usage".to_owned(),
            reasoning_mapping: "not_exposed".to_owned(),
            finish_reason_mapping: "chat_completion_finish_reason".to_owned(),
            error_mapping: "http_status_and_error_body".to_owned(),
            rate_limit_mapping: "http_429".to_owned(),
            cost_model: "external".to_owned(),
            capability_matrix_ref: None,
            golden_fixture_refs: Vec::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub enum ConfiguredProvider {
    Mock(Box<MockProvider>),
    OpenAiCompatible(OpenAiCompatibleProvider),
}

impl ConfiguredProvider {
    #[must_use]
    pub fn from_env_or_mock(mock: MockProvider) -> Self {
        if std::env::var("GOLUTRA_PROVIDER_MODE").as_deref() != Ok("live") {
            return Self::Mock(Box::new(mock));
        }
        OpenAiCompatibleProvider::from_env()
            .map(Self::OpenAiCompatible)
            .unwrap_or_else(|_| Self::Mock(Box::new(mock)))
    }
}

#[async_trait]
impl LlmProvider for ConfiguredProvider {
    async fn complete(&self, request: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        match self {
            Self::Mock(provider) => provider.complete(request).await,
            Self::OpenAiCompatible(provider) => provider.complete(request).await,
        }
    }

    fn contract(&self) -> ProviderContract {
        match self {
            Self::Mock(provider) => provider.contract(),
            Self::OpenAiCompatible(provider) => provider.contract(),
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

fn openai_message(message: &ProviderMessage) -> Value {
    json!({
        "role": match message.role {
            ProviderRole::System => "system",
            ProviderRole::User => "user",
            ProviderRole::Assistant => "assistant",
            ProviderRole::Tool => "tool",
        },
        "content": message.content,
    })
}

fn openai_tool_schema(tool_name: &String) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": tool_name,
            "description": format!("Golutra workspace tool: {tool_name}"),
            "parameters": {
                "type": "object",
                "additionalProperties": true
            }
        }
    })
}

fn provider_response_from_openai(
    value: Value,
    _task_id: TaskId,
    _turn_id: TurnId,
) -> ProviderResponse {
    let choice = value
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .cloned()
        .unwrap_or_else(|| json!({}));
    let message = choice.get("message").cloned().unwrap_or_else(|| json!({}));
    let content = message
        .get("content")
        .and_then(Value::as_str)
        .filter(|content| !content.is_empty())
        .map(|content| ProviderMessage {
            role: ProviderRole::Assistant,
            content: content.to_owned(),
        });
    let tool_calls = message
        .get("tool_calls")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(provider_tool_call_from_openai)
        .collect::<Vec<_>>();
    let usage_value = value.get("usage").cloned().unwrap_or_else(|| json!({}));

    ProviderResponse {
        response_id: ProviderResponseId::new(),
        message: content,
        tool_calls,
        usage: ProviderUsage {
            input_tokens: usage_value.get("prompt_tokens").and_then(Value::as_u64),
            output_tokens: usage_value.get("completion_tokens").and_then(Value::as_u64),
            reasoning_tokens: None,
            cached_input_tokens: None,
            total_tokens: usage_value.get("total_tokens").and_then(Value::as_u64),
            usage_source: UsageSource::Provider,
            raw: usage_value,
        },
        finish_reason: finish_reason_from_openai(
            choice
                .get("finish_reason")
                .and_then(Value::as_str)
                .unwrap_or_default(),
        ),
        raw_metadata: value,
    }
}

fn provider_tool_call_from_openai(value: &Value) -> Option<ProviderToolCall> {
    let function = value.get("function")?;
    let arguments = function
        .get("arguments")
        .and_then(Value::as_str)
        .and_then(|arguments| serde_json::from_str(arguments).ok())
        .unwrap_or_else(|| json!({}));
    Some(ProviderToolCall {
        tool_call_id: value
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("provider-tool-call")
            .to_owned(),
        tool_name: function.get("name")?.as_str()?.to_owned(),
        arguments,
    })
}

fn finish_reason_from_openai(value: &str) -> ProviderFinishReason {
    match value {
        "stop" => ProviderFinishReason::Stop,
        "tool_calls" | "function_call" => ProviderFinishReason::ToolCalls,
        "length" => ProviderFinishReason::Length,
        "content_filter" => ProviderFinishReason::ContentFilter,
        _ => ProviderFinishReason::Unknown,
    }
}

fn provider_error_message(value: &Value) -> String {
    value
        .get("error")
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .unwrap_or("provider request failed")
        .to_owned()
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
