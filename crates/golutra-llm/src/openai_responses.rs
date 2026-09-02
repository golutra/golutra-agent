use std::{collections::HashMap, fmt, sync::Arc};

use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use futures_util::StreamExt;
use genai::{
    Client, Headers, ModelIden, ServiceTarget, WebConfig,
    adapter::AdapterKind,
    chat::{ChatOptions, ChatStreamEvent, StreamEnd, ToolCall, ToolChoice},
    resolver::{AuthData, Endpoint},
};
use golutra_auth::{CredentialProvider, FixedCredentialProvider};
use golutra_core::{PromptCachePolicy, ProviderContract};
use reqwest::header::{ACCEPT, HeaderMap, HeaderValue, USER_AGENT};
use secrecy::ExposeSecret;
use serde_json::{Value, json};

use super::genai_adapter::{
    genai_chat_options, genai_chat_request, genai_error_http_status, genai_error_metadata,
    genai_stream_error_requires_auth_refresh, map_genai_error, provider_response_from_genai_stream,
    restore_wire_tool_name,
};
use super::{
    GOLUTRA_PROVIDER_AUTH_PROVIDER, GOLUTRA_PROVIDER_ROUTE_ID, LlmProvider,
    MAX_PROVIDER_MESSAGE_BYTES, MAX_PROVIDER_RESPONSE_BYTES, MAX_PROVIDER_TOOL_ARGUMENT_BYTES,
    MAX_PROVIDER_TOOL_CALL_ID_BYTES, MAX_PROVIDER_TOOL_NAME_BYTES, ProviderCacheProfile,
    ProviderError, ProviderErrorMetadata, ProviderFinishReason, ProviderGenerationConfig,
    ProviderHttpHeaders, ProviderMessage, ProviderMessageMetadata, ProviderProbeResult,
    ProviderProtocol, ProviderRequest, ProviderResponse, ProviderRole, ProviderStreamEvent,
    ProviderUsage, RESERVED_AFFINITY_HEADERS, configured_or_first_env, custom_headers_from_reader,
    env_mapping, first_env, generation_config_from_reader, missing_env_error,
    protocol_capabilities, provider_credential_error, provider_http_client,
    provider_http_error_with_headers, provider_transport_error, response_json_or_error,
    sanitize_provider_error, validate_native_base_url,
};

const CHATGPT_ACCOUNT_ID_HEADER: &str = "ChatGPT-Account-Id";
const DEFAULT_PROVIDER_ID: &str = "openai-chatgpt";

struct StreamedToolCall {
    index: usize,
    argument_bytes: usize,
    announced: bool,
}

#[derive(Clone, PartialEq, Eq)]
pub struct OpenAiResponsesProviderConfig {
    pub api_key: String,
    pub api_key_env: String,
    pub provider_id: String,
    pub base_url: String,
    pub model_id: String,
    pub generation_config: ProviderGenerationConfig,
    pub custom_headers: ProviderHttpHeaders,
}

#[derive(Clone)]
pub struct OpenAiResponsesProvider {
    credential: Arc<dyn CredentialProvider>,
    config: OpenAiResponsesProviderConfig,
    cache_profile: ProviderCacheProfile,
    client: Client,
    probe_client: reqwest::Client,
}

impl fmt::Debug for OpenAiResponsesProviderConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpenAiResponsesProviderConfig")
            .field("api_key_env", &self.api_key_env)
            .field("provider_id", &self.provider_id)
            .field("base_url", &self.base_url)
            .field("model_id", &self.model_id)
            .field("generation_config", &self.generation_config)
            .field("custom_header_names", &self.custom_headers.names())
            .finish_non_exhaustive()
    }
}

impl fmt::Debug for OpenAiResponsesProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpenAiResponsesProvider")
            .field("api_key_env", &self.config.api_key_env)
            .field("provider_id", &self.config.provider_id)
            .field("base_url", &self.config.base_url)
            .field("model_id", &self.config.model_id)
            .field("generation_config", &self.config.generation_config)
            .field("custom_header_names", &self.config.custom_headers.names())
            .finish_non_exhaustive()
    }
}

impl OpenAiResponsesProvider {
    #[must_use]
    pub fn from_config(config: OpenAiResponsesProviderConfig) -> Self {
        let credential = Arc::new(FixedCredentialProvider::new(
            config.api_key.clone(),
            config.api_key_env.clone(),
        ));
        Self::from_config_with_credential(config, credential)
    }

    #[must_use]
    pub fn from_config_with_credential(
        config: OpenAiResponsesProviderConfig,
        credential: Arc<dyn CredentialProvider>,
    ) -> Self {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let web_config = WebConfig::default()
            .with_connect_timeout(std::time::Duration::from_secs(10))
            .with_timeout(std::time::Duration::from_secs(3_600));
        let cache_profile = ProviderCacheProfile::for_provider(
            ProviderProtocol::OpenAiResponses,
            &config.provider_id,
        );
        Self {
            credential,
            config,
            cache_profile,
            client: Client::builder().with_web_config(web_config).build(),
            probe_client: provider_http_client(),
        }
    }

    pub fn config_from_env_reader<F>(
        reader: F,
    ) -> Result<OpenAiResponsesProviderConfig, ProviderError>
    where
        F: Fn(&str) -> Option<String>,
    {
        let mapping = env_mapping(ProviderProtocol::OpenAiResponses);
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
        let provider_id = reader(GOLUTRA_PROVIDER_AUTH_PROVIDER)
            .filter(|value| !value.trim().is_empty())
            .or_else(|| reader(GOLUTRA_PROVIDER_ROUTE_ID).filter(|value| !value.trim().is_empty()))
            .unwrap_or_else(|| DEFAULT_PROVIDER_ID.to_owned());
        Ok(OpenAiResponsesProviderConfig {
            api_key,
            api_key_env,
            provider_id,
            base_url,
            model_id,
            generation_config,
            custom_headers: custom_headers_from_reader(&reader)?,
        })
    }

    pub async fn probe(&self) -> Result<ProviderProbeResult, ProviderError> {
        let mut response = self.send_probe(false).await?;
        if response.status().as_u16() == 401 {
            response = self.send_probe(true).await?;
        }
        let status = response.status();
        let headers = response.headers().clone();
        let value = response_json_or_error(response).await?;
        if !status.is_success() {
            return Err(provider_http_error_with_headers(status, &headers, &value));
        }
        let discovered_models = value
            .get("models")
            .or_else(|| value.get("data"))
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|model| {
                model
                    .get("slug")
                    .or_else(|| model.get("id"))
                    .and_then(Value::as_str)
            })
            .map(ToOwned::to_owned)
            .collect::<Vec<_>>();
        let model_available = if discovered_models.is_empty() {
            None
        } else {
            Some(
                discovered_models
                    .iter()
                    .any(|model| model == &self.config.model_id),
            )
        };
        Ok(ProviderProbeResult {
            provider_id: self.config.provider_id.clone(),
            protocol: "openai_responses_sse".to_owned(),
            base_url: self.config.base_url.clone(),
            model_id: self.config.model_id.clone(),
            model_available,
            discovered_models,
            capabilities: protocol_capabilities(ProviderProtocol::OpenAiResponses),
        })
    }

    async fn send_probe(&self, force_refresh: bool) -> Result<reqwest::Response, ProviderError> {
        let (token, account_id) = self.resolve_credential(force_refresh).await?;
        self.authenticated_probe_request(
            self.probe_client.get(format!(
                "{}/models?client_version={}",
                self.config.base_url.trim_end_matches('/'),
                env!("CARGO_PKG_VERSION")
            )),
            token.expose_secret(),
            account_id.as_deref(),
        )
        .send()
        .await
        .map_err(provider_transport_error)
    }

    fn authenticated_probe_request(
        &self,
        builder: reqwest::RequestBuilder,
        access_token: &str,
        account_id: Option<&str>,
    ) -> reqwest::RequestBuilder {
        let mut headers = HeaderMap::new();
        headers.insert(
            USER_AGENT,
            HeaderValue::from_str(&format!("golutra/{}", env!("CARGO_PKG_VERSION")))
                .unwrap_or_else(|_| HeaderValue::from_static("golutra")),
        );
        headers.insert("originator", HeaderValue::from_static("golutra"));
        if let Some(account_id) = account_id
            && let Ok(value) = HeaderValue::from_str(account_id)
        {
            headers.insert(CHATGPT_ACCOUNT_ID_HEADER, value);
        }
        headers.extend(self.config.custom_headers.to_header_map());
        // affinity 是 provider 能力，不允许自定义 header 绕过 profile gate。
        // 探测请求也必须遵守同一边界，避免把会话路由状态带到能力发现端点。
        for header in RESERVED_AFFINITY_HEADERS {
            headers.remove(*header);
        }
        builder.bearer_auth(access_token).headers(headers)
    }

    async fn resolve_credential(
        &self,
        force_refresh: bool,
    ) -> Result<(secrecy::SecretString, Option<String>), ProviderError> {
        let token = self
            .credential
            .credential(force_refresh)
            .await
            .map_err(provider_credential_error)?;
        let account_id = match chatgpt_account_id(token.expose_secret()) {
            Some(account_id) => Some(account_id),
            None => {
                self.credential
                    .metadata()
                    .await
                    .map_err(provider_credential_error)?
                    .account_id
            }
        };
        Ok((token, account_id))
    }

    fn service_target(&self, api_key: &str) -> ServiceTarget {
        ServiceTarget {
            endpoint: Endpoint::from_owned(format!(
                "{}/",
                self.config.base_url.trim_end_matches('/')
            )),
            auth: AuthData::from_single(api_key.to_owned()),
            model: ModelIden::new(AdapterKind::OpenAIResp, self.config.model_id.clone()),
        }
    }

    fn chat_options(
        &self,
        request: &ProviderRequest,
        account_id: Option<&str>,
    ) -> Result<ChatOptions, ProviderError> {
        let mut options = genai_chat_options(
            &self.config.generation_config,
            request.max_output_tokens,
            true,
            self.cache_identity_for_request(request).as_ref(),
            request.cache_policy,
            self.cache_profile,
        )?
        .with_extra_headers(self.request_headers(request, account_id));
        let mut reasoning = json!({"summary": "auto"});
        if let Some(effort) = options
            .reasoning_effort
            .as_ref()
            .and_then(|effort| effort.as_keyword())
        {
            reasoning["effort"] = Value::String(effort.to_owned());
        }
        let mut extra_body = json!({"reasoning": reasoning});
        if !request.tools.is_empty() {
            options = options.with_tool_choice(ToolChoice::Auto);
            extra_body["parallel_tool_calls"] = Value::Bool(true);
        }
        Ok(options.with_extra_body(extra_body))
    }

    fn request_headers(&self, request: &ProviderRequest, account_id: Option<&str>) -> Headers {
        let mut headers = HeaderMap::new();
        headers.insert(ACCEPT, HeaderValue::from_static("text/event-stream"));
        headers.insert(
            USER_AGENT,
            HeaderValue::from_str(&format!("golutra/{}", env!("CARGO_PKG_VERSION")))
                .unwrap_or_else(|_| HeaderValue::from_static("golutra")),
        );
        headers.insert("originator", HeaderValue::from_static("golutra"));
        headers.extend(self.config.custom_headers.to_header_map());
        // affinity 是 provider 能力，不允许自定义 header 绕过 profile gate。
        for header in RESERVED_AFFINITY_HEADERS {
            headers.remove(*header);
        }
        if let Ok(value) = HeaderValue::from_str(&request.affinity_id()) {
            for header in self.cache_profile.affinity_headers(request.cache_policy) {
                if let Ok(name) = reqwest::header::HeaderName::from_bytes(header.as_bytes()) {
                    headers.insert(name, value.clone());
                }
            }
        }
        if let Some(account_id) = account_id
            && let Ok(value) = HeaderValue::from_str(account_id)
        {
            headers.insert(CHATGPT_ACCOUNT_ID_HEADER, value);
        }
        Headers::from(
            headers
                .iter()
                .filter_map(|(name, value)| {
                    value
                        .to_str()
                        .ok()
                        .map(|value| (name.as_str().to_owned(), value.to_owned()))
                })
                .collect::<Vec<_>>(),
        )
    }

    async fn execute_stream(
        &self,
        request: &ProviderRequest,
        on_event: &mut (dyn FnMut(ProviderStreamEvent) + Send),
    ) -> Result<ProviderResponse, ProviderError> {
        // The request shape and cache options do not depend on credentials.
        // Reuse them across a 401 refresh so retries spend their time on the
        // network instead of rebuilding the full message/tool projection.
        let chat_request = genai_chat_request(request, ProviderProtocol::OpenAiResponses)?;
        let mut force_refresh = false;
        loop {
            let (token, account_id) = self.resolve_credential(force_refresh).await?;
            let options = self.chat_options(request, account_id.as_deref())?;
            let response = match self
                .client
                .exec_chat_stream(
                    self.service_target(token.expose_secret()),
                    chat_request.clone(),
                    Some(&options),
                )
                .await
            {
                Ok(response) => response,
                Err(error)
                    if !force_refresh && genai_stream_error_requires_auth_refresh(&error) =>
                {
                    force_refresh = true;
                    continue;
                }
                Err(error) => return Err(map_responses_genai_error(error)),
            };

            let model_id = response.model_iden.to_string();
            let mut stream = response.stream;
            let mut stream_end = None;
            let mut retry_with_refresh = false;
            let mut business_event_seen = false;
            let mut captured_response_bytes = 0_usize;
            let mut captured_text_bytes = 0_usize;
            let mut tool_calls = HashMap::<String, StreamedToolCall>::new();

            while let Some(event) = stream.next().await {
                let event = match event {
                    Ok(event) => event,
                    Err(error)
                        if !force_refresh
                            && !business_event_seen
                            && genai_stream_error_requires_auth_refresh(&error) =>
                    {
                        retry_with_refresh = true;
                        break;
                    }
                    Err(error) => return Err(map_responses_genai_error(error)),
                };
                match event {
                    ChatStreamEvent::Start => {}
                    ChatStreamEvent::ThoughtSignatureChunk(chunk) => {
                        add_stream_bytes(&mut captured_response_bytes, chunk.content.len())?;
                    }
                    ChatStreamEvent::Chunk(chunk) => {
                        if !chunk.content.is_empty() {
                            add_stream_bytes(&mut captured_response_bytes, chunk.content.len())?;
                            add_message_bytes(&mut captured_text_bytes, chunk.content.len())?;
                            business_event_seen = true;
                            on_event(ProviderStreamEvent::TextDelta {
                                text: chunk.content,
                            });
                        }
                    }
                    ChatStreamEvent::ReasoningChunk(chunk) => {
                        if !chunk.content.is_empty() {
                            add_stream_bytes(&mut captured_response_bytes, chunk.content.len())?;
                            business_event_seen = true;
                            on_event(ProviderStreamEvent::ReasoningDelta {
                                text: chunk.content,
                            });
                        }
                    }
                    ChatStreamEvent::ToolCallChunk(chunk) => {
                        let call = &chunk.tool_call;
                        let argument_bytes = validate_streamed_tool_call(call)?;
                        business_event_seen = true;
                        let next_index = tool_calls.len();
                        let state =
                            tool_calls
                                .entry(call.call_id.clone())
                                .or_insert(StreamedToolCall {
                                    index: next_index,
                                    argument_bytes: 0,
                                    announced: false,
                                });
                        if argument_bytes > state.argument_bytes {
                            add_stream_bytes(
                                &mut captured_response_bytes,
                                argument_bytes - state.argument_bytes,
                            )?;
                            state.argument_bytes = argument_bytes;
                        }
                        if !state.announced
                            && (!call.call_id.is_empty() || !call.fn_name.is_empty())
                        {
                            on_event(ProviderStreamEvent::ToolCallDelta {
                                index: state.index,
                                tool_call_id: (!call.call_id.is_empty())
                                    .then(|| call.call_id.clone()),
                                tool_name: (!call.fn_name.is_empty())
                                    .then(|| restore_wire_tool_name(&call.fn_name)),
                            });
                            state.announced = true;
                        }
                    }
                    ChatStreamEvent::End(end) => {
                        stream_end = Some(end);
                        break;
                    }
                }
            }

            if retry_with_refresh {
                force_refresh = true;
                continue;
            }
            let end = stream_end.ok_or_else(|| ProviderError::Unavailable {
                message: "responses stream ended before a terminal event".to_owned(),
            })?;
            return responses_provider_response(end, &model_id, &self.config.provider_id);
        }
    }
}

#[async_trait]
impl super::LlmProvider for OpenAiResponsesProvider {
    async fn complete(&self, request: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        let mut ignore = |_| {};
        self.execute_stream(&request, &mut ignore).await
    }

    async fn complete_stream(
        &self,
        request: ProviderRequest,
        on_event: &mut (dyn FnMut(ProviderStreamEvent) + Send),
    ) -> Result<ProviderResponse, ProviderError> {
        self.execute_stream(&request, on_event).await
    }

    fn supports_buffered_transport(&self) -> bool {
        false
    }

    fn cache_namespace(&self) -> String {
        super::route_cache_namespace("openai_responses_sse", &self.config.base_url)
    }

    fn preferred_cache_policy(&self) -> PromptCachePolicy {
        self.cache_profile.preferred_cache_policy()
    }

    fn contract(&self) -> ProviderContract {
        ProviderContract {
            provider_id: self.config.provider_id.clone(),
            model_id: self.config.model_id.clone(),
            native_protocol: "openai_responses_sse".to_owned(),
            stream_event_mapping: "rust_genai_responses_stream".to_owned(),
            tool_call_mapping: "rust_genai_responses_function_calls".to_owned(),
            usage_mapping: "rust_genai_responses_usage".to_owned(),
            reasoning_mapping: "rust_genai_responses_reasoning".to_owned(),
            finish_reason_mapping: "rust_genai_responses_stop_reason".to_owned(),
            error_mapping: "rust_genai_responses_errors".to_owned(),
            rate_limit_mapping: "http_429".to_owned(),
            cost_model: "subscription".to_owned(),
            capability_matrix_ref: None,
            golden_fixture_refs: vec![
                "tests/fixtures/openai-responses/request.json".to_owned(),
                "tests/fixtures/openai-responses/tool-response.sse".to_owned(),
                "tests/fixtures/openai-responses/text-response.sse".to_owned(),
            ],
        }
    }
}

fn add_stream_bytes(total: &mut usize, bytes: usize) -> Result<(), ProviderError> {
    *total = total.saturating_add(bytes);
    if *total > MAX_PROVIDER_RESPONSE_BYTES {
        return Err(ProviderError::Malformed {
            message: format!("provider response exceeds {MAX_PROVIDER_RESPONSE_BYTES} byte limit"),
        });
    }
    Ok(())
}

fn add_message_bytes(total: &mut usize, bytes: usize) -> Result<(), ProviderError> {
    *total = total.saturating_add(bytes);
    if *total > MAX_PROVIDER_MESSAGE_BYTES {
        return Err(ProviderError::Malformed {
            message: format!("assistant message exceeds {MAX_PROVIDER_MESSAGE_BYTES} byte limit"),
        });
    }
    Ok(())
}

fn validate_streamed_tool_call(call: &ToolCall) -> Result<usize, ProviderError> {
    if call.call_id.len() > MAX_PROVIDER_TOOL_CALL_ID_BYTES {
        return Err(ProviderError::Malformed {
            message: format!("tool call id exceeds {MAX_PROVIDER_TOOL_CALL_ID_BYTES} byte limit"),
        });
    }
    if call.fn_name.len() > MAX_PROVIDER_TOOL_NAME_BYTES {
        return Err(ProviderError::Malformed {
            message: format!("tool name exceeds {MAX_PROVIDER_TOOL_NAME_BYTES} byte limit"),
        });
    }
    let argument_bytes = match call.fn_arguments.as_str() {
        Some(arguments) => arguments.len(),
        None => serde_json::to_vec(&call.fn_arguments)
            .map_err(|error| ProviderError::Malformed {
                message: format!("tool call arguments could not be serialized: {error}"),
            })?
            .len(),
    };
    if argument_bytes > MAX_PROVIDER_TOOL_ARGUMENT_BYTES {
        return Err(ProviderError::Malformed {
            message: format!(
                "tool call arguments exceed {MAX_PROVIDER_TOOL_ARGUMENT_BYTES} byte limit"
            ),
        });
    }
    Ok(argument_bytes)
}

fn responses_provider_response(
    end: StreamEnd,
    model_id: &str,
    provider_id: &str,
) -> Result<ProviderResponse, ProviderError> {
    let response_id = end
        .captured_response_id
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| ProviderError::Unavailable {
            message: "responses stream ended before response.completed".to_owned(),
        })?
        .to_owned();
    if response_id.len() > MAX_PROVIDER_TOOL_CALL_ID_BYTES {
        return Err(ProviderError::Malformed {
            message: format!(
                "responses response id exceeds {MAX_PROVIDER_TOOL_CALL_ID_BYTES} byte limit"
            ),
        });
    }
    let replay_items = end
        .captured_content
        .as_ref()
        .map(|content| content.thought_signatures())
        .unwrap_or_default()
        .into_iter()
        .map(|encrypted_content| {
            if encrypted_content.is_empty() || encrypted_content.len() > MAX_PROVIDER_MESSAGE_BYTES {
                return Err(ProviderError::Malformed {
                    message: format!(
                        "Responses encrypted reasoning exceeds {MAX_PROVIDER_MESSAGE_BYTES} byte limit"
                    ),
                });
            }
            Ok(json!({
                "type": "reasoning",
                "encrypted_content": encrypted_content,
                "summary": [],
            }))
        })
        .collect::<Result<Vec<_>, ProviderError>>()?;
    let stop_reason = end.captured_stop_reason.as_ref().map(ToString::to_string);
    let mut response = provider_response_from_genai_stream(end, model_id)?;
    apply_responses_cache_semantics(&mut response.usage);
    if !replay_items.is_empty() {
        let message = response.message.get_or_insert_with(|| ProviderMessage {
            role: ProviderRole::Assistant,
            content: String::new(),
            tool_call_id: None,
            tool_name: None,
            tool_calls: Vec::new(),
            metadata: ProviderMessageMetadata::default(),
        });
        message.metadata.openai_responses_replay_items = replay_items;
    }
    if !response.tool_calls.is_empty() {
        response.finish_reason = ProviderFinishReason::ToolCalls;
    }
    response.raw_metadata = json!({
        "provider": provider_id,
        "provider_model": model_id,
        "response_id": response_id,
        "stop_reason": stop_reason,
        "streamed": true,
        "usage": response.usage.raw,
    });
    Ok(response)
}

fn apply_responses_cache_semantics(usage: &mut ProviderUsage) {
    if usage.usage_source != golutra_core::UsageSource::Provider || usage.input_tokens.is_none() {
        return;
    }
    // Responses 在 input token details 中定义缓存读取，但没有独立的缓存写入计费项。
    // rust-genai 会把 wire 零值折叠为 None，因此必须在协议边界恢复真实零值。
    if usage.cached_input_tokens.is_none() {
        usage.cached_input_tokens = Some(0);
    }
    if !usage.raw.is_object() {
        usage.raw = json!({});
    }
    let object = usage.raw.as_object_mut().expect("usage raw is an object");
    let details = object
        .entry("input_tokens_details".to_owned())
        .or_insert_with(|| json!({}));
    if !details.is_object() {
        *details = json!({});
    }
    let details = details
        .as_object_mut()
        .expect("usage details are an object");
    details
        .entry("cached_tokens".to_owned())
        .or_insert_with(|| Value::from(usage.cached_input_tokens.unwrap_or_default()));
    details
        .entry("cache_write_tokens".to_owned())
        .or_insert_with(|| Value::from(0_u64));
}

fn map_responses_genai_error(error: genai::Error) -> ProviderError {
    let message = sanitize_provider_error(&error.to_string());
    let status = genai_error_http_status(&error);
    let metadata = genai_error_metadata(&error);
    let mapped = match status {
        Some(429) => ProviderError::RateLimited { message },
        Some(status) if (500..600).contains(&status) => ProviderError::Unavailable { message },
        Some(_) => ProviderError::Failed { message },
        // Responses maps `response.failed` to StreamParse. Treat all such
        // parser/business failures as hard errors; transport truncation is
        // represented separately as WebStream and remains retryable.
        None if matches!(error, genai::Error::StreamParse { .. }) => {
            ProviderError::Failed { message }
        }
        None => map_genai_error(error),
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

fn chatgpt_account_id(access_token: &str) -> Option<String> {
    let payload = access_token.split('.').nth(1)?;
    let decoded = URL_SAFE_NO_PAD.decode(payload).ok()?;
    let claims: Value = serde_json::from_slice(&decoded).ok()?;
    claims
        .get("chatgpt_account_id")
        .and_then(Value::as_str)
        .or_else(|| {
            claims
                .get("https://api.openai.com/auth")
                .and_then(|auth| auth.get("chatgpt_account_id"))
                .and_then(Value::as_str)
        })
        .or_else(|| {
            claims
                .get("organizations")
                .and_then(Value::as_array)
                .and_then(|organizations| organizations.first())
                .and_then(|organization| organization.get("id"))
                .and_then(Value::as_str)
        })
        .filter(|value| !value.trim().is_empty() && value.len() <= 512)
        .map(ToOwned::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_failed_stream_event_is_not_replayed() {
        let parser_error =
            serde_json::from_str::<Value>("response.failed").expect_err("invalid JSON");
        let error = genai::Error::StreamParse {
            model_iden: ModelIden::new(AdapterKind::OpenAIResp, "gpt-test"),
            serde_error: parser_error,
        };

        assert!(matches!(
            map_responses_genai_error(error),
            ProviderError::Failed { .. }
        ));
    }

    #[test]
    fn config_debug_never_contains_the_api_key() {
        let provider = OpenAiResponsesProvider::from_config(OpenAiResponsesProviderConfig {
            api_key: "secret-responses-key".to_owned(),
            api_key_env: "TEST_RESPONSES_KEY".to_owned(),
            provider_id: "openai-chatgpt".to_owned(),
            base_url: "https://api.openai.com/v1".to_owned(),
            model_id: "gpt-test".to_owned(),
            generation_config: ProviderGenerationConfig::default(),
            custom_headers: ProviderHttpHeaders::default(),
        });

        let debug = format!("{provider:?}");
        assert!(!debug.contains("secret-responses-key"));
        assert!(debug.contains("TEST_RESPONSES_KEY"));
    }

    #[test]
    fn account_id_is_read_from_nested_claim() {
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"none"}"#);
        let payload = URL_SAFE_NO_PAD
            .encode(br#"{"https://api.openai.com/auth":{"chatgpt_account_id":"acct-test"}}"#);
        let token = format!("{header}.{payload}.signature");

        assert_eq!(chatgpt_account_id(&token).as_deref(), Some("acct-test"));
    }

    #[test]
    fn responses_usage_restores_cold_cache_zeroes() {
        let mut usage = ProviderUsage {
            input_tokens: Some(100),
            output_tokens: Some(5),
            reasoning_tokens: None,
            cached_input_tokens: None,
            total_tokens: Some(105),
            usage_source: golutra_core::UsageSource::Provider,
            raw: json!({"input_tokens": 100, "output_tokens": 5}),
        };

        apply_responses_cache_semantics(&mut usage);
        let normalized = usage.normalize();

        assert_eq!(normalized.cache_read_tokens, Some(0));
        assert_eq!(normalized.cache_write_tokens, Some(0));
        assert_eq!(normalized.input_tokens_non_cached, Some(100));
    }

    #[test]
    fn responses_usage_preserves_a_real_cache_hit() {
        let mut usage = ProviderUsage {
            input_tokens: Some(100),
            output_tokens: Some(5),
            reasoning_tokens: None,
            cached_input_tokens: Some(64),
            total_tokens: Some(105),
            usage_source: golutra_core::UsageSource::Provider,
            raw: json!({"input_tokens": 100, "output_tokens": 5}),
        };

        apply_responses_cache_semantics(&mut usage);
        let normalized = usage.normalize();

        assert_eq!(normalized.cache_read_tokens, Some(64));
        assert_eq!(normalized.cache_write_tokens, Some(0));
        assert_eq!(normalized.input_tokens_non_cached, Some(36));
    }

    #[test]
    fn responses_probe_strips_all_reserved_affinity_headers() {
        let custom_headers = ProviderHttpHeaders::from_resolved(
            [
                ("session-id".to_owned(), "custom-session".to_owned()),
                (
                    "session_id".to_owned(),
                    "custom-session-underscore".to_owned(),
                ),
                (
                    "x-client-request-id".to_owned(),
                    "custom-request".to_owned(),
                ),
                (
                    "x-session-affinity".to_owned(),
                    "custom-affinity".to_owned(),
                ),
                ("x-safe-header".to_owned(), "preserved".to_owned()),
            ]
            .into_iter()
            .collect(),
        )
        .expect("valid probe headers");
        let provider = OpenAiResponsesProvider::from_config(OpenAiResponsesProviderConfig {
            api_key: "probe-key".to_owned(),
            api_key_env: "PROBE_KEY".to_owned(),
            provider_id: "openai-chatgpt".to_owned(),
            base_url: "https://api.openai.com/v1".to_owned(),
            model_id: "gpt-test".to_owned(),
            generation_config: ProviderGenerationConfig::default(),
            custom_headers,
        });

        let request = provider
            .authenticated_probe_request(
                provider.probe_client.get("https://example.test/models"),
                "access-token",
                None,
            )
            .build()
            .expect("probe request builds");

        for header in RESERVED_AFFINITY_HEADERS {
            assert!(
                !request.headers().contains_key(*header),
                "probe carried reserved affinity header {header}"
            );
        }
        assert_eq!(
            request
                .headers()
                .get("x-safe-header")
                .and_then(|value| value.to_str().ok()),
            Some("preserved")
        );
    }
}
