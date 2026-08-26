use serde_json::json;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

use super::*;

#[tokio::test]
async fn mock_provider_can_emit_a_deterministic_failure() {
    let provider = MockProvider::failure("forced failure");
    let error = provider
        .complete(ProviderRequest {
            request_id: ProviderRequestId::new(),
            task_id: TaskId::new(),
            turn_id: TurnId::new(),
            session_id: None,
            provider_id: "mock".to_owned(),
            model_id: "mock".to_owned(),
            messages: Vec::new(),
            tools: Vec::new(),
            cache_policy: Default::default(),
        })
        .await
        .expect_err("mock failure");

    assert_eq!(
        error,
        ProviderError::Failed {
            message: "forced failure".to_owned()
        }
    );
}

#[tokio::test]
async fn mock_provider_returns_text_response() {
    let provider = MockProvider::text_response("done");
    let response = provider.complete(request()).await.expect("response");

    assert_eq!(response.finish_reason, ProviderFinishReason::Stop);
    assert_eq!(response.message.expect("message").content, "done");
    assert_eq!(response.usage.total_tokens, Some(160));
}

#[tokio::test]
async fn configured_mock_provider_probe_reports_ready() {
    let result = ConfiguredProvider::probe_from_reader(|key| {
        (key == GOLUTRA_PROVIDER_PROTOCOL).then(|| "mock".to_owned())
    })
    .await
    .expect("mock probe");

    assert_eq!(result.provider_id, "mock");
    assert_eq!(result.protocol, "in_memory");
    assert_eq!(result.model_available, Some(true));
    assert_eq!(result.discovered_models, vec!["mock-model"]);
}

#[tokio::test]
async fn unconfigured_provider_probe_matches_default_mock_resolution() {
    let result = ConfiguredProvider::probe_from_reader(|_| None)
        .await
        .expect("default mock probe");

    assert_eq!(result.provider_id, "mock");
    assert_eq!(result.protocol, "in_memory");
    assert_eq!(result.model_available, Some(true));
}

#[tokio::test]
async fn provider_response_rejects_oversized_content_length_before_buffering() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
    let address = listener.local_addr().expect("address");
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept");
        let mut request = [0_u8; 1024];
        let _ = socket.read(&mut request).await.expect("request");
        socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 16777217\r\nconnection: close\r\n\r\n",
                )
                .await
                .expect("response");
    });
    let response = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("client")
        .get(format!("http://{address}"))
        .send()
        .await
        .expect("HTTP response");

    let error = response_json_or_error(response)
        .await
        .expect_err("oversized response must be rejected");

    assert!(matches!(error, ProviderError::Malformed { .. }));
}

#[tokio::test]
async fn truncated_provider_response_body_is_retryable() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
    let address = listener.local_addr().expect("address");
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept");
        let mut request = [0_u8; 1024];
        let _ = socket.read(&mut request).await.expect("request");
        socket
            .write_all(
                b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 128\r\nconnection: close\r\n\r\n{\"choices\":[]}",
            )
            .await
            .expect("truncated response");
    });
    let response = reqwest::Client::new()
        .get(format!("http://{address}"))
        .send()
        .await
        .expect("HTTP response headers");

    let error = response_json_or_error(response)
        .await
        .expect_err("truncated body must remain retryable");

    assert!(
        matches!(error, ProviderError::Unavailable { .. }),
        "{error:?}"
    );
}

#[tokio::test]
async fn provider_request_transport_failure_is_retryable() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
    let address = listener.local_addr().expect("address");
    drop(listener);

    let error = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("client")
        .get(format!("http://{address}"))
        .send()
        .await
        .expect_err("closed listener must reject the request");

    assert!(matches!(
        provider_transport_error(error),
        ProviderError::Unavailable { .. }
    ));
}

#[tokio::test]
async fn mock_provider_returns_tool_call() {
    let provider = MockProvider::tool_call("read_file", json!({"path": "README.md"}));
    let response = provider.complete(request()).await.expect("response");

    assert_eq!(response.finish_reason, ProviderFinishReason::ToolCalls);
    assert_eq!(response.tool_calls[0].tool_name, "read_file");
}

#[test]
fn openai_tool_parameters_come_from_the_runtime_tool_contract() {
    let input_schema = json!({
        "type": "object",
        "properties": {
            "custom": {
                "type": "integer",
                "minimum": 1,
                "maximum": 10,
                "description": "runtime-only bound"
            },
            "description": {"type": "string", "maxLength": 32}
        },
        "required": ["custom"],
        "additionalProperties": false,
        "oneOf": [
            {"type": "object", "properties": {"custom": {"type": "integer"}}},
            {"type": "null"}
        ]
    });
    let contract = ToolContract {
        tool_name: "custom_tool".to_owned(),
        input_schema: input_schema.clone(),
        output_schema: json!({}),
        error_schema: json!({}),
        side_effect_type: golutra_core::SideEffectType::None,
        idempotency_key_policy: "not_required".to_owned(),
        timeout_policy: "bounded".to_owned(),
        cancellation_policy: "supported".to_owned(),
        retry_policy: "none".to_owned(),
        artifact_policy: "none".to_owned(),
        permission_policy_ref: None,
    };

    let schema = openai_tool_schema(&contract);

    let parameters = &schema["function"]["parameters"];
    assert_eq!(parameters["type"], "object");
    assert_eq!(parameters["required"], json!(["custom"]));
    assert_eq!(parameters["additionalProperties"], false);
    assert!(parameters["properties"]["custom"].get("minimum").is_none());
    assert!(parameters["properties"]["custom"].get("maximum").is_none());
    assert_eq!(
        parameters["properties"]["custom"]["description"],
        "runtime-only bound"
    );
    assert_eq!(parameters["properties"]["description"]["type"], "string");
    assert!(
        parameters["properties"]["description"]
            .get("maxLength")
            .is_none()
    );
    assert_eq!(parameters["oneOf"][0]["type"], "object");
    assert_eq!(parameters["oneOf"][1]["type"], "null");
}

#[test]
fn shell_provider_description_distinguishes_lifetime_from_initial_wait() {
    let description = provider_tool_description("shell");

    assert!(description.contains("as argv"));
    assert!(description.contains("Python heredoc"));
    assert!(description.contains("timeout_ms is the absolute process lifetime"));
    assert!(description.contains("yield_time_ms only the initial wait"));
    assert!(description.contains("runtime owns the process"));
    assert!(description.contains("must outlive the runtime"));
    assert!(description.len() < 400);
}

#[test]
fn provider_tool_descriptions_own_file_and_question_usage_details() {
    let write_file = provider_tool_description("write_file");
    assert!(write_file.contains("complete UTF-8 content"));
    assert!(write_file.contains("workspace-relative file"));

    let ask_user = provider_tool_description("ask_user");
    assert!(ask_user.contains("consequential decision"));
    assert!(ask_user.contains("cannot be resolved safely"));

    assert_ne!(
        provider_tool_description("apply_patch"),
        "Golutra workspace tool."
    );
    let subagent = provider_tool_description("subagent");
    assert!(subagent.contains("isolated child task"));
    assert!(subagent.contains("cannot create another child"));
    assert!(provider_tool_description("web_search").contains("network"));
    assert!(provider_tool_description("shell_session").contains("background"));
    assert_ne!(
        provider_tool_description("process_list"),
        "Golutra workspace tool."
    );
    assert_ne!(
        provider_tool_description("process_poll"),
        "Golutra workspace tool."
    );
    assert_ne!(
        provider_tool_description("process_write"),
        "Golutra workspace tool."
    );
    assert_ne!(
        provider_tool_description("process_terminate"),
        "Golutra workspace tool."
    );
    assert_ne!(
        provider_tool_description("process_reconnect"),
        "Golutra workspace tool."
    );
}

#[test]
fn provider_schema_projection_keeps_strict_structure_and_nested_combinators() {
    let schema = json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "items": {
                "type": "array",
                "minItems": 1,
                "maxItems": 4,
                "items": {
                    "type": "string",
                    "pattern": "^[a-z]+$"
                }
            }
        },
        "required": ["items"],
        "anyOf": [
            {"type": "object", "additionalProperties": false},
            {"type": "null"}
        ],
        "allOf": [{"type": "object", "properties": {"items": {"type": "array"}}}]
    });

    let projected = provider_tool_schema_projection(&schema);
    assert_eq!(projected["additionalProperties"], false);
    assert_eq!(projected["required"], json!(["items"]));
    assert!(projected["properties"]["items"].get("minItems").is_none());
    assert!(projected["properties"]["items"].get("maxItems").is_none());
    assert!(
        projected["properties"]["items"]["items"]
            .get("pattern")
            .is_none()
    );
    assert_eq!(projected["anyOf"][0]["additionalProperties"], false);
    assert_eq!(
        projected["allOf"][0]["properties"]["items"]["type"],
        "array"
    );
}

#[test]
fn provider_schema_projection_keeps_compact_parameter_semantics() {
    let schema = json!({
        "type": "object",
        "description": "  Use   this parameter for a bounded operation.  ",
        "properties": {
            "command": {
                "type": "string",
                "description": "  A command with   important shell semantics.  ",
                "maxLength": 64
            }
        }
    });

    let projected = provider_tool_schema_projection(&schema);
    assert_eq!(
        projected["description"],
        "Use this parameter for a bounded operation."
    );
    assert_eq!(
        projected["properties"]["command"]["description"],
        "A command with important shell semantics."
    );

    let long_description = "x ".repeat(400);
    let bounded = provider_tool_schema_projection(&json!({
        "type": "string",
        "description": long_description
    }));
    assert_eq!(
        bounded["description"]
            .as_str()
            .expect("description")
            .chars()
            .count(),
        512
    );
}

#[test]
fn openai_tool_call_requires_a_non_empty_provider_id() {
    let error = provider_tool_call_from_openai(&json!({
        "function": {
            "name": "read_file",
            "arguments": "{\"path\":\"README.md\"}"
        }
    }))
    .expect_err("missing tool call id");

    assert!(matches!(error, ProviderError::Malformed { .. }));
    assert!(error.to_string().contains("non-empty id"));
}

#[test]
fn openai_tool_call_requires_a_non_empty_function_name() {
    let error = provider_tool_call_from_openai(&json!({
        "id": "call-1",
        "function": {
            "name": "",
            "arguments": "{}"
        }
    }))
    .expect_err("empty tool name");

    assert!(matches!(error, ProviderError::Malformed { .. }));
    assert!(error.to_string().contains("non-empty name"));
}

#[test]
fn openai_response_rejects_oversized_event_fields() {
    let oversized_message = json!({
        "choices": [{
            "message": {"content": "x".repeat(MAX_PROVIDER_MESSAGE_BYTES + 1)},
            "finish_reason": "stop"
        }]
    });
    let error = provider_response_from_openai(oversized_message, TaskId::new(), TurnId::new())
        .expect_err("oversized assistant message");
    assert!(error.to_string().contains("assistant message exceeds"));

    let error = provider_tool_call_from_openai(&json!({
        "id": "call-1",
        "function": {
            "name": "x".repeat(MAX_PROVIDER_TOOL_NAME_BYTES + 1),
            "arguments": "{}"
        }
    }))
    .expect_err("oversized tool name");
    assert!(error.to_string().contains("tool name exceeds"));
}

#[test]
fn configured_provider_resolves_native_anthropic_adapter() {
    let provider = ConfiguredProvider::resolve_from_reader(
        MockProvider::text_response("mock"),
        |key| match key {
            GOLUTRA_PROVIDER_PROTOCOL => Some("anthropic".to_owned()),
            ANTHROPIC_API_KEY => Some("anthropic-key".to_owned()),
            ANTHROPIC_MODEL => Some("claude-test".to_owned()),
            _ => None,
        },
    )
    .expect("native provider");

    assert!(matches!(provider, ConfiguredProvider::Anthropic(_)));
    assert_eq!(provider.contract().native_protocol, "anthropic_messages");
    assert!(!provider.contract().golden_fixture_refs.is_empty());
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
fn openai_model_metadata_updates_dynamic_capabilities() {
    let capabilities = openai_capabilities_from_models(
        &json!({
            "data": [{
                "id": "model-test",
                "context_length": 262_144,
                "max_output_tokens": 16_384,
                "supported_parameters": [
                    "stream",
                    "tools",
                    "response_format",
                    "reasoning_effort"
                ],
                "architecture": {"input_modalities": ["text", "image"]}
            }]
        }),
        "model-test",
    );

    assert_eq!(capabilities.source, ProviderCapabilitySource::Discovered);
    assert!(capabilities.supports_streaming);
    assert!(capabilities.supports_tools);
    assert!(capabilities.supports_json_schema);
    assert!(capabilities.supports_reasoning);
    assert!(capabilities.supports_vision);
    assert_eq!(capabilities.context_window, Some(262_144));
    assert_eq!(capabilities.max_output_tokens, Some(16_384));
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
            ProviderProtocol::OpenAiResponses,
            ProviderProtocol::Anthropic,
            ProviderProtocol::Gemini,
            ProviderProtocol::VertexAi,
            ProviderProtocol::Genai,
            ProviderProtocol::Mock,
        ]
    );
}

#[test]
fn native_protocol_redacted_config_reports_missing_fields() {
    let config = redacted_native_from_reader(ProviderProtocol::Anthropic, &|_| None);

    assert_eq!(config.protocol, ProviderProtocol::Anthropic);
    assert!(config.supported);
    assert_eq!(config.status, "missing_env");
    assert!(
        config
            .missing_env
            .iter()
            .any(|value| value.contains(ANTHROPIC_API_KEY))
    );
}

#[test]
fn native_protocol_redacted_config_prefers_configured_custom_api_key_env() {
    let config = ConfiguredProvider::redacted_from_reader(|key| match key {
        GOLUTRA_PROVIDER_PROTOCOL => Some("anthropic".to_owned()),
        GOLUTRA_PROVIDER_API_KEY_ENV => Some("GOLUTRA_CUSTOM_PROVIDER_API_KEY_TEST".to_owned()),
        "GOLUTRA_CUSTOM_PROVIDER_API_KEY_TEST" => Some("custom-key".to_owned()),
        ANTHROPIC_API_KEY => Some("generic-key".to_owned()),
        ANTHROPIC_MODEL => Some("claude-sonnet-4".to_owned()),
        _ => None,
    })
    .expect("config");

    assert_eq!(config.protocol, ProviderProtocol::Anthropic);
    assert_eq!(
        config.api_key_env.as_deref(),
        Some("GOLUTRA_CUSTOM_PROVIDER_API_KEY_TEST")
    );
    assert!(config.api_key_configured);
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
fn openai_base_url_validation_rejects_missing_hosts_and_unsafe_components() {
    for invalid in [
        "",
        "file:///tmp/provider",
        "https://user:secret@api.example.com/v1",
        "https://api.example.com/v1?token=secret",
        "https://api.example.com/v1#fragment",
    ] {
        assert!(
            validate_openai_base_url(invalid).is_err(),
            "{invalid} must be rejected"
        );
    }
    assert_eq!(
        validate_openai_base_url("api.golutra.cn").expect("bare host"),
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
fn openai_config_prefers_configured_custom_api_key_env() {
    let config = OpenAiCompatibleProvider::config_from_env_reader(|key| match key {
        GOLUTRA_PROVIDER_API_KEY_ENV => Some("GOLUTRA_CUSTOM_PROVIDER_API_KEY_TEST".to_owned()),
        "GOLUTRA_CUSTOM_PROVIDER_API_KEY_TEST" => Some("custom-key".to_owned()),
        GOLUTRA_PROVIDER_API_KEY => Some("generic-key".to_owned()),
        GOLUTRA_PROVIDER_MODEL => Some("gpt-5.5".to_owned()),
        GOLUTRA_PROVIDER_BASE_URL => Some("https://api.example.com/v1".to_owned()),
        _ => None,
    })
    .expect("config");

    assert_eq!(config.api_key, "custom-key");
    assert_eq!(config.api_key_env, "GOLUTRA_CUSTOM_PROVIDER_API_KEY_TEST");
}

#[test]
fn openai_config_reads_generation_config_and_applies_request_body() {
    let config = OpenAiCompatibleProvider::config_from_env_reader(|key| match key {
        GOLUTRA_PROVIDER_API_KEY => Some("golutra-key".to_owned()),
        GOLUTRA_PROVIDER_MODEL => Some("gpt-5.5".to_owned()),
        GOLUTRA_PROVIDER_BASE_URL => Some("api.golutra.cn".to_owned()),
        GOLUTRA_PROVIDER_GENERATION_CONFIG => Some(
            json!({
                "enable_thinking": true,
                "reasoning_effort": "high",
                "context_window_size": 128000,
                "max_tokens": 512
            })
            .to_string(),
        ),
        _ => None,
    })
    .expect("config");
    let mut body = json!({"model": "gpt-5.5", "messages": []});

    apply_generation_config_to_openai_body(&mut body, &config.generation_config);

    assert_eq!(config.generation_config.context_window_size, Some(128_000));
    assert_eq!(body["enable_thinking"], json!(true));
    assert!(body.get("extra_body").is_none());
    assert_eq!(body["reasoning_effort"], json!("high"));
    assert_eq!(body["max_tokens"], json!(512));
    assert!(body.get("context_window_size").is_none());
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
fn redacted_config_prefers_configured_custom_api_key_env() {
    let config = ConfiguredProvider::redacted_from_reader(|key| match key {
        GOLUTRA_PROVIDER_PROTOCOL => Some("openai-compatible".to_owned()),
        GOLUTRA_PROVIDER_API_KEY_ENV => Some("GOLUTRA_CUSTOM_PROVIDER_API_KEY_TEST".to_owned()),
        "GOLUTRA_CUSTOM_PROVIDER_API_KEY_TEST" => Some("custom-key".to_owned()),
        GOLUTRA_PROVIDER_API_KEY => Some("generic-key".to_owned()),
        GOLUTRA_PROVIDER_MODEL => Some("gpt-5.5".to_owned()),
        GOLUTRA_PROVIDER_BASE_URL => Some("https://api.example.com/v1".to_owned()),
        _ => None,
    })
    .expect("config");

    assert_eq!(
        config.api_key_env.as_deref(),
        Some("GOLUTRA_CUSTOM_PROVIDER_API_KEY_TEST")
    );
    assert!(config.api_key_configured);
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

#[test]
fn provider_error_message_reads_top_level_code_and_message() {
    let value = json!({
        "code": "INVALID_API_KEY",
        "message": "Invalid API key"
    });

    assert_eq!(
        provider_error_message(&value),
        "INVALID_API_KEY: Invalid API key"
    );
}

#[test]
fn provider_error_message_reads_string_error() {
    let value = json!({
        "error": "model does not support tools"
    });

    assert_eq!(
        provider_error_message(&value),
        "model does not support tools"
    );
}

#[test]
fn provider_error_message_redacts_api_key_fragments() {
    let value = json!({
        "error": {
            "code": "invalid_api_key",
            "message": "Incorrect API key provided: sk-test1234567890abcdef. You can find your API key in settings."
        }
    });

    let message = provider_error_message(&value);

    assert!(message.contains("<redacted-api-key>"));
    assert!(!message.contains("sk-test"));
    assert!(!message.contains("abcdef"));
}

#[test]
fn provider_error_message_redacts_masked_api_key_fragments() {
    let value = json!({
        "error": {
            "message": "Incorrect API key provided: sk-123456**********************abcd."
        }
    });

    let message = provider_error_message(&value);

    assert!(message.contains("<redacted-api-key>"));
    assert!(!message.contains("123456"));
    assert!(!message.contains("abcd"));
}

fn request() -> ProviderRequest {
    ProviderRequest {
        request_id: ProviderRequestId::new(),
        task_id: TaskId::new(),
        turn_id: TurnId::new(),
        session_id: None,
        provider_id: "mock".to_owned(),
        model_id: "mock-model".to_owned(),
        messages: vec![ProviderMessage {
            role: ProviderRole::User,
            content: "hello".to_owned(),
            tool_call_id: None,
            tool_name: None,
            tool_calls: Vec::new(),
            metadata: ProviderMessageMetadata::default(),
        }],
        tools: Vec::new(),
        cache_policy: Default::default(),
    }
}

#[test]
fn prompt_cache_identity_is_stable_and_session_scoped() {
    let mut first = request();
    first.session_id = Some(golutra_core::SessionId::new());
    first.cache_policy = golutra_core::PromptCachePolicy::Auto;
    let second = first.clone();
    assert_eq!(first.cache_identity(), second.cache_identity());

    let mut changed = first.clone();
    changed.messages[0].content.push('!');
    assert_eq!(first.cache_identity(), changed.cache_identity());

    changed.task_id = golutra_core::TaskId::new();
    assert_eq!(first.cache_identity(), changed.cache_identity());

    changed.provider_id = "other-provider".to_owned();
    assert_ne!(first.cache_identity(), changed.cache_identity());

    first.cache_policy = golutra_core::PromptCachePolicy::None;
    assert!(first.cache_identity().is_none());
}

#[test]
fn provider_tool_projection_excludes_internal_contract_policy() {
    let mut contract = golutra_core::ToolContract {
        tool_name: "read_file".to_owned(),
        input_schema: json!({"type": "object", "properties": {"path": {"type": "string"}}}),
        output_schema: json!({"internal": true}),
        error_schema: json!({"internal": true}),
        side_effect_type: golutra_core::SideEffectType::None,
        idempotency_key_policy: "internal".to_owned(),
        timeout_policy: "internal".to_owned(),
        cancellation_policy: "internal".to_owned(),
        retry_policy: "internal".to_owned(),
        artifact_policy: "internal".to_owned(),
        permission_policy_ref: Some(golutra_core::PolicyId::new()),
    };
    let projected = provider_tool_wire_projection(&contract);
    assert_eq!(projected["function"]["name"], "read_file");
    assert!(projected["function"].get("parameters").is_some());
    assert!(projected.get("output_schema").is_none());
    assert!(projected["function"].get("retry_policy").is_none());

    contract.input_schema["properties"]["path"] = json!({"type": "string", "maxLength": 32});
    assert!(estimate_provider_tool_tokens(&[contract]) > 0);
}

#[test]
fn openai_cache_fields_are_sent_only_for_supported_endpoint() {
    let mut request = request();
    request.session_id = Some(golutra_core::SessionId::new());
    request.cache_policy = golutra_core::PromptCachePolicy::Long;

    let supported = openai_completion_body(
        &request,
        "gpt-test",
        &ProviderGenerationConfig::default(),
        false,
        true,
    );
    assert_eq!(
        supported["prompt_cache_key"].as_str().map(str::len),
        Some(71)
    );
    assert_eq!(supported["prompt_cache_retention"], "24h");

    let custom = openai_completion_body(
        &request,
        "gpt-test",
        &ProviderGenerationConfig::default(),
        false,
        false,
    );
    assert!(custom.get("prompt_cache_key").is_none());
    assert!(custom.get("prompt_cache_retention").is_none());
}
