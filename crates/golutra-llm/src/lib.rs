use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    fmt,
    sync::{
        Arc, Mutex, OnceLock, RwLock,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use eventsource_stream::Eventsource;
use futures_util::StreamExt;
use golutra_auth::{CredentialProvider, FixedCredentialProvider};
use golutra_core::{
    CacheIdentity, NormalizedUsage, PromptCachePolicy, ProviderContract, ProviderRequestId,
    ProviderResponseId, SessionId, TaskId, ThreadId, ToolContract, TurnId,
};
pub use golutra_core::{ProviderUsage, UsageSource};
use reqwest::header::HeaderMap;
use secrecy::ExposeSecret;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use thiserror::Error;

mod genai_adapter;
mod openai_responses;
mod provider_config;

pub use genai_adapter::{GenaiProviderAdapter, GenaiProviderConfig};
pub use openai_responses::{OpenAiResponsesProvider, OpenAiResponsesProviderConfig};
pub(crate) use provider_config::{
    apply_generation_config_to_openai_body, cache_capabilities_from_reader,
    configured_or_first_env, custom_headers_from_reader, env_mapping, first_env,
    generation_config_from_reader, is_false, missing_env_error, normalize_protocol_value,
    protocol_spec, redacted_native_from_reader, redacted_openai_from_reader,
    redacted_openai_responses_from_reader, sanitize_provider_error, selected_protocol_from_reader,
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
/// 非敏感的 provider route identity；只用于声明能力选择，不包含凭据或用户内容。
pub const GOLUTRA_PROVIDER_ROUTE_ID: &str = "GOLUTRA_PROVIDER_ROUTE_ID";
/// 非敏感的缓存能力声明；由配置层传入，适配器只执行声明而不猜测网关能力。
pub const GOLUTRA_PROVIDER_CACHE_CAPABILITIES: &str = "GOLUTRA_PROVIDER_CACHE_CAPABILITIES";
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
const SESSION_AFFINITY_HEADER: &str = "session-id";
const SESSION_ID_HEADER: &str = "session_id";
const CLIENT_REQUEST_ID_HEADER: &str = "x-client-request-id";
const SESSION_AFFINITY_ALIAS_HEADER: &str = "x-session-affinity";
const RESERVED_AFFINITY_HEADERS: &[&str] = &[
    SESSION_AFFINITY_HEADER,
    SESSION_ID_HEADER,
    CLIENT_REQUEST_ID_HEADER,
    SESSION_AFFINITY_ALIAS_HEADER,
];

/// Provider 支持的会话亲和 header。只允许这些已知 wire 名称，避免自定义
/// header 绕过能力门控或把凭据/用户内容误带到上游。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProviderAffinityHeader {
    #[serde(rename = "session_id")]
    SessionId,
    #[serde(rename = "session-id")]
    SessionDashId,
    #[serde(rename = "x-client-request-id")]
    ClientRequestId,
    #[serde(rename = "x-session-affinity")]
    SessionAffinity,
}

impl ProviderAffinityHeader {
    #[must_use]
    pub const fn wire_name(self) -> &'static str {
        match self {
            Self::SessionId => SESSION_ID_HEADER,
            Self::SessionDashId => SESSION_AFFINITY_HEADER,
            Self::ClientRequestId => CLIENT_REQUEST_ID_HEADER,
            Self::SessionAffinity => SESSION_AFFINITY_ALIAS_HEADER,
        }
    }

    const fn bit(self) -> u8 {
        match self {
            Self::SessionId => 1 << 0,
            Self::SessionDashId => 1 << 1,
            Self::ClientRequestId => 1 << 2,
            Self::SessionAffinity => 1 << 3,
        }
    }
}

/// provider 的缓存能力矩阵。该结构是配置的一部分，而不是由 URL/hostname
/// 推断出来的运行时事实；同一协议可用不同声明安全地服务不同网关。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderCacheCapabilities {
    #[serde(default)]
    pub prompt_cache_key: bool,
    #[serde(default)]
    pub supports_long_retention: bool,
    #[serde(default)]
    pub supports_cache_control: bool,
    #[serde(default)]
    pub affinity_headers: Vec<ProviderAffinityHeader>,
}

impl ProviderCacheCapabilities {
    #[must_use]
    pub fn disabled() -> Self {
        Self::default()
    }

    /// 通用 Responses 配置，等价于 Pi 的默认 session cache 约定。
    #[must_use]
    pub fn responses() -> Self {
        Self {
            prompt_cache_key: true,
            supports_long_retention: true,
            supports_cache_control: true,
            affinity_headers: vec![
                ProviderAffinityHeader::SessionId,
                ProviderAffinityHeader::ClientRequestId,
            ],
        }
    }

    /// ChatGPT/Codex Responses 后端使用连字符形式的 session header，且不
    /// 假定通用 Responses 的 retention 扩展字段。
    #[must_use]
    pub fn codex_responses() -> Self {
        Self {
            prompt_cache_key: true,
            supports_long_retention: false,
            supports_cache_control: false,
            affinity_headers: vec![
                ProviderAffinityHeader::SessionDashId,
                ProviderAffinityHeader::ClientRequestId,
            ],
        }
    }

    #[must_use]
    pub fn anthropic() -> Self {
        Self {
            prompt_cache_key: false,
            supports_long_retention: true,
            supports_cache_control: true,
            affinity_headers: Vec::new(),
        }
    }

    #[must_use]
    pub fn anthropic_with_affinity() -> Self {
        Self {
            affinity_headers: vec![ProviderAffinityHeader::SessionAffinity],
            ..Self::anthropic()
        }
    }

    /// OpenAI-compatible 网关的显式 preset。只有配置入口选择该 preset 时
    /// 才会启用缓存；未知兼容网关继续保持 disabled。
    #[must_use]
    pub fn compatible() -> Self {
        Self {
            prompt_cache_key: true,
            supports_long_retention: true,
            supports_cache_control: true,
            affinity_headers: vec![
                ProviderAffinityHeader::SessionId,
                ProviderAffinityHeader::ClientRequestId,
                ProviderAffinityHeader::SessionAffinity,
            ],
        }
    }

    #[must_use]
    pub fn for_protocol(protocol: ProviderProtocol) -> Self {
        match protocol {
            ProviderProtocol::OpenAiResponses => Self::responses(),
            ProviderProtocol::Anthropic => Self::anthropic(),
            // 兼容端点的缓存语义并不由协议本身保证，默认关闭，等待显式
            // profile/preset 声明。
            ProviderProtocol::OpenAiCompatible
            | ProviderProtocol::Gemini
            | ProviderProtocol::VertexAi
            | ProviderProtocol::Genai
            | ProviderProtocol::Mock => Self::disabled(),
        }
    }

    /// 配置入口的一次性 preset 选择。适配器热路径不会再调用此方法。
    #[must_use]
    pub fn for_provider(protocol: ProviderProtocol, provider_id: &str) -> Self {
        let provider_id = canonical_provider_identity(provider_id);
        match protocol {
            ProviderProtocol::Anthropic
                if matches!(
                    provider_id.as_str(),
                    "fireworks" | "fireworks-ai" | "cloudflare" | "cloudflare-workers-ai" | "groq"
                ) =>
            {
                Self::anthropic_with_affinity()
            }
            ProviderProtocol::Anthropic => Self::anthropic(),
            ProviderProtocol::OpenAiResponses
                if matches!(
                    provider_id.as_str(),
                    "openai-chatgpt" | "chatgpt" | "chatgpt-codex" | "codex"
                ) =>
            {
                Self::codex_responses()
            }
            ProviderProtocol::OpenAiResponses => Self::responses(),
            ProviderProtocol::OpenAiCompatible
                if matches!(provider_id.as_str(), "openai" | "golutra" | "golutra-agent") =>
            {
                Self::compatible()
            }
            _ => Self::disabled(),
        }
    }

    pub fn validate_for_protocol(&self, protocol: ProviderProtocol) -> Result<(), String> {
        let mut seen = BTreeMap::new();
        for header in &self.affinity_headers {
            if seen.insert(header.wire_name(), ()).is_some() {
                return Err(format!(
                    "provider cache affinity header `{}` is configured more than once",
                    header.wire_name()
                ));
            }
        }
        if self.prompt_cache_key
            && !matches!(
                protocol,
                ProviderProtocol::OpenAiResponses | ProviderProtocol::OpenAiCompatible
            )
        {
            return Err(format!(
                "prompt_cache_key is not supported by protocol `{}`",
                protocol.id()
            ));
        }
        let cache_protocol = matches!(
            protocol,
            ProviderProtocol::OpenAiResponses
                | ProviderProtocol::OpenAiCompatible
                | ProviderProtocol::Anthropic
        );
        if !cache_protocol
            && (self.supports_long_retention
                || self.supports_cache_control
                || !self.affinity_headers.is_empty())
        {
            return Err(format!(
                "cache capabilities are not supported by protocol `{}`",
                protocol.id()
            ));
        }
        if self.supports_long_retention && !self.prompt_cache_key && !self.supports_cache_control {
            return Err(
                "supports_long_retention requires prompt_cache_key or cache_control".to_owned(),
            );
        }
        if !self.prompt_cache_key
            && !self.supports_cache_control
            && !self.affinity_headers.is_empty()
        {
            return Err(
                "affinity headers require an enabled cache key or cache control capability"
                    .to_owned(),
            );
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProviderCacheMode {
    Disabled,
    Responses,
    Anthropic,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ProviderCacheProfile {
    pub(crate) mode: ProviderCacheMode,
    affinity_mask: u8,
    supports_prompt_cache_key: bool,
    supports_cache_control: bool,
    supports_long_retention: bool,
}

impl ProviderCacheProfile {
    fn from_capabilities(
        protocol: ProviderProtocol,
        capabilities: &ProviderCacheCapabilities,
    ) -> Self {
        let mode = match protocol {
            ProviderProtocol::OpenAiResponses | ProviderProtocol::OpenAiCompatible => {
                ProviderCacheMode::Responses
            }
            ProviderProtocol::Anthropic => ProviderCacheMode::Anthropic,
            _ => ProviderCacheMode::Disabled,
        };
        let valid = capabilities.validate_for_protocol(protocol).is_ok();
        let mut affinity_mask = 0;
        if valid {
            for header in &capabilities.affinity_headers {
                affinity_mask |= header.bit();
            }
        }
        Self {
            mode: if valid {
                mode
            } else {
                ProviderCacheMode::Disabled
            },
            affinity_mask,
            supports_prompt_cache_key: valid && capabilities.prompt_cache_key,
            supports_cache_control: valid && capabilities.supports_cache_control,
            supports_long_retention: valid && capabilities.supports_long_retention,
        }
    }

    #[cfg(test)]
    fn for_provider(protocol: ProviderProtocol, provider_id: &str) -> Self {
        Self::from_capabilities(
            protocol,
            &ProviderCacheCapabilities::for_provider(protocol, provider_id),
        )
    }

    fn prompt_cache_key(self, policy: PromptCachePolicy) -> bool {
        policy != PromptCachePolicy::None
            && self.mode == ProviderCacheMode::Responses
            && self.supports_prompt_cache_key
    }

    fn affinity_headers(self, policy: PromptCachePolicy) -> Vec<&'static str> {
        if policy == PromptCachePolicy::None {
            Vec::new()
        } else {
            let mut headers = Vec::with_capacity(4);
            for header in [
                ProviderAffinityHeader::SessionId,
                ProviderAffinityHeader::SessionDashId,
                ProviderAffinityHeader::ClientRequestId,
                ProviderAffinityHeader::SessionAffinity,
            ] {
                if self.affinity_mask & header.bit() != 0 {
                    headers.push(header.wire_name());
                }
            }
            headers
        }
    }

    fn supports_long_retention(self, policy: PromptCachePolicy) -> bool {
        policy == PromptCachePolicy::Long && self.supports_long_retention
    }

    fn supports_cache_control(self, policy: PromptCachePolicy) -> bool {
        policy != PromptCachePolicy::None && self.supports_cache_control
    }

    /// 主会话遵循 provider 的默认 retention。长期保留仍由调用方通过
    /// `PromptCachePolicy::Long` 显式请求，并在 `supports_long_retention`
    /// 中做能力门控，避免把长期策略隐式写入每一轮请求。
    fn preferred_cache_policy(self) -> PromptCachePolicy {
        PromptCachePolicy::Auto
    }
}

fn canonical_provider_identity(provider_id: &str) -> String {
    provider_id
        .trim()
        .to_ascii_lowercase()
        .replace(['_', ' '], "-")
}

/// 脱敏后的 provider 诊断元数据，只用于重试决策和可行动的观测。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProviderErrorMetadata {
    pub http_status: Option<u16>,
    pub provider_code: Option<String>,
    pub retry_after: Option<Duration>,
    pub request_id: Option<String>,
}

impl ProviderErrorMetadata {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.http_status.is_none()
            && self.provider_code.is_none()
            && self.retry_after.is_none()
            && self.request_id.is_none()
    }
}

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
    /// 保留原有语义错误，同时携带脱敏 HTTP/provider 诊断信息。
    #[error("{error}")]
    WithMetadata {
        error: Box<ProviderError>,
        metadata: ProviderErrorMetadata,
    },
}

impl ProviderError {
    #[must_use]
    pub fn with_metadata(self, metadata: ProviderErrorMetadata) -> Self {
        if metadata.is_empty() {
            self
        } else {
            Self::WithMetadata {
                error: Box::new(self),
                metadata,
            }
        }
    }

    #[must_use]
    pub fn metadata(&self) -> Option<&ProviderErrorMetadata> {
        match self {
            Self::WithMetadata { metadata, .. } => Some(metadata),
            _ => None,
        }
    }

    #[must_use]
    pub fn retry_after(&self) -> Option<Duration> {
        self.metadata().and_then(|metadata| metadata.retry_after)
    }

    #[must_use]
    pub fn http_status(&self) -> Option<u16> {
        self.metadata().and_then(|metadata| metadata.http_status)
    }

    #[must_use]
    pub fn is_rate_limited(&self) -> bool {
        match self {
            Self::RateLimited { .. } => true,
            Self::WithMetadata { error, .. } => error.is_rate_limited(),
            _ => false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptCacheScopeKind {
    Session,
    Fork,
    Subagent,
    Compaction,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptCacheScope {
    session_id: SessionId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    thread_id: Option<ThreadId>,
    kind: PromptCacheScopeKind,
    key: String,
}

impl PromptCacheScope {
    #[must_use]
    pub fn session(session_id: SessionId, thread_id: Option<ThreadId>) -> Self {
        Self {
            session_id,
            thread_id,
            kind: PromptCacheScopeKind::Session,
            key: session_id.to_string(),
        }
    }

    #[must_use]
    pub fn fork(
        session_id: SessionId,
        thread_id: ThreadId,
        parent_cache_session_id: SessionId,
    ) -> Self {
        Self::parent_scoped(
            session_id,
            thread_id,
            parent_cache_session_id,
            PromptCacheScopeKind::Fork,
        )
    }

    #[must_use]
    pub fn subagent(
        session_id: SessionId,
        thread_id: ThreadId,
        parent_cache_session_id: SessionId,
    ) -> Self {
        Self::parent_scoped(
            session_id,
            thread_id,
            parent_cache_session_id,
            PromptCacheScopeKind::Subagent,
        )
    }

    #[must_use]
    pub fn compaction(&self) -> Self {
        Self {
            session_id: self.session_id,
            thread_id: self.thread_id,
            kind: PromptCacheScopeKind::Compaction,
            key: self.key.clone(),
        }
    }

    fn parent_scoped(
        session_id: SessionId,
        thread_id: ThreadId,
        parent_cache_session_id: SessionId,
        kind: PromptCacheScopeKind,
    ) -> Self {
        Self {
            session_id,
            thread_id: Some(thread_id),
            kind,
            key: parent_cache_session_id.to_string(),
        }
    }

    #[must_use]
    pub fn session_id(&self) -> SessionId {
        self.session_id
    }

    #[must_use]
    pub fn thread_id(&self) -> Option<ThreadId> {
        self.thread_id
    }

    #[must_use]
    pub fn kind(&self) -> PromptCacheScopeKind {
        self.kind
    }

    #[must_use]
    pub fn key(&self) -> &str {
        &self.key
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderRequest {
    pub request_id: ProviderRequestId,
    pub task_id: TaskId,
    pub turn_id: TurnId,
    #[serde(default)]
    pub session_id: Option<SessionId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_scope: Option<PromptCacheScope>,
    pub provider_id: String,
    pub model_id: String,
    pub messages: Vec<ProviderMessage>,
    pub tools: Vec<ToolContract>,
    #[serde(default)]
    pub cache_policy: PromptCachePolicy,
    #[serde(default)]
    pub max_output_tokens: Option<u64>,
}

impl ProviderRequest {
    fn affinity_id(&self) -> String {
        self.cache_scope.as_ref().map_or_else(
            || {
                self.session_id.map_or_else(
                    || self.task_id.to_string(),
                    |session_id| session_id.to_string(),
                )
            },
            |scope| scope.key().to_owned(),
        )
    }

    #[must_use]
    pub fn cache_identity(&self) -> Option<CacheIdentity> {
        self.cache_identity_with_namespace("default")
    }

    /// 为稳定的 wire 作用域构造 provider 本地观测身份。
    /// provider、模型和路由只保留为本地元数据；上游 key 仅使用可读的 session
    /// 或可信父线程作用域。
    #[must_use]
    pub fn cache_identity_with_namespace(&self, namespace: &str) -> Option<CacheIdentity> {
        let session_id = self.session_id?;
        if self.cache_policy == PromptCachePolicy::None {
            return None;
        }
        let canonical_provider = self.provider_id.trim().to_ascii_lowercase();
        let canonical_model = self.model_id.trim().to_ascii_lowercase();
        let (thread_id, key) = self.cache_scope.as_ref().map_or_else(
            || (None, session_id.to_string()),
            |scope| (scope.thread_id(), scope.key().to_owned()),
        );
        Some(CacheIdentity {
            session_id,
            thread_id,
            provider_id: canonical_provider,
            model_id: canonical_model,
            route_namespace: namespace.trim().to_owned(),
            key,
        })
    }

    #[must_use]
    pub fn normalized_usage(&self, usage: &ProviderUsage) -> NormalizedUsage {
        usage.normalize()
    }
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

    /// Stable route identity used to isolate prompt caches across protocols and
    /// endpoints that happen to expose the same provider/model names.
    fn cache_namespace(&self) -> String {
        let contract = self.contract();
        format!("{}\0{}", contract.native_protocol, contract.provider_id)
    }

    #[must_use]
    fn cache_identity_for_request(&self, request: &ProviderRequest) -> Option<CacheIdentity> {
        request.cache_identity_with_namespace(&self.cache_namespace())
    }

    /// 返回主会话请求使用的缓存保留策略。辅助请求（例如 compaction）由
    /// 调用方显式指定策略，不会被该偏好覆盖。
    fn preferred_cache_policy(&self) -> PromptCachePolicy {
        PromptCachePolicy::Auto
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
    state: Arc<Mutex<MockProviderState>>,
}

#[derive(Debug, Clone)]
enum MockProviderOutcome {
    Response(Box<ProviderResponse>),
    Error(ProviderError),
}

#[derive(Debug, Default)]
struct MockProviderState {
    task_id: Option<TaskId>,
    tool_call_emitted: bool,
    request_id: Option<ProviderRequestId>,
    request_phase: Option<MockResponsePhase>,
}

#[derive(Debug, Clone, Copy)]
enum MockResponsePhase {
    ToolCall,
    Completion,
}

impl MockProvider {
    /// 只保留当前任务和最近请求，避免长时间复用模拟 provider 时状态无界增长。
    fn response_phase(&self, request: &ProviderRequest) -> MockResponsePhase {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.task_id != Some(request.task_id) {
            state.task_id = Some(request.task_id);
            state.tool_call_emitted = false;
            state.request_id = None;
            state.request_phase = None;
        }
        if state.request_id == Some(request.request_id)
            && let Some(phase) = state.request_phase
        {
            return phase;
        }
        let phase = if request
            .messages
            .last()
            .is_some_and(|message| message.role == ProviderRole::Tool)
            || state.tool_call_emitted
        {
            MockResponsePhase::Completion
        } else {
            state.tool_call_emitted = true;
            MockResponsePhase::ToolCall
        };
        state.request_id = Some(request.request_id);
        state.request_phase = Some(phase);
        phase
    }

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
            state: Arc::new(Mutex::new(MockProviderState::default())),
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
            state: Arc::new(Mutex::new(MockProviderState::default())),
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
            state: Arc::new(Mutex::new(MockProviderState::default())),
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
        if !response.tool_calls.is_empty() {
            let phase = self.response_phase(&request);
            if !matches!(phase, MockResponsePhase::Completion) {
                return Ok(response.clone());
            }
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
    cache_profile: ProviderCacheProfile,
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
    /// 已验证的 provider 缓存能力；缺省时使用协议级保守默认值。
    pub cache_capabilities: Option<ProviderCacheCapabilities>,
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
        affinity_headers: &HeaderMap,
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
        let mut custom_headers = self.custom_headers.to_header_map();
        // affinity 是 provider 能力，不允许自定义 header 绕过 profile gate。
        for header in RESERVED_AFFINITY_HEADERS {
            custom_headers.remove(*header);
        }
        builder
            .headers(custom_headers)
            .headers(affinity_headers.clone())
    }

    fn affinity_headers(&self, request: &ProviderRequest) -> HeaderMap {
        let mut headers = HeaderMap::new();
        let affinity_id = request.affinity_id();
        if let Ok(value) = reqwest::header::HeaderValue::from_str(&affinity_id) {
            for header in self.cache_profile.affinity_headers(request.cache_policy) {
                if let Ok(name) = reqwest::header::HeaderName::from_bytes(header.as_bytes()) {
                    headers.insert(name, value.clone());
                }
            }
        }
        headers
    }

    #[must_use]
    pub fn new(
        api_key: impl Into<String>,
        base_url: impl Into<String>,
        model_id: impl Into<String>,
    ) -> Self {
        let api_key = api_key.into();
        let base_url = normalize_openai_base_url(&base_url.into());
        Self {
            credential: Arc::new(FixedCredentialProvider::new(
                api_key,
                GOLUTRA_PROVIDER_API_KEY,
            )),
            api_key_env: GOLUTRA_PROVIDER_API_KEY.to_owned(),
            provider_id: "openai-compatible".to_owned(),
            cache_profile: ProviderCacheProfile::from_capabilities(
                ProviderProtocol::OpenAiCompatible,
                &ProviderCacheCapabilities::disabled(),
            ),
            base_url,
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
        let base_url = normalize_openai_base_url(&config.base_url);
        let default_capabilities = ProviderCacheCapabilities::for_protocol(config.protocol);
        let capabilities = config
            .cache_capabilities
            .as_ref()
            .unwrap_or(&default_capabilities);
        let cache_profile = ProviderCacheProfile::from_capabilities(config.protocol, capabilities);
        Self {
            credential,
            api_key_env: config.api_key_env,
            provider_id: config.provider_id,
            cache_profile,
            base_url,
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
        let provider_id = reader(GOLUTRA_PROVIDER_AUTH_PROVIDER)
            .filter(|value| !value.trim().is_empty())
            .or_else(|| reader(GOLUTRA_PROVIDER_ROUTE_ID).filter(|value| !value.trim().is_empty()))
            .unwrap_or_else(|| "openai-compatible".to_owned());
        let cache_capabilities = Some(cache_capabilities_from_reader(
            &reader,
            protocol,
            &provider_id,
        )?);
        Ok(OpenAiCompatibleProviderConfig {
            api_key,
            api_key_env,
            provider_id,
            base_url,
            model_id,
            protocol,
            generation_config,
            custom_headers,
            cache_capabilities,
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
            .authenticated_request(
                self.client.get(url),
                token.expose_secret(),
                "user",
                &HeaderMap::new(),
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
            self.client.get(url),
            token.expose_secret(),
            "user",
            &HeaderMap::new(),
        )
        .send()
        .await
        .map_err(provider_transport_error)
    }

    async fn post_with_auth_retry(
        &self,
        url: &str,
        body: &Value,
        affinity_headers: &HeaderMap,
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
                affinity_headers,
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
            affinity_headers,
        )
        .send()
        .await
        .map_err(provider_transport_error)
    }

    pub async fn probe(&self) -> Result<ProviderProbeResult, ProviderError> {
        let url = format!("{}/models", self.base_url.trim_end_matches('/'));
        let response = self.get_with_auth_retry(&url).await?;
        let status = response.status();
        let headers = response.headers().clone();
        let value = response_json_or_error(response).await?;
        if status.as_u16() == 429 {
            return Err(provider_error_from_value(
                &value,
                Some(status.as_u16()),
                &headers,
            ));
        }
        if !status.is_success() {
            return Err(provider_http_error_with_headers(status, &headers, &value));
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
        let cache_identity = self.cache_identity_for_request(&request);
        let body = openai_completion_body_with_identity(
            &request,
            &self.model_id,
            &self.generation_config,
            false,
            self.cache_profile,
            cache_identity.as_ref(),
        );
        let affinity_headers = self.affinity_headers(&request);
        let response = self
            .post_with_auth_retry(&url, &body, &affinity_headers)
            .await?;
        let status = response.status();
        let headers = response.headers().clone();
        let value = response_json_or_error(response).await?;
        if status.as_u16() == 429 {
            return Err(provider_error_from_value(
                &value,
                Some(status.as_u16()),
                &headers,
            ));
        }
        if !status.is_success() {
            return Err(provider_http_error_with_headers(status, &headers, &value));
        }

        provider_response_from_openai(value, request.task_id, request.turn_id)
    }

    async fn complete_stream(
        &self,
        request: ProviderRequest,
        on_event: &mut (dyn FnMut(ProviderStreamEvent) + Send),
    ) -> Result<ProviderResponse, ProviderError> {
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        let cache_identity = self.cache_identity_for_request(&request);
        let body = openai_completion_body_with_identity(
            &request,
            &self.model_id,
            &self.generation_config,
            true,
            self.cache_profile,
            cache_identity.as_ref(),
        );
        let affinity_headers = self.affinity_headers(&request);
        let response = self
            .post_with_auth_retry(&url, &body, &affinity_headers)
            .await?;
        let status = response.status();
        if status.as_u16() == 429 {
            let headers = response.headers().clone();
            let value = response_json_or_error(response).await?;
            return Err(provider_error_from_value(
                &value,
                Some(status.as_u16()),
                &headers,
            ));
        }
        if !status.is_success() {
            let headers = response.headers().clone();
            let value = response_json_or_error(response).await?;
            return Err(provider_http_error_with_headers(status, &headers, &value));
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

    fn cache_namespace(&self) -> String {
        route_cache_namespace("openai_chat_completions", &self.base_url)
    }

    fn preferred_cache_policy(&self) -> PromptCachePolicy {
        self.cache_profile.preferred_cache_policy()
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

    fn cache_namespace(&self) -> String {
        match self {
            Self::Mock(provider) => provider.cache_namespace(),
            Self::OpenAiCompatible(provider) => provider.cache_namespace(),
            Self::OpenAiResponses(provider) => provider.cache_namespace(),
            Self::Anthropic(provider)
            | Self::Gemini(provider)
            | Self::VertexAi(provider)
            | Self::Genai(provider) => provider.cache_namespace(),
        }
    }

    fn preferred_cache_policy(&self) -> PromptCachePolicy {
        match self {
            Self::Mock(provider) => provider.preferred_cache_policy(),
            Self::OpenAiCompatible(provider) => provider.preferred_cache_policy(),
            Self::OpenAiResponses(provider) => provider.preferred_cache_policy(),
            Self::Anthropic(provider)
            | Self::Gemini(provider)
            | Self::VertexAi(provider)
            | Self::Genai(provider) => provider.preferred_cache_policy(),
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

pub(crate) fn route_cache_namespace(protocol: &str, base_url: &str) -> String {
    // URL 本身只作为哈希输入，不进入日志或 provider payload；去掉末尾斜杠
    // 后同一路由的配置变体不会平白拆分缓存。
    format!("{protocol}\0{}", base_url.trim().trim_end_matches('/'))
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
        value["name"] = Value::String(provider_tool_wire_name(tool_name));
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
            "name": provider_tool_wire_name(&tool_call.tool_name),
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

pub fn provider_tool_description(tool_name: &str) -> &'static str {
    match tool_name {
        "read_file" => "Read workspace lines; use offset/limit and continuation.next_offset.",
        "write_file" => {
            "Create a new UTF-8 file or completely rewrite one; returns status and digest."
        }
        "edit_file" => {
            "Edit one UTF-8 file with exact, non-overlapping edits[]; batch disjoint edits; returns status, digest, preview."
        }
        "apply_patch" => {
            "Atomically apply one unified or Begin/Update/Add/Delete patch; batch related multi-file changes; returns status, digest, preview."
        }
        "web_search" => "Search the network when enabled; return source-backed results.",
        "shell_session" => {
            "Control a background shell; authoritative_pid must match. Reuse cursor; one bounded wait_for_terminal=true, or write/terminate."
        }
        "subagent" => {
            "Run one isolated child task; it cannot create another child; return a bounded result."
        }
        "list_dir" => "List entries in a workspace-relative directory.",
        "rg_search" => "Search workspace files with ripgrep.",
        "symbol_search" => "Search the workspace code graph for matching symbol definitions.",
        "find_references" => "Find workspace code-graph references to a named symbol.",
        "ask_user" => {
            "Ask up to three concise structured questions only when a consequential decision cannot be resolved safely from the request or workspace. Provide two to eight clear options per question and use multiple mode only when multiple selections are valid."
        }
        "delegate_task" => {
            "Delegate one complete, self-contained task to an isolated child agent and wait for its result. The child does not receive this conversation. Omit model and reasoning_effort to inherit the current agent settings; specify either field only when the task benefits from an explicit override."
        }
        "shell" => {
            "Run via argv or command; use bash -lc for pipes, redirects, heredoc, or compound commands. background=true returns after initial return; shell_session waits. timeout_ms is a hard lifetime; omit timeout_ms normally."
        }
        "process_list" => {
            "List managed background processes owned by the current session, including redacted commands, states, exit codes, and output statistics. This does not consume process output or advance a cursor."
        }
        "process_poll" => {
            "Read new output and status from a managed background process. Reuse the returned cursor to avoid replaying output; wait_ms optionally performs a bounded wait."
        }
        "process_write" => {
            "Send input verbatim to a managed background process and read output after the supplied cursor. Include a newline when the process expects Enter."
        }
        "process_terminate" => {
            "Terminate a managed background process and return its final status."
        }
        "process_reconnect" => {
            "Recover current output and status for a managed background process after an interrupted tool interaction, starting at the supplied cursor."
        }
        _ => "Golutra workspace tool.",
    }
}

/// Names used in provider tool payloads. `web_search` is an internal runtime
/// capability name, while several OpenAI-compatible adapters reserve that
/// literal for a native tool. Keeping the alias here makes projection,
/// accounting, and every transport share one wire representation.
pub(crate) const WEB_SEARCH_WIRE_ALIAS: &str = "golutra_web_search";

pub(crate) fn provider_tool_wire_name(name: &str) -> String {
    if name == "web_search" {
        WEB_SEARCH_WIRE_ALIAS.to_owned()
    } else {
        name.to_owned()
    }
}

pub(crate) fn restore_provider_tool_wire_name(name: &str) -> String {
    if name == WEB_SEARCH_WIRE_ALIAS {
        "web_search".to_owned()
    } else {
        name.to_owned()
    }
}

// 发送给模型的 schema 只保留表达调用结构和必要语义的字段；长度、范围和格式等边界
// 仍由 runtime 使用原始 ToolContract 校验。保留紧凑的参数描述，避免压缩输入时损伤
// 模型对 shell、子代理等高风险参数的使用判断。
const PROVIDER_SCHEMA_BOUNDARY_KEYS: &[&str] = &[
    "title",
    "$comment",
    "default",
    "examples",
    "deprecated",
    "readOnly",
    "writeOnly",
    "minLength",
    "maxLength",
    "pattern",
    "format",
    "minimum",
    "maximum",
    "exclusiveMinimum",
    "exclusiveMaximum",
    "multipleOf",
    "minItems",
    "maxItems",
    "uniqueItems",
    "minProperties",
    "maxProperties",
    "contentEncoding",
    "contentMediaType",
];

const MAX_PROVIDER_SCHEMA_DESCRIPTION_CHARS: usize = 512;

fn is_provider_schema_boundary_key(key: &str) -> bool {
    PROVIDER_SCHEMA_BOUNDARY_KEYS.contains(&key)
}

fn project_provider_schema_description(value: &Value) -> Value {
    let Some(description) = value.as_str() else {
        return value.clone();
    };
    let compact = description.split_whitespace().collect::<Vec<_>>().join(" ");
    Value::String(
        compact
            .chars()
            .take(MAX_PROVIDER_SCHEMA_DESCRIPTION_CHARS)
            .collect(),
    )
}

fn project_provider_schema_map(value: &Value) -> Value {
    let Some(properties) = value.as_object() else {
        return project_provider_schema_value(value);
    };

    Value::Object(
        properties
            .iter()
            .map(|(name, schema)| (name.clone(), project_provider_schema_value(schema)))
            .collect(),
    )
}

fn project_provider_schema_value(value: &Value) -> Value {
    match value {
        Value::Object(object) => Value::Object(
            object
                .iter()
                .filter_map(|(key, child)| {
                    if is_provider_schema_boundary_key(key) {
                        return None;
                    }

                    let projected = match key.as_str() {
                        "description" => project_provider_schema_description(child),
                        // 这些字段的 map key 是用户属性名，不能误删名为 `description` 的属性。
                        "properties" | "patternProperties" | "$defs" | "definitions"
                        | "dependentSchemas" => project_provider_schema_map(child),
                        // enum、const、required 携带的是数据，不是嵌套 schema 对象。
                        "enum" | "const" | "required" => child.clone(),
                        _ => project_provider_schema_value(child),
                    };
                    Some((key.clone(), projected))
                })
                .collect(),
        ),
        Value::Array(values) => {
            Value::Array(values.iter().map(project_provider_schema_value).collect())
        }
        _ => value.clone(),
    }
}

/// 将内部 JSON Schema 投影为紧凑的 provider-facing 形式。
///
/// 工具执行仍以内部 schema 为准；`additionalProperties: false` 等结构字段
/// 会保留，确保 strict Responses 工具继续满足 provider 的契约。
#[must_use]
pub fn provider_tool_schema_projection(schema: &Value) -> Value {
    canonicalize_json(&project_provider_schema_value(schema))
}

const PROVIDER_TOOL_PROJECTION_CACHE_CAPACITY: usize = 128;

static PROVIDER_TOOL_PROJECTION_CACHE_HITS: AtomicU64 = AtomicU64::new(0);
static PROVIDER_TOOL_PROJECTION_CACHE_MISSES: AtomicU64 = AtomicU64::new(0);

#[derive(Debug)]
struct CachedProviderToolProjection {
    schema: Value,
    wire: Value,
    digest: String,
    token_count: u64,
}

#[derive(Debug, Default)]
struct ProviderToolProjectionCache {
    entries: HashMap<ProviderToolProjectionCacheKey, Arc<CachedProviderToolProjection>>,
    order: VecDeque<ProviderToolProjectionCacheKey>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct ProviderToolProjectionCacheKey([u8; 32]);

static PROVIDER_TOOL_PROJECTION_CACHE: OnceLock<RwLock<ProviderToolProjectionCache>> =
    OnceLock::new();

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderToolProjectionCacheStats {
    pub hits: u64,
    pub misses: u64,
    pub entries: usize,
}

fn provider_tool_projection_cache() -> &'static RwLock<ProviderToolProjectionCache> {
    PROVIDER_TOOL_PROJECTION_CACHE
        .get_or_init(|| RwLock::new(ProviderToolProjectionCache::default()))
}

fn provider_tool_projection_cache_key(
    contract: &ToolContract,
    canonical_schema_digest: &[u8; 32],
) -> ProviderToolProjectionCacheKey {
    // Registry contracts are immutable during a turn. Hashing the canonical
    // tree directly keeps equivalent map insertion orders together while
    // avoiding a full normalized JSON allocation on every cache hit.
    let wire_name = provider_tool_wire_name(&contract.tool_name);
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"provider-tool-projection-v4\0");
    bytes.extend_from_slice(wire_name.as_bytes());
    bytes.push(0);
    bytes.extend_from_slice(provider_tool_description(&contract.tool_name).as_bytes());
    bytes.push(0);
    bytes.extend_from_slice(canonical_schema_digest);
    let digest = Sha256::digest(bytes);
    let mut key = [0_u8; 32];
    key.copy_from_slice(&digest);
    ProviderToolProjectionCacheKey(key)
}

fn cached_provider_tool_projection(contract: &ToolContract) -> Arc<CachedProviderToolProjection> {
    // This digest is deliberately cheaper than canonicalization: cache hits
    // only walk the source tree and never allocate a second JSON value.
    let canonical_schema_digest = canonical_json_digest(&contract.input_schema);
    let key = provider_tool_projection_cache_key(contract, &canonical_schema_digest);
    let cache = provider_tool_projection_cache();
    {
        let mut cache = cache
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(entry) = cache.entries.get(&key).cloned() {
            // Keep the deque as a true access-order list. A plain insertion
            // order silently turns the bounded cache into FIFO under load.
            cache.order.retain(|candidate| candidate != &key);
            cache.order.push_back(key);
            PROVIDER_TOOL_PROJECTION_CACHE_HITS.fetch_add(1, Ordering::Relaxed);
            return entry;
        }
    }

    PROVIDER_TOOL_PROJECTION_CACHE_MISSES.fetch_add(1, Ordering::Relaxed);

    // JSON 对象顺序不影响 schema 语义，但会影响 provider 的字节前缀；
    // 统一排序后，等价的工具定义共享同一份投影和缓存身份。
    let canonical_schema = canonicalize_json(&contract.input_schema);
    let schema = project_provider_schema_value(&canonical_schema);
    let wire = json!({
        "type": "function",
        "function": {
            "name": provider_tool_wire_name(&contract.tool_name),
            "description": provider_tool_description(&contract.tool_name),
            "parameters": schema
        }
    });
    let serialized = serde_json::to_string(&wire).unwrap_or_default();
    let entry = Arc::new(CachedProviderToolProjection {
        schema: wire
            .get("function")
            .and_then(Value::as_object)
            .and_then(|function| function.get("parameters"))
            .cloned()
            .unwrap_or(Value::Null),
        digest: format!("sha256:{:x}", Sha256::digest(serialized.as_bytes())),
        token_count: serialized.chars().count().div_ceil(4) as u64,
        wire,
    });

    let mut cache = cache
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(existing) = cache.entries.get(&key).cloned() {
        cache.order.retain(|candidate| candidate != &key);
        cache.order.push_back(key);
        return existing;
    }
    cache.entries.insert(key, Arc::clone(&entry));
    cache.order.push_back(key);
    while cache.entries.len() > PROVIDER_TOOL_PROJECTION_CACHE_CAPACITY {
        let Some(oldest) = cache.order.pop_front() else {
            break;
        };
        // A stale deque entry should never evict a newer value with the same
        // key. This guard also repairs the order if a future caller changes
        // insertion behavior.
        if cache.entries.remove(&oldest).is_none() {
            continue;
        }
    }
    entry
}

/// Compute a canonical JSON digest without materializing a sorted clone. The
/// output is only a cache identity; provider wire values are still built from
/// `canonicalize_json` on a miss so their existing ordering contract remains.
fn canonical_json_digest(value: &Value) -> [u8; 32] {
    let mut digest = Sha256::new();
    canonical_json_digest_into(value, &mut digest);
    let finalized = digest.finalize();
    let mut result = [0_u8; 32];
    result.copy_from_slice(&finalized);
    result
}

fn canonical_json_digest_into(value: &Value, digest: &mut Sha256) {
    match value {
        Value::Null => digest.update(b"n"),
        Value::Bool(value) => {
            digest.update(b"b");
            digest.update([u8::from(*value)]);
        }
        Value::Number(value) => {
            digest.update(b"d");
            digest_field_bytes(digest, value.to_string().as_bytes());
        }
        Value::String(value) => {
            digest.update(b"s");
            digest_field_bytes(digest, value.as_bytes());
        }
        Value::Array(values) => {
            digest.update(b"a");
            digest.update((values.len() as u64).to_le_bytes());
            for value in values {
                canonical_json_digest_into(value, digest);
            }
        }
        Value::Object(object) => {
            digest.update(b"o");
            digest.update((object.len() as u64).to_le_bytes());
            let mut keys = object.keys().collect::<Vec<_>>();
            keys.sort_unstable();
            for key in keys {
                digest_field_bytes(digest, key.as_bytes());
                // The key was collected from this object, so the lookup is
                // infallible unless the map is concurrently mutated (which a
                // serde_json::Value reference does not permit).
                if let Some(child) = object.get(key) {
                    canonical_json_digest_into(child, digest);
                }
            }
        }
    }
}

fn digest_field_bytes(digest: &mut Sha256, field: &[u8]) {
    digest.update((field.len() as u64).to_le_bytes());
    digest.update(field);
}

/// Return inexpensive process-local cache counters for diagnostics and
/// benchmark reports.  The cache itself remains bounded and private.
#[must_use]
pub fn provider_tool_projection_cache_stats() -> ProviderToolProjectionCacheStats {
    let entries = provider_tool_projection_cache()
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .entries
        .len();
    ProviderToolProjectionCacheStats {
        hits: PROVIDER_TOOL_PROJECTION_CACHE_HITS.load(Ordering::Relaxed),
        misses: PROVIDER_TOOL_PROJECTION_CACHE_MISSES.load(Ordering::Relaxed),
        entries,
    }
}

fn canonicalize_json(value: &Value) -> Value {
    match value {
        Value::Object(object) => {
            let mut entries = object.iter().collect::<Vec<_>>();
            entries.sort_by(|left, right| left.0.cmp(right.0));
            Value::Object(
                entries
                    .into_iter()
                    .map(|(key, child)| (key.clone(), canonicalize_json(child)))
                    .collect(),
            )
        }
        Value::Array(values) => Value::Array(values.iter().map(canonicalize_json).collect()),
        _ => value.clone(),
    }
}

/// Return the cached provider schema for a complete tool contract.
#[must_use]
pub(crate) fn provider_tool_schema_for_contract(contract: &ToolContract) -> Value {
    cached_provider_tool_projection(contract).schema.clone()
}

/// Return the digest of the exact provider-facing tool wire value.
#[must_use]
pub fn provider_tool_wire_digest(contract: &ToolContract) -> String {
    cached_provider_tool_projection(contract).digest.clone()
}

/// Return the cached estimate for the exact provider-facing tool wire value.
#[must_use]
pub fn provider_tool_wire_tokens(contract: &ToolContract) -> u64 {
    cached_provider_tool_projection(contract).token_count
}

/// Return the digest and token estimate from one projection-cache lookup.
/// Runtime hot paths commonly need both values for the same contract; keeping
/// this pair together avoids serializing and hashing the source schema twice.
#[must_use]
pub fn provider_tool_wire_stats(contract: &ToolContract) -> (String, u64) {
    let cached = cached_provider_tool_projection(contract);
    (cached.digest.clone(), cached.token_count)
}

/// Return the smallest stable tool representation sent by the Chat Completions
/// transport. Runtime budgeting and context snapshots use the same shape so
/// internal recovery/policy fields are never charged as model input.
#[must_use]
pub fn provider_tool_wire_projection(contract: &ToolContract) -> Value {
    cached_provider_tool_projection(contract).wire.clone()
}

/// Estimate tool schema tokens from the provider-facing projection, not from
/// the full internal contract used by the executor and governance layers.
#[must_use]
pub fn estimate_provider_tool_tokens(tools: &[ToolContract]) -> u64 {
    tools.iter().map(provider_tool_wire_tokens).sum()
}

fn openai_tool_schema(contract: &ToolContract) -> Value {
    provider_tool_wire_projection(contract)
}

#[cfg(test)]
fn openai_completion_body(
    request: &ProviderRequest,
    model_id: &str,
    generation_config: &ProviderGenerationConfig,
    streaming: bool,
    prompt_cache_supported: bool,
) -> Value {
    let cache_identity = request.cache_identity();
    let cache_profile = ProviderCacheProfile::for_provider(
        ProviderProtocol::OpenAiCompatible,
        if prompt_cache_supported {
            "golutra"
        } else {
            "custom"
        },
    );
    openai_completion_body_with_identity(
        request,
        model_id,
        generation_config,
        streaming,
        cache_profile,
        cache_identity.as_ref(),
    )
}

fn openai_completion_body_with_identity(
    request: &ProviderRequest,
    model_id: &str,
    generation_config: &ProviderGenerationConfig,
    streaming: bool,
    cache_profile: ProviderCacheProfile,
    cache_identity: Option<&CacheIdentity>,
) -> Value {
    let mut body = json!({
        "model": model_id,
        "messages": request.messages.iter().map(openai_message).collect::<Vec<_>>(),
    });
    if !request.tools.is_empty() {
        body["tools"] = Value::Array(request.tools.iter().map(openai_tool_schema).collect());
        body["tool_choice"] = Value::String("auto".to_owned());
        // 明确要求 provider 在一次响应中并行发出彼此独立的工具调用，
        // 避免兼容端点因默认值差异把多文件任务退化为串行回合。
        body["parallel_tool_calls"] = Value::Bool(true);
    }
    if streaming {
        body["stream"] = Value::Bool(true);
        body["stream_options"] = json!({"include_usage": true});
    }
    if cache_profile.prompt_cache_key(request.cache_policy)
        && let Some(identity) = cache_identity
    {
        body["prompt_cache_key"] = Value::String(identity.key.clone());
        if cache_profile.supports_long_retention(request.cache_policy) {
            body["prompt_cache_retention"] = Value::String("24h".to_owned());
        }
    }
    apply_generation_config_to_openai_body(&mut body, generation_config);
    if let Some(max_output_tokens) = request.max_output_tokens {
        body["max_tokens"] = Value::Number(max_output_tokens.into());
    }
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
    let response_status = response.status().as_u16();
    let response_headers = response.headers().clone();
    let mut stream = response.bytes_stream().eventsource();
    let mut parsed_bytes = 0_usize;
    let mut output_text = String::new();
    let mut tool_calls = BTreeMap::<usize, OpenAiToolCallAccumulator>::new();
    let mut usage_value = json!({});
    let mut response_id = None;
    let mut finish_reason = None;
    let mut stream_terminated = false;

    while let Some(event) = stream.next().await {
        let event = event.map_err(|error| {
            ProviderError::Unavailable {
                message: sanitize_provider_error(&error.to_string()),
            }
            .with_metadata(provider_error_metadata(
                Some(response_status),
                &response_headers,
                None,
            ))
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
        if value.get("error").is_some()
            || value.get("type").and_then(Value::as_str) == Some("error")
        {
            return Err(provider_error_from_value(
                &value,
                Some(response_status),
                &response_headers,
            ));
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
        }
        .with_metadata(provider_error_metadata(
            Some(response_status),
            &response_headers,
            None,
        )));
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
        usage: provider_usage_from_openai_value(usage_value.clone()),
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
        usage: provider_usage_from_openai_value(usage_value),
        finish_reason: finish_reason_from_openai(
            choice
                .get("finish_reason")
                .and_then(Value::as_str)
                .unwrap_or_default(),
        ),
        raw_metadata: value,
    })
}

fn provider_usage_from_openai_value(usage_value: Value) -> ProviderUsage {
    let input_tokens = first_usage_u64(
        &usage_value,
        &[
            "/prompt_tokens",
            "/input_tokens",
            "/prompt_tokens_total",
            "/input_tokens_total",
            "/promptTokens",
            "/inputTokens",
            "/promptTokensTotal",
            "/inputTokensTotal",
        ],
    );
    let output_tokens = first_usage_u64(
        &usage_value,
        &[
            "/completion_tokens",
            "/output_tokens",
            "/completion_tokens_total",
            "/output_tokens_total",
            "/completionTokens",
            "/outputTokens",
            "/completionTokensTotal",
            "/outputTokensTotal",
        ],
    );
    let reasoning_tokens = first_usage_u64(
        &usage_value,
        &[
            "/completion_tokens_details/reasoning_tokens",
            "/output_tokens_details/reasoning_tokens",
            "/reasoning_tokens",
            "/completionTokensDetails/reasoningTokens",
            "/outputTokensDetails/reasoningTokens",
            "/completionTokensDetails/reasoning_tokens",
            "/outputTokensDetails/reasoning_tokens",
            "/reasoningTokens",
        ],
    );
    let cached_input_tokens = first_usage_u64(
        &usage_value,
        &[
            "/prompt_tokens_details/cached_tokens",
            "/input_tokens_details/cached_tokens",
            "/prompt_tokens_details/cache_read_tokens",
            "/input_tokens_details/cache_read_tokens",
            "/prompt_tokens_details/cache_read_input_tokens",
            "/input_tokens_details/cache_read_input_tokens",
            "/cached_input_tokens",
            "/cache_read_tokens",
            "/promptTokensDetails/cachedTokens",
            "/inputTokensDetails/cachedTokens",
            "/promptTokensDetails/cacheReadTokens",
            "/inputTokensDetails/cacheReadTokens",
            "/promptTokensDetails/cacheReadInputTokens",
            "/inputTokensDetails/cacheReadInputTokens",
            "/cachedInputTokens",
            "/cacheReadTokens",
            "/cacheReadInputTokens",
        ],
    );
    let total_tokens = first_usage_u64(
        &usage_value,
        &[
            "/total_tokens",
            "/total_token_count",
            "/total_tokens_count",
            "/totalTokens",
            "/totalTokenCount",
            "/totalTokensCount",
        ],
    );
    let usage_source = if usage_value
        .as_object()
        .is_some_and(|value| value.is_empty())
    {
        UsageSource::Unknown
    } else {
        UsageSource::Provider
    };
    ProviderUsage {
        input_tokens,
        output_tokens,
        reasoning_tokens,
        cached_input_tokens,
        total_tokens,
        usage_source,
        raw: usage_value,
    }
}

fn first_usage_u64(value: &Value, paths: &[&str]) -> Option<u64> {
    paths.iter().find_map(|path| {
        value.pointer(path).and_then(|value| {
            value
                .as_u64()
                .or_else(|| value.as_i64().and_then(|value| u64::try_from(value).ok()))
                .or_else(|| value.as_str().and_then(|value| value.trim().parse().ok()))
        })
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
        tool_name: restore_provider_tool_wire_name(tool_name),
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

fn provider_error_status(value: &Value) -> Option<u16> {
    [
        value.get("error").and_then(|error| error.get("status")),
        value
            .get("error")
            .and_then(|error| error.get("status_code")),
        value.get("status"),
        value.get("http_status"),
        value.get("status_code"),
    ]
    .into_iter()
    .flatten()
    .find_map(value_as_status)
}

fn value_as_status(value: &Value) -> Option<u16> {
    value
        .as_u64()
        .and_then(|status| u16::try_from(status).ok())
        .or_else(|| {
            value
                .as_str()
                .and_then(|status| status.trim().parse::<u16>().ok())
        })
}

fn provider_error_code(value: &Value) -> Option<String> {
    value
        .get("error")
        .and_then(|error| error.get("code"))
        .or_else(|| value.get("code"))
        .and_then(|code| {
            code.as_str()
                .map(ToOwned::to_owned)
                .or_else(|| code.as_u64().map(|code| code.to_string()))
        })
        .map(|code| sanitize_provider_error(&code))
        .filter(|code| !code.is_empty())
}

fn provider_error_type(value: &Value) -> Option<String> {
    value
        .get("error")
        .and_then(|error| error.get("type"))
        .or_else(|| value.get("error_type"))
        .and_then(Value::as_str)
        .map(sanitize_provider_error)
        .filter(|error_type| !error_type.is_empty())
}

fn provider_error_retry_after(value: &Value) -> Option<Duration> {
    let retry_after_ms = value
        .get("error")
        .and_then(|error| error.get("retry_after_ms"))
        .or_else(|| value.get("retry_after_ms"));
    if let Some(milliseconds) = retry_after_ms.and_then(Value::as_u64) {
        return Some(Duration::from_millis(milliseconds.min(60_000)));
    }
    let retry_after = value
        .get("error")
        .and_then(|error| error.get("retry_after"))
        .or_else(|| value.get("retry_after"))?;
    retry_after
        .as_u64()
        .or_else(|| {
            retry_after
                .as_str()
                .and_then(|value| value.trim().parse::<u64>().ok())
        })
        .map(|seconds| Duration::from_secs(seconds.min(60)))
}

pub(crate) fn retry_after_from_headers(headers: &HeaderMap) -> Option<Duration> {
    if let Some(milliseconds) = headers
        .get("retry-after-ms")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok())
    {
        return Some(Duration::from_millis(milliseconds.min(60_000)));
    }
    headers
        .get("retry-after")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok())
        .map(|seconds| Duration::from_secs(seconds.min(60)))
}

pub(crate) fn request_id_from_headers(headers: &HeaderMap) -> Option<String> {
    ["x-request-id", "request-id", "openai-request-id"]
        .into_iter()
        .find_map(|name| {
            headers
                .get(name)
                .and_then(|value| value.to_str().ok())
                .map(sanitize_provider_error)
                .filter(|value| !value.is_empty())
        })
}

fn request_id_from_value(value: &Value) -> Option<String> {
    value
        .get("error")
        .and_then(|error| error.get("request_id").or_else(|| error.get("requestId")))
        .or_else(|| value.get("request_id"))
        .and_then(Value::as_str)
        .map(sanitize_provider_error)
        .filter(|value| !value.is_empty())
}

fn provider_error_metadata(
    status: Option<u16>,
    headers: &HeaderMap,
    value: Option<&Value>,
) -> ProviderErrorMetadata {
    let payload_status = value.and_then(provider_error_status);
    let payload_retry_after = value.and_then(provider_error_retry_after);
    let payload_request_id = value.and_then(request_id_from_value);
    ProviderErrorMetadata {
        http_status: status.filter(|status| *status >= 400).or(payload_status),
        provider_code: value.and_then(provider_error_code),
        retry_after: payload_retry_after.or_else(|| retry_after_from_headers(headers)),
        request_id: payload_request_id.or_else(|| request_id_from_headers(headers)),
    }
}

fn provider_error_kind(status: Option<u16>, value: &Value) -> ProviderError {
    let message = provider_error_message(value);
    match status {
        Some(429) => ProviderError::RateLimited { message },
        Some(status) if (500..600).contains(&status) => ProviderError::Unavailable { message },
        Some(_) => ProviderError::Failed { message },
        None => {
            // 网关常用 code/type 表达瞬态错误，不能只依赖 HTTP status 或 message。
            let marker_text = format!(
                "{} {} {}",
                message,
                provider_error_code(value).unwrap_or_default(),
                provider_error_type(value).unwrap_or_default()
            )
            .to_ascii_lowercase();
            if [
                "rate limit",
                "rate_limit",
                "rate limited",
                "too many requests",
                "quota exceeded",
            ]
            .iter()
            .any(|marker| marker_text.contains(marker))
            {
                ProviderError::RateLimited { message }
            } else if [
                "bad gateway",
                "bad_gateway",
                "gateway timeout",
                "gateway_timeout",
                "service unavailable",
                "service_unavailable",
                "temporarily unavailable",
                "temporarily_unavailable",
                "server error",
                "server_error",
                "internal server",
                "internal_error",
                "internal_server_error",
                "upstream",
                "overloaded",
                "overload",
                "connection reset",
                "502",
                "503",
                "504",
            ]
            .iter()
            .any(|marker| marker_text.contains(marker))
            {
                ProviderError::Unavailable { message }
            } else {
                ProviderError::Failed { message }
            }
        }
    }
}

fn provider_error_from_value(
    value: &Value,
    response_status: Option<u16>,
    headers: &HeaderMap,
) -> ProviderError {
    // 实际 HTTP 状态是服务端行为的权威来源；只有成功响应里的 SSE/JSON
    // 错误事件才使用 payload status。
    let status = response_status
        .filter(|status| *status >= 400)
        .or_else(|| provider_error_status(value));
    let mapped = provider_error_kind(status, value);
    if matches!(
        &mapped,
        ProviderError::Unavailable { .. } | ProviderError::RateLimited { .. }
    ) {
        mapped.with_metadata(provider_error_metadata(status, headers, Some(value)))
    } else {
        mapped
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

fn provider_http_error_with_headers(
    status: reqwest::StatusCode,
    headers: &HeaderMap,
    value: &Value,
) -> ProviderError {
    provider_error_from_value(value, Some(status.as_u16()), headers)
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
