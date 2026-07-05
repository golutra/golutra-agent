use async_trait::async_trait;
use golutra_core::{ProviderContract, ProviderRequestId, ProviderResponseId, TaskId, TurnId};
pub use golutra_core::{ProviderUsage, UsageSource};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;

const GOLUTRA_PROVIDER_MODE: &str = "GOLUTRA_PROVIDER_MODE";
const GOLUTRA_PROVIDER_PROTOCOL: &str = "GOLUTRA_PROVIDER_PROTOCOL";
const GOLUTRA_PROVIDER_API_KEY: &str = "GOLUTRA_PROVIDER_API_KEY";
const GOLUTRA_PROVIDER_MODEL: &str = "GOLUTRA_PROVIDER_MODEL";
const GOLUTRA_PROVIDER_BASE_URL: &str = "GOLUTRA_PROVIDER_BASE_URL";
const OPENAI_API_KEY: &str = "OPENAI_API_KEY";
const OPENAI_MODEL: &str = "OPENAI_MODEL";
const OPENAI_BASE_URL: &str = "OPENAI_BASE_URL";
const ANTHROPIC_API_KEY: &str = "ANTHROPIC_API_KEY";
const ANTHROPIC_MODEL: &str = "ANTHROPIC_MODEL";
const ANTHROPIC_BASE_URL: &str = "ANTHROPIC_BASE_URL";
const GEMINI_API_KEY: &str = "GEMINI_API_KEY";
const GEMINI_MODEL: &str = "GEMINI_MODEL";
const GOOGLE_API_KEY: &str = "GOOGLE_API_KEY";
const GOOGLE_MODEL: &str = "GOOGLE_MODEL";
const DEFAULT_OPENAI_BASE_URL: &str = "https://api.openai.com/v1";

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderProtocol {
    Mock,
    #[serde(rename = "openai-compatible")]
    OpenAiCompatible,
    Anthropic,
    Gemini,
    VertexAi,
    Genai,
}

impl ProviderProtocol {
    #[must_use]
    pub fn id(self) -> &'static str {
        match self {
            Self::Mock => "mock",
            Self::OpenAiCompatible => "openai-compatible",
            Self::Anthropic => "anthropic",
            Self::Gemini => "gemini",
            Self::VertexAi => "vertex-ai",
            Self::Genai => "genai",
        }
    }

    #[must_use]
    pub fn from_config_value(value: &str) -> Option<Self> {
        match normalize_protocol_value(value).as_str() {
            "mock" => Some(Self::Mock),
            "live" | "openai" | "openai-compatible" | "open-ai-compatible" => {
                Some(Self::OpenAiCompatible)
            }
            "anthropic" | "claude" => Some(Self::Anthropic),
            "gemini" | "google-genai" => Some(Self::Gemini),
            "vertex-ai" | "vertex" => Some(Self::VertexAi),
            "genai" | "rust-genai" => Some(Self::Genai),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderProtocolSpec {
    pub protocol: ProviderProtocol,
    pub display_name: String,
    pub status: String,
    pub api_key_env: Vec<String>,
    pub base_url_env: Vec<String>,
    pub model_env: Vec<String>,
    pub default_base_url: Option<String>,
    pub supports_tool_calls: bool,
    pub supports_probe: bool,
    pub notes: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ProviderEnvMapping {
    api_key: &'static [&'static str],
    base_url: &'static [&'static str],
    model: &'static [&'static str],
    default_base_url: Option<&'static str>,
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
    api_key_env: String,
    base_url: String,
    model_id: String,
    client: reqwest::Client,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenAiCompatibleProviderConfig {
    pub api_key: String,
    pub api_key_env: String,
    pub base_url: String,
    pub model_id: String,
    pub protocol: ProviderProtocol,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RedactedProviderConfig {
    pub mode: String,
    pub provider_id: String,
    pub protocol: ProviderProtocol,
    pub native_protocol: String,
    pub base_url: Option<String>,
    pub model_id: Option<String>,
    pub api_key_env: Option<String>,
    pub api_key_configured: bool,
    pub missing_env: Vec<String>,
    pub supported: bool,
    pub status: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderProbeResult {
    pub provider_id: String,
    pub protocol: String,
    pub base_url: String,
    pub model_id: String,
    pub model_available: Option<bool>,
    pub discovered_models: Vec<String>,
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
            api_key_env: GOLUTRA_PROVIDER_API_KEY.to_owned(),
            base_url: normalize_openai_base_url(&base_url.into()),
            model_id: model_id.into(),
            client: reqwest::Client::new(),
        }
    }

    #[must_use]
    pub fn from_config(config: OpenAiCompatibleProviderConfig) -> Self {
        Self {
            api_key: config.api_key,
            api_key_env: config.api_key_env,
            base_url: normalize_openai_base_url(&config.base_url),
            model_id: config.model_id,
            client: reqwest::Client::new(),
        }
    }

    pub fn from_env() -> Result<Self, ProviderError> {
        Self::config_from_env().map(Self::from_config)
    }

    pub fn config_from_env() -> Result<OpenAiCompatibleProviderConfig, ProviderError> {
        Self::config_from_env_reader(|key| std::env::var(key).ok())
    }

    pub fn config_from_env_reader<F>(
        reader: F,
    ) -> Result<OpenAiCompatibleProviderConfig, ProviderError>
    where
        F: Fn(&str) -> Option<String>,
    {
        let protocol =
            selected_protocol_from_reader(&reader).unwrap_or(ProviderProtocol::OpenAiCompatible);
        if protocol != ProviderProtocol::OpenAiCompatible {
            return Err(unsupported_protocol_error(protocol));
        }
        let mapping = env_mapping(protocol);
        let (api_key_env, api_key) = first_env(&reader, mapping.api_key)
            .ok_or_else(|| missing_env_error(mapping.api_key))?;
        let (_, model_id) =
            first_env(&reader, mapping.model).ok_or_else(|| missing_env_error(mapping.model))?;
        let base_url = first_env(&reader, mapping.base_url)
            .map(|(_, value)| value)
            .or_else(|| mapping.default_base_url.map(ToOwned::to_owned))
            .ok_or_else(|| missing_env_error(mapping.base_url))?;
        Ok(OpenAiCompatibleProviderConfig {
            api_key,
            api_key_env,
            base_url: normalize_openai_base_url(&base_url),
            model_id,
            protocol,
        })
    }

    #[must_use]
    pub fn redacted_config(&self) -> RedactedProviderConfig {
        RedactedProviderConfig {
            mode: "live".to_owned(),
            provider_id: "openai_compatible".to_owned(),
            protocol: ProviderProtocol::OpenAiCompatible,
            native_protocol: "openai_chat_completions".to_owned(),
            base_url: Some(self.base_url.clone()),
            model_id: Some(self.model_id.clone()),
            api_key_env: Some(self.api_key_env.clone()),
            api_key_configured: !self.api_key.is_empty(),
            missing_env: Vec::new(),
            supported: true,
            status: "ready".to_owned(),
        }
    }

    pub async fn probe(&self) -> Result<ProviderProbeResult, ProviderError> {
        let response = self
            .client
            .get(format!("{}/models", self.base_url.trim_end_matches('/')))
            .bearer_auth(&self.api_key)
            .send()
            .await
            .map_err(|error| ProviderError::Failed {
                message: error.to_string(),
            })?;
        let status = response.status();
        let value = response_json_or_error(response).await?;
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
        let discovered_models = value
            .get("data")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|model| model.get("id").and_then(Value::as_str))
            .map(ToOwned::to_owned)
            .collect::<Vec<_>>();
        let model_available = if discovered_models.is_empty() {
            None
        } else {
            Some(
                discovered_models
                    .iter()
                    .any(|model| model == &self.model_id),
            )
        };
        Ok(ProviderProbeResult {
            provider_id: "openai_compatible".to_owned(),
            protocol: "openai_chat_completions".to_owned(),
            base_url: self.base_url.clone(),
            model_id: self.model_id.clone(),
            model_available,
            discovered_models,
        })
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
        let value = response_json_or_error(response).await?;
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
    pub fn resolve_from_env(mock: MockProvider) -> Result<Self, ProviderError> {
        Self::resolve_from_reader(mock, |key| std::env::var(key).ok())
    }

    pub fn resolve_from_reader<F>(mock: MockProvider, reader: F) -> Result<Self, ProviderError>
    where
        F: Fn(&str) -> Option<String>,
    {
        let protocol = selected_protocol_from_reader(&reader);
        if protocol.is_none_or(|protocol| protocol == ProviderProtocol::Mock) {
            return Ok(Self::Mock(Box::new(mock)));
        }
        let protocol = protocol.expect("checked above");
        if protocol != ProviderProtocol::OpenAiCompatible {
            return Err(unsupported_protocol_error(protocol));
        }
        OpenAiCompatibleProvider::config_from_env_reader(reader)
            .map(OpenAiCompatibleProvider::from_config)
            .map(Self::OpenAiCompatible)
    }

    #[must_use]
    pub fn from_env_or_mock(mock: MockProvider) -> Self {
        if selected_protocol_from_env().is_none_or(|protocol| protocol == ProviderProtocol::Mock) {
            return Self::Mock(Box::new(mock));
        }
        OpenAiCompatibleProvider::from_env()
            .map(Self::OpenAiCompatible)
            .unwrap_or_else(|_| Self::Mock(Box::new(mock)))
    }

    pub fn redacted_from_env() -> Result<RedactedProviderConfig, ProviderError> {
        Self::redacted_from_reader(|key| std::env::var(key).ok())
    }

    pub fn redacted_from_reader<F>(reader: F) -> Result<RedactedProviderConfig, ProviderError>
    where
        F: Fn(&str) -> Option<String>,
    {
        let protocol = selected_protocol_from_reader(&reader).unwrap_or(ProviderProtocol::Mock);
        if protocol == ProviderProtocol::Mock {
            return Ok(RedactedProviderConfig {
                mode: "mock".to_owned(),
                provider_id: "mock".to_owned(),
                protocol: ProviderProtocol::Mock,
                native_protocol: "in_memory".to_owned(),
                base_url: None,
                model_id: Some("mock-model".to_owned()),
                api_key_env: None,
                api_key_configured: false,
                missing_env: Vec::new(),
                supported: true,
                status: "ready".to_owned(),
            });
        }
        if protocol == ProviderProtocol::OpenAiCompatible {
            return Ok(redacted_openai_from_reader(&reader));
        }
        Ok(redacted_unsupported_from_reader(protocol, &reader))
    }

    pub async fn probe_from_env() -> Result<ProviderProbeResult, ProviderError> {
        Self::probe_from_reader(|key| std::env::var(key).ok()).await
    }

    pub async fn probe_from_reader<F>(reader: F) -> Result<ProviderProbeResult, ProviderError>
    where
        F: Fn(&str) -> Option<String>,
    {
        let protocol =
            selected_protocol_from_reader(&reader).unwrap_or(ProviderProtocol::OpenAiCompatible);
        if protocol != ProviderProtocol::OpenAiCompatible {
            return Err(unsupported_protocol_error(protocol));
        }
        OpenAiCompatibleProvider::config_from_env_reader(reader)
            .map(OpenAiCompatibleProvider::from_config)?
            .probe()
            .await
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
    pub protocol: ProviderProtocol,
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
                    protocol: ProviderProtocol::Mock,
                    model_id: "mock-model".to_owned(),
                    auth_env: None,
                    base_url: None,
                    enabled: true,
                },
                ProviderConfig {
                    provider_id: "openai-compatible".to_owned(),
                    protocol: ProviderProtocol::OpenAiCompatible,
                    model_id: "configured-at-runtime".to_owned(),
                    auth_env: Some("GOLUTRA_PROVIDER_API_KEY".to_owned()),
                    base_url: Some(DEFAULT_OPENAI_BASE_URL.to_owned()),
                    enabled: false,
                },
                ProviderConfig {
                    provider_id: "anthropic".to_owned(),
                    protocol: ProviderProtocol::Anthropic,
                    model_id: "configured-at-runtime".to_owned(),
                    auth_env: Some(ANTHROPIC_API_KEY.to_owned()),
                    base_url: None,
                    enabled: false,
                },
                ProviderConfig {
                    provider_id: "gemini".to_owned(),
                    protocol: ProviderProtocol::Gemini,
                    model_id: "configured-at-runtime".to_owned(),
                    auth_env: Some(GEMINI_API_KEY.to_owned()),
                    base_url: None,
                    enabled: false,
                },
                ProviderConfig {
                    provider_id: "vertex-ai".to_owned(),
                    protocol: ProviderProtocol::VertexAi,
                    model_id: "configured-at-runtime".to_owned(),
                    auth_env: Some(GOOGLE_API_KEY.to_owned()),
                    base_url: None,
                    enabled: false,
                },
                ProviderConfig {
                    provider_id: "genai".to_owned(),
                    protocol: ProviderProtocol::Genai,
                    model_id: "configured-at-runtime".to_owned(),
                    auth_env: Some(GOLUTRA_PROVIDER_API_KEY.to_owned()),
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
                    provider_id: "openai-compatible".to_owned(),
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

#[must_use]
pub fn provider_protocol_catalog() -> Vec<ProviderProtocolSpec> {
    [
        ProviderProtocol::OpenAiCompatible,
        ProviderProtocol::Anthropic,
        ProviderProtocol::Gemini,
        ProviderProtocol::VertexAi,
        ProviderProtocol::Genai,
        ProviderProtocol::Mock,
    ]
    .into_iter()
    .map(protocol_spec)
    .collect()
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
    let (description, parameters) = match tool_name.as_str() {
        "read_file" => (
            "Read a UTF-8 text file from the current workspace.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Workspace-relative file path"}
                },
                "required": ["path"],
                "additionalProperties": false
            }),
        ),
        "write_file" => (
            "Write UTF-8 text content to a workspace-relative file.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Workspace-relative file path"},
                    "content": {"type": "string", "description": "Full file content to write"}
                },
                "required": ["path", "content"],
                "additionalProperties": false
            }),
        ),
        "edit_file" => (
            "Replace the first exact text match in a workspace-relative file.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Workspace-relative file path"},
                    "search": {"type": "string", "description": "Exact text to replace"},
                    "replace": {"type": "string", "description": "Replacement text"}
                },
                "required": ["path", "search", "replace"],
                "additionalProperties": false
            }),
        ),
        "list_dir" => (
            "List entries in a workspace-relative directory.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Workspace-relative directory path, defaults to ."}
                },
                "additionalProperties": false
            }),
        ),
        "rg_search" => (
            "Search workspace files with ripgrep.",
            json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string", "description": "ripgrep search pattern"},
                    "path": {"type": "string", "description": "Workspace-relative path, defaults to ."}
                },
                "required": ["pattern"],
                "additionalProperties": false
            }),
        ),
        "shell" => (
            "Run a simple command without shell metacharacters in the workspace.",
            json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "description": "Command and arguments, for example `cargo test -p golutra-llm`"},
                    "timeout_ms": {"type": "integer", "minimum": 1, "maximum": 30000}
                },
                "required": ["command"],
                "additionalProperties": false
            }),
        ),
        _ => (
            "Golutra workspace tool.",
            json!({
                "type": "object",
                "additionalProperties": true
            }),
        ),
    };
    json!({
        "type": "function",
        "function": {
            "name": tool_name,
            "description": description,
            "parameters": parameters
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
    sanitize_provider_error(
        value
            .get("error")
            .and_then(|error| error.get("message"))
            .and_then(Value::as_str)
            .unwrap_or("provider request failed"),
    )
}

async fn response_json_or_error(response: reqwest::Response) -> Result<Value, ProviderError> {
    let text = response
        .text()
        .await
        .map_err(|error| ProviderError::Failed {
            message: error.to_string(),
        })?;
    Ok(serde_json::from_str(&text).unwrap_or_else(|_| {
        json!({
            "error": {
                "message": sanitize_provider_error(&text)
            }
        })
    }))
}

fn normalize_protocol_value(value: &str) -> String {
    value.trim().to_ascii_lowercase().replace('_', "-")
}

fn selected_protocol_from_reader<F>(reader: &F) -> Option<ProviderProtocol>
where
    F: Fn(&str) -> Option<String>,
{
    reader(GOLUTRA_PROVIDER_PROTOCOL)
        .and_then(|value| ProviderProtocol::from_config_value(&value))
        .or_else(|| {
            reader(GOLUTRA_PROVIDER_MODE)
                .and_then(|value| ProviderProtocol::from_config_value(&value))
        })
}

fn selected_protocol_from_env() -> Option<ProviderProtocol> {
    selected_protocol_from_reader(&|key| std::env::var(key).ok())
}

fn env_mapping(protocol: ProviderProtocol) -> ProviderEnvMapping {
    match protocol {
        ProviderProtocol::Mock => ProviderEnvMapping {
            api_key: &[],
            base_url: &[],
            model: &[],
            default_base_url: None,
        },
        ProviderProtocol::OpenAiCompatible => ProviderEnvMapping {
            api_key: &[GOLUTRA_PROVIDER_API_KEY, OPENAI_API_KEY],
            base_url: &[GOLUTRA_PROVIDER_BASE_URL, OPENAI_BASE_URL],
            model: &[GOLUTRA_PROVIDER_MODEL, OPENAI_MODEL],
            default_base_url: Some(DEFAULT_OPENAI_BASE_URL),
        },
        ProviderProtocol::Anthropic => ProviderEnvMapping {
            api_key: &[GOLUTRA_PROVIDER_API_KEY, ANTHROPIC_API_KEY],
            base_url: &[GOLUTRA_PROVIDER_BASE_URL, ANTHROPIC_BASE_URL],
            model: &[GOLUTRA_PROVIDER_MODEL, ANTHROPIC_MODEL],
            default_base_url: None,
        },
        ProviderProtocol::Gemini => ProviderEnvMapping {
            api_key: &[GOLUTRA_PROVIDER_API_KEY, GEMINI_API_KEY],
            base_url: &[GOLUTRA_PROVIDER_BASE_URL],
            model: &[GOLUTRA_PROVIDER_MODEL, GEMINI_MODEL],
            default_base_url: None,
        },
        ProviderProtocol::VertexAi => ProviderEnvMapping {
            api_key: &[GOLUTRA_PROVIDER_API_KEY, GOOGLE_API_KEY],
            base_url: &[GOLUTRA_PROVIDER_BASE_URL],
            model: &[GOLUTRA_PROVIDER_MODEL, GOOGLE_MODEL],
            default_base_url: None,
        },
        ProviderProtocol::Genai => ProviderEnvMapping {
            api_key: &[
                GOLUTRA_PROVIDER_API_KEY,
                OPENAI_API_KEY,
                ANTHROPIC_API_KEY,
                GEMINI_API_KEY,
                GOOGLE_API_KEY,
            ],
            base_url: &[
                GOLUTRA_PROVIDER_BASE_URL,
                OPENAI_BASE_URL,
                ANTHROPIC_BASE_URL,
            ],
            model: &[
                GOLUTRA_PROVIDER_MODEL,
                OPENAI_MODEL,
                ANTHROPIC_MODEL,
                GEMINI_MODEL,
                GOOGLE_MODEL,
            ],
            default_base_url: None,
        },
    }
}

fn missing_env_error(keys: &[&str]) -> ProviderError {
    ProviderError::NotConfigured {
        message: format!("required env is not set: {}", keys.join(" or ")),
    }
}

fn unsupported_protocol_error(protocol: ProviderProtocol) -> ProviderError {
    ProviderError::NotConfigured {
        message: format!(
            "provider protocol `{}` is registered but its live adapter is not implemented yet",
            protocol.id()
        ),
    }
}

fn redacted_openai_from_reader<F>(reader: &F) -> RedactedProviderConfig
where
    F: Fn(&str) -> Option<String>,
{
    let mapping = env_mapping(ProviderProtocol::OpenAiCompatible);
    let api_key = first_env(reader, mapping.api_key);
    let model = first_env(reader, mapping.model);
    let base_url = first_env(reader, mapping.base_url)
        .map(|(_, value)| value)
        .or_else(|| mapping.default_base_url.map(ToOwned::to_owned));
    let mut missing_env = Vec::new();
    if api_key.is_none() {
        missing_env.push(mapping.api_key.join(" or "));
    }
    if model.is_none() {
        missing_env.push(mapping.model.join(" or "));
    }
    let ready = missing_env.is_empty();

    RedactedProviderConfig {
        mode: "live".to_owned(),
        provider_id: "openai_compatible".to_owned(),
        protocol: ProviderProtocol::OpenAiCompatible,
        native_protocol: "openai_chat_completions".to_owned(),
        base_url: base_url.map(|value| normalize_openai_base_url(&value)),
        model_id: model.as_ref().map(|(_, value)| value.clone()),
        api_key_env: api_key.as_ref().map(|(key, _)| key.clone()),
        api_key_configured: api_key.is_some(),
        missing_env,
        supported: true,
        status: if ready { "ready" } else { "missing_env" }.to_owned(),
    }
}

fn redacted_unsupported_from_reader<F>(
    protocol: ProviderProtocol,
    reader: &F,
) -> RedactedProviderConfig
where
    F: Fn(&str) -> Option<String>,
{
    let mapping = env_mapping(protocol);
    let api_key = first_env(reader, mapping.api_key);
    let model = first_env(reader, mapping.model);
    let base_url = first_env(reader, mapping.base_url)
        .map(|(_, value)| value)
        .or_else(|| mapping.default_base_url.map(ToOwned::to_owned));
    let mut missing_env = Vec::new();
    if !mapping.api_key.is_empty() && api_key.is_none() {
        missing_env.push(mapping.api_key.join(" or "));
    }
    if !mapping.model.is_empty() && model.is_none() {
        missing_env.push(mapping.model.join(" or "));
    }
    if !mapping.base_url.is_empty() && mapping.default_base_url.is_none() && base_url.is_none() {
        missing_env.push(mapping.base_url.join(" or "));
    }

    RedactedProviderConfig {
        mode: "live".to_owned(),
        provider_id: protocol.id().to_owned(),
        protocol,
        native_protocol: protocol.id().to_owned(),
        base_url,
        model_id: model.as_ref().map(|(_, value)| value.clone()),
        api_key_env: api_key.as_ref().map(|(key, _)| key.clone()),
        api_key_configured: api_key.is_some(),
        missing_env,
        supported: false,
        status: "adapter_not_implemented".to_owned(),
    }
}

fn protocol_spec(protocol: ProviderProtocol) -> ProviderProtocolSpec {
    let mapping = env_mapping(protocol);
    let (display_name, status, supports_tool_calls, supports_probe, notes) = match protocol {
        ProviderProtocol::Mock => (
            "Mock".to_owned(),
            "supported".to_owned(),
            true,
            false,
            "Deterministic local provider for smoke tests, replay, and offline development."
                .to_owned(),
        ),
        ProviderProtocol::OpenAiCompatible => (
            "OpenAI-compatible".to_owned(),
            "supported".to_owned(),
            true,
            true,
            "Live Chat Completions adapter for OpenAI-compatible endpoints.".to_owned(),
        ),
        ProviderProtocol::Anthropic => (
            "Anthropic".to_owned(),
            "catalog_only".to_owned(),
            true,
            false,
            "Protocol selection and diagnostics are available; native live adapter is pending."
                .to_owned(),
        ),
        ProviderProtocol::Gemini => (
            "Gemini".to_owned(),
            "catalog_only".to_owned(),
            true,
            false,
            "Protocol selection and diagnostics are available; native live adapter is pending."
                .to_owned(),
        ),
        ProviderProtocol::VertexAi => (
            "Vertex AI".to_owned(),
            "catalog_only".to_owned(),
            true,
            false,
            "Protocol selection and diagnostics are available; native live adapter is pending."
                .to_owned(),
        ),
        ProviderProtocol::Genai => (
            "rust-genai".to_owned(),
            "catalog_only".to_owned(),
            true,
            false,
            "Reserved aggregation protocol for future multi-provider adapters.".to_owned(),
        ),
    };

    ProviderProtocolSpec {
        protocol,
        display_name,
        status,
        api_key_env: mapping
            .api_key
            .iter()
            .map(|key| (*key).to_owned())
            .collect(),
        base_url_env: mapping
            .base_url
            .iter()
            .map(|key| (*key).to_owned())
            .collect(),
        model_env: mapping.model.iter().map(|key| (*key).to_owned()).collect(),
        default_base_url: mapping.default_base_url.map(ToOwned::to_owned),
        supports_tool_calls,
        supports_probe,
        notes,
    }
}

fn first_env<F>(reader: &F, keys: &[&str]) -> Option<(String, String)>
where
    F: Fn(&str) -> Option<String>,
{
    keys.iter().find_map(|key| {
        reader(key)
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
            .map(|value| ((*key).to_owned(), value))
    })
}

#[must_use]
pub fn normalize_openai_base_url(value: &str) -> String {
    let trimmed = value.trim().trim_end_matches('/');
    let with_scheme = if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        trimmed.to_owned()
    } else {
        format!("https://{trimmed}")
    };
    let without_slash = with_scheme.trim_end_matches('/').to_owned();
    let after_scheme = without_slash
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(without_slash.as_str());
    if after_scheme.contains('/') {
        without_slash
    } else {
        format!("{without_slash}/v1")
    }
}

fn sanitize_provider_error(message: &str) -> String {
    let single_line = message.replace(['\n', '\r'], " ");
    let trimmed = single_line.trim();
    if trimmed.len() <= 512 {
        trimmed.to_owned()
    } else {
        format!("{}...", &trimmed[..512])
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
        assert!(
            catalog
                .providers
                .iter()
                .any(|provider| provider.protocol == ProviderProtocol::Anthropic)
        );
        assert!(
            catalog
                .providers
                .iter()
                .any(|provider| provider.protocol == ProviderProtocol::Gemini)
        );
    }

    #[test]
    fn provider_protocol_parses_qwen_style_aliases() {
        assert_eq!(
            ProviderProtocol::from_config_value("anthropic"),
            Some(ProviderProtocol::Anthropic)
        );
        assert_eq!(
            ProviderProtocol::from_config_value("openai_compatible"),
            Some(ProviderProtocol::OpenAiCompatible)
        );
        assert_eq!(
            ProviderProtocol::from_config_value("vertex_ai"),
            Some(ProviderProtocol::VertexAi)
        );
    }

    #[test]
    fn provider_protocol_serializes_stable_wire_ids() {
        assert_eq!(
            serde_json::to_value(ProviderProtocol::OpenAiCompatible).expect("json"),
            json!("openai-compatible")
        );
        assert_eq!(
            serde_json::to_value(ProviderProtocol::VertexAi).expect("json"),
            json!("vertex-ai")
        );
    }

    #[test]
    fn provider_protocol_selection_accepts_mode_and_protocol() {
        let from_mode = selected_protocol_from_reader(&|key| match key {
            GOLUTRA_PROVIDER_MODE => Some("live".to_owned()),
            _ => None,
        });
        let from_protocol = selected_protocol_from_reader(&|key| match key {
            GOLUTRA_PROVIDER_PROTOCOL => Some("openai-compatible".to_owned()),
            GOLUTRA_PROVIDER_MODE => Some("mock".to_owned()),
            _ => None,
        });

        assert_eq!(from_mode, Some(ProviderProtocol::OpenAiCompatible));
        assert_eq!(from_protocol, Some(ProviderProtocol::OpenAiCompatible));
    }

    #[test]
    fn provider_protocol_catalog_includes_registered_protocols() {
        let protocols = provider_protocol_catalog()
            .into_iter()
            .map(|spec| spec.protocol)
            .collect::<Vec<_>>();

        assert_eq!(
            protocols,
            vec![
                ProviderProtocol::OpenAiCompatible,
                ProviderProtocol::Anthropic,
                ProviderProtocol::Gemini,
                ProviderProtocol::VertexAi,
                ProviderProtocol::Genai,
                ProviderProtocol::Mock,
            ]
        );
    }

    #[test]
    fn unsupported_protocol_redacted_config_is_diagnostic_only() {
        let config = redacted_unsupported_from_reader(ProviderProtocol::Anthropic, &|_| None);

        assert_eq!(config.protocol, ProviderProtocol::Anthropic);
        assert!(!config.supported);
        assert_eq!(config.status, "adapter_not_implemented");
        assert!(
            config
                .missing_env
                .iter()
                .any(|value| value.contains(ANTHROPIC_API_KEY))
        );
    }

    #[test]
    fn openai_base_url_accepts_bare_host() {
        assert_eq!(
            normalize_openai_base_url("api.golutra.cn"),
            "https://api.golutra.cn/v1"
        );
        assert_eq!(
            normalize_openai_base_url("https://api.golutra.cn/v1/"),
            "https://api.golutra.cn/v1"
        );
    }

    #[test]
    fn openai_config_reads_golutra_env_first() {
        let config = OpenAiCompatibleProvider::config_from_env_reader(|key| match key {
            GOLUTRA_PROVIDER_API_KEY => Some("golutra-key".to_owned()),
            OPENAI_API_KEY => Some("openai-key".to_owned()),
            GOLUTRA_PROVIDER_MODEL => Some("golutra-model".to_owned()),
            GOLUTRA_PROVIDER_BASE_URL => Some("api.golutra.cn".to_owned()),
            _ => None,
        })
        .expect("config");

        assert_eq!(config.api_key, "golutra-key");
        assert_eq!(config.api_key_env, GOLUTRA_PROVIDER_API_KEY);
        assert_eq!(config.model_id, "golutra-model");
        assert_eq!(config.base_url, "https://api.golutra.cn/v1");
        assert_eq!(config.protocol, ProviderProtocol::OpenAiCompatible);
    }

    #[test]
    fn openai_config_falls_back_to_openai_env_names() {
        let config = OpenAiCompatibleProvider::config_from_env_reader(|key| match key {
            OPENAI_API_KEY => Some("openai-key".to_owned()),
            OPENAI_MODEL => Some("gpt-test".to_owned()),
            OPENAI_BASE_URL => Some("http://localhost:11434/v1".to_owned()),
            _ => None,
        })
        .expect("config");

        assert_eq!(config.api_key_env, OPENAI_API_KEY);
        assert_eq!(config.model_id, "gpt-test");
        assert_eq!(config.base_url, "http://localhost:11434/v1");
        assert_eq!(config.protocol, ProviderProtocol::OpenAiCompatible);
    }

    #[test]
    fn openai_config_rejects_registered_unsupported_protocol() {
        let error = OpenAiCompatibleProvider::config_from_env_reader(|key| match key {
            GOLUTRA_PROVIDER_PROTOCOL => Some("anthropic".to_owned()),
            ANTHROPIC_API_KEY => Some("anthropic-key".to_owned()),
            ANTHROPIC_MODEL => Some("claude-test".to_owned()),
            _ => None,
        })
        .expect_err("anthropic adapter is not openai-compatible");

        assert!(matches!(error, ProviderError::NotConfigured { .. }));
    }

    #[test]
    fn openai_config_requires_model() {
        let error = OpenAiCompatibleProvider::config_from_env_reader(|key| match key {
            GOLUTRA_PROVIDER_API_KEY => Some("golutra-key".to_owned()),
            _ => None,
        })
        .expect_err("model is required");

        assert!(matches!(error, ProviderError::NotConfigured { .. }));
    }

    #[test]
    fn write_file_tool_schema_has_required_arguments() {
        let schema = openai_tool_schema(&"write_file".to_owned());
        let required = schema
            .pointer("/function/parameters/required")
            .and_then(Value::as_array)
            .expect("required");

        assert!(required.contains(&json!("path")));
        assert!(required.contains(&json!("content")));
    }

    #[test]
    fn provider_error_message_is_single_line_and_bounded() {
        let value = json!({
            "error": {
                "message": format!("bad\n{}", "x".repeat(700))
            }
        });
        let message = provider_error_message(&value);

        assert!(!message.contains('\n'));
        assert!(message.len() <= 515);
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
