use std::{fmt, sync::Arc};

use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use eventsource_stream::Eventsource;
use futures_util::StreamExt;
use golutra_auth::{CredentialProvider, FixedCredentialProvider};
use golutra_core::{ProviderContract, ProviderResponseId, ToolContract};
use reqwest::header::{ACCEPT, HeaderMap, HeaderValue, USER_AGENT};
use secrecy::ExposeSecret;
use serde_json::{Value, json};

use super::{
    GOLUTRA_PROVIDER_AUTH_PROVIDER, MAX_PROVIDER_MESSAGE_BYTES, MAX_PROVIDER_RESPONSE_BYTES,
    MAX_PROVIDER_TOOL_ARGUMENT_BYTES, MAX_PROVIDER_TOOL_CALL_ID_BYTES,
    MAX_PROVIDER_TOOL_NAME_BYTES, ProviderError, ProviderFinishReason, ProviderGenerationConfig,
    ProviderHttpHeaders, ProviderMessage, ProviderMessageMetadata, ProviderProbeResult,
    ProviderProtocol, ProviderRequest, ProviderResponse, ProviderRole, ProviderStreamEvent,
    ProviderToolCall, ProviderUsage, UsageSource, configured_or_first_env,
    custom_headers_from_reader, env_mapping, first_env, generation_config_from_reader,
    missing_env_error, protocol_capabilities, provider_credential_error, provider_error_message,
    provider_http_client, provider_http_error, provider_transport_error, response_json_or_error,
    validate_native_base_url,
};

const CHATGPT_ACCOUNT_ID_HEADER: &str = "ChatGPT-Account-Id";
const DEFAULT_PROVIDER_ID: &str = "openai-chatgpt";

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
    client: reqwest::Client,
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
        Self {
            credential,
            config,
            client: provider_http_client(),
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
        let provider_id = reader(GOLUTRA_PROVIDER_AUTH_PROVIDER)
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_PROVIDER_ID.to_owned());
        Ok(OpenAiResponsesProviderConfig {
            api_key,
            api_key_env,
            provider_id,
            base_url,
            model_id,
            generation_config: generation_config_from_reader(&reader)?,
            custom_headers: custom_headers_from_reader(&reader)?,
        })
    }

    pub async fn probe(&self) -> Result<ProviderProbeResult, ProviderError> {
        let mut response = self.send_probe(false).await?;
        if response.status().as_u16() == 401 {
            response = self.send_probe(true).await?;
        }
        let status = response.status();
        let value = response_json_or_error(response).await?;
        if !status.is_success() {
            return Err(provider_http_error(status, &value));
        }
        let discovered_models = value
            .get("models")
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
        self.authenticated_request(
            self.client.get(format!(
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

    fn authenticated_request(
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

    async fn send_completion(
        &self,
        request: &ProviderRequest,
        force_refresh: bool,
    ) -> Result<reqwest::Response, ProviderError> {
        let (token, account_id) = self.resolve_credential(force_refresh).await?;
        let body = responses_request_body(request, &self.config)?;
        self.authenticated_request(
            self.client
                .post(format!(
                    "{}/responses",
                    self.config.base_url.trim_end_matches('/')
                ))
                .header(ACCEPT, "text/event-stream")
                .header("session-id", request.task_id.to_string())
                .json(&body),
            token.expose_secret(),
            account_id.as_deref(),
        )
        .send()
        .await
        .map_err(provider_transport_error)
    }
}

#[async_trait]
impl super::LlmProvider for OpenAiResponsesProvider {
    async fn complete(&self, request: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        let mut ignore = |_| {};
        self.complete_stream(request, &mut ignore).await
    }

    async fn complete_stream(
        &self,
        request: ProviderRequest,
        on_event: &mut (dyn FnMut(ProviderStreamEvent) + Send),
    ) -> Result<ProviderResponse, ProviderError> {
        let mut response = self.send_completion(&request, false).await?;
        if response.status().as_u16() == 401 {
            response = self.send_completion(&request, true).await?;
        }
        let status = response.status();
        if !status.is_success() {
            let value = response_json_or_error(response).await?;
            if status.as_u16() == 429 {
                return Err(ProviderError::RateLimited {
                    message: provider_error_message(&value),
                });
            }
            return Err(provider_http_error(status, &value));
        }
        responses_provider_response(response, on_event).await
    }

    fn supports_buffered_transport(&self) -> bool {
        false
    }

    fn contract(&self) -> ProviderContract {
        ProviderContract {
            provider_id: self.config.provider_id.clone(),
            model_id: self.config.model_id.clone(),
            native_protocol: "openai_responses_sse".to_owned(),
            stream_event_mapping: "responses_sse_delta".to_owned(),
            tool_call_mapping: "responses_function_call".to_owned(),
            usage_mapping: "responses_completed_usage".to_owned(),
            reasoning_mapping: "responses_reasoning_effort".to_owned(),
            finish_reason_mapping: "responses_completed_or_tool_call".to_owned(),
            error_mapping: "responses_http_and_sse_error".to_owned(),
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

fn responses_request_body(
    request: &ProviderRequest,
    config: &OpenAiResponsesProviderConfig,
) -> Result<Value, ProviderError> {
    let instructions = request
        .messages
        .iter()
        .filter(|message| message.role == ProviderRole::System)
        .map(|message| message.content.as_str())
        .collect::<Vec<_>>()
        .join("\n\n");
    let mut input = Vec::new();
    for message in request
        .messages
        .iter()
        .filter(|message| message.role != ProviderRole::System)
    {
        match message.role {
            ProviderRole::System => {}
            ProviderRole::User => input.push(responses_message(message, "user", "input_text")),
            ProviderRole::Assistant => {
                input.extend(responses_replay_items(message)?);
                if !message.content.is_empty() {
                    input.push(responses_message(message, "assistant", "output_text"));
                }
                input.extend(message.tool_calls.iter().map(|call| {
                    json!({
                        "type": "function_call",
                        "call_id": call.tool_call_id,
                        "name": call.tool_name,
                        "arguments": call.arguments.to_string(),
                    })
                }));
            }
            ProviderRole::Tool => {
                let call_id = message
                    .tool_call_id
                    .as_deref()
                    .filter(|value| !value.trim().is_empty())
                    .ok_or_else(|| ProviderError::Malformed {
                        message: "tool response has no non-empty tool_call_id".to_owned(),
                    })?;
                input.push(json!({
                    "type": "function_call_output",
                    "call_id": call_id,
                    "output": message.content,
                }));
            }
        }
    }

    let tools = request
        .tools
        .iter()
        .map(responses_tool_schema)
        .collect::<Vec<_>>();
    let effort = config
        .generation_config
        .reasoning_effort
        .map(|value| value.as_wire_value())
        .or(config.generation_config.enable_thinking.then_some("medium"));
    let mut body = json!({
        "model": config.model_id,
        "input": input,
        "instructions": instructions,
        "store": false,
        "stream": true,
        "include": ["reasoning.encrypted_content"],
    });
    if !tools.is_empty() {
        body["tools"] = Value::Array(tools);
        body["tool_choice"] = Value::String("auto".to_owned());
        body["parallel_tool_calls"] = Value::Bool(true);
    }
    if let Some(effort) = effort {
        body["reasoning"] = json!({"effort": effort, "summary": "auto"});
    }
    Ok(body)
}

fn responses_message(message: &ProviderMessage, role: &str, content_type: &str) -> Value {
    json!({
        "role": role,
        "content": [{"type": content_type, "text": message.content}],
    })
}

fn responses_replay_items(message: &ProviderMessage) -> Result<Vec<Value>, ProviderError> {
    message
        .metadata
        .openai_responses_replay_items
        .iter()
        .map(normalize_reasoning_replay_item)
        .collect()
}

fn normalize_reasoning_replay_item(item: &Value) -> Result<Value, ProviderError> {
    if item.get("type").and_then(Value::as_str) != Some("reasoning") {
        return Err(ProviderError::Malformed {
            message: "Responses replay metadata contains an unsupported item".to_owned(),
        });
    }
    let encrypted_content = bounded_non_empty_string(
        item.get("encrypted_content"),
        "Responses reasoning item has no encrypted_content",
        "Responses encrypted reasoning",
        MAX_PROVIDER_MESSAGE_BYTES,
    )?;
    let mut replay = json!({
        "type": "reasoning",
        "encrypted_content": encrypted_content,
    });
    if let Some(id) = item.get("id") {
        replay["id"] = Value::String(bounded_non_empty_string(
            Some(id),
            "Responses reasoning item has an empty id",
            "Responses reasoning item id",
            MAX_PROVIDER_TOOL_CALL_ID_BYTES,
        )?);
    }
    if let Some(summary) = item.get("summary") {
        if !summary.is_array() {
            return Err(ProviderError::Malformed {
                message: "Responses reasoning item summary is not an array".to_owned(),
            });
        }
        if serde_json::to_vec(summary)
            .map_err(|error| ProviderError::Malformed {
                message: format!("Responses reasoning summary is invalid: {error}"),
            })?
            .len()
            > MAX_PROVIDER_MESSAGE_BYTES
        {
            return Err(ProviderError::Malformed {
                message: format!(
                    "Responses reasoning summary exceeds {MAX_PROVIDER_MESSAGE_BYTES} byte limit"
                ),
            });
        }
        replay["summary"] = summary.clone();
    }
    Ok(replay)
}

fn responses_tool_schema(contract: &ToolContract) -> Value {
    let description = match contract.tool_name.as_str() {
        "read_file" => "Read a UTF-8 text file from the current workspace.",
        "write_file" => "Write UTF-8 text content to a workspace-relative file.",
        "edit_file" => "Replace the first exact text match in a workspace-relative file.",
        "list_dir" => "List entries in a workspace-relative directory.",
        "rg_search" => "Search workspace files with ripgrep.",
        "shell" => {
            "Run a workspace command as argv; for pipes, redirection, or compound scripts, explicitly invoke bash -lc with the complete script as one argument."
        }
        _ => "Golutra workspace tool.",
    };
    json!({
        "type": "function",
        "name": contract.tool_name,
        "description": description,
        "parameters": contract.input_schema,
        "strict": true,
    })
}

async fn responses_provider_response(
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
    let mut completed_text = None;
    let mut tool_calls = Vec::new();
    let mut replay_items = Vec::new();
    let mut response_id = None;
    let mut usage_value = json!({});
    let mut completed = false;

    while let Some(event) = stream.next().await {
        let event = event.map_err(|error| ProviderError::Unavailable {
            message: super::sanitize_provider_error(&error.to_string()),
        })?;
        parsed_bytes = parsed_bytes.saturating_add(event.data.len());
        if parsed_bytes > MAX_PROVIDER_RESPONSE_BYTES {
            return Err(ProviderError::Malformed {
                message: format!(
                    "provider response exceeds {MAX_PROVIDER_RESPONSE_BYTES} byte limit"
                ),
            });
        }
        if event.data == "[DONE]" || event.data.trim().is_empty() {
            continue;
        }
        let value: Value =
            serde_json::from_str(&event.data).map_err(|error| ProviderError::Malformed {
                message: format!("responses SSE event is invalid JSON: {error}"),
            })?;
        match value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
        {
            "response.output_text.delta" => {
                if let Some(delta) = value.get("delta").and_then(Value::as_str) {
                    if output_text.len().saturating_add(delta.len()) > MAX_PROVIDER_MESSAGE_BYTES {
                        return Err(ProviderError::Malformed {
                            message: format!(
                                "assistant message exceeds {MAX_PROVIDER_MESSAGE_BYTES} byte limit"
                            ),
                        });
                    }
                    output_text.push_str(delta);
                    on_event(ProviderStreamEvent::TextDelta {
                        text: delta.to_owned(),
                    });
                }
            }
            "response.reasoning_summary_text.delta" => {
                if let Some(delta) = value.get("delta").and_then(Value::as_str)
                    && !delta.is_empty()
                {
                    on_event(ProviderStreamEvent::ReasoningDelta {
                        text: delta.to_owned(),
                    });
                }
            }
            "response.output_item.done" => {
                if let Some(item) = value.get("item") {
                    match item.get("type").and_then(Value::as_str).unwrap_or_default() {
                        "function_call" => {
                            let call = responses_tool_call(item)?;
                            on_event(ProviderStreamEvent::ToolCallDelta {
                                index: tool_calls.len(),
                                tool_call_id: Some(call.tool_call_id.clone()),
                                tool_name: Some(call.tool_name.clone()),
                            });
                            tool_calls.push(call);
                        }
                        "reasoning" => {
                            replay_items.push(normalize_reasoning_replay_item(item)?);
                        }
                        "message" => {
                            completed_text = responses_completed_message(item)?;
                        }
                        _ => {}
                    }
                }
            }
            "response.completed" => {
                let response = value.get("response").cloned().unwrap_or_else(|| json!({}));
                response_id = response
                    .get("id")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned);
                usage_value = response.get("usage").cloned().unwrap_or_else(|| json!({}));
                completed = true;
            }
            "response.failed" | "error" => {
                let error = value.get("response").unwrap_or(&value);
                return Err(ProviderError::Failed {
                    message: provider_error_message(error),
                });
            }
            _ => {}
        }
    }
    if !completed {
        return Err(ProviderError::Unavailable {
            message: "responses SSE stream ended before response.completed".to_owned(),
        });
    }
    if output_text.is_empty()
        && let Some(text) = completed_text
    {
        on_event(ProviderStreamEvent::TextDelta { text: text.clone() });
        output_text = text;
    }
    let message =
        (!output_text.is_empty() || !replay_items.is_empty()).then_some(ProviderMessage {
            role: ProviderRole::Assistant,
            content: output_text,
            tool_call_id: None,
            tool_name: None,
            tool_calls: Vec::new(),
            metadata: ProviderMessageMetadata {
                openai_responses_replay_items: replay_items,
            },
        });
    let finish_reason = if tool_calls.is_empty() {
        ProviderFinishReason::Stop
    } else {
        ProviderFinishReason::ToolCalls
    };
    Ok(ProviderResponse {
        response_id: ProviderResponseId::new(),
        message,
        tool_calls,
        usage: responses_usage(&usage_value),
        finish_reason,
        raw_metadata: json!({
            "provider": DEFAULT_PROVIDER_ID,
            "response_id": response_id,
            "usage": usage_value,
        }),
    })
}

fn responses_completed_message(item: &Value) -> Result<Option<String>, ProviderError> {
    let mut text = String::new();
    for content in item
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if content.get("type").and_then(Value::as_str) == Some("output_text")
            && let Some(value) = content.get("text").and_then(Value::as_str)
        {
            if text.len().saturating_add(value.len()) > MAX_PROVIDER_MESSAGE_BYTES {
                return Err(ProviderError::Malformed {
                    message: format!(
                        "assistant message exceeds {MAX_PROVIDER_MESSAGE_BYTES} byte limit"
                    ),
                });
            }
            text.push_str(value);
        }
    }
    Ok((!text.is_empty()).then_some(text))
}

fn responses_tool_call(item: &Value) -> Result<ProviderToolCall, ProviderError> {
    let tool_call_id = bounded_non_empty_string(
        item.get("call_id"),
        "responses function call has no call_id",
        "responses function call id",
        MAX_PROVIDER_TOOL_CALL_ID_BYTES,
    )?;
    let tool_name = bounded_non_empty_string(
        item.get("name"),
        "responses function call has no name",
        "responses function call name",
        MAX_PROVIDER_TOOL_NAME_BYTES,
    )?;
    let arguments = item
        .get("arguments")
        .and_then(Value::as_str)
        .ok_or_else(|| ProviderError::Malformed {
            message: "responses function call arguments is not a string".to_owned(),
        })?;
    if arguments.len() > MAX_PROVIDER_TOOL_ARGUMENT_BYTES {
        return Err(ProviderError::Malformed {
            message: format!(
                "tool call arguments exceed {MAX_PROVIDER_TOOL_ARGUMENT_BYTES} byte limit"
            ),
        });
    }
    let arguments = serde_json::from_str(arguments).map_err(|error| ProviderError::Malformed {
        message: format!("responses function call arguments is invalid JSON: {error}"),
    })?;
    Ok(ProviderToolCall {
        tool_call_id,
        tool_name,
        arguments,
    })
}

fn bounded_non_empty_string(
    value: Option<&Value>,
    missing_message: &str,
    label: &str,
    limit: usize,
) -> Result<String, ProviderError> {
    let value = value
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| ProviderError::Malformed {
            message: missing_message.to_owned(),
        })?;
    if value.len() > limit {
        return Err(ProviderError::Malformed {
            message: format!("{label} exceeds {limit} byte limit"),
        });
    }
    Ok(value.to_owned())
}

fn responses_usage(value: &Value) -> ProviderUsage {
    ProviderUsage {
        input_tokens: value.get("input_tokens").and_then(Value::as_u64),
        output_tokens: value.get("output_tokens").and_then(Value::as_u64),
        reasoning_tokens: value
            .get("output_tokens_details")
            .and_then(|details| details.get("reasoning_tokens"))
            .and_then(Value::as_u64),
        cached_input_tokens: value
            .get("input_tokens_details")
            .and_then(|details| details.get("cached_tokens"))
            .and_then(Value::as_u64),
        total_tokens: value.get("total_tokens").and_then(Value::as_u64),
        usage_source: UsageSource::Provider,
        raw: value.clone(),
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
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use golutra_core::{ProviderRequestId, SideEffectType, TaskId, TurnId};

    fn request() -> ProviderRequest {
        ProviderRequest {
            request_id: ProviderRequestId::new(),
            task_id: TaskId::new(),
            turn_id: TurnId::new(),
            provider_id: "openai-chatgpt".to_owned(),
            model_id: "gpt-5.5".to_owned(),
            messages: vec![
                ProviderMessage {
                    role: ProviderRole::System,
                    content: "Use tools.".to_owned(),
                    tool_call_id: None,
                    tool_name: None,
                    tool_calls: Vec::new(),
                    metadata: ProviderMessageMetadata::default(),
                },
                ProviderMessage {
                    role: ProviderRole::User,
                    content: "Read Cargo.toml".to_owned(),
                    tool_call_id: None,
                    tool_name: None,
                    tool_calls: Vec::new(),
                    metadata: ProviderMessageMetadata::default(),
                },
            ],
            tools: vec![ToolContract {
                tool_name: "read_file".to_owned(),
                input_schema: json!({
                    "type": "object",
                    "properties": {"path": {"type": "string"}},
                    "required": ["path"],
                    "additionalProperties": false
                }),
                output_schema: json!({"type": "object"}),
                error_schema: json!({"type": "object"}),
                side_effect_type: SideEffectType::None,
                idempotency_key_policy: "not_required".to_owned(),
                timeout_policy: "bounded".to_owned(),
                cancellation_policy: "supported".to_owned(),
                retry_policy: "none".to_owned(),
                artifact_policy: "none".to_owned(),
                permission_policy_ref: None,
            }],
        }
    }

    #[test]
    fn request_uses_responses_wire_shape() {
        let body = responses_request_body(
            &request(),
            &OpenAiResponsesProviderConfig {
                api_key: "unused".to_owned(),
                api_key_env: "test".to_owned(),
                provider_id: "openai-chatgpt".to_owned(),
                base_url: "https://chatgpt.com/backend-api/codex".to_owned(),
                model_id: "gpt-5.5".to_owned(),
                generation_config: ProviderGenerationConfig::default(),
                custom_headers: ProviderHttpHeaders::default(),
            },
        )
        .expect("request body");

        assert_eq!(body["instructions"], "Use tools.");
        assert_eq!(body["input"][0]["content"][0]["type"], "input_text");
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["name"], "read_file");
        assert_eq!(body["stream"], true);
    }

    #[test]
    fn account_id_is_read_from_nested_claim() {
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"none"}"#);
        let payload = URL_SAFE_NO_PAD
            .encode(br#"{"https://api.openai.com/auth":{"chatgpt_account_id":"acct-test"}}"#);
        let token = format!("{header}.{payload}.signature");

        assert_eq!(chatgpt_account_id(&token).as_deref(), Some("acct-test"));
    }
}
