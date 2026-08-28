use std::{collections::BTreeMap, sync::Arc};

use async_trait::async_trait;
use golutra_auth::{AuthError, CredentialMetadata, CredentialProvider};
use golutra_core::{
    PolicyId, PromptCachePolicy, ProviderRequestId, SessionId, SideEffectType, TaskId,
    ToolContract, TurnId,
};
use golutra_llm::{
    GenaiProviderAdapter, GenaiProviderConfig, LlmProvider, OpenAiCompatibleProvider,
    OpenAiCompatibleProviderConfig, OpenAiResponsesProvider, OpenAiResponsesProviderConfig,
    ProviderError, ProviderFinishReason, ProviderGenerationConfig, ProviderMessage,
    ProviderProtocol, ProviderReasoningEffort, ProviderRequest, ProviderRole, ProviderStreamEvent,
    ProviderToolCall,
};
use secrecy::SecretString;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::oneshot,
};

const TEST_API_KEY: &str = "golden-test-key";

#[derive(Debug, Clone, Copy)]
struct ProtocolCase {
    protocol: ProviderProtocol,
    model: &'static str,
    request_fixture: &'static str,
    text_response_fixture: &'static str,
    tool_response_fixture: &'static str,
    error_response_fixture: &'static str,
    expected_auth_header: &'static str,
}

#[derive(Debug, Deserialize, PartialEq)]
struct GoldenRequest {
    path: String,
    body: Value,
}

#[derive(Debug)]
struct CapturedRequest {
    path: String,
    headers: BTreeMap<String, String>,
    body: Value,
}

struct TestProviderResponse {
    status: u16,
    content_type: &'static str,
    body: String,
    headers: Vec<(String, String)>,
}

impl TestProviderResponse {
    fn json(status: u16, body: impl Into<String>) -> Self {
        Self {
            status,
            content_type: "application/json",
            body: body.into(),
            headers: Vec::new(),
        }
    }

    fn sse(status: u16, body: impl Into<String>) -> Self {
        Self {
            status,
            content_type: "text/event-stream",
            body: body.into(),
            headers: Vec::new(),
        }
    }

    fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_owned(), value.to_owned()));
        self
    }
}

#[derive(Debug)]
struct RefreshingCredential;

#[async_trait]
impl CredentialProvider for RefreshingCredential {
    async fn credential(&self, force_refresh: bool) -> Result<SecretString, AuthError> {
        Ok(SecretString::from(
            if force_refresh {
                "golden-refreshed-key"
            } else {
                "golden-initial-key"
            }
            .to_owned(),
        ))
    }

    async fn metadata(&self) -> Result<CredentialMetadata, AuthError> {
        Ok(CredentialMetadata {
            account_id: Some("account-golden".to_owned()),
        })
    }

    fn source_label(&self) -> String {
        "golden-refreshing-credential".to_owned()
    }
}

#[tokio::test]
async fn native_provider_request_and_text_response_match_goldens() {
    for case in cases() {
        let (base_url, captured) = spawn_provider(200, case.text_response_fixture).await;
        let response = provider(case, base_url)
            .complete(comprehensive_request(case.model))
            .await
            .unwrap_or_else(|error| panic!("{} text response failed: {error}", case.protocol.id()));
        let captured = captured
            .await
            .unwrap_or_else(|_| panic!("{} request was not captured", case.protocol.id()));
        let expected: GoldenRequest = serde_json::from_str(case.request_fixture)
            .unwrap_or_else(|error| panic!("{} request fixture: {error}", case.protocol.id()));

        assert_eq!(
            GoldenRequest {
                path: captured.path,
                body: captured.body,
            },
            expected,
            "{} request golden changed",
            case.protocol.id()
        );
        assert_eq!(
            captured.headers.get(case.expected_auth_header),
            Some(&expected_auth_value(case.protocol)),
            "{} auth header",
            case.protocol.id()
        );
        assert_eq!(response.finish_reason, ProviderFinishReason::Stop);
        assert_eq!(
            response
                .message
                .as_ref()
                .map(|message| message.content.as_str()),
            Some("golden answer")
        );
        assert_eq!(response.usage.input_tokens, Some(12));
        assert_eq!(response.usage.output_tokens, Some(3));
        assert_eq!(response.usage.total_tokens, Some(15));
    }
}

#[tokio::test]
async fn native_provider_tool_calls_match_goldens() {
    for case in cases() {
        let (base_url, _captured) = spawn_provider(200, case.tool_response_fixture).await;
        let response = provider(case, base_url)
            .complete(simple_request(case.model))
            .await
            .unwrap_or_else(|error| panic!("{} tool response failed: {error}", case.protocol.id()));

        assert_eq!(response.finish_reason, ProviderFinishReason::ToolCalls);
        assert_eq!(response.tool_calls.len(), 1);
        assert_eq!(response.tool_calls[0].tool_name, "read_file");
        assert_eq!(
            response.tool_calls[0].arguments,
            json!({"path": "README.md"})
        );
        assert!(!response.tool_calls[0].tool_call_id.is_empty());
    }
}

#[tokio::test]
async fn native_provider_errors_match_goldens() {
    for case in cases() {
        let (base_url, _captured) = spawn_provider(401, case.error_response_fixture).await;
        let error = provider(case, base_url)
            .complete(simple_request(case.model))
            .await
            .expect_err("golden provider error");

        assert!(matches!(error, ProviderError::Failed { .. }));
        assert!(
            error.to_string().contains("invalid golden key"),
            "{} error was not preserved: {error}",
            case.protocol.id()
        );
        assert!(!error.to_string().contains(TEST_API_KEY));
    }
}

#[tokio::test]
async fn native_provider_rejects_oversized_captured_raw_response() {
    let case = cases()[0];
    let response = json!({
        "id": "msg_oversized",
        "type": "message",
        "role": "assistant",
        "model": case.model,
        "content": [{"type": "text", "text": "small answer"}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 1, "output_tokens": 1},
        "ignored_padding": "x".repeat(16 * 1024 * 1024),
    })
    .to_string();
    let (base_url, _captured) = spawn_provider(200, response).await;

    let error = provider(case, base_url)
        .complete(simple_request(case.model))
        .await
        .expect_err("oversized native response must be rejected");

    assert!(matches!(error, ProviderError::Malformed { .. }));
    assert!(error.to_string().contains("exceeds"));
}

#[tokio::test]
async fn openai_compatible_provider_matches_goldens() {
    let (base_url, captured) = spawn_provider(
        200,
        include_str!("fixtures/openai-compatible/text_response.json"),
    )
    .await;
    let provider = OpenAiCompatibleProvider::new(TEST_API_KEY, base_url, "gpt-golden");
    let session_id = SessionId::new();
    let mut request = comprehensive_request("gpt-golden");
    request.session_id = Some(session_id);
    let response = provider
        .complete(request)
        .await
        .expect("OpenAI-compatible text response");
    let captured = captured.await.expect("OpenAI-compatible request");
    let expected: GoldenRequest =
        serde_json::from_str(include_str!("fixtures/openai-compatible/request.json"))
            .expect("OpenAI-compatible request fixture");

    assert_eq!(
        GoldenRequest {
            path: captured.path,
            body: captured.body,
        },
        expected
    );
    assert_eq!(
        captured.headers.get("authorization"),
        Some(&format!("Bearer {TEST_API_KEY}"))
    );
    assert!(!captured.headers.contains_key("session-id"));
    assert_eq!(response.finish_reason, ProviderFinishReason::Stop);
    assert_eq!(response.usage.total_tokens, Some(15));
    assert_eq!(
        response
            .message
            .as_ref()
            .map(|message| message.content.as_str()),
        Some("golden answer")
    );

    let (base_url, _) = spawn_provider(
        200,
        include_str!("fixtures/openai-compatible/tool_response.json"),
    )
    .await;
    let response = OpenAiCompatibleProvider::new(TEST_API_KEY, base_url, "gpt-golden")
        .complete(simple_request("gpt-golden"))
        .await
        .expect("OpenAI-compatible tool response");
    assert_eq!(response.finish_reason, ProviderFinishReason::ToolCalls);
    assert_eq!(response.tool_calls[0].tool_name, "read_file");

    let (base_url, captured) = spawn_provider_attempts(
        401,
        include_str!("fixtures/openai-compatible/error_response.json"),
        2,
    )
    .await;
    let error = OpenAiCompatibleProvider::from_config_with_credential(
        OpenAiCompatibleProviderConfig {
            api_key: "<resolved-credential>".to_owned(),
            api_key_env: "golden-refreshing-credential".to_owned(),
            provider_id: "openai-compatible".to_owned(),
            base_url,
            model_id: "gpt-golden".to_owned(),
            protocol: ProviderProtocol::OpenAiCompatible,
            generation_config: ProviderGenerationConfig::default(),
            custom_headers: Default::default(),
        },
        Arc::new(RefreshingCredential),
    )
    .complete(simple_request("gpt-golden"))
    .await
    .expect_err("OpenAI-compatible error");
    let captured = captured.await.expect("OpenAI-compatible retry requests");
    assert!(matches!(error, ProviderError::Failed { .. }));
    assert!(error.to_string().contains("invalid golden key"));
    assert_eq!(captured.len(), 2);
    assert_eq!(
        captured[0].headers.get("authorization").map(String::as_str),
        Some("Bearer golden-initial-key")
    );
    assert_eq!(
        captured[1].headers.get("authorization").map(String::as_str),
        Some("Bearer golden-refreshed-key")
    );
    assert!(!error.to_string().contains("golden-refreshed-key"));
}

#[tokio::test]
async fn openai_compatible_stream_emits_ordered_text_deltas_and_usage() {
    let (base_url, captured) = spawn_provider_sequence(vec![TestProviderResponse::sse(
        200,
        include_str!("fixtures/openai-compatible/stream-response.sse"),
    )])
    .await;
    let provider = OpenAiCompatibleProvider::new(TEST_API_KEY, base_url, "gpt-golden");
    let mut events = Vec::new();
    let response = provider
        .complete_stream(simple_request("gpt-golden"), &mut |event| {
            events.push(event);
        })
        .await
        .expect("OpenAI-compatible stream response");
    let captured = captured.await.expect("stream request capture");

    assert_eq!(captured[0].body["stream"], true);
    assert_eq!(captured[0].body["stream_options"]["include_usage"], true);
    assert_eq!(
        events,
        vec![
            ProviderStreamEvent::TextDelta {
                text: "golden ".to_owned(),
            },
            ProviderStreamEvent::TextDelta {
                text: "stream".to_owned(),
            },
        ]
    );
    assert_eq!(response.finish_reason, ProviderFinishReason::Stop);
    assert_eq!(response.usage.total_tokens, Some(14));
    assert_eq!(
        response
            .message
            .as_ref()
            .map(|message| message.content.as_str()),
        Some("golden stream")
    );
}

#[tokio::test]
async fn openai_compatible_sse_error_status_is_classified_as_retryable() {
    let error_stream = concat!(
        "event: error\n",
        "data: {\"error\":{\"status\":502,\"code\":\"bad_gateway\",\"message\":\"upstream unavailable\"}}\n\n",
    );
    let (base_url, _captured) = spawn_provider_sequence(vec![
        TestProviderResponse::sse(200, error_stream)
            .header("retry-after", "1")
            .header("x-request-id", "req-sse-502"),
    ])
    .await;
    let provider = OpenAiCompatibleProvider::new(TEST_API_KEY, base_url, "gpt-golden");
    let error = provider
        .complete_stream(simple_request("gpt-golden"), &mut |_| {})
        .await
        .expect_err("SSE error event");

    assert_eq!(error.http_status(), Some(502));
    assert_eq!(error.retry_after(), Some(std::time::Duration::from_secs(1)));
    assert_eq!(
        error
            .metadata()
            .and_then(|metadata| metadata.request_id.as_deref()),
        Some("req-sse-502")
    );
}

#[tokio::test]
async fn openai_compatible_http_503_preserves_retry_after_metadata() {
    let (base_url, _captured) = spawn_provider_sequence(vec![
        TestProviderResponse::json(503, r#"{"error":{"message":"busy"}}"#)
            .header("retry-after", "2")
            .header("x-request-id", "req-http-503"),
    ])
    .await;
    let provider = OpenAiCompatibleProvider::new(TEST_API_KEY, base_url, "gpt-golden");
    let error = provider
        .complete(simple_request("gpt-golden"))
        .await
        .expect_err("HTTP 503");

    assert_eq!(error.http_status(), Some(503));
    assert_eq!(error.retry_after(), Some(std::time::Duration::from_secs(2)));
    assert!(matches!(error, ProviderError::WithMetadata { .. }));
}

#[tokio::test]
async fn openai_compatible_custom_headers_are_applied_without_debug_values() {
    let (base_url, captured) = spawn_provider(
        200,
        include_str!("fixtures/openai-compatible/text_response.json"),
    )
    .await;
    let config = OpenAiCompatibleProvider::config_from_env_reader(|key| match key {
        "GOLUTRA_PROVIDER_PROTOCOL" => Some("openai-compatible".to_owned()),
        "GOLUTRA_PROVIDER_API_KEY" => Some(TEST_API_KEY.to_owned()),
        "GOLUTRA_PROVIDER_MODEL" => Some("gpt-golden".to_owned()),
        "GOLUTRA_PROVIDER_BASE_URL" => Some(base_url.clone()),
        "GOLUTRA_PROVIDER_CUSTOM_HEADERS" => Some(
            json!({
                "X-Api-Key": "fake-supplemental-key",
                "X-Client-Name": "golutra-golden",
                "Session-Id": "external-affinity-must-not-win",
            })
            .to_string(),
        ),
        _ => None,
    })
    .expect("custom header config");
    let debug = format!("{config:?}");
    let provider = OpenAiCompatibleProvider::from_config(config);
    let session_id = SessionId::new();
    let mut request = simple_request("gpt-golden");
    request.session_id = Some(session_id);
    provider
        .complete(request)
        .await
        .expect("custom header request");
    let captured = captured.await.expect("custom header capture");

    assert_eq!(
        captured.headers.get("x-api-key").map(String::as_str),
        Some("fake-supplemental-key")
    );
    assert_eq!(
        captured.headers.get("x-client-name").map(String::as_str),
        Some("golutra-golden")
    );
    assert!(!captured.headers.contains_key("session-id"));
    assert!(!debug.contains("fake-supplemental-key"));
}

#[tokio::test]
async fn openai_responses_internal_affinity_overrides_the_custom_session_header() {
    let (base_url, captured) = spawn_provider_sequence(vec![
        TestProviderResponse::sse(
            200,
            include_str!("fixtures/openai-responses/text-response.sse"),
        ),
        TestProviderResponse::sse(
            200,
            include_str!("fixtures/openai-responses/text-response.sse"),
        ),
    ])
    .await;
    let config = OpenAiResponsesProvider::config_from_env_reader(|key| match key {
        "GOLUTRA_PROVIDER_PROTOCOL" => Some("openai-responses".to_owned()),
        "GOLUTRA_PROVIDER_API_KEY" => Some(TEST_API_KEY.to_owned()),
        "GOLUTRA_PROVIDER_MODEL" => Some("gpt-golden".to_owned()),
        "GOLUTRA_PROVIDER_BASE_URL" => Some(base_url.clone()),
        "GOLUTRA_PROVIDER_CUSTOM_HEADERS" => {
            Some(json!({"Session-Id": "external-affinity-must-not-win"}).to_string())
        }
        _ => None,
    })
    .expect("Responses custom header config");
    let provider = OpenAiResponsesProvider::from_config(config);
    let session_id = SessionId::new();
    let session_header = session_id.to_string();
    let mut request = simple_request("gpt-golden");
    request.session_id = Some(session_id);

    provider
        .complete(request.clone())
        .await
        .expect("Responses custom header request");
    request.cache_policy = PromptCachePolicy::None;
    provider
        .complete(request)
        .await
        .expect("Responses cache-disabled custom header request");
    let captured = captured.await.expect("Responses custom header capture");
    assert_eq!(
        captured[0].headers.get("session-id").map(String::as_str),
        Some(session_header.as_str())
    );
    assert!(!captured[1].headers.contains_key("session-id"));
}

#[tokio::test]
async fn openai_responses_provider_matches_sse_goldens_and_auth_headers() {
    let (base_url, captured) = spawn_provider_sequence(vec![TestProviderResponse::sse(
        200,
        include_str!("fixtures/openai-responses/text-response.sse"),
    )])
    .await;
    let provider = openai_responses_provider(base_url);
    let mut stream_events = Vec::new();
    let response = provider
        .complete_stream(simple_request("gpt-golden"), &mut |event| {
            stream_events.push(event);
        })
        .await
        .expect("Responses text response");
    let captured = captured.await.expect("Responses request capture");
    let captured = &captured[0];
    let expected: Value =
        serde_json::from_str(include_str!("fixtures/openai-responses/request.json"))
            .expect("Responses request fixture");

    assert_eq!(captured.path, "/responses");
    assert_eq!(captured.body, expected);
    assert_eq!(
        captured.headers.get("authorization").map(String::as_str),
        Some("Bearer golden-initial-key")
    );
    assert_eq!(
        captured
            .headers
            .get("chatgpt-account-id")
            .map(String::as_str),
        Some("account-golden")
    );
    assert_eq!(
        captured.headers.get("originator").map(String::as_str),
        Some("golutra")
    );
    assert!(captured.headers.contains_key("session-id"));
    assert_eq!(response.finish_reason, ProviderFinishReason::Stop);
    assert_eq!(
        stream_events,
        vec![
            ProviderStreamEvent::TextDelta {
                text: "golden ".to_owned(),
            },
            ProviderStreamEvent::TextDelta {
                text: "answer".to_owned(),
            },
        ]
    );
    assert_eq!(
        response
            .message
            .as_ref()
            .map(|message| message.content.as_str()),
        Some("golden answer")
    );
    assert_eq!(response.usage.input_tokens, Some(12));
    assert_eq!(response.usage.cached_input_tokens, Some(4));
    assert_eq!(response.usage.output_tokens, Some(3));
    assert_eq!(response.usage.reasoning_tokens, Some(1));
    assert_eq!(response.usage.total_tokens, Some(15));
    assert_eq!(response.raw_metadata["response_id"], "resp_golden_text");

    let (base_url, _captured) = spawn_provider_sequence(vec![TestProviderResponse::sse(
        200,
        include_str!("fixtures/openai-responses/tool-response.sse"),
    )])
    .await;
    let mut tool_stream_events = Vec::new();
    let response = openai_responses_provider(base_url)
        .complete_stream(simple_request("gpt-golden"), &mut |event| {
            tool_stream_events.push(event);
        })
        .await
        .expect("Responses tool response");
    assert_eq!(response.finish_reason, ProviderFinishReason::ToolCalls);
    assert_eq!(response.tool_calls.len(), 1);
    assert_eq!(response.tool_calls[0].tool_name, "read_file");
    assert_eq!(
        response.tool_calls[0].arguments,
        json!({"path": "README.md"})
    );
    assert_eq!(
        tool_stream_events,
        vec![ProviderStreamEvent::ToolCallDelta {
            index: 0,
            tool_call_id: Some("call_read_1".to_owned()),
            tool_name: Some("read_file".to_owned()),
        }]
    );
    let replay_items = &response
        .message
        .as_ref()
        .expect("Responses replay metadata")
        .metadata
        .openai_responses_replay_items;
    assert_eq!(replay_items.len(), 1);
    assert_eq!(replay_items[0]["type"], "reasoning");
    assert_eq!(
        replay_items[0]["encrypted_content"],
        "encrypted-golden-reasoning"
    );

    let (base_url, captured) = spawn_provider_sequence(vec![TestProviderResponse::sse(
        200,
        include_str!("fixtures/openai-responses/text-response.sse"),
    )])
    .await;
    let mut continuation = simple_request("gpt-golden");
    continuation.messages = vec![
        ProviderMessage {
            role: ProviderRole::Assistant,
            content: String::new(),
            tool_call_id: None,
            tool_name: None,
            tool_calls: response.tool_calls,
            metadata: response
                .message
                .expect("Responses assistant message")
                .metadata,
        },
        ProviderMessage {
            role: ProviderRole::Tool,
            content: serde_json::json!({"summary": "project file read"}).to_string(),
            tool_call_id: Some("call_read_1".to_owned()),
            tool_name: Some("read_file".to_owned()),
            tool_calls: Vec::new(),
            metadata: Default::default(),
        },
    ];
    openai_responses_provider(base_url)
        .complete(continuation)
        .await
        .expect("Responses continuation");
    let captured = captured.await.expect("Responses continuation capture");
    assert_eq!(captured[0].body["input"][0]["type"], "reasoning");
    assert_eq!(
        captured[0].body["input"][0]["encrypted_content"],
        "encrypted-golden-reasoning"
    );
    assert_eq!(captured[0].body["input"][1]["type"], "function_call");
    assert_eq!(captured[0].body["input"][2]["type"], "function_call_output");

    let (base_url, _captured) = spawn_provider_sequence(vec![TestProviderResponse::sse(
        200,
        include_str!("fixtures/openai-responses/error-response.sse"),
    )])
    .await;
    let error = openai_responses_provider(base_url)
        .complete(simple_request("gpt-golden"))
        .await
        .expect_err("Responses SSE error");
    assert!(matches!(error, ProviderError::Failed { .. }));
    assert!(error.to_string().contains("golden responses failure"));
}

#[tokio::test]
async fn openai_responses_auto_summary_preserves_reasoning_effort_and_parallel_tools() {
    let (base_url, captured) = spawn_provider_sequence(vec![TestProviderResponse::sse(
        200,
        include_str!("fixtures/openai-responses/text-response.sse"),
    )])
    .await;
    let provider = OpenAiResponsesProvider::from_config(OpenAiResponsesProviderConfig {
        api_key: TEST_API_KEY.to_owned(),
        api_key_env: "GOLDEN_TEST_API_KEY".to_owned(),
        provider_id: "openai-responses-golden".to_owned(),
        base_url,
        model_id: "gpt-golden".to_owned(),
        generation_config: ProviderGenerationConfig {
            reasoning_effort: Some(ProviderReasoningEffort::High),
            ..ProviderGenerationConfig::default()
        },
        custom_headers: Default::default(),
    });

    provider
        .complete(comprehensive_request("gpt-golden"))
        .await
        .expect("Responses reasoning options request");
    let captured = captured.await.expect("Responses reasoning options capture");
    let body = &captured[0].body;

    assert_eq!(body["reasoning"]["summary"], "auto");
    assert_eq!(body["reasoning"]["effort"], "high");
    assert_eq!(body["parallel_tool_calls"], true);
    assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
}

#[tokio::test]
async fn openai_responses_completion_and_probe_refresh_once_after_401() {
    let unauthorized = serde_json::json!({
        "error": {"code": "token_expired", "message": "access token expired"}
    })
    .to_string();
    let (base_url, captured) = spawn_provider_sequence(vec![
        TestProviderResponse::json(401, unauthorized.clone()),
        TestProviderResponse::sse(
            200,
            include_str!("fixtures/openai-responses/text-response.sse"),
        ),
    ])
    .await;
    openai_responses_provider(base_url)
        .complete(simple_request("gpt-golden"))
        .await
        .expect("Responses completion refresh");
    let captured = captured.await.expect("completion retry capture");
    assert_eq!(captured.len(), 2);
    assert_eq!(
        captured[0].headers.get("authorization").map(String::as_str),
        Some("Bearer golden-initial-key")
    );
    assert_eq!(
        captured[1].headers.get("authorization").map(String::as_str),
        Some("Bearer golden-refreshed-key")
    );

    let (base_url, captured) = spawn_provider_sequence(vec![
        TestProviderResponse::json(401, unauthorized),
        TestProviderResponse::json(
            200,
            serde_json::json!({"data": [{"id": "gpt-golden"}]}).to_string(),
        ),
    ])
    .await;
    let probe = openai_responses_provider(base_url)
        .probe()
        .await
        .expect("Responses probe refresh");
    let captured = captured.await.expect("probe retry capture");
    assert_eq!(captured.len(), 2);
    assert!(
        captured
            .iter()
            .all(|request| request.path.starts_with("/models?"))
    );
    assert_eq!(probe.model_available, Some(true));
    assert_eq!(
        captured[1].headers.get("authorization").map(String::as_str),
        Some("Bearer golden-refreshed-key")
    );
}

#[tokio::test]
async fn openai_responses_forces_rust_genai_responses_routing_for_grok_model() {
    let (base_url, captured) = spawn_provider_sequence(vec![TestProviderResponse::sse(
        200,
        include_str!("fixtures/openai-responses/text-response.sse"),
    )])
    .await;

    openai_responses_provider_with_model(base_url, "grok-4.5")
        .complete(simple_request("grok-4.5"))
        .await
        .expect("grok Responses request");
    let captured = captured.await.expect("grok Responses capture");

    assert_eq!(captured[0].path, "/responses");
    assert_eq!(captured[0].body["model"], "grok-4.5");
    assert_eq!(captured[0].body["stream"], true);
}

#[tokio::test]
async fn openai_responses_projects_stable_cache_identity_and_retention() {
    let response = include_str!("fixtures/openai-responses/text-response.sse");
    let (base_url, captured) = spawn_provider_sequence(vec![
        TestProviderResponse::sse(200, response),
        TestProviderResponse::sse(200, response),
        TestProviderResponse::sse(200, response),
        TestProviderResponse::sse(200, response),
    ])
    .await;
    let provider = openai_responses_provider(base_url);
    let session_id = SessionId::new();
    let mut first = simple_request("gpt-golden");
    first.session_id = Some(session_id);
    first.cache_policy = PromptCachePolicy::Long;

    provider
        .complete(first.clone())
        .await
        .expect("first cached Responses request");
    provider
        .complete(first.clone())
        .await
        .expect("second cached Responses request");

    let mut other_session = first.clone();
    other_session.session_id = Some(SessionId::new());
    provider
        .complete(other_session)
        .await
        .expect("isolated cached Responses request");

    let mut disabled = first;
    disabled.cache_policy = PromptCachePolicy::None;
    provider
        .complete(disabled)
        .await
        .expect("cache-disabled Responses request");

    let requests = captured.await.expect("Responses cache request capture");
    assert_eq!(requests.len(), 4);
    let first_key = requests[0].body["prompt_cache_key"]
        .as_str()
        .expect("first request cache key");
    let second_key = requests[1].body["prompt_cache_key"]
        .as_str()
        .expect("second request cache key");
    let other_key = requests[2].body["prompt_cache_key"]
        .as_str()
        .expect("isolated request cache key");
    assert_eq!(first_key, session_id.to_string());
    assert_eq!(first_key, second_key);
    assert_ne!(first_key, other_key);
    let session_header = session_id.to_string();
    for request in &requests[..3] {
        assert_eq!(request.path, "/responses");
        assert_eq!(request.body["prompt_cache_retention"], "24h");
    }
    assert_eq!(
        requests[0].headers.get("session-id").map(String::as_str),
        Some(session_header.as_str())
    );
    assert_eq!(
        requests[1].headers.get("session-id").map(String::as_str),
        Some(session_header.as_str())
    );
    assert!(requests[3].body.get("prompt_cache_key").is_none());
    assert!(requests[3].body.get("prompt_cache_retention").is_none());
    assert!(!requests[3].headers.contains_key("session-id"));
}

#[tokio::test]
async fn openai_responses_rejects_stream_without_completed_response_id() {
    let truncated = concat!(
        "event: response.output_text.delta\n",
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"partial\"}\n\n",
        "data: [DONE]\n\n",
    );
    let (base_url, _captured) =
        spawn_provider_sequence(vec![TestProviderResponse::sse(200, truncated)]).await;

    let error = openai_responses_provider(base_url)
        .complete(simple_request("gpt-golden"))
        .await
        .expect_err("truncated Responses stream");

    assert!(matches!(error, ProviderError::Unavailable { .. }));
    assert!(error.to_string().contains("response.completed"));
}

#[tokio::test]
async fn openai_responses_rejects_oversized_text_before_streaming_it() {
    const PROVIDER_MESSAGE_LIMIT: usize = 128 * 1024;
    let response = format!(
        "event: response.output_text.delta\ndata: {}\n\n",
        json!({
            "type": "response.output_text.delta",
            "delta": "x".repeat(PROVIDER_MESSAGE_LIMIT + 1),
        })
    );
    let (base_url, _captured) =
        spawn_provider_sequence(vec![TestProviderResponse::sse(200, response)]).await;
    let mut events = Vec::new();

    let error = openai_responses_provider(base_url)
        .complete_stream(simple_request("gpt-golden"), &mut |event| {
            events.push(event)
        })
        .await
        .expect_err("oversized Responses text");

    assert!(matches!(error, ProviderError::Malformed { .. }));
    assert!(error.to_string().contains("assistant message exceeds"));
    assert!(events.is_empty(), "oversized text must not reach consumers");
}

#[tokio::test]
async fn github_copilot_adapter_adds_provider_required_headers() {
    let (base_url, captured) = spawn_provider(
        200,
        include_str!("fixtures/openai-compatible/text_response.json"),
    )
    .await;
    let provider = OpenAiCompatibleProvider::from_config_with_credential(
        OpenAiCompatibleProviderConfig {
            api_key: "<resolved-credential>".to_owned(),
            api_key_env: "golden-refreshing-credential".to_owned(),
            provider_id: "github-copilot".to_owned(),
            base_url,
            model_id: "gpt-golden".to_owned(),
            protocol: ProviderProtocol::OpenAiCompatible,
            generation_config: ProviderGenerationConfig::default(),
            custom_headers: Default::default(),
        },
        Arc::new(RefreshingCredential),
    );

    provider
        .complete(simple_request("gpt-golden"))
        .await
        .expect("Copilot completion");
    let captured = captured.await.expect("Copilot request");

    assert_eq!(
        captured
            .headers
            .get("x-github-api-version")
            .map(String::as_str),
        Some("2026-06-01")
    );
    assert_eq!(
        captured.headers.get("openai-intent").map(String::as_str),
        Some("conversation-edits")
    );
    assert_eq!(
        captured.headers.get("x-initiator").map(String::as_str),
        Some("user")
    );
    assert!(
        captured
            .headers
            .get("user-agent")
            .is_some_and(|value| value.starts_with("golutra/"))
    );
}

#[tokio::test]
async fn live_provider_smoke_is_opt_in_and_never_reads_normal_user_credentials() {
    let openai_key = std::env::var("GOLUTRA_LIVE_OPENAI_COMPATIBLE_API_KEY").ok();
    let openai_base_url = std::env::var("GOLUTRA_LIVE_OPENAI_COMPATIBLE_BASE_URL").ok();
    let openai_model = std::env::var("GOLUTRA_LIVE_OPENAI_COMPATIBLE_MODEL").ok();
    if let (Some(api_key), Some(base_url), Some(model)) =
        (openai_key, openai_base_url, openai_model)
    {
        let response = OpenAiCompatibleProvider::new(api_key, base_url, model.clone())
            .complete(simple_request(&model))
            .await
            .expect("OpenAI-compatible live smoke");
        assert!(response.message.is_some() || !response.tool_calls.is_empty());
    } else {
        eprintln!("skipping openai-compatible live smoke: dedicated env is incomplete");
    }
    let responses_key = std::env::var("GOLUTRA_LIVE_OPENAI_RESPONSES_API_KEY").ok();
    let responses_base_url = std::env::var("GOLUTRA_LIVE_OPENAI_RESPONSES_BASE_URL").ok();
    let responses_model = std::env::var("GOLUTRA_LIVE_OPENAI_RESPONSES_MODEL").ok();
    if let (Some(api_key), Some(base_url), Some(model)) =
        (responses_key, responses_base_url, responses_model)
    {
        let response = OpenAiResponsesProvider::from_config(OpenAiResponsesProviderConfig {
            api_key,
            api_key_env: "GOLUTRA_LIVE_OPENAI_RESPONSES_API_KEY".to_owned(),
            provider_id: "openai-responses-live".to_owned(),
            base_url,
            model_id: model.clone(),
            generation_config: ProviderGenerationConfig {
                max_tokens: Some(32),
                ..ProviderGenerationConfig::default()
            },
            custom_headers: Default::default(),
        })
        .complete(simple_request(&model))
        .await
        .expect("OpenAI Responses live smoke");
        assert!(response.message.is_some() || !response.tool_calls.is_empty());
    } else {
        eprintln!("skipping OpenAI Responses live smoke: dedicated env is incomplete");
    }
    for protocol in [
        ProviderProtocol::Anthropic,
        ProviderProtocol::Gemini,
        ProviderProtocol::VertexAi,
        ProviderProtocol::Genai,
    ] {
        let prefix = format!(
            "GOLUTRA_LIVE_{}",
            protocol.id().replace('-', "_").to_ascii_uppercase()
        );
        let key = std::env::var(format!("{prefix}_API_KEY")).ok();
        let base_url = std::env::var(format!("{prefix}_BASE_URL")).ok();
        let model = std::env::var(format!("{prefix}_MODEL")).ok();
        let (Some(api_key), Some(base_url), Some(model)) = (key, base_url, model) else {
            eprintln!(
                "skipping {} live smoke: dedicated env is incomplete",
                protocol.id()
            );
            continue;
        };
        let provider = GenaiProviderAdapter::from_config(GenaiProviderConfig {
            api_key,
            api_key_env: format!("{prefix}_API_KEY"),
            base_url,
            model_id: model.clone(),
            protocol,
            generation_config: ProviderGenerationConfig {
                max_tokens: Some(8),
                ..ProviderGenerationConfig::default()
            },
            custom_headers: Default::default(),
        });

        let response = provider
            .complete(simple_request(&model))
            .await
            .unwrap_or_else(|error| panic!("{} live smoke failed: {error}", protocol.id()));

        assert!(response.message.is_some() || !response.tool_calls.is_empty());
    }
}

fn cases() -> [ProtocolCase; 4] {
    [
        ProtocolCase {
            protocol: ProviderProtocol::Anthropic,
            model: "claude-test",
            request_fixture: include_str!("fixtures/anthropic/request.json"),
            text_response_fixture: include_str!("fixtures/anthropic/text_response.json"),
            tool_response_fixture: include_str!("fixtures/anthropic/tool_response.json"),
            error_response_fixture: include_str!("fixtures/anthropic/error_response.json"),
            expected_auth_header: "x-api-key",
        },
        ProtocolCase {
            protocol: ProviderProtocol::Gemini,
            model: "gemini-test",
            request_fixture: include_str!("fixtures/gemini/request.json"),
            text_response_fixture: include_str!("fixtures/gemini/text_response.json"),
            tool_response_fixture: include_str!("fixtures/gemini/tool_response.json"),
            error_response_fixture: include_str!("fixtures/gemini/error_response.json"),
            expected_auth_header: "x-goog-api-key",
        },
        ProtocolCase {
            protocol: ProviderProtocol::VertexAi,
            model: "gemini-test",
            request_fixture: include_str!("fixtures/vertex-ai/request.json"),
            text_response_fixture: include_str!("fixtures/vertex-ai/text_response.json"),
            tool_response_fixture: include_str!("fixtures/vertex-ai/tool_response.json"),
            error_response_fixture: include_str!("fixtures/vertex-ai/error_response.json"),
            expected_auth_header: "authorization",
        },
        ProtocolCase {
            protocol: ProviderProtocol::Genai,
            model: "deepseek-chat",
            request_fixture: include_str!("fixtures/genai/request.json"),
            text_response_fixture: include_str!("fixtures/genai/text_response.json"),
            tool_response_fixture: include_str!("fixtures/genai/tool_response.json"),
            error_response_fixture: include_str!("fixtures/genai/error_response.json"),
            expected_auth_header: "authorization",
        },
    ]
}

fn provider(case: ProtocolCase, base_url: String) -> GenaiProviderAdapter {
    let base_url = match case.protocol {
        ProviderProtocol::Anthropic => format!("{base_url}/v1"),
        ProviderProtocol::Gemini => format!("{base_url}/v1beta"),
        ProviderProtocol::VertexAi => {
            format!("{base_url}/v1/projects/golden-project/locations/us-central1")
        }
        ProviderProtocol::Genai => base_url,
        ProviderProtocol::Mock
        | ProviderProtocol::OpenAiCompatible
        | ProviderProtocol::OpenAiResponses => {
            unreachable!("native golden cases only contain native protocols")
        }
    };
    GenaiProviderAdapter::from_config(GenaiProviderConfig {
        api_key: TEST_API_KEY.to_owned(),
        api_key_env: "GOLDEN_TEST_API_KEY".to_owned(),
        base_url,
        model_id: case.model.to_owned(),
        protocol: case.protocol,
        generation_config: ProviderGenerationConfig {
            max_tokens: Some(128),
            ..ProviderGenerationConfig::default()
        },
        custom_headers: Default::default(),
    })
}

fn openai_responses_provider(base_url: String) -> OpenAiResponsesProvider {
    openai_responses_provider_with_model(base_url, "gpt-golden")
}

fn openai_responses_provider_with_model(
    base_url: String,
    model_id: &str,
) -> OpenAiResponsesProvider {
    OpenAiResponsesProvider::from_config_with_credential(
        OpenAiResponsesProviderConfig {
            api_key: "<resolved-credential>".to_owned(),
            api_key_env: "golden-refreshing-credential".to_owned(),
            provider_id: "openai-chatgpt".to_owned(),
            base_url,
            model_id: model_id.to_owned(),
            generation_config: ProviderGenerationConfig::default(),
            custom_headers: Default::default(),
        },
        Arc::new(RefreshingCredential),
    )
}

fn comprehensive_request(model: &str) -> ProviderRequest {
    let mut request = simple_request(model);
    request.messages = vec![
        message(ProviderRole::System, "You are concise."),
        message(ProviderRole::User, "Read the project file."),
        ProviderMessage {
            role: ProviderRole::Assistant,
            content: String::new(),
            tool_call_id: None,
            tool_name: None,
            tool_calls: vec![ProviderToolCall {
                tool_call_id: "call-read-1".to_owned(),
                tool_name: "read_file".to_owned(),
                arguments: json!({"path": "README.md"}),
            }],
            metadata: Default::default(),
        },
        ProviderMessage {
            role: ProviderRole::Tool,
            content: json!({"summary": "project file read"}).to_string(),
            tool_call_id: Some("call-read-1".to_owned()),
            tool_name: Some("read_file".to_owned()),
            tool_calls: Vec::new(),
            metadata: Default::default(),
        },
        message(ProviderRole::User, "Summarize it."),
    ];
    request.tools = vec![ToolContract {
        tool_name: "read_file".to_owned(),
        input_schema: json!({
            "type": "object",
            "properties": {"path": {"type": "string"}},
            "required": ["path"]
        }),
        output_schema: json!({"type": "object"}),
        error_schema: json!({"type": "object"}),
        side_effect_type: SideEffectType::None,
        idempotency_key_policy: "not_required".to_owned(),
        timeout_policy: "bounded".to_owned(),
        cancellation_policy: "supported".to_owned(),
        retry_policy: "none".to_owned(),
        artifact_policy: "none".to_owned(),
        permission_policy_ref: Some(PolicyId::new()),
    }];
    request
}

fn simple_request(model: &str) -> ProviderRequest {
    ProviderRequest {
        request_id: ProviderRequestId::new(),
        task_id: TaskId::new(),
        turn_id: TurnId::new(),
        session_id: None,
        cache_scope: None,
        provider_id: "golden".to_owned(),
        model_id: model.to_owned(),
        messages: vec![message(ProviderRole::User, "Say hello.")],
        tools: Vec::new(),
        cache_policy: Default::default(),
        max_output_tokens: None,
    }
}

fn message(role: ProviderRole, content: &str) -> ProviderMessage {
    ProviderMessage {
        role,
        content: content.to_owned(),
        tool_call_id: None,
        tool_name: None,
        tool_calls: Vec::new(),
        metadata: Default::default(),
    }
}

fn expected_auth_value(protocol: ProviderProtocol) -> String {
    if matches!(
        protocol,
        ProviderProtocol::VertexAi | ProviderProtocol::Genai
    ) {
        format!("Bearer {TEST_API_KEY}")
    } else {
        TEST_API_KEY.to_owned()
    }
}

async fn spawn_provider(
    status: u16,
    response_body: impl Into<String>,
) -> (String, oneshot::Receiver<CapturedRequest>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind provider");
    let address = listener.local_addr().expect("provider address");
    let (sender, receiver) = oneshot::channel();
    let response_body = response_body.into();
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("provider connection");
        let request = read_request(&mut stream).await;
        let reason = if status == 200 { "OK" } else { "Unauthorized" };
        let response = format!(
            "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{response_body}",
            response_body.len()
        );
        stream
            .write_all(response.as_bytes())
            .await
            .expect("provider response");
        let _ = sender.send(request);
    });
    (format!("http://{address}"), receiver)
}

async fn spawn_provider_attempts(
    status: u16,
    response_body: impl Into<String>,
    attempts: usize,
) -> (String, oneshot::Receiver<Vec<CapturedRequest>>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind provider");
    let address = listener.local_addr().expect("provider address");
    let (sender, receiver) = oneshot::channel();
    let response_body = response_body.into();
    tokio::spawn(async move {
        let mut requests = Vec::with_capacity(attempts);
        for _ in 0..attempts {
            let (mut stream, _) = listener.accept().await.expect("provider connection");
            requests.push(read_request(&mut stream).await);
            let reason = if status == 200 { "OK" } else { "Unauthorized" };
            let response = format!(
                "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{response_body}",
                response_body.len()
            );
            stream
                .write_all(response.as_bytes())
                .await
                .expect("provider response");
        }
        let _ = sender.send(requests);
    });
    (format!("http://{address}"), receiver)
}

async fn spawn_provider_sequence(
    responses: Vec<TestProviderResponse>,
) -> (String, oneshot::Receiver<Vec<CapturedRequest>>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind provider");
    let address = listener.local_addr().expect("provider address");
    let (sender, receiver) = oneshot::channel();
    tokio::spawn(async move {
        let mut requests = Vec::with_capacity(responses.len());
        for response in responses {
            let (mut stream, _) = listener.accept().await.expect("provider connection");
            requests.push(read_request(&mut stream).await);
            let reason = if response.status == 200 {
                "OK"
            } else {
                "Unauthorized"
            };
            let message = format!(
                "HTTP/1.1 {} {reason}\r\ncontent-type: {}\r\n{}content-length: {}\r\nconnection: close\r\n\r\n{}",
                response.status,
                response.content_type,
                response
                    .headers
                    .iter()
                    .map(|(name, value)| format!("{name}: {value}\r\n"))
                    .collect::<String>(),
                response.body.len(),
                response.body
            );
            stream
                .write_all(message.as_bytes())
                .await
                .expect("provider response");
        }
        let _ = sender.send(requests);
    });
    (format!("http://{address}"), receiver)
}

async fn read_request(stream: &mut tokio::net::TcpStream) -> CapturedRequest {
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 4096];
    let header_end = loop {
        let read = stream
            .read(&mut chunk)
            .await
            .expect("read provider request");
        assert!(read > 0, "provider request ended before headers");
        bytes.extend_from_slice(&chunk[..read]);
        if let Some(index) = find_bytes(&bytes, b"\r\n\r\n") {
            break index + 4;
        }
    };
    let headers_text = String::from_utf8(bytes[..header_end].to_vec()).expect("request headers");
    let mut lines = headers_text.split("\r\n");
    let request_line = lines.next().expect("request line");
    let path = request_line
        .split_whitespace()
        .nth(1)
        .expect("request path")
        .to_owned();
    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_owned()))
        .collect::<BTreeMap<_, _>>();
    let content_length = headers
        .get("content-length")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or_default();
    while bytes.len() < header_end + content_length {
        let read = stream.read(&mut chunk).await.expect("read provider body");
        assert!(read > 0, "provider request body was truncated");
        bytes.extend_from_slice(&chunk[..read]);
    }
    let body = if content_length == 0 {
        Value::Null
    } else {
        serde_json::from_slice(&bytes[header_end..header_end + content_length])
            .expect("provider JSON body")
    };
    CapturedRequest {
        path,
        headers,
        body,
    }
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}
