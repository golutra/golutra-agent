use std::{fmt, sync::Arc};

use async_trait::async_trait;
use futures_util::StreamExt;
use genai::{
    Client, ModelIden, ServiceTarget, WebConfig,
    adapter::AdapterKind,
    chat::{
        ChatMessage, ChatOptions, ChatRequest, ChatStreamEvent, ContentPart, MessageContent,
        ReasoningEffort, StopReason, StreamEnd, Tool, ToolCall, ToolResponse,
    },
    resolver::{AuthData, Endpoint},
};
use golutra_auth::{CredentialProvider, FixedCredentialProvider};
use golutra_core::{ProviderContract, ProviderRequestId, ProviderResponseId, TaskId, TurnId};
use secrecy::ExposeSecret;
use serde_json::{Value, json};

use super::{
    ProviderError, ProviderFinishReason, ProviderGenerationConfig, ProviderHttpHeaders,
    ProviderMessage, ProviderProbeResult, ProviderProtocol, ProviderRequest, ProviderResponse,
    ProviderRole, ProviderStreamEvent, ProviderToolCall, ProviderUsage, UsageSource,
    configured_or_first_env, custom_headers_from_reader, env_mapping, first_env,
    generation_config_from_reader, missing_env_error, protocol_capabilities,
    sanitize_provider_error, selected_protocol_from_reader, validate_native_base_url,
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
        let api_key = self
            .credential
            .credential(force_refresh)
            .await
            .map_err(super::provider_credential_error)?;
        let chat_request = genai_chat_request(request, self.config.protocol)?;
        let options = genai_chat_options(&self.config.generation_config, false)?;
        let response = self
            .client
            .exec_chat(
                self.service_target(api_key.expose_secret())?,
                chat_request,
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
                let chat_request = genai_chat_request(request, self.config.protocol)?;
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
        let mut force_refresh = false;
        let response = loop {
            let api_key = self
                .credential
                .credential(force_refresh)
                .await
                .map_err(super::provider_credential_error)?;
            let chat_request = genai_chat_request(request, self.config.protocol)?;
            let options = genai_chat_options(&self.config.generation_config, true)?;
            match self
                .client
                .exec_chat_stream(
                    self.service_target(api_key.expose_secret())?,
                    chat_request,
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
                ChatStreamEvent::Heartbeat => on_event(ProviderStreamEvent::Heartbeat),
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
                            .then(|| chunk.tool_call.fn_name.clone()),
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
            let tool = Tool::new(contract.tool_name.clone())
                .with_description(tool_description(&contract.tool_name))
                .with_schema(contract.input_schema.clone());
            if is_openai_responses {
                tool.with_strict(true)
            } else {
                tool
            }
        }));
    }
    Ok(chat_request)
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
                    fn_name: tool_call.tool_name.clone(),
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
                response = response.with_fn_name(tool_name);
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
    streaming: bool,
) -> Result<ChatOptions, ProviderError> {
    let mut options = ChatOptions::default().with_capture_raw_body(true);
    if streaming {
        options = options
            .with_capture_usage(true)
            .with_capture_content(true)
            .with_capture_reasoning_content(true)
            .with_capture_tool_calls(true);
    }
    if let Some(max_tokens) = config.max_tokens {
        options = options.with_max_tokens(u32::try_from(max_tokens).map_err(|_| {
            ProviderError::NotConfigured {
                message: "provider max_tokens exceeds the supported u32 range".to_owned(),
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
                tool_name: call.fn_name.clone(),
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
                tool_name: call.fn_name.clone(),
                arguments: call.fn_arguments.clone(),
            })
        })
        .collect::<Result<Vec<_>, ProviderError>>()?;
    let usage = provider_usage_from_genai(&response.usage)?;
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
    let usage_raw = serde_json::to_value(usage).map_err(|error| ProviderError::Malformed {
        message: format!("genai usage could not be serialized: {error}"),
    })?;
    Ok(ProviderUsage {
        input_tokens: non_negative_u64(usage.prompt_tokens),
        output_tokens: non_negative_u64(usage.completion_tokens),
        reasoning_tokens: usage
            .completion_tokens_details
            .as_ref()
            .and_then(|details| non_negative_u64(details.reasoning_tokens)),
        cached_input_tokens: usage
            .prompt_tokens_details
            .as_ref()
            .and_then(|details| non_negative_u64(details.cached_tokens)),
        total_tokens: non_negative_u64(usage.total_tokens),
        usage_source: UsageSource::Provider,
        raw: usage_raw,
    })
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
    if genai_error_http_status(&error) == Some(429) {
        return ProviderError::RateLimited { message };
    }
    if genai_error_http_status(&error).is_some_and(|status| (500..600).contains(&status)) {
        return ProviderError::Unavailable { message };
    }
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
            .and_then(genai_error_http_status),
        genai::Error::WebAdapterCall { webc_error, .. }
        | genai::Error::WebModelCall { webc_error, .. } => match webc_error {
            genai::webc::Error::ResponseFailedStatus { status, .. } => Some(status.as_u16()),
            genai::webc::Error::Reqwest(error) => error.status().map(|status| status.as_u16()),
            _ => None,
        },
        _ => None,
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
    fn generation_config_maps_reasoning_effort_and_output_limit() {
        let options = genai_chat_options(
            &ProviderGenerationConfig {
                enable_thinking: true,
                reasoning_effort: Some(super::super::ProviderReasoningEffort::Xhigh),
                context_window_size: Some(128_000),
                max_tokens: Some(4_096),
            },
            false,
        )
        .expect("generation options");

        assert_eq!(options.max_tokens, Some(4_096));
        assert!(matches!(
            options.reasoning_effort,
            Some(ReasoningEffort::XHigh)
        ));
    }
}
