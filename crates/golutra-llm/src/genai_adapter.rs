use std::{fmt, sync::Arc};

use async_trait::async_trait;
use futures_util::StreamExt;
use genai::{
    Client, ModelIden, ServiceTarget, WebConfig,
    adapter::AdapterKind,
    chat::{
        CacheControl, ChatMessage, ChatOptions, ChatRequest, ChatStreamEvent, ContentPart,
        MessageContent, ReasoningEffort, StopReason, StreamEnd, Tool, ToolCall, ToolName,
        ToolResponse,
    },
    resolver::{AuthData, Endpoint},
};
use golutra_auth::{CredentialProvider, FixedCredentialProvider};
use golutra_core::{
    PromptCachePolicy, ProviderContract, ProviderRequestId, ProviderResponseId, TaskId, TurnId,
};
use secrecy::ExposeSecret;
use serde_json::{Value, json};

use super::{
    LlmProvider, ProviderError, ProviderErrorMetadata, ProviderFinishReason,
    ProviderGenerationConfig, ProviderHttpHeaders, ProviderMessage, ProviderProbeResult,
    ProviderProtocol, ProviderRequest, ProviderResponse, ProviderRole, ProviderStreamEvent,
    ProviderToolCall, ProviderUsage, UsageSource, configured_or_first_env,
    custom_headers_from_reader, env_mapping, first_env, generation_config_from_reader,
    missing_env_error, protocol_capabilities, provider_tool_schema_for_contract,
    request_id_from_headers, retry_after_from_headers, sanitize_provider_error,
    selected_protocol_from_reader, validate_native_base_url,
};

#[derive(Clone, PartialEq, Eq)]
pub struct GenaiProviderConfig {
    pub api_key: String,
    pub api_key_env: String,
    pub base_url: String,
    pub model_id: String,
    pub protocol: ProviderProtocol,
    pub generation_config: ProviderGenerationConfig,
    pub custom_headers: ProviderHttpHeaders,
}

#[derive(Clone)]
pub struct GenaiProviderAdapter {
    config: GenaiProviderConfig,
    credential: Arc<dyn CredentialProvider>,
    client: Client,
}

impl fmt::Debug for GenaiProviderConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GenaiProviderConfig")
            .field("api_key_env", &self.api_key_env)
            .field("base_url", &self.base_url)
            .field("model_id", &self.model_id)
            .field("protocol", &self.protocol)
            .field("generation_config", &self.generation_config)
            .field("custom_header_names", &self.custom_headers.names())
            .finish_non_exhaustive()
    }
}

impl fmt::Debug for GenaiProviderAdapter {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GenaiProviderAdapter")
            .field("api_key_env", &self.config.api_key_env)
            .field("base_url", &self.config.base_url)
            .field("model_id", &self.config.model_id)
            .field("protocol", &self.config.protocol)
            .field("generation_config", &self.config.generation_config)
            .field("custom_header_names", &self.config.custom_headers.names())
            .finish_non_exhaustive()
    }
}

impl GenaiProviderAdapter {
    #[must_use]
    pub fn from_config(config: GenaiProviderConfig) -> Self {
        let credential = Arc::new(FixedCredentialProvider::new(
            config.api_key.clone(),
            config.api_key_env.clone(),
        ));
        Self::from_config_with_credential(config, credential)
    }

    #[must_use]
    pub fn from_config_with_credential(
        config: GenaiProviderConfig,
        credential: Arc<dyn CredentialProvider>,
    ) -> Self {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let web_config = WebConfig::default()
            .with_connect_timeout(std::time::Duration::from_secs(10))
            // ProviderSession enforces per-event idle and buffered request
            // deadlines; keep the adapter's coarse total timeout out of the
            // way of an active long-running stream.
            .with_timeout(std::time::Duration::from_secs(3_600))
            .with_default_headers(config.custom_headers.to_header_map());
        Self {
            config,
            credential,
            client: Client::builder().with_web_config(web_config).build(),
        }
    }

    pub fn config_from_env_reader<F>(reader: F) -> Result<GenaiProviderConfig, ProviderError>
    where
        F: Fn(&str) -> Option<String>,
    {
        let protocol =
            selected_protocol_from_reader(&reader).ok_or_else(|| ProviderError::NotConfigured {
                message: "provider protocol is not configured".to_owned(),
            })?;
        if matches!(
            protocol,
            ProviderProtocol::Mock
                | ProviderProtocol::OpenAiCompatible
                | ProviderProtocol::OpenAiResponses
        ) {
            return Err(ProviderError::NotConfigured {
                message: format!(
                    "provider protocol `{}` is not handled by the native genai adapter",
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
        let base_url = validate_native_base_url(&base_url)
            .map_err(|message| ProviderError::NotConfigured { message })?;
        let generation_config = generation_config_from_reader(&reader)?;
        if generation_config
            .max_tokens
            .is_some_and(|value| value > u64::from(u32::MAX))
        {
            return Err(ProviderError::NotConfigured {
                message: "provider max_tokens exceeds the supported u32 range".to_owned(),
            });
        }
        Ok(GenaiProviderConfig {
            api_key,
            api_key_env,
            base_url,
            model_id,
            protocol,
            generation_config,
            custom_headers: custom_headers_from_reader(&reader)?,
        })
    }

    pub async fn probe(&self) -> Result<ProviderProbeResult, ProviderError> {
        super::LlmProvider::complete(
            self,
            ProviderRequest {
                request_id: ProviderRequestId::new(),
                task_id: TaskId::new(),
                turn_id: TurnId::new(),
                session_id: None,
                provider_id: self.config.protocol.id().to_owned(),
                model_id: self.config.model_id.clone(),
                messages: vec![ProviderMessage {
                    role: ProviderRole::User,
                    content: "Reply with OK.".to_owned(),
                    tool_call_id: None,
                    tool_name: None,
                    tool_calls: Vec::new(),
                    metadata: Default::default(),
                }],
                tools: Vec::new(),
                cache_policy: Default::default(),
                max_output_tokens: None,
            },
        )
        .await?;
        Ok(ProviderProbeResult {
            provider_id: self.config.protocol.id().to_owned(),
            protocol: native_protocol(self.config.protocol).to_owned(),
            base_url: self.config.base_url.clone(),
            model_id: self.config.model_id.clone(),
            model_available: Some(true),
            discovered_models: vec![self.config.model_id.clone()],
            capabilities: protocol_capabilities(self.config.protocol),
        })
    }

    fn service_target(&self, api_key: &str) -> Result<ServiceTarget, ProviderError> {
        let adapter_kind = match self.config.protocol {
            ProviderProtocol::Anthropic => AdapterKind::Anthropic,
            ProviderProtocol::Gemini => AdapterKind::Gemini,
            ProviderProtocol::VertexAi => AdapterKind::Vertex,
            ProviderProtocol::Genai => {
                AdapterKind::from_model(&self.config.model_id).map_err(map_genai_error)?
            }
            ProviderProtocol::Mock
            | ProviderProtocol::OpenAiCompatible
            | ProviderProtocol::OpenAiResponses => {
                return Err(ProviderError::NotConfigured {
                    message: format!(
                        "provider protocol `{}` cannot use the native genai adapter",
                        self.config.protocol.id()
                    ),
                });
            }
        };
        Ok(ServiceTarget {
            endpoint: Endpoint::from_owned(format!(
                "{}/",
                self.config.base_url.trim_end_matches('/')
            )),
            auth: AuthData::from_single(api_key.to_owned()),
            model: ModelIden::new(adapter_kind, self.config.model_id.clone()),
        })
    }

    async fn execute(
        &self,
        request: &ProviderRequest,
        force_refresh: bool,
    ) -> Result<ProviderResponse, ProviderError> {
        // Request shaping is deterministic for a logical attempt. Build it
        // once so an authentication refresh retries the network call without
        // re-projecting every message and tool schema.
        let chat_request = genai_chat_request(request, self.config.protocol)?;
        let options = genai_chat_options(
            &self.config.generation_config,
            request.max_output_tokens,
            false,
            self.cache_identity_for_request(request).as_ref(),
            request.cache_policy,
        )?;
        let api_key = self
            .credential
            .credential(force_refresh)
            .await
            .map_err(super::provider_credential_error)?;
        let response = self
            .client
            .exec_chat(
                self.service_target(api_key.expose_secret())?,
                chat_request.clone(),
                Some(&options),
            )
            .await;
        match response {
            Ok(response) => provider_response_from_genai(response),
            Err(error) if !force_refresh && genai_error_requires_auth_refresh(&error) => {
                let api_key = self
                    .credential
                    .credential(true)
                    .await
                    .map_err(super::provider_credential_error)?;
                self.client
                    .exec_chat(
                        self.service_target(api_key.expose_secret())?,
                        chat_request,
                        Some(&options),
                    )
                    .await
                    .map_err(map_genai_error)
                    .and_then(provider_response_from_genai)
            }
            Err(error) => Err(map_genai_error(error)),
        }
    }

    async fn execute_stream(
        &self,
        request: &ProviderRequest,
        on_event: &mut (dyn FnMut(ProviderStreamEvent) + Send),
    ) -> Result<ProviderResponse, ProviderError> {
        // Keep the shaped request and options stable across a credential
        // refresh. This is especially valuable for long tool schemas.
        let chat_request = genai_chat_request(request, self.config.protocol)?;
        let options = genai_chat_options(
            &self.config.generation_config,
            request.max_output_tokens,
            true,
            self.cache_identity_for_request(request).as_ref(),
            request.cache_policy,
        )?;
        let mut force_refresh = false;
        let response = loop {
            let api_key = self
                .credential
                .credential(force_refresh)
                .await
                .map_err(super::provider_credential_error)?;
            match self
                .client
                .exec_chat_stream(
                    self.service_target(api_key.expose_secret())?,
                    chat_request.clone(),
                    Some(&options),
                )
                .await
            {
                Ok(response) => break response,
                Err(error) if !force_refresh && genai_error_requires_auth_refresh(&error) => {
                    force_refresh = true;
                }
                Err(error) => return Err(map_genai_error(error)),
            }
        };
        let model_id = response.model_iden.to_string();
        let mut stream = response.stream;
        let mut stream_end = None;
        let mut tool_delta_index = 0_usize;
        while let Some(event) = stream.next().await {
            match event.map_err(map_genai_error)? {
                ChatStreamEvent::Start | ChatStreamEvent::ThoughtSignatureChunk(_) => {}
                ChatStreamEvent::Chunk(chunk) => {
                    if !chunk.content.is_empty() {
                        on_event(ProviderStreamEvent::TextDelta {
                            text: chunk.content,
                        });
                    }
                }
                ChatStreamEvent::ReasoningChunk(chunk) => {
                    if !chunk.content.is_empty() {
                        on_event(ProviderStreamEvent::ReasoningDelta {
                            text: chunk.content,
                        });
                    }
                }
                ChatStreamEvent::ToolCallChunk(chunk) => {
                    on_event(ProviderStreamEvent::ToolCallDelta {
                        index: tool_delta_index,
                        tool_call_id: (!chunk.tool_call.call_id.is_empty())
                            .then(|| chunk.tool_call.call_id.clone()),
                        tool_name: (!chunk.tool_call.fn_name.is_empty())
                            .then(|| restore_wire_tool_name(&chunk.tool_call.fn_name)),
                    });
                    tool_delta_index = tool_delta_index.saturating_add(1);
                }
                ChatStreamEvent::End(end) => {
                    stream_end = Some(end);
                    break;
                }
            }
        }
        let end = stream_end.ok_or_else(|| ProviderError::Unavailable {
            message: "native provider stream ended before a terminal event".to_owned(),
        })?;
        provider_response_from_genai_stream(end, &model_id)
    }
}

#[async_trait]
impl super::LlmProvider for GenaiProviderAdapter {
    async fn complete(&self, request: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        self.execute(&request, false).await
    }

    async fn complete_stream(
        &self,
        request: ProviderRequest,
        on_event: &mut (dyn FnMut(ProviderStreamEvent) + Send),
    ) -> Result<ProviderResponse, ProviderError> {
        self.execute_stream(&request, on_event).await
    }

    fn contract(&self) -> ProviderContract {
        ProviderContract {
            provider_id: self.config.protocol.id().to_owned(),
            model_id: self.config.model_id.clone(),
            native_protocol: native_protocol(self.config.protocol).to_owned(),
            stream_event_mapping: "genai_normalized_stream".to_owned(),
            tool_call_mapping: "genai_normalized_function_calls".to_owned(),
            usage_mapping: "genai_normalized_usage".to_owned(),
            reasoning_mapping: "genai_reasoning_effort".to_owned(),
            finish_reason_mapping: "genai_stop_reason".to_owned(),
            error_mapping: "genai_http_and_adapter_errors".to_owned(),
            rate_limit_mapping: "http_429".to_owned(),
            cost_model: "external".to_owned(),
            capability_matrix_ref: None,
            golden_fixture_refs: golden_fixture_refs(self.config.protocol),
        }
    }

    fn cache_namespace(&self) -> String {
        super::route_cache_namespace(native_protocol(self.config.protocol), &self.config.base_url)
    }
}

pub(crate) fn genai_chat_request(
    request: &ProviderRequest,
    protocol: ProviderProtocol,
) -> Result<ChatRequest, ProviderError> {
    let is_openai_responses = protocol == ProviderProtocol::OpenAiResponses;
    let systems = request
        .messages
        .iter()
        .filter(|message| message.role == ProviderRole::System)
        .map(|message| message.content.as_str())
        .collect::<Vec<_>>()
        .join("\n\n");
    let messages = request
        .messages
        .iter()
        .filter(|message| message.role != ProviderRole::System)
        .map(|message| genai_message(message, is_openai_responses))
        .collect::<Result<Vec<_>, _>>()?;
    let mut chat_request = ChatRequest::new(messages);
    if !systems.is_empty() {
        chat_request = chat_request.with_system(systems);
    }
    if !request.tools.is_empty() {
        chat_request = chat_request.with_tools(request.tools.iter().map(|contract| {
            let schema = provider_tool_schema_for_contract(contract);
            // rust-genai reserves the literal `web_search` name for its native
            // provider tool on several adapters. Golutra owns a regular
            // function with that name, so use a stable wire alias and restore
            // it at the provider response boundary.
            let tool = Tool::new(ToolName::Custom(wire_tool_name(&contract.tool_name)))
                .with_description(tool_description(&contract.tool_name))
                .with_schema(schema.clone());
            if is_openai_responses {
                // Responses strict 要求对象的每个属性都列入 `required`；运行时工具
                // 有意暴露可选控制项（例如 shell 超时），因此保留完整 schema，
                // 仅在投影 schema 不满足该契约时关闭 strict 解码。
                tool.with_strict(responses_schema_supports_strict(&schema))
            } else {
                tool
            }
        }));
    }
    Ok(chat_request)
}

fn wire_tool_name(name: &str) -> String {
    super::provider_tool_wire_name(name)
}

pub(crate) fn restore_wire_tool_name(name: &str) -> String {
    super::restore_provider_tool_wire_name(name)
}

fn responses_schema_supports_strict(schema: &Value) -> bool {
    match schema {
        Value::Object(object) => {
            if let Some(properties) = object.get("properties").and_then(Value::as_object) {
                let required = object
                    .get("required")
                    .and_then(Value::as_array)
                    .map(|values| {
                        values
                            .iter()
                            .filter_map(Value::as_str)
                            .collect::<std::collections::HashSet<_>>()
                    })
                    .unwrap_or_default();
                if properties
                    .keys()
                    .any(|name| !required.contains(name.as_str()))
                {
                    return false;
                }
            }
            object.values().all(responses_schema_supports_strict)
        }
        Value::Array(values) => values.iter().all(responses_schema_supports_strict),
        _ => true,
    }
}

fn genai_message(
    message: &ProviderMessage,
    replay_openai_responses_reasoning: bool,
) -> Result<ChatMessage, ProviderError> {
    match message.role {
        ProviderRole::System => Ok(ChatMessage::system(message.content.clone())),
        ProviderRole::User => Ok(ChatMessage::user(message.content.clone())),
        ProviderRole::Assistant => {
            let mut parts = Vec::new();
            if replay_openai_responses_reasoning {
                parts.extend(
                    message
                        .metadata
                        .openai_responses_replay_items
                        .iter()
                        .map(openai_responses_thought_signature)
                        .collect::<Result<Vec<_>, _>>()?
                        .into_iter()
                        .map(ContentPart::ThoughtSignature),
                );
            }
            if !message.content.is_empty() {
                parts.push(ContentPart::Text(message.content.clone()));
            }
            parts.extend(message.tool_calls.iter().map(|tool_call| {
                ContentPart::ToolCall(ToolCall {
                    call_id: tool_call.tool_call_id.clone(),
                    fn_name: wire_tool_name(&tool_call.tool_name),
                    fn_arguments: tool_call.arguments.clone(),
                    thought_signatures: None,
                })
            }));
            Ok(ChatMessage::assistant(MessageContent::from_parts(parts)))
        }
        ProviderRole::Tool => {
            let call_id = message
                .tool_call_id
                .as_deref()
                .filter(|value| !value.trim().is_empty())
                .ok_or_else(|| ProviderError::Malformed {
                    message: "tool response has no non-empty tool_call_id".to_owned(),
                })?;
            let mut response = ToolResponse::new(call_id, message.content.clone());
            if let Some(tool_name) = message
                .tool_name
                .as_deref()
                .filter(|value| !value.trim().is_empty())
            {
                response = response.with_fn_name(wire_tool_name(tool_name));
            }
            Ok(ChatMessage::from(response))
        }
    }
}

fn openai_responses_thought_signature(item: &Value) -> Result<String, ProviderError> {
    if item.get("type").and_then(Value::as_str) != Some("reasoning") {
        return Err(ProviderError::Malformed {
            message: "Responses replay metadata contains an unsupported item".to_owned(),
        });
    }
    let encrypted_content = item
        .get("encrypted_content")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| ProviderError::Malformed {
            message: "Responses reasoning item has no encrypted_content".to_owned(),
        })?;
    if encrypted_content.len() > super::MAX_PROVIDER_MESSAGE_BYTES {
        return Err(ProviderError::Malformed {
            message: format!(
                "Responses encrypted reasoning exceeds {} byte limit",
                super::MAX_PROVIDER_MESSAGE_BYTES
            ),
        });
    }
    Ok(encrypted_content.to_owned())
}

pub(crate) fn genai_chat_options(
    config: &ProviderGenerationConfig,
    request_max_output_tokens: Option<u64>,
    streaming: bool,
    cache_identity: Option<&golutra_core::CacheIdentity>,
    cache_policy: PromptCachePolicy,
) -> Result<ChatOptions, ProviderError> {
    let mut options = ChatOptions::default().with_capture_raw_body(true);
    if let Some(identity) = cache_identity {
        options = options.with_prompt_cache_key(identity.key.clone());
        options = match cache_policy {
            PromptCachePolicy::Short => options.with_cache_control(CacheControl::Ephemeral5m),
            PromptCachePolicy::Long => options.with_cache_control(CacheControl::Ephemeral24h),
            PromptCachePolicy::Auto | PromptCachePolicy::None => options,
        };
    }
    if streaming {
        options = options
            .with_capture_usage(true)
            .with_capture_content(true)
            .with_capture_reasoning_content(true)
            .with_capture_tool_calls(true);
    }
    if let Some(max_tokens) = request_max_output_tokens.or(config.max_tokens) {
        options = options.with_max_tokens(u32::try_from(max_tokens).map_err(|_| {
            ProviderError::NotConfigured {
                message: "provider max output tokens exceeds the supported u32 range".to_owned(),
            }
        })?);
    }
    let effort = config
        .reasoning_effort
        .map(|effort| match effort {
            super::ProviderReasoningEffort::Low => ReasoningEffort::Low,
            super::ProviderReasoningEffort::Medium => ReasoningEffort::Medium,
            super::ProviderReasoningEffort::High => ReasoningEffort::High,
            super::ProviderReasoningEffort::Xhigh => ReasoningEffort::XHigh,
        })
        .or(config.enable_thinking.then_some(ReasoningEffort::Medium));
    if let Some(effort) = effort {
        options = options.with_reasoning_effort(effort);
    }
    Ok(options)
}

pub(crate) fn provider_response_from_genai_stream(
    end: StreamEnd,
    model_id: &str,
) -> Result<ProviderResponse, ProviderError> {
    let captured_size = serde_json::to_vec(&end).map_err(|error| ProviderError::Malformed {
        message: format!("provider stream result could not be serialized: {error}"),
    })?;
    if captured_size.len() > super::MAX_PROVIDER_RESPONSE_BYTES {
        return Err(ProviderError::Malformed {
            message: format!(
                "provider response exceeds {} byte limit",
                super::MAX_PROVIDER_RESPONSE_BYTES
            ),
        });
    }
    let content = end
        .captured_content
        .as_ref()
        .map(|content| content.texts().join(""))
        .unwrap_or_default();
    if content.len() > super::MAX_PROVIDER_MESSAGE_BYTES {
        return Err(ProviderError::Malformed {
            message: format!(
                "assistant message exceeds {} byte limit",
                super::MAX_PROVIDER_MESSAGE_BYTES
            ),
        });
    }
    let tool_calls = end
        .captured_content
        .as_ref()
        .map(MessageContent::tool_calls)
        .unwrap_or_default()
        .into_iter()
        .map(|call| {
            validate_tool_call(&call.call_id, &call.fn_name, &call.fn_arguments)?;
            Ok(ProviderToolCall {
                tool_call_id: call.call_id.clone(),
                tool_name: restore_wire_tool_name(&call.fn_name),
                arguments: call.fn_arguments.clone(),
            })
        })
        .collect::<Result<Vec<_>, ProviderError>>()?;
    let usage = match end.captured_usage.as_ref() {
        Some(usage) => provider_usage_from_genai(usage)?,
        None => ProviderUsage {
            input_tokens: None,
            output_tokens: None,
            reasoning_tokens: None,
            cached_input_tokens: None,
            total_tokens: None,
            usage_source: UsageSource::Unknown,
            raw: json!({}),
        },
    };
    let finish_reason = finish_reason_from_genai(end.captured_stop_reason.as_ref());
    Ok(ProviderResponse {
        response_id: ProviderResponseId::new(),
        message: (!content.is_empty()).then_some(ProviderMessage {
            role: ProviderRole::Assistant,
            content,
            tool_call_id: None,
            tool_name: None,
            tool_calls: Vec::new(),
            metadata: Default::default(),
        }),
        tool_calls,
        usage,
        finish_reason,
        raw_metadata: json!({
            "provider_model": model_id,
            "response_id": end.captured_response_id,
            "streamed": true,
        }),
    })
}

fn provider_response_from_genai(
    response: genai::chat::ChatResponse,
) -> Result<ProviderResponse, ProviderError> {
    if let Some(raw_body) = &response.captured_raw_body {
        let raw_body_size = serde_json::to_vec(raw_body)
            .map_err(|error| ProviderError::Malformed {
                message: format!("provider raw response could not be serialized: {error}"),
            })?
            .len();
        if raw_body_size > super::MAX_PROVIDER_RESPONSE_BYTES {
            return Err(ProviderError::Malformed {
                message: format!(
                    "provider response exceeds {} byte limit",
                    super::MAX_PROVIDER_RESPONSE_BYTES
                ),
            });
        }
    }
    let content = response.texts().join("");
    if content.len() > super::MAX_PROVIDER_MESSAGE_BYTES {
        return Err(ProviderError::Malformed {
            message: format!(
                "assistant message exceeds {} byte limit",
                super::MAX_PROVIDER_MESSAGE_BYTES
            ),
        });
    }
    let tool_calls = response
        .tool_calls()
        .into_iter()
        .map(|call| {
            validate_tool_call(&call.call_id, &call.fn_name, &call.fn_arguments)?;
            Ok(ProviderToolCall {
                tool_call_id: call.call_id.clone(),
                tool_name: restore_wire_tool_name(&call.fn_name),
                arguments: call.fn_arguments.clone(),
            })
        })
        .collect::<Result<Vec<_>, ProviderError>>()?;
    let raw_usage = response
        .captured_raw_body
        .as_ref()
        .and_then(raw_usage_value);
    let usage = provider_usage_from_genai_with_raw(&response.usage, raw_usage.as_ref())?;
    let finish_reason = finish_reason_from_genai(response.stop_reason.as_ref());
    let raw_metadata = response.captured_raw_body.unwrap_or_else(|| {
        json!({
            "provider_model": response.provider_model_iden.to_string(),
            "stop_reason": response.stop_reason.as_ref().map(ToString::to_string),
        })
    });
    Ok(ProviderResponse {
        response_id: ProviderResponseId::new(),
        message: (!content.is_empty()).then_some(ProviderMessage {
            role: ProviderRole::Assistant,
            content,
            tool_call_id: None,
            tool_name: None,
            tool_calls: Vec::new(),
            metadata: Default::default(),
        }),
        tool_calls,
        usage,
        finish_reason,
        raw_metadata,
    })
}

fn provider_usage_from_genai(usage: &genai::chat::Usage) -> Result<ProviderUsage, ProviderError> {
    provider_usage_from_genai_with_raw(usage, None)
}

fn provider_usage_from_genai_with_raw(
    usage: &genai::chat::Usage,
    raw_usage: Option<&Value>,
) -> Result<ProviderUsage, ProviderError> {
    // The raw response body already contains the provider's usage shape on
    // normal network responses.  Do not eagerly serialize the typed genai
    // usage before `unwrap_or`: that allocation used to happen even when the
    // raw value was available and was paid on every streamed turn.
    let usage_raw = match raw_usage {
        Some(raw_usage) => raw_usage.clone(),
        None => serde_json::to_value(usage).map_err(|error| ProviderError::Malformed {
            message: format!("genai usage could not be serialized: {error}"),
        })?,
    };
    let cache_read_tokens = usage
        .prompt_tokens_details
        .as_ref()
        .and_then(|details| non_negative_u64(details.cached_tokens))
        .or_else(|| {
            super::first_usage_u64(
                &usage_raw,
                &[
                    "/cached_tokens",
                    "/cache_read_tokens",
                    "/cacheReadTokens",
                    "/cache_read_input_tokens",
                    "/cacheReadInputTokens",
                    "/prompt_tokens_details/cached_tokens",
                    "/prompt_tokens_details/cache_read_tokens",
                    "/prompt_tokens_details/cacheReadTokens",
                    "/input_tokens_details/cached_tokens",
                    "/input_tokens_details/cache_read_tokens",
                    "/input_tokens_details/cacheReadTokens",
                ],
            )
        });
    Ok(ProviderUsage {
        input_tokens: non_negative_u64(usage.prompt_tokens).or_else(|| {
            super::first_usage_u64(
                &usage_raw,
                &[
                    "/prompt_tokens",
                    "/input_tokens",
                    "/promptTokens",
                    "/inputTokens",
                ],
            )
        }),
        output_tokens: non_negative_u64(usage.completion_tokens).or_else(|| {
            super::first_usage_u64(
                &usage_raw,
                &[
                    "/completion_tokens",
                    "/output_tokens",
                    "/completionTokens",
                    "/outputTokens",
                ],
            )
        }),
        reasoning_tokens: usage
            .completion_tokens_details
            .as_ref()
            .and_then(|details| non_negative_u64(details.reasoning_tokens))
            .or_else(|| {
                super::first_usage_u64(
                    &usage_raw,
                    &[
                        "/completion_tokens_details/reasoning_tokens",
                        "/output_tokens_details/reasoning_tokens",
                        "/completionTokensDetails/reasoningTokens",
                        "/outputTokensDetails/reasoningTokens",
                        "/reasoning_tokens",
                        "/reasoningTokens",
                    ],
                )
            }),
        cached_input_tokens: cache_read_tokens,
        total_tokens: non_negative_u64(usage.total_tokens).or_else(|| {
            super::first_usage_u64(
                &usage_raw,
                &[
                    "/total_tokens",
                    "/totalTokens",
                    "/total_token_count",
                    "/totalTokenCount",
                ],
            )
        }),
        usage_source: UsageSource::Provider,
        raw: usage_raw,
    })
}

fn raw_usage_value(raw_body: &Value) -> Option<Value> {
    ["/usage", "/response/usage"]
        .iter()
        .find_map(|path| raw_body.pointer(path).cloned())
}

fn finish_reason_from_genai(reason: Option<&StopReason>) -> ProviderFinishReason {
    match reason {
        Some(StopReason::Completed(_) | StopReason::StopSequence(_)) => ProviderFinishReason::Stop,
        Some(StopReason::MaxTokens(_)) => ProviderFinishReason::Length,
        Some(StopReason::ToolCall(_)) => ProviderFinishReason::ToolCalls,
        Some(StopReason::ContentFilter(_)) => ProviderFinishReason::ContentFilter,
        Some(StopReason::Other(_)) | None => ProviderFinishReason::Unknown,
    }
}

fn validate_tool_call(call_id: &str, name: &str, arguments: &Value) -> Result<(), ProviderError> {
    if call_id.trim().is_empty() {
        return Err(ProviderError::Malformed {
            message: "tool call has no non-empty id".to_owned(),
        });
    }
    if call_id.len() > super::MAX_PROVIDER_TOOL_CALL_ID_BYTES {
        return Err(ProviderError::Malformed {
            message: format!(
                "tool call id exceeds {} byte limit",
                super::MAX_PROVIDER_TOOL_CALL_ID_BYTES
            ),
        });
    }
    if name.trim().is_empty() {
        return Err(ProviderError::Malformed {
            message: "tool call function has no non-empty name".to_owned(),
        });
    }
    if name.len() > super::MAX_PROVIDER_TOOL_NAME_BYTES {
        return Err(ProviderError::Malformed {
            message: format!(
                "tool name exceeds {} byte limit",
                super::MAX_PROVIDER_TOOL_NAME_BYTES
            ),
        });
    }
    let size = serde_json::to_vec(arguments)
        .map_err(|error| ProviderError::Malformed {
            message: format!("tool call arguments could not be serialized: {error}"),
        })?
        .len();
    if size > super::MAX_PROVIDER_TOOL_ARGUMENT_BYTES {
        return Err(ProviderError::Malformed {
            message: format!(
                "tool call arguments exceed {} byte limit",
                super::MAX_PROVIDER_TOOL_ARGUMENT_BYTES
            ),
        });
    }
    Ok(())
}

fn non_negative_u64(value: Option<i32>) -> Option<u64> {
    value.and_then(|value| u64::try_from(value).ok())
}

pub(crate) fn map_genai_error(error: genai::Error) -> ProviderError {
    let message = sanitize_provider_error(&error.to_string());
    let status = genai_error_http_status(&error);
    let metadata = genai_error_metadata(&error);
    let mapped = if status == Some(429) {
        ProviderError::RateLimited { message }
    } else if status.is_some_and(|status| (500..600).contains(&status)) {
        ProviderError::Unavailable { message }
    } else {
        match error {
            genai::Error::RequiresApiKey { .. }
            | genai::Error::NoAuthResolver { .. }
            | genai::Error::NoAuthData { .. } => ProviderError::NotConfigured { message },
            genai::Error::InvalidJsonResponseElement { .. }
            | genai::Error::ChatResponseGeneration { .. }
            | genai::Error::SerdeJson(_) => ProviderError::Malformed { message },
            _ if message.to_ascii_lowercase().contains("timed out") => {
                ProviderError::Timeout { message }
            }
            _ if [
                "stream",
                "connection",
                "connect",
                "disconnect",
                "reset",
                "transport",
                "broken pipe",
            ]
            .iter()
            .any(|marker| message.to_ascii_lowercase().contains(marker)) =>
            {
                ProviderError::Unavailable { message }
            }
            _ => ProviderError::Failed { message },
        }
    };
    if status.is_some_and(|status| status == 429 || (500..600).contains(&status)) {
        mapped.with_metadata(ProviderErrorMetadata {
            http_status: status.or(metadata.http_status),
            ..metadata
        })
    } else {
        mapped
    }
}

pub(crate) fn genai_error_requires_auth_refresh(error: &genai::Error) -> bool {
    matches!(
        error,
        genai::Error::HttpError { status, .. } if status.as_u16() == 401
    )
}

pub(crate) fn genai_stream_error_requires_auth_refresh(error: &genai::Error) -> bool {
    genai_error_http_status(error) == Some(401)
}

pub(crate) fn genai_error_http_status(error: &genai::Error) -> Option<u16> {
    match error {
        genai::Error::HttpError { status, .. } => Some(status.as_u16()),
        genai::Error::WebStream { error, .. } => error
            .downcast_ref::<genai::Error>()
            .and_then(genai_error_http_status)
            .or_else(|| {
                error
                    .downcast_ref::<genai::webc::Error>()
                    .and_then(webc_error_http_status)
            }),
        genai::Error::WebAdapterCall { webc_error, .. }
        | genai::Error::WebModelCall { webc_error, .. } => webc_error_http_status(webc_error),
        _ => None,
    }
}

/// 从 rust-genai 的错误包装层提取可重试请求的脱敏元数据。
pub(crate) fn genai_error_metadata(error: &genai::Error) -> ProviderErrorMetadata {
    match error {
        genai::Error::WebStream { error, .. } => error
            .downcast_ref::<genai::Error>()
            .map(genai_error_metadata)
            .or_else(|| {
                error
                    .downcast_ref::<genai::webc::Error>()
                    .map(webc_error_metadata)
            })
            .unwrap_or_default(),
        genai::Error::WebAdapterCall { webc_error, .. }
        | genai::Error::WebModelCall { webc_error, .. } => webc_error_metadata(webc_error),
        _ => ProviderErrorMetadata::default(),
    }
}

fn webc_error_http_status(error: &genai::webc::Error) -> Option<u16> {
    match error {
        genai::webc::Error::ResponseFailedStatus { status, .. } => Some(status.as_u16()),
        genai::webc::Error::Reqwest(error) => error.status().map(|status| status.as_u16()),
        _ => None,
    }
}

fn webc_error_metadata(error: &genai::webc::Error) -> ProviderErrorMetadata {
    match error {
        genai::webc::Error::ResponseFailedStatus {
            status, headers, ..
        } => ProviderErrorMetadata {
            http_status: Some(status.as_u16()),
            retry_after: retry_after_from_headers(headers),
            request_id: request_id_from_headers(headers),
            ..ProviderErrorMetadata::default()
        },
        genai::webc::Error::Reqwest(error) => ProviderErrorMetadata {
            http_status: error.status().map(|status| status.as_u16()),
            ..ProviderErrorMetadata::default()
        },
        _ => ProviderErrorMetadata::default(),
    }
}

fn native_protocol(protocol: ProviderProtocol) -> &'static str {
    match protocol {
        ProviderProtocol::Anthropic => "anthropic_messages",
        ProviderProtocol::Gemini => "gemini_generate_content",
        ProviderProtocol::VertexAi => "vertex_ai_generate_content",
        ProviderProtocol::Genai => "rust_genai_router",
        ProviderProtocol::Mock => "in_memory",
        ProviderProtocol::OpenAiCompatible => "openai_chat_completions",
        ProviderProtocol::OpenAiResponses => "openai_responses_sse",
    }
}

fn golden_fixture_refs(protocol: ProviderProtocol) -> Vec<String> {
    let protocol = protocol.id();
    [
        "request",
        "text_response",
        "tool_response",
        "error_response",
    ]
    .into_iter()
    .map(|fixture| format!("tests/fixtures/{protocol}/{fixture}.json"))
    .collect()
}

fn tool_description(tool_name: &str) -> &'static str {
    crate::provider_tool_description(tool_name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_debug_never_contains_the_api_key() {
        let provider = GenaiProviderAdapter::from_config(GenaiProviderConfig {
            api_key: "secret-provider-key".to_owned(),
            api_key_env: crate::GOLUTRA_PROVIDER_API_KEY.to_owned(),
            base_url: "https://api.anthropic.com/v1".to_owned(),
            model_id: "claude-test".to_owned(),
            protocol: ProviderProtocol::Anthropic,
            generation_config: ProviderGenerationConfig::default(),
            custom_headers: ProviderHttpHeaders::default(),
        });

        let debug = format!("{provider:?}");

        assert!(!debug.contains("secret-provider-key"));
        assert!(debug.contains(crate::GOLUTRA_PROVIDER_API_KEY));
    }

    #[test]
    fn request_output_limit_overrides_generation_config() {
        let options = genai_chat_options(
            &ProviderGenerationConfig {
                enable_thinking: true,
                reasoning_effort: Some(super::super::ProviderReasoningEffort::Xhigh),
                context_window_size: Some(128_000),
                max_tokens: Some(4_096),
            },
            Some(1_024),
            false,
            None,
            PromptCachePolicy::Auto,
        )
        .expect("generation options");

        assert_eq!(options.max_tokens, Some(1_024));
        assert!(matches!(
            options.reasoning_effort,
            Some(ReasoningEffort::XHigh)
        ));
    }

    #[test]
    fn cache_policy_maps_to_stable_key_and_explicit_retention() {
        let identity = golutra_core::CacheIdentity {
            session_id: golutra_core::SessionId::new(),
            thread_id: None,
            provider_id: "provider".to_owned(),
            model_id: "model".to_owned(),
            key: "sha256:test-key".to_owned(),
        };
        let short = genai_chat_options(
            &ProviderGenerationConfig::default(),
            None,
            false,
            Some(&identity),
            PromptCachePolicy::Short,
        )
        .expect("short cache options");
        assert_eq!(short.prompt_cache_key.as_deref(), Some("sha256:test-key"));
        assert_eq!(short.cache_control, Some(CacheControl::Ephemeral5m));

        let long = genai_chat_options(
            &ProviderGenerationConfig::default(),
            None,
            false,
            Some(&identity),
            PromptCachePolicy::Long,
        )
        .expect("long cache options");
        assert_eq!(long.cache_control, Some(CacheControl::Ephemeral24h));

        let none = genai_chat_options(
            &ProviderGenerationConfig::default(),
            None,
            false,
            None,
            PromptCachePolicy::None,
        )
        .expect("none cache options");
        assert!(none.prompt_cache_key.is_none());
        assert!(none.cache_control.is_none());
    }

    #[test]
    fn responses_web_stream_preserves_http_status_from_webc_error() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            "retry-after",
            reqwest::header::HeaderValue::from_static("2"),
        );
        headers.insert(
            "x-request-id",
            reqwest::header::HeaderValue::from_static("req-stream-502"),
        );
        let webc_error = genai::webc::Error::ResponseFailedStatus {
            status: reqwest::StatusCode::BAD_GATEWAY,
            body: "gateway unavailable".to_owned(),
            headers: Box::new(headers),
        };
        let error = genai::Error::WebStream {
            model_iden: genai::ModelIden::new(AdapterKind::OpenAIResp, "gpt-test"),
            cause: webc_error.to_string(),
            error: Box::new(webc_error),
        };

        assert_eq!(genai_error_http_status(&error), Some(502));
        let mapped = map_genai_error(error);
        assert_eq!(
            mapped.retry_after(),
            Some(std::time::Duration::from_secs(2))
        );
        assert_eq!(
            mapped
                .metadata()
                .and_then(|metadata| metadata.request_id.as_deref()),
            Some("req-stream-502")
        );
        assert!(matches!(
            mapped,
            ProviderError::WithMetadata {
                error,
                metadata: ProviderErrorMetadata {
                    http_status: Some(502),
                    ..
                }
            } if matches!(*error, ProviderError::Unavailable { .. })
        ));
    }

    #[test]
    fn genai_usage_keeps_omitted_cache_write_unknown() {
        let usage = genai::chat::Usage {
            prompt_tokens: Some(100),
            prompt_tokens_details: Some(genai::chat::PromptTokensDetails {
                cache_creation_tokens: None,
                cache_creation_details: None,
                cached_tokens: Some(64),
                audio_tokens: None,
            }),
            completion_tokens: Some(5),
            completion_tokens_details: None,
            total_tokens: Some(105),
        };

        let projected = provider_usage_from_genai(&usage).expect("genai usage");
        let normalized = projected.normalize();
        assert_eq!(normalized.input_tokens_non_cached, Some(36));
        assert_eq!(normalized.cache_read_tokens, Some(64));
        assert_eq!(normalized.cache_write_tokens, None);
    }

    #[test]
    fn genai_usage_prefers_raw_response_breakdown() {
        let usage = genai::chat::Usage {
            prompt_tokens: Some(100),
            prompt_tokens_details: None,
            completion_tokens: Some(5),
            completion_tokens_details: None,
            total_tokens: Some(105),
        };
        let raw = json!({
            "input_tokens": 100,
            "input_tokens_details": {"cached_tokens": 64, "cache_write_tokens": 0},
            "output_tokens": 5,
            "total_tokens": 105
        });

        let projected =
            provider_usage_from_genai_with_raw(&usage, Some(&raw)).expect("genai raw usage");
        assert_eq!(projected.cached_input_tokens, Some(64));
        let normalized = projected.normalize();
        assert_eq!(normalized.cache_read_tokens, Some(64));
        assert_eq!(normalized.cache_write_tokens, Some(0));
        assert_eq!(normalized.input_tokens_non_cached, Some(36));
    }

    #[test]
    fn genai_request_uses_compact_schema_for_strict_responses_tools() {
        let request = ProviderRequest {
            request_id: ProviderRequestId::new(),
            task_id: TaskId::new(),
            turn_id: TurnId::new(),
            session_id: None,
            provider_id: "openai-responses".to_owned(),
            model_id: "gpt-test".to_owned(),
            messages: vec![ProviderMessage {
                role: ProviderRole::User,
                content: "use the tool".to_owned(),
                tool_call_id: None,
                tool_name: None,
                tool_calls: Vec::new(),
                metadata: Default::default(),
            }],
            tools: vec![golutra_core::ToolContract {
                tool_name: "read_file".to_owned(),
                input_schema: json!({
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "path": {
                            "type": "string",
                            "maxLength": 128,
                            "description": "workspace-relative path"
                        }
                    },
                    "required": ["path"]
                }),
                output_schema: json!({}),
                error_schema: json!({}),
                side_effect_type: golutra_core::SideEffectType::None,
                idempotency_key_policy: "none".to_owned(),
                timeout_policy: "bounded".to_owned(),
                cancellation_policy: "supported".to_owned(),
                retry_policy: "none".to_owned(),
                artifact_policy: "none".to_owned(),
                permission_policy_ref: None,
            }],
            cache_policy: PromptCachePolicy::None,
            max_output_tokens: None,
        };

        let chat_request =
            genai_chat_request(&request, ProviderProtocol::OpenAiResponses).expect("genai request");
        let tool = &chat_request.tools.expect("tool list")[0];
        assert_eq!(tool.strict, Some(true));
        assert_eq!(
            tool.schema.as_ref().expect("schema")["additionalProperties"],
            false
        );
        assert!(
            tool.schema.as_ref().expect("schema")["properties"]["path"]
                .get("maxLength")
                .is_none()
        );
        assert_eq!(
            tool.schema.as_ref().expect("schema")["properties"]["path"]["description"],
            "workspace-relative path"
        );
    }

    #[test]
    fn genai_request_disables_strict_for_optional_responses_tool_fields() {
        let request = ProviderRequest {
            request_id: ProviderRequestId::new(),
            task_id: TaskId::new(),
            turn_id: TurnId::new(),
            session_id: None,
            provider_id: "openai-responses".to_owned(),
            model_id: "gpt-test".to_owned(),
            messages: vec![ProviderMessage {
                role: ProviderRole::User,
                content: "run the command".to_owned(),
                tool_call_id: None,
                tool_name: None,
                tool_calls: Vec::new(),
                metadata: Default::default(),
            }],
            tools: vec![golutra_core::ToolContract {
                tool_name: "shell".to_owned(),
                input_schema: json!({
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "command": {"type": "string"},
                        "background": {"type": "boolean"}
                    },
                    "required": ["command"]
                }),
                output_schema: json!({}),
                error_schema: json!({}),
                side_effect_type: golutra_core::SideEffectType::None,
                idempotency_key_policy: "none".to_owned(),
                timeout_policy: "bounded".to_owned(),
                cancellation_policy: "supported".to_owned(),
                retry_policy: "none".to_owned(),
                artifact_policy: "none".to_owned(),
                permission_policy_ref: None,
            }],
            cache_policy: PromptCachePolicy::None,
            max_output_tokens: None,
        };

        let chat_request =
            genai_chat_request(&request, ProviderProtocol::OpenAiResponses).expect("genai request");
        let tool = &chat_request.tools.expect("tool list")[0];
        assert_eq!(tool.strict, Some(false));
    }

    #[test]
    fn web_search_is_sent_as_a_custom_function_and_restored_on_return() {
        assert_eq!(wire_tool_name("web_search"), "golutra_web_search");
        assert_eq!(restore_wire_tool_name("golutra_web_search"), "web_search");

        let request = ProviderRequest {
            request_id: ProviderRequestId::new(),
            task_id: TaskId::new(),
            turn_id: TurnId::new(),
            session_id: None,
            provider_id: "openai-responses".to_owned(),
            model_id: "gpt-test".to_owned(),
            messages: vec![ProviderMessage {
                role: ProviderRole::User,
                content: "search".to_owned(),
                tool_call_id: None,
                tool_name: None,
                tool_calls: Vec::new(),
                metadata: Default::default(),
            }],
            tools: vec![golutra_core::ToolContract {
                tool_name: "web_search".to_owned(),
                input_schema: json!({
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {"query": {"type": "string"}},
                    "required": ["query"]
                }),
                output_schema: json!({}),
                error_schema: json!({}),
                side_effect_type: golutra_core::SideEffectType::None,
                idempotency_key_policy: "none".to_owned(),
                timeout_policy: "bounded".to_owned(),
                cancellation_policy: "supported".to_owned(),
                retry_policy: "none".to_owned(),
                artifact_policy: "none".to_owned(),
                permission_policy_ref: None,
            }],
            cache_policy: PromptCachePolicy::None,
            max_output_tokens: None,
        };

        let chat_request =
            genai_chat_request(&request, ProviderProtocol::OpenAiResponses).expect("request");
        let tool = &chat_request.tools.expect("tool list")[0];
        assert_eq!(tool.name.as_ref(), "golutra_web_search");
        assert!(tool.schema.is_some(), "custom function must retain schema");
    }
}
