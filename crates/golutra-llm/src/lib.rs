use std::{collections::BTreeMap, fmt, sync::Arc};

use async_trait::async_trait;
use eventsource_stream::Eventsource;
use futures_util::StreamExt;
use golutra_auth::{CredentialProvider, FixedCredentialProvider};
use golutra_core::{
    ProviderContract, ProviderRequestId, ProviderResponseId, TaskId, ToolContract, TurnId,
};
pub use golutra_core::{ProviderUsage, UsageSource};
use secrecy::ExposeSecret;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;

mod genai_adapter;
mod openai_responses;
mod provider_config;

pub use genai_adapter::{GenaiProviderAdapter, GenaiProviderConfig};
pub use openai_responses::{OpenAiResponsesProvider, OpenAiResponsesProviderConfig};
pub(crate) use provider_config::{
    apply_generation_config_to_openai_body, configured_or_first_env, custom_headers_from_reader,
    env_mapping, first_env, generation_config_from_reader, is_false, missing_env_error,
    normalize_protocol_value, protocol_spec, redacted_native_from_reader,
    redacted_openai_from_reader, redacted_openai_responses_from_reader, sanitize_provider_error,
    selected_protocol_from_reader,
};
pub use provider_config::{
    normalize_openai_base_url, validate_native_base_url, validate_openai_base_url,
};

const GOLUTRA_PROVIDER_MODE: &str = "GOLUTRA_PROVIDER_MODE";
const GOLUTRA_PROVIDER_PROTOCOL: &str = "GOLUTRA_PROVIDER_PROTOCOL";
const GOLUTRA_PROVIDER_API_KEY: &str = "GOLUTRA_PROVIDER_API_KEY";
const GOLUTRA_PROVIDER_API_KEY_ENV: &str = "GOLUTRA_PROVIDER_API_KEY_ENV";
const GOLUTRA_PROVIDER_MODEL: &str = "GOLUTRA_PROVIDER_MODEL";
const GOLUTRA_PROVIDER_BASE_URL: &str = "GOLUTRA_PROVIDER_BASE_URL";
const GOLUTRA_PROVIDER_GENERATION_CONFIG: &str = "GOLUTRA_PROVIDER_GENERATION_CONFIG";
pub const GOLUTRA_PROVIDER_CUSTOM_HEADERS: &str = "GOLUTRA_PROVIDER_CUSTOM_HEADERS";
const GOLUTRA_PROVIDER_AUTH_PROVIDER: &str = "GOLUTRA_PROVIDER_AUTH_PROVIDER";
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
const GOOGLE_OAUTH_ACCESS_TOKEN: &str = "GOOGLE_OAUTH_ACCESS_TOKEN";
const VERTEX_API_KEY: &str = "VERTEX_API_KEY";
const GENAI_API_KEY: &str = "GENAI_API_KEY";
const GENAI_MODEL: &str = "GENAI_MODEL";
const GENAI_BASE_URL: &str = "GENAI_BASE_URL";
const DEFAULT_OPENAI_BASE_URL: &str = "https://api.openai.com/v1";
const DEFAULT_ANTHROPIC_BASE_URL: &str = "https://api.anthropic.com/v1";
const DEFAULT_GEMINI_BASE_URL: &str = "https://generativelanguage.googleapis.com/v1beta";
const MAX_PROVIDER_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
const MAX_PROVIDER_MESSAGE_BYTES: usize = 128 * 1024;
const MAX_PROVIDER_TOOL_ARGUMENT_BYTES: usize = 256 * 1024;
const MAX_PROVIDER_TOOL_CALL_ID_BYTES: usize = 256;
const MAX_PROVIDER_TOOL_NAME_BYTES: usize = 128;
const MAX_PROVIDER_CUSTOM_HEADERS: usize = 32;
const MAX_PROVIDER_HEADER_VALUE_BYTES: usize = 8 * 1024;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ProviderError {
    #[error("provider failed: {message}")]
    Failed { message: String },
    #[error("provider is temporarily unavailable: {message}")]
    Unavailable { message: String },
    #[error("provider rate limited: {message}")]
    RateLimited { message: String },
    #[error("provider is not configured: {message}")]
    NotConfigured { message: String },
    #[error("provider response is malformed: {message}")]
    Malformed { message: String },
    #[error("provider request timed out: {message}")]
    Timeout { message: String },
    #[error("provider request was cancelled")]
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderRequest {
    pub request_id: ProviderRequestId,
    pub task_id: TaskId,
    pub turn_id: TurnId,
    pub provider_id: String,
    pub model_id: String,
    pub messages: Vec<ProviderMessage>,
    pub tools: Vec<ToolContract>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderMessage {
    pub role: ProviderRole,
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ProviderToolCall>,
    #[serde(default, skip_serializing_if = "ProviderMessageMetadata::is_empty")]
    pub metadata: ProviderMessageMetadata,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderMessageMetadata {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub openai_responses_replay_items: Vec<Value>,
}

impl ProviderMessageMetadata {
    fn is_empty(&self) -> bool {
        self.openai_responses_replay_items.is_empty()
    }
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProviderStreamEvent {
    TextDelta {
        text: String,
    },
    ReasoningDelta {
        text: String,
    },
    ToolCallDelta {
        index: usize,
        tool_call_id: Option<String>,
        tool_name: Option<String>,
    },
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
    #[serde(rename = "openai-responses")]
    OpenAiResponses,
    Anthropic,
    Gemini,
    VertexAi,
    Genai,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum ProviderHeaderValue {
    Literal { value: String },
    Environment { key: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderHeaderConfig {
    pub name: String,
    pub value: ProviderHeaderValue,
}

impl ProviderHeaderConfig {
    pub fn validate(&self) -> Result<(), String> {
        let name = reqwest::header::HeaderName::from_bytes(self.name.as_bytes())
            .map_err(|_| format!("provider header name `{}` is invalid", self.name))?;
        if is_forbidden_custom_header(name.as_str()) {
            return Err(format!(
                "provider header `{}` is controlled by the HTTP transport",
                self.name
            ));
        }
        match &self.value {
            ProviderHeaderValue::Literal { value } => {
                if is_sensitive_header(name.as_str()) {
                    return Err(format!(
                        "sensitive provider header `{}` must use an environment source",
                        self.name
                    ));
                }
                validate_provider_header_value(value)?;
            }
            ProviderHeaderValue::Environment { key } => {
                if key.trim().is_empty() || key.len() > 256 {
                    return Err("provider header environment key is invalid".to_owned());
                }
            }
        }
        Ok(())
    }
}

#[derive(Clone, Default, PartialEq, Eq)]
pub struct ProviderHttpHeaders {
    values: BTreeMap<String, String>,
}

impl fmt::Debug for ProviderHttpHeaders {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderHttpHeaders")
            .field("names", &self.values.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl ProviderHttpHeaders {
    fn from_resolved(values: BTreeMap<String, String>) -> Result<Self, ProviderError> {
        if values.len() > MAX_PROVIDER_CUSTOM_HEADERS {
            return Err(ProviderError::NotConfigured {
                message: format!(
                    "provider custom headers exceed the {MAX_PROVIDER_CUSTOM_HEADERS} entry limit"
                ),
            });
        }
        for (name, value) in &values {
            ProviderHeaderConfig {
                name: name.clone(),
                value: ProviderHeaderValue::Environment {
                    key: "RESOLVED_PROVIDER_HEADER".to_owned(),
                },
            }
            .validate()
            .map_err(|message| ProviderError::NotConfigured { message })?;
            validate_provider_header_value(value)
                .map_err(|message| ProviderError::NotConfigured { message })?;
        }
        Ok(Self { values })
    }

    fn to_header_map(&self) -> reqwest::header::HeaderMap {
        self.values
            .iter()
            .filter_map(|(name, value)| {
                let name = reqwest::header::HeaderName::from_bytes(name.as_bytes()).ok()?;
                let value = reqwest::header::HeaderValue::from_str(value).ok()?;
                Some((name, value))
            })
            .collect()
    }

    #[must_use]
    pub fn names(&self) -> Vec<String> {
        self.values.keys().cloned().collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderReasoningEffort {
    Low,
    Medium,
    High,
    Xhigh,
}

impl ProviderReasoningEffort {
    #[must_use]
    pub fn as_wire_value(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderGenerationConfig {
    #[serde(default, skip_serializing_if = "is_false")]
    pub enable_thinking: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<ProviderReasoningEffort>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window_size: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
}

impl ProviderGenerationConfig {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        !self.enable_thinking
            && self.reasoning_effort.is_none()
            && self.context_window_size.is_none()
            && self.max_tokens.is_none()
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.context_window_size == Some(0) {
            return Err("context_window_size must be greater than zero".to_owned());
        }
        if self.max_tokens == Some(0) {
            return Err("max_tokens must be greater than zero".to_owned());
        }
        if let (Some(context_window), Some(max_tokens)) =
            (self.context_window_size, self.max_tokens)
            && max_tokens >= context_window
        {
            return Err("max_tokens must be smaller than context_window_size".to_owned());
        }
        Ok(())
    }
}

impl ProviderProtocol {
    #[must_use]
    pub fn id(self) -> &'static str {
        match self {
            Self::Mock => "mock",
            Self::OpenAiCompatible => "openai-compatible",
            Self::OpenAiResponses => "openai-responses",
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
            "openai-responses" | "responses" | "chatgpt-codex" => Some(Self::OpenAiResponses),
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
    pub supports_probe: bool,
    pub capabilities: ProviderCapabilities,
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

    /// Whether `complete` uses a transport distinct from `complete_stream`.
    /// ProviderSession uses this to avoid claiming a fallback when both paths
    /// are backed by the same streaming protocol.
    fn supports_buffered_transport(&self) -> bool {
        true
    }

    async fn complete_stream(
        &self,
        request: ProviderRequest,
        on_event: &mut (dyn FnMut(ProviderStreamEvent) + Send),
    ) -> Result<ProviderResponse, ProviderError> {
        let response = self.complete(request).await?;
        if let Some(message) = &response.message
            && !message.content.is_empty()
        {
            on_event(ProviderStreamEvent::TextDelta {
                text: message.content.clone(),
            });
        }
        for (index, tool_call) in response.tool_calls.iter().enumerate() {
            on_event(ProviderStreamEvent::ToolCallDelta {
                index,
                tool_call_id: Some(tool_call.tool_call_id.clone()),
                tool_name: Some(tool_call.tool_name.clone()),
            });
        }
        Ok(response)
    }

    fn contract(&self) -> ProviderContract;
}

#[derive(Debug, Clone)]
pub struct MockProvider {
    contract: ProviderContract,
    outcome: MockProviderOutcome,
}

#[derive(Debug, Clone)]
enum MockProviderOutcome {
    Response(Box<ProviderResponse>),
    Error(ProviderError),
}

impl MockProvider {
    #[must_use]
    pub fn text_response(content: impl Into<String>) -> Self {
        Self {
            contract: mock_contract(),
            outcome: MockProviderOutcome::Response(Box::new(ProviderResponse {
                response_id: ProviderResponseId::new(),
                message: Some(ProviderMessage {
                    role: ProviderRole::Assistant,
                    content: content.into(),
                    tool_call_id: None,
                    tool_name: None,
                    tool_calls: Vec::new(),
                    metadata: ProviderMessageMetadata::default(),
                }),
                tool_calls: Vec::new(),
                usage: usage(128, 32),
                finish_reason: ProviderFinishReason::Stop,
                raw_metadata: serde_json::json!({"provider": "mock"}),
            })),
        }
    }

    #[must_use]
    pub fn tool_call(tool_name: impl Into<String>, arguments: Value) -> Self {
        let tool_name = tool_name.into();
        Self {
            contract: mock_contract(),
            outcome: MockProviderOutcome::Response(Box::new(ProviderResponse {
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
            })),
        }
    }

    /// 为 Runtime 失败链和重试策略提供确定性错误，不依赖外部网络夹具。
    #[must_use]
    pub fn failure(message: impl Into<String>) -> Self {
        Self {
            contract: mock_contract(),
            outcome: MockProviderOutcome::Error(ProviderError::Failed {
                message: message.into(),
            }),
        }
    }
}

#[async_trait]
impl LlmProvider for MockProvider {
    async fn complete(&self, request: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        let response = match &self.outcome {
            MockProviderOutcome::Response(response) => response.as_ref(),
            MockProviderOutcome::Error(error) => return Err(error.clone()),
        };
        if !response.tool_calls.is_empty()
            && request
                .messages
                .iter()
                .any(|message| message.role == ProviderRole::Tool)
        {
            let summary = request
                .messages
                .iter()
                .rev()
                .find(|message| message.role == ProviderRole::Tool)
                .and_then(|message| serde_json::from_str::<Value>(&message.content).ok())
                .and_then(|value| {
                    value
                        .get("summary")
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned)
                })
                .unwrap_or_else(|| "tool result accepted".to_owned());
            return Ok(ProviderResponse {
                response_id: ProviderResponseId::new(),
                message: Some(ProviderMessage {
                    role: ProviderRole::Assistant,
                    content: format!("Completed: {summary}"),
                    tool_call_id: None,
                    tool_name: None,
                    tool_calls: Vec::new(),
                    metadata: ProviderMessageMetadata::default(),
                }),
                tool_calls: Vec::new(),
                usage: usage(64, 16),
                finish_reason: ProviderFinishReason::Stop,
                raw_metadata: json!({"provider": "mock", "phase": "after_tool"}),
            });
        }
        Ok(response.clone())
    }

    fn contract(&self) -> ProviderContract {
        self.contract.clone()
    }
}

#[derive(Clone)]
pub struct OpenAiCompatibleProvider {
    credential: Arc<dyn CredentialProvider>,
    api_key_env: String,
    provider_id: String,
    base_url: String,
    model_id: String,
    generation_config: ProviderGenerationConfig,
    custom_headers: ProviderHttpHeaders,
    client: reqwest::Client,
}

#[derive(Clone, PartialEq, Eq)]
pub struct OpenAiCompatibleProviderConfig {
    pub api_key: String,
    pub api_key_env: String,
    pub provider_id: String,
    pub base_url: String,
    pub model_id: String,
    pub protocol: ProviderProtocol,
    pub generation_config: ProviderGenerationConfig,
    pub custom_headers: ProviderHttpHeaders,
}

impl fmt::Debug for OpenAiCompatibleProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpenAiCompatibleProvider")
            .field("credential_source", &self.api_key_env)
            .field("provider_id", &self.provider_id)
            .field("base_url", &self.base_url)
            .field("model_id", &self.model_id)
            .field("generation_config", &self.generation_config)
            .field("custom_header_names", &self.custom_headers.names())
            .finish_non_exhaustive()
    }
}

impl fmt::Debug for OpenAiCompatibleProviderConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpenAiCompatibleProviderConfig")
            .field("api_key_env", &self.api_key_env)
            .field("provider_id", &self.provider_id)
            .field("base_url", &self.base_url)
            .field("model_id", &self.model_id)
            .field("protocol", &self.protocol)
            .field("generation_config", &self.generation_config)
            .field("custom_header_names", &self.custom_headers.names())
            .finish_non_exhaustive()
    }
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
    pub generation_config: Option<ProviderGenerationConfig>,
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
    pub capabilities: ProviderCapabilities,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderCapabilitySource {
    Declared,
    Discovered,
    Inferred,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderCapabilities {
    pub supports_streaming: bool,
    pub supports_tools: bool,
    pub supports_json_schema: bool,
    pub supports_reasoning: bool,
    pub supports_vision: bool,
    pub context_window: Option<u64>,
    pub max_output_tokens: Option<u64>,
    pub source: ProviderCapabilitySource,
}

impl OpenAiCompatibleProvider {
    fn authenticated_request(
        &self,
        builder: reqwest::RequestBuilder,
        token: &str,
        initiator: &str,
    ) -> reqwest::RequestBuilder {
        let builder = builder.bearer_auth(token);
        let builder = if self.provider_id == "github-copilot" {
            builder
                .header(
                    reqwest::header::USER_AGENT,
                    format!("golutra/{}", env!("CARGO_PKG_VERSION")),
                )
                .header("X-GitHub-Api-Version", "2026-06-01")
                .header("Openai-Intent", "conversation-edits")
                .header("x-initiator", initiator)
        } else {
            builder
        };
        builder.headers(self.custom_headers.to_header_map())
    }

    #[must_use]
    pub fn new(
        api_key: impl Into<String>,
        base_url: impl Into<String>,
        model_id: impl Into<String>,
    ) -> Self {
        let api_key = api_key.into();
        Self {
            credential: Arc::new(FixedCredentialProvider::new(
                api_key,
                GOLUTRA_PROVIDER_API_KEY,
            )),
            api_key_env: GOLUTRA_PROVIDER_API_KEY.to_owned(),
            provider_id: "openai-compatible".to_owned(),
            base_url: normalize_openai_base_url(&base_url.into()),
            model_id: model_id.into(),
            generation_config: ProviderGenerationConfig::default(),
            custom_headers: ProviderHttpHeaders::default(),
            client: provider_http_client(),
        }
    }

    #[must_use]
    pub fn from_config(config: OpenAiCompatibleProviderConfig) -> Self {
        let credential = Arc::new(FixedCredentialProvider::new(
            config.api_key.clone(),
            config.api_key_env.clone(),
        ));
        Self::from_config_with_credential(config, credential)
    }

    #[must_use]
    pub fn from_config_with_credential(
        config: OpenAiCompatibleProviderConfig,
        credential: Arc<dyn CredentialProvider>,
    ) -> Self {
        Self {
            credential,
            api_key_env: config.api_key_env,
            provider_id: config.provider_id,
            base_url: normalize_openai_base_url(&config.base_url),
            model_id: config.model_id,
            generation_config: config.generation_config,
            custom_headers: config.custom_headers,
            client: provider_http_client(),
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
            return Err(ProviderError::NotConfigured {
                message: format!(
                    "provider protocol `{}` is not OpenAI-compatible",
                    protocol.id()
                ),
            });
        }
        let mapping = env_mapping(protocol);
        let (api_key_env, api_key) = configured_or_first_env(&reader, mapping.api_key)
            .ok_or_else(|| missing_env_error(mapping.api_key))?;
        let (_, model_id) =
            first_env(&reader, mapping.model).ok_or_else(|| missing_env_error(mapping.model))?;
        let base_url = first_env(&reader, mapping.base_url)
            .map(|(_, value)| value)
            .or_else(|| mapping.default_base_url.map(ToOwned::to_owned))
            .ok_or_else(|| missing_env_error(mapping.base_url))?;
        let base_url = validate_openai_base_url(&base_url)
            .map_err(|message| ProviderError::NotConfigured { message })?;
        let generation_config = generation_config_from_reader(&reader)?;
        let custom_headers = custom_headers_from_reader(&reader)?;
        Ok(OpenAiCompatibleProviderConfig {
            api_key,
            api_key_env,
            provider_id: reader(GOLUTRA_PROVIDER_AUTH_PROVIDER)
                .filter(|value| !value.trim().is_empty())
                .unwrap_or_else(|| "openai-compatible".to_owned()),
            base_url,
            model_id,
            protocol,
            generation_config,
            custom_headers,
        })
    }

    #[must_use]
    pub fn redacted_config(&self) -> RedactedProviderConfig {
        RedactedProviderConfig {
            mode: "live".to_owned(),
            provider_id: self.provider_id.clone(),
            protocol: ProviderProtocol::OpenAiCompatible,
            native_protocol: "openai_chat_completions".to_owned(),
            base_url: Some(self.base_url.clone()),
            model_id: Some(self.model_id.clone()),
            api_key_env: Some(self.api_key_env.clone()),
            api_key_configured: true,
            generation_config: (!self.generation_config.is_empty())
                .then_some(self.generation_config.clone()),
            missing_env: Vec::new(),
            supported: true,
            status: "ready".to_owned(),
        }
    }

    async fn get_with_auth_retry(&self, url: &str) -> Result<reqwest::Response, ProviderError> {
        let token = self
            .credential
            .credential(false)
            .await
            .map_err(provider_credential_error)?;
        let response = self
            .authenticated_request(self.client.get(url), token.expose_secret(), "user")
            .send()
            .await
            .map_err(provider_transport_error)?;
        if response.status().as_u16() != 401 {
            return Ok(response);
        }
        let token = self
            .credential
            .credential(true)
            .await
            .map_err(provider_credential_error)?;
        self.authenticated_request(self.client.get(url), token.expose_secret(), "user")
            .send()
            .await
            .map_err(provider_transport_error)
    }

    async fn post_with_auth_retry(
        &self,
        url: &str,
        body: &Value,
    ) -> Result<reqwest::Response, ProviderError> {
        let token = self
            .credential
            .credential(false)
            .await
            .map_err(provider_credential_error)?;
        let initiator = openai_request_initiator(body);
        let response = self
            .authenticated_request(
                self.client.post(url).json(body),
                token.expose_secret(),
                initiator,
            )
            .send()
            .await
            .map_err(provider_transport_error)?;
        if response.status().as_u16() != 401 {
            return Ok(response);
        }
        let token = self
            .credential
            .credential(true)
            .await
            .map_err(provider_credential_error)?;
        self.authenticated_request(
            self.client.post(url).json(body),
            token.expose_secret(),
            initiator,
        )
        .send()
        .await
        .map_err(provider_transport_error)
    }

    pub async fn probe(&self) -> Result<ProviderProbeResult, ProviderError> {
        let url = format!("{}/models", self.base_url.trim_end_matches('/'));
        let response = self.get_with_auth_retry(&url).await?;
        let status = response.status();
        let value = response_json_or_error(response).await?;
        if status.as_u16() == 429 {
            return Err(ProviderError::RateLimited {
                message: provider_error_message(&value),
            });
        }
        if !status.is_success() {
            return Err(provider_http_error(status, &value));
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
            provider_id: self.provider_id.clone(),
            protocol: "openai_chat_completions".to_owned(),
            base_url: self.base_url.clone(),
            model_id: self.model_id.clone(),
            model_available,
            discovered_models,
            capabilities: openai_capabilities_from_models(&value, &self.model_id),
        })
    }
}

#[async_trait]
impl LlmProvider for OpenAiCompatibleProvider {
    async fn complete(&self, request: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        let body = openai_completion_body(&request, &self.model_id, &self.generation_config, false);

        let response = self.post_with_auth_retry(&url, &body).await?;
        let status = response.status();
        let value = response_json_or_error(response).await?;
        if status.as_u16() == 429 {
            return Err(ProviderError::RateLimited {
                message: provider_error_message(&value),
            });
        }
        if !status.is_success() {
            return Err(provider_http_error(status, &value));
        }

        provider_response_from_openai(value, request.task_id, request.turn_id)
    }

    async fn complete_stream(
        &self,
        request: ProviderRequest,
        on_event: &mut (dyn FnMut(ProviderStreamEvent) + Send),
    ) -> Result<ProviderResponse, ProviderError> {
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        let body = openai_completion_body(&request, &self.model_id, &self.generation_config, true);
        let response = self.post_with_auth_retry(&url, &body).await?;
        let status = response.status();
        if status.as_u16() == 429 {
            let value = response_json_or_error(response).await?;
            return Err(ProviderError::RateLimited {
                message: provider_error_message(&value),
            });
        }
        if !status.is_success() {
            let value = response_json_or_error(response).await?;
            return Err(provider_http_error(status, &value));
        }
        provider_response_from_openai_stream(response, on_event).await
    }

    fn contract(&self) -> ProviderContract {
        ProviderContract {
            provider_id: self.provider_id.clone(),
            model_id: self.model_id.clone(),
            native_protocol: "openai_chat_completions".to_owned(),
            stream_event_mapping: "chat_completion_sse_delta".to_owned(),
            tool_call_mapping: "function_tool_calls".to_owned(),
            usage_mapping: "chat_completion_usage".to_owned(),
            reasoning_mapping: "not_exposed".to_owned(),
            finish_reason_mapping: "chat_completion_finish_reason".to_owned(),
            error_mapping: "http_status_and_error_body".to_owned(),
            rate_limit_mapping: "http_429".to_owned(),
            cost_model: "external".to_owned(),
            capability_matrix_ref: None,
            golden_fixture_refs: [
                "request",
                "text_response",
                "tool_response",
                "error_response",
            ]
            .into_iter()
            .map(|fixture| format!("tests/fixtures/openai-compatible/{fixture}.json"))
            .collect(),
        }
    }
}

#[derive(Debug, Clone)]
pub enum ConfiguredProvider {
    Mock(Box<MockProvider>),
    OpenAiCompatible(OpenAiCompatibleProvider),
    OpenAiResponses(OpenAiResponsesProvider),
    Anthropic(GenaiProviderAdapter),
    Gemini(GenaiProviderAdapter),
    VertexAi(GenaiProviderAdapter),
    Genai(GenaiProviderAdapter),
}

impl ConfiguredProvider {
    pub fn resolve_from_env(mock: MockProvider) -> Result<Self, ProviderError> {
        Self::resolve_from_reader(mock, |key| std::env::var(key).ok())
    }

    pub fn resolve_from_reader<F>(mock: MockProvider, reader: F) -> Result<Self, ProviderError>
    where
        F: Fn(&str) -> Option<String>,
    {
        Self::resolve_from_reader_with_credential(mock, reader, None)
    }

    pub fn resolve_from_reader_with_credential<F>(
        mock: MockProvider,
        reader: F,
        credential: Option<Arc<dyn CredentialProvider>>,
    ) -> Result<Self, ProviderError>
    where
        F: Fn(&str) -> Option<String>,
    {
        let Some(protocol) = selected_protocol_from_reader(&reader) else {
            return Ok(Self::Mock(Box::new(mock)));
        };
        if protocol == ProviderProtocol::Mock {
            return Ok(Self::Mock(Box::new(mock)));
        }
        if protocol == ProviderProtocol::OpenAiCompatible {
            return OpenAiCompatibleProvider::config_from_env_reader(reader)
                .map(|config| match credential {
                    Some(credential) => {
                        OpenAiCompatibleProvider::from_config_with_credential(config, credential)
                    }
                    None => OpenAiCompatibleProvider::from_config(config),
                })
                .map(Self::OpenAiCompatible);
        }
        if protocol == ProviderProtocol::OpenAiResponses {
            return OpenAiResponsesProvider::config_from_env_reader(reader)
                .map(|config| match credential {
                    Some(credential) => {
                        OpenAiResponsesProvider::from_config_with_credential(config, credential)
                    }
                    None => OpenAiResponsesProvider::from_config(config),
                })
                .map(Self::OpenAiResponses);
        }
        let config = GenaiProviderAdapter::config_from_env_reader(reader)?;
        let provider = match credential {
            Some(credential) => {
                GenaiProviderAdapter::from_config_with_credential(config, credential)
            }
            None => GenaiProviderAdapter::from_config(config),
        };
        Ok(configured_native_provider(protocol, provider))
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
                generation_config: None,
                missing_env: Vec::new(),
                supported: true,
                status: "ready".to_owned(),
            });
        }
        match protocol {
            ProviderProtocol::Mock => unreachable!("mock is returned above"),
            ProviderProtocol::OpenAiCompatible => Ok(redacted_openai_from_reader(&reader)),
            ProviderProtocol::OpenAiResponses => Ok(redacted_openai_responses_from_reader(&reader)),
            ProviderProtocol::Anthropic
            | ProviderProtocol::Gemini
            | ProviderProtocol::VertexAi
            | ProviderProtocol::Genai => Ok(redacted_native_from_reader(protocol, &reader)),
        }
    }

    pub async fn probe_from_env() -> Result<ProviderProbeResult, ProviderError> {
        Self::probe_from_reader(|key| std::env::var(key).ok()).await
    }

    pub async fn probe_from_reader<F>(reader: F) -> Result<ProviderProbeResult, ProviderError>
    where
        F: Fn(&str) -> Option<String>,
    {
        Self::probe_from_reader_with_credential(reader, None).await
    }

    pub async fn probe_from_reader_with_credential<F>(
        reader: F,
        credential: Option<Arc<dyn CredentialProvider>>,
    ) -> Result<ProviderProbeResult, ProviderError>
    where
        F: Fn(&str) -> Option<String>,
    {
        let protocol = selected_protocol_from_reader(&reader).unwrap_or(ProviderProtocol::Mock);
        if protocol == ProviderProtocol::Mock {
            return Ok(ProviderProbeResult {
                provider_id: "mock".to_owned(),
                protocol: "in_memory".to_owned(),
                base_url: "in-memory".to_owned(),
                model_id: "mock-model".to_owned(),
                model_available: Some(true),
                discovered_models: vec!["mock-model".to_owned()],
                capabilities: protocol_capabilities(ProviderProtocol::Mock),
            });
        }
        if protocol == ProviderProtocol::OpenAiCompatible {
            let config = OpenAiCompatibleProvider::config_from_env_reader(reader)?;
            return match credential {
                Some(credential) => {
                    OpenAiCompatibleProvider::from_config_with_credential(config, credential)
                }
                None => OpenAiCompatibleProvider::from_config(config),
            }
            .probe()
            .await;
        }
        if protocol == ProviderProtocol::OpenAiResponses {
            let config = OpenAiResponsesProvider::config_from_env_reader(reader)?;
            return match credential {
                Some(credential) => {
                    OpenAiResponsesProvider::from_config_with_credential(config, credential)
                }
                None => OpenAiResponsesProvider::from_config(config),
            }
            .probe()
            .await;
        }
        let config = GenaiProviderAdapter::config_from_env_reader(reader)?;
        match credential {
            Some(credential) => {
                GenaiProviderAdapter::from_config_with_credential(config, credential)
            }
            None => GenaiProviderAdapter::from_config(config),
        }
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
            Self::OpenAiResponses(provider) => provider.complete(request).await,
            Self::Anthropic(provider)
            | Self::Gemini(provider)
            | Self::VertexAi(provider)
            | Self::Genai(provider) => provider.complete(request).await,
        }
    }

    async fn complete_stream(
        &self,
        request: ProviderRequest,
        on_event: &mut (dyn FnMut(ProviderStreamEvent) + Send),
    ) -> Result<ProviderResponse, ProviderError> {
        match self {
            Self::Mock(provider) => provider.complete_stream(request, on_event).await,
            Self::OpenAiCompatible(provider) => provider.complete_stream(request, on_event).await,
            Self::OpenAiResponses(provider) => provider.complete_stream(request, on_event).await,
            Self::Anthropic(provider)
            | Self::Gemini(provider)
            | Self::VertexAi(provider)
            | Self::Genai(provider) => provider.complete_stream(request, on_event).await,
        }
    }

    fn supports_buffered_transport(&self) -> bool {
        match self {
            Self::Mock(provider) => provider.supports_buffered_transport(),
            Self::OpenAiCompatible(provider) => provider.supports_buffered_transport(),
            Self::OpenAiResponses(provider) => provider.supports_buffered_transport(),
            Self::Anthropic(provider)
            | Self::Gemini(provider)
            | Self::VertexAi(provider)
            | Self::Genai(provider) => provider.supports_buffered_transport(),
        }
    }

    fn contract(&self) -> ProviderContract {
        match self {
            Self::Mock(provider) => provider.contract(),
            Self::OpenAiCompatible(provider) => provider.contract(),
            Self::OpenAiResponses(provider) => provider.contract(),
            Self::Anthropic(provider)
            | Self::Gemini(provider)
            | Self::VertexAi(provider)
            | Self::Genai(provider) => provider.contract(),
        }
    }
}

fn configured_native_provider(
    protocol: ProviderProtocol,
    provider: GenaiProviderAdapter,
) -> ConfiguredProvider {
    match protocol {
        ProviderProtocol::Anthropic => ConfiguredProvider::Anthropic(provider),
        ProviderProtocol::Gemini => ConfiguredProvider::Gemini(provider),
        ProviderProtocol::VertexAi => ConfiguredProvider::VertexAi(provider),
        ProviderProtocol::Genai => ConfiguredProvider::Genai(provider),
        ProviderProtocol::Mock
        | ProviderProtocol::OpenAiCompatible
        | ProviderProtocol::OpenAiResponses => {
            unreachable!("native provider helper only accepts native protocols")
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
    pub capabilities: ProviderCapabilities,
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
                    provider_id: "openai-chatgpt".to_owned(),
                    protocol: ProviderProtocol::OpenAiResponses,
                    model_id: "configured-at-runtime".to_owned(),
                    auth_env: Some(GOLUTRA_PROVIDER_API_KEY.to_owned()),
                    base_url: Some("https://chatgpt.com/backend-api/codex".to_owned()),
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
                    capabilities: protocol_capabilities(ProviderProtocol::Mock),
                },
                ModelCapability {
                    provider_id: "openai-compatible".to_owned(),
                    model_id: "configured-at-runtime".to_owned(),
                    capabilities: protocol_capabilities(ProviderProtocol::OpenAiCompatible),
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

    pub fn apply_probe(&mut self, probe: &ProviderProbeResult) {
        let capability = ModelCapability {
            provider_id: probe.provider_id.clone(),
            model_id: probe.model_id.clone(),
            capabilities: probe.capabilities.clone(),
        };
        if let Some(existing) = self.capabilities.iter_mut().find(|existing| {
            existing.provider_id == capability.provider_id
                && existing.model_id == capability.model_id
        }) {
            *existing = capability;
        } else {
            self.capabilities.push(capability);
        }
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
        ProviderProtocol::OpenAiResponses,
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

#[must_use]
pub fn protocol_capabilities(protocol: ProviderProtocol) -> ProviderCapabilities {
    let (streaming, tools, json_schema, reasoning, context_window, max_output_tokens) =
        match protocol {
            ProviderProtocol::Mock => (false, true, false, false, Some(8_192), Some(1_024)),
            ProviderProtocol::OpenAiCompatible => {
                (true, true, true, true, Some(128_000), Some(8_192))
            }
            ProviderProtocol::OpenAiResponses => {
                (true, true, false, true, Some(200_000), Some(100_000))
            }
            ProviderProtocol::Anthropic => (true, true, false, true, Some(200_000), Some(8_192)),
            ProviderProtocol::Gemini | ProviderProtocol::VertexAi => {
                (true, true, false, true, Some(1_000_000), Some(8_192))
            }
            ProviderProtocol::Genai => (true, true, false, true, None, None),
        };
    ProviderCapabilities {
        supports_streaming: streaming,
        supports_tools: tools,
        supports_json_schema: json_schema,
        supports_reasoning: reasoning,
        supports_vision: false,
        context_window,
        max_output_tokens,
        source: ProviderCapabilitySource::Declared,
    }
}

fn openai_capabilities_from_models(value: &Value, model_id: &str) -> ProviderCapabilities {
    let mut capabilities = protocol_capabilities(ProviderProtocol::OpenAiCompatible);
    let Some(model) = value
        .get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find(|model| model.get("id").and_then(Value::as_str) == Some(model_id))
    else {
        return capabilities;
    };
    let parameters = model
        .get("supported_parameters")
        .and_then(Value::as_array)
        .map(|values| values.iter().filter_map(Value::as_str).collect::<Vec<_>>())
        .unwrap_or_default();
    if !parameters.is_empty() {
        capabilities.supports_streaming = parameters.contains(&"stream");
        capabilities.supports_tools = parameters
            .iter()
            .any(|value| matches!(*value, "tools" | "tool_choice" | "function_call"));
        capabilities.supports_json_schema = parameters.iter().any(|value| {
            matches!(
                *value,
                "response_format" | "structured_outputs" | "json_schema"
            )
        });
        capabilities.supports_reasoning = parameters
            .iter()
            .any(|value| matches!(*value, "reasoning" | "reasoning_effort"));
    }
    capabilities.context_window = model
        .get("context_length")
        .or_else(|| model.get("context_window"))
        .and_then(Value::as_u64)
        .or(capabilities.context_window);
    capabilities.max_output_tokens = model
        .get("max_output_tokens")
        .or_else(|| {
            model
                .get("top_provider")
                .and_then(|provider| provider.get("max_completion_tokens"))
        })
        .and_then(Value::as_u64)
        .or(capabilities.max_output_tokens);
    capabilities.supports_vision = model
        .get("architecture")
        .and_then(|architecture| architecture.get("input_modalities"))
        .and_then(Value::as_array)
        .is_some_and(|values| values.iter().any(|value| value.as_str() == Some("image")));
    capabilities.source = ProviderCapabilitySource::Discovered;
    capabilities
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
    let mut value = json!({
        "role": match message.role {
            ProviderRole::System => "system",
            ProviderRole::User => "user",
            ProviderRole::Assistant => "assistant",
            ProviderRole::Tool => "tool",
        },
        "content": message.content,
    });
    if let Some(tool_call_id) = &message.tool_call_id {
        value["tool_call_id"] = Value::String(tool_call_id.clone());
    }
    if let Some(tool_name) = &message.tool_name {
        value["name"] = Value::String(tool_name.clone());
    }
    if !message.tool_calls.is_empty() {
        value["tool_calls"] = Value::Array(
            message
                .tool_calls
                .iter()
                .map(openai_assistant_tool_call)
                .collect(),
        );
    }
    value
}

fn openai_assistant_tool_call(tool_call: &ProviderToolCall) -> Value {
    json!({
        "id": tool_call.tool_call_id,
        "type": "function",
        "function": {
            "name": tool_call.tool_name,
            "arguments": tool_call.arguments.to_string(),
        }
    })
}

fn openai_request_initiator(body: &Value) -> &'static str {
    let last_role = body
        .get("messages")
        .and_then(Value::as_array)
        .and_then(|messages| messages.last())
        .and_then(|message| message.get("role"))
        .and_then(Value::as_str);
    if last_role == Some("user") {
        "user"
    } else {
        "agent"
    }
}

fn validate_provider_header_value(value: &str) -> Result<(), String> {
    if value.len() > MAX_PROVIDER_HEADER_VALUE_BYTES {
        return Err(format!(
            "provider header value exceeds {MAX_PROVIDER_HEADER_VALUE_BYTES} byte limit"
        ));
    }
    reqwest::header::HeaderValue::from_str(value)
        .map(|_| ())
        .map_err(|_| "provider header value contains invalid characters".to_owned())
}

fn is_forbidden_custom_header(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "authorization"
            | "proxy-authorization"
            | "connection"
            | "content-length"
            | "host"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
    )
}

fn is_sensitive_header(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    name.contains("api-key")
        || name.contains("token")
        || name.contains("secret")
        || name.contains("credential")
        || name == "cookie"
}

pub(crate) fn provider_tool_description(tool_name: &str) -> &'static str {
    match tool_name {
        "read_file" => "Read a UTF-8 text file from the current workspace.",
        "write_file" => "Write UTF-8 text content to a workspace-relative file.",
        "edit_file" => "Replace the first exact text match in a workspace-relative file.",
        "list_dir" => "List entries in a workspace-relative directory.",
        "rg_search" => "Search workspace files with ripgrep.",
        "shell" => {
            "Run a workspace command as argv. A complete quoted foreground Python heredoc such as python - <<'PY' is passed directly on stdin; for other pipes, redirection, or compound scripts, explicitly invoke bash -lc with the complete script as one argument. With background=true, timeout_ms is the absolute process lifetime and yield_time_ms only controls the initial wait. Managed background processes are runtime-scoped and stop when the runtime ends. If another process or evaluator must connect after the final response, do not use background=true; launch a self-daemonizing service with background omitted, for example bash -lc 'nohup ... </dev/null >service.log 2>&1 &', and verify it before returning."
        }
        _ => "Golutra workspace tool.",
    }
}

fn openai_tool_schema(contract: &ToolContract) -> Value {
    let description = provider_tool_description(&contract.tool_name);
    json!({
        "type": "function",
        "function": {
            "name": contract.tool_name,
            "description": description,
            "parameters": contract.input_schema
        }
    })
}

fn openai_completion_body(
    request: &ProviderRequest,
    model_id: &str,
    generation_config: &ProviderGenerationConfig,
    streaming: bool,
) -> Value {
    let mut body = json!({
        "model": model_id,
        "messages": request.messages.iter().map(openai_message).collect::<Vec<_>>(),
    });
    if !request.tools.is_empty() {
        body["tools"] = Value::Array(request.tools.iter().map(openai_tool_schema).collect());
        body["tool_choice"] = Value::String("auto".to_owned());
    }
    if streaming {
        body["stream"] = Value::Bool(true);
        body["stream_options"] = json!({"include_usage": true});
    }
    apply_generation_config_to_openai_body(&mut body, generation_config);
    body
}

#[derive(Debug, Default)]
struct OpenAiToolCallAccumulator {
    tool_call_id: String,
    tool_name: String,
    arguments: String,
}

async fn provider_response_from_openai_stream(
    response: reqwest::Response,
    on_event: &mut (dyn FnMut(ProviderStreamEvent) + Send),
) -> Result<ProviderResponse, ProviderError> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_PROVIDER_RESPONSE_BYTES as u64)
    {
        return Err(ProviderError::Malformed {
            message: format!("provider response exceeds {MAX_PROVIDER_RESPONSE_BYTES} byte limit"),
        });
    }
    let mut stream = response.bytes_stream().eventsource();
    let mut parsed_bytes = 0_usize;
    let mut output_text = String::new();
    let mut tool_calls = BTreeMap::<usize, OpenAiToolCallAccumulator>::new();
    let mut usage_value = json!({});
    let mut response_id = None;
    let mut finish_reason = None;
    let mut stream_terminated = false;

    while let Some(event) = stream.next().await {
        let event = event.map_err(|error| ProviderError::Unavailable {
            message: sanitize_provider_error(&error.to_string()),
        })?;
        parsed_bytes = parsed_bytes.saturating_add(event.data.len());
        if parsed_bytes > MAX_PROVIDER_RESPONSE_BYTES {
            return Err(ProviderError::Malformed {
                message: format!(
                    "provider response exceeds {MAX_PROVIDER_RESPONSE_BYTES} byte limit"
                ),
            });
        }
        if event.data.trim() == "[DONE]" {
            stream_terminated = true;
            continue;
        }
        if event.data.trim().is_empty() {
            continue;
        }
        let value: Value =
            serde_json::from_str(&event.data).map_err(|error| ProviderError::Malformed {
                message: format!("chat completion SSE event is invalid JSON: {error}"),
            })?;
        if value.get("error").is_some() {
            return Err(ProviderError::Failed {
                message: provider_error_message(&value),
            });
        }
        response_id = response_id.or_else(|| {
            value
                .get("id")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
        });
        if let Some(usage) = value.get("usage")
            && !usage.is_null()
        {
            usage_value = usage.clone();
        }
        let Some(choice) = value
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|choices| choices.first())
        else {
            continue;
        };
        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            finish_reason = Some(finish_reason_from_openai(reason));
            stream_terminated = true;
        }
        let Some(delta) = choice.get("delta") else {
            continue;
        };
        if let Some(text) = delta.get("content").and_then(Value::as_str)
            && !text.is_empty()
        {
            if output_text.len().saturating_add(text.len()) > MAX_PROVIDER_MESSAGE_BYTES {
                return Err(ProviderError::Malformed {
                    message: format!(
                        "assistant message exceeds {MAX_PROVIDER_MESSAGE_BYTES} byte limit"
                    ),
                });
            }
            output_text.push_str(text);
            on_event(ProviderStreamEvent::TextDelta {
                text: text.to_owned(),
            });
        }
        if let Some(text) = delta
            .get("reasoning_content")
            .or_else(|| delta.get("reasoning"))
            .and_then(Value::as_str)
            && !text.is_empty()
        {
            on_event(ProviderStreamEvent::ReasoningDelta {
                text: text.to_owned(),
            });
        }
        for tool_delta in delta
            .get("tool_calls")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let index = tool_delta
                .get("index")
                .and_then(Value::as_u64)
                .and_then(|value| usize::try_from(value).ok())
                .ok_or_else(|| ProviderError::Malformed {
                    message: "streamed tool call has no valid index".to_owned(),
                })?;
            let accumulator = tool_calls.entry(index).or_default();
            if let Some(id) = tool_delta.get("id").and_then(Value::as_str)
                && !id.is_empty()
            {
                accumulator.tool_call_id = id.to_owned();
            }
            let function = tool_delta.get("function").unwrap_or(&Value::Null);
            if let Some(name) = function.get("name").and_then(Value::as_str)
                && !name.is_empty()
            {
                accumulator.tool_name.push_str(name);
            }
            if let Some(arguments) = function.get("arguments").and_then(Value::as_str) {
                if accumulator.arguments.len().saturating_add(arguments.len())
                    > MAX_PROVIDER_TOOL_ARGUMENT_BYTES
                {
                    return Err(ProviderError::Malformed {
                        message: format!(
                            "tool call arguments exceed {MAX_PROVIDER_TOOL_ARGUMENT_BYTES} byte limit"
                        ),
                    });
                }
                accumulator.arguments.push_str(arguments);
            }
            on_event(ProviderStreamEvent::ToolCallDelta {
                index,
                tool_call_id: (!accumulator.tool_call_id.is_empty())
                    .then(|| accumulator.tool_call_id.clone()),
                tool_name: (!accumulator.tool_name.is_empty())
                    .then(|| accumulator.tool_name.clone()),
            });
        }
    }
    if !stream_terminated {
        return Err(ProviderError::Unavailable {
            message: "chat completion SSE stream ended before a terminal event".to_owned(),
        });
    }

    let tool_calls = tool_calls
        .into_values()
        .map(|call| {
            provider_tool_call_from_openai(&json!({
                "id": call.tool_call_id,
                "function": {
                    "name": call.tool_name,
                    "arguments": call.arguments,
                }
            }))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let message = (!output_text.is_empty()).then_some(ProviderMessage {
        role: ProviderRole::Assistant,
        content: output_text,
        tool_call_id: None,
        tool_name: None,
        tool_calls: Vec::new(),
        metadata: ProviderMessageMetadata::default(),
    });
    let finish_reason = finish_reason.unwrap_or({
        if tool_calls.is_empty() {
            ProviderFinishReason::Unknown
        } else {
            ProviderFinishReason::ToolCalls
        }
    });
    Ok(ProviderResponse {
        response_id: ProviderResponseId::new(),
        message,
        tool_calls,
        usage: ProviderUsage {
            input_tokens: usage_value.get("prompt_tokens").and_then(Value::as_u64),
            output_tokens: usage_value.get("completion_tokens").and_then(Value::as_u64),
            reasoning_tokens: usage_value
                .get("completion_tokens_details")
                .and_then(|details| details.get("reasoning_tokens"))
                .and_then(Value::as_u64),
            cached_input_tokens: usage_value
                .get("prompt_tokens_details")
                .and_then(|details| details.get("cached_tokens"))
                .and_then(Value::as_u64),
            total_tokens: usage_value.get("total_tokens").and_then(Value::as_u64),
            usage_source: if usage_value
                .as_object()
                .is_some_and(|value| value.is_empty())
            {
                UsageSource::Unknown
            } else {
                UsageSource::Provider
            },
            raw: usage_value.clone(),
        },
        finish_reason,
        raw_metadata: json!({
            "provider": "openai-compatible",
            "response_id": response_id,
            "streamed": true,
            "usage": usage_value,
        }),
    })
}

fn provider_response_from_openai(
    value: Value,
    _task_id: TaskId,
    _turn_id: TurnId,
) -> Result<ProviderResponse, ProviderError> {
    let choice = value
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .cloned()
        .ok_or_else(|| ProviderError::Malformed {
            message: "response choices is empty".to_owned(),
        })?;
    let message = choice
        .get("message")
        .cloned()
        .ok_or_else(|| ProviderError::Malformed {
            message: "response choice has no message".to_owned(),
        })?;
    let content = message
        .get("content")
        .and_then(Value::as_str)
        .filter(|content| !content.is_empty())
        .map(|content| {
            if content.len() > MAX_PROVIDER_MESSAGE_BYTES {
                return Err(ProviderError::Malformed {
                    message: format!(
                        "assistant message exceeds {MAX_PROVIDER_MESSAGE_BYTES} byte limit"
                    ),
                });
            }
            Ok(ProviderMessage {
                role: ProviderRole::Assistant,
                content: content.to_owned(),
                tool_call_id: None,
                tool_name: None,
                tool_calls: Vec::new(),
                metadata: ProviderMessageMetadata::default(),
            })
        })
        .transpose()?;
    let tool_calls = message
        .get("tool_calls")
        .and_then(Value::as_array)
        .map(|calls| {
            calls
                .iter()
                .map(provider_tool_call_from_openai)
                .collect::<Result<Vec<_>, _>>()
        })
        .transpose()?
        .unwrap_or_default();
    let usage_value = value.get("usage").cloned().unwrap_or_else(|| json!({}));

    Ok(ProviderResponse {
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
    })
}

fn provider_tool_call_from_openai(value: &Value) -> Result<ProviderToolCall, ProviderError> {
    let function = value
        .get("function")
        .ok_or_else(|| ProviderError::Malformed {
            message: "tool call has no function".to_owned(),
        })?;
    let arguments = function
        .get("arguments")
        .and_then(Value::as_str)
        .ok_or_else(|| ProviderError::Malformed {
            message: "tool call arguments is not a JSON string".to_owned(),
        })
        .and_then(|arguments| {
            serde_json::from_str(arguments).map_err(|error| ProviderError::Malformed {
                message: format!("tool call arguments is invalid JSON: {error}"),
            })
        })?;
    let serialized_argument_size = serde_json::to_vec(&arguments)
        .map_err(|error| ProviderError::Malformed {
            message: format!("tool call arguments could not be serialized: {error}"),
        })?
        .len();
    if serialized_argument_size > MAX_PROVIDER_TOOL_ARGUMENT_BYTES {
        return Err(ProviderError::Malformed {
            message: format!(
                "tool call arguments exceed {MAX_PROVIDER_TOOL_ARGUMENT_BYTES} byte limit"
            ),
        });
    }
    let tool_call_id = value
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.trim().is_empty())
        .ok_or_else(|| ProviderError::Malformed {
            message: "tool call has no non-empty id".to_owned(),
        })?;
    if tool_call_id.len() > MAX_PROVIDER_TOOL_CALL_ID_BYTES {
        return Err(ProviderError::Malformed {
            message: format!("tool call id exceeds {MAX_PROVIDER_TOOL_CALL_ID_BYTES} byte limit"),
        });
    }
    let tool_name = function
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.trim().is_empty())
        .ok_or_else(|| ProviderError::Malformed {
            message: "tool call function has no non-empty name".to_owned(),
        })?;
    if tool_name.len() > MAX_PROVIDER_TOOL_NAME_BYTES {
        return Err(ProviderError::Malformed {
            message: format!("tool name exceeds {MAX_PROVIDER_TOOL_NAME_BYTES} byte limit"),
        });
    }
    Ok(ProviderToolCall {
        tool_call_id: tool_call_id.to_owned(),
        tool_name: tool_name.to_owned(),
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
    let message = value
        .get("error")
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .or_else(|| value.get("message").and_then(Value::as_str))
        .or_else(|| value.get("detail").and_then(Value::as_str))
        .or_else(|| value.get("error").and_then(Value::as_str))
        .unwrap_or("provider request failed");
    let message = sanitize_provider_error(message);
    let code = value
        .get("error")
        .and_then(|error| error.get("code"))
        .and_then(Value::as_str)
        .or_else(|| value.get("code").and_then(Value::as_str))
        .map(sanitize_provider_error)
        .filter(|code| !code.is_empty());

    match code {
        Some(code) if message != "provider request failed" => format!("{code}: {message}"),
        _ => message,
    }
}

fn provider_credential_error(error: golutra_auth::AuthError) -> ProviderError {
    ProviderError::NotConfigured {
        message: sanitize_provider_error(&error.to_string()),
    }
}

fn provider_transport_error(error: reqwest::Error) -> ProviderError {
    if error.is_timeout() {
        ProviderError::Timeout {
            message: sanitize_provider_error(&error.to_string()),
        }
    } else if error.is_connect() || error.is_request() || error.is_body() || error.is_decode() {
        ProviderError::Unavailable {
            message: sanitize_provider_error(&error.to_string()),
        }
    } else {
        ProviderError::Failed {
            message: sanitize_provider_error(&error.to_string()),
        }
    }
}

fn provider_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        // Streaming requests are bounded by ProviderSession's idle deadline;
        // a total HTTP deadline would incorrectly kill active long turns.
        .read_timeout(std::time::Duration::from_secs(300))
        .build()
        .expect("static reqwest client configuration is valid")
}

fn provider_http_error(status: reqwest::StatusCode, value: &Value) -> ProviderError {
    let message = provider_error_message(value);
    if status.is_server_error() {
        ProviderError::Unavailable { message }
    } else {
        ProviderError::Failed { message }
    }
}

async fn response_json_or_error(response: reqwest::Response) -> Result<Value, ProviderError> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_PROVIDER_RESPONSE_BYTES as u64)
    {
        return Err(ProviderError::Malformed {
            message: format!("provider response exceeds {MAX_PROVIDER_RESPONSE_BYTES} byte limit"),
        });
    }
    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(provider_transport_error)?;
        if bytes.len().saturating_add(chunk.len()) > MAX_PROVIDER_RESPONSE_BYTES {
            return Err(ProviderError::Malformed {
                message: format!(
                    "provider response exceeds {MAX_PROVIDER_RESPONSE_BYTES} byte limit"
                ),
            });
        }
        bytes.extend_from_slice(&chunk);
    }
    let text = String::from_utf8_lossy(&bytes);
    Ok(serde_json::from_str(&text).unwrap_or_else(|_| {
        json!({
            "error": {
                "message": sanitize_provider_error(&text)
            }
        })
    }))
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
mod tests;
