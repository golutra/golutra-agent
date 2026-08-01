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
            provider_id: "mock".to_owned(),
            model_id: "mock".to_owned(),
            messages: Vec::new(),
            tools: Vec::new(),
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
        "properties": {"custom": {"type": "integer"}},
        "required": ["custom"],
        "additionalProperties": false
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

    assert_eq!(schema["function"]["parameters"], input_schema);
}

#[test]
fn shell_provider_description_distinguishes_lifetime_from_initial_wait() {
    let description = provider_tool_description("shell");

    assert!(description.contains("timeout_ms is the absolute process lifetime"));
    assert!(description.contains("yield"));
    assert!(description.contains("only controls the initial wait"));
    assert!(description.contains("runtime-scoped"));
    assert!(description.contains("do not use background=true"));
    assert!(description.contains("nohup"));
    assert!(description.contains("verify it before returning"));
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
    }
}
