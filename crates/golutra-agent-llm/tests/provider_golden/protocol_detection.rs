//! 以真实协议适配器和本地 HTTP 验证自动选择、失败边界及独立探测请求。

use super::*;
use golutra_agent_llm::detect_provider_protocol;

const ORDER: &[ProviderProtocol] = &[
    ProviderProtocol::OpenAiResponses,
    ProviderProtocol::Anthropic,
    ProviderProtocol::Gemini,
    ProviderProtocol::OpenAiCompatible,
    ProviderProtocol::VertexAi,
    ProviderProtocol::Genai,
];

#[tokio::test]
async fn detection_accepts_native_protocols_after_unavailable_routes() {
    use super::terminal_contract::{Wire, stream};
    for (skipped, protocol, wire, model, reason) in [
        (
            1,
            ProviderProtocol::Anthropic,
            Wire::Anthropic,
            "claude-test",
            "end_turn",
        ),
        (
            2,
            ProviderProtocol::Gemini,
            Wire::Gemini,
            "gemini-test",
            "STOP",
        ),
    ] {
        let mut responses: Vec<_> = (0..skipped)
            .map(|_| {
                TestProviderResponse::json(
                    404,
                    r#"{"error":{"type":"not_found_error","message":"Unknown endpoint"}}"#,
                )
            })
            .collect();
        responses.push(TestProviderResponse::sse(
            200,
            stream(wire, Some(reason), false, true),
        ));
        let (url, captured) = spawn_provider_sequence(responses).await;
        assert_eq!(
            detect_provider_protocol(ORDER, &url, model, TEST_API_KEY, None, vec![])
                .await
                .unwrap(),
            protocol
        );
        assert_eq!(captured.await.unwrap().len(), skipped + 1);
    }
}

#[tokio::test]
async fn detection_times_out_without_trying_a_different_protocol() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let run = tokio::spawn(async move {
        detect_provider_protocol(ORDER, &url, "gpt-test", TEST_API_KEY, None, vec![]).await
    });
    let (mut socket, _) =
        tokio::time::timeout(std::time::Duration::from_secs(5), listener.accept())
            .await
            .unwrap()
            .unwrap();
    let request = read_request(&mut socket).await;
    assert!(request.path.ends_with("/responses"));
    let error = tokio::time::timeout(std::time::Duration::from_secs(25), run)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert!(error.contains("timed out"), "{error}");
    assert!(error.contains("unconfirmed, not unsupported"), "{error}");
    assert!(error.contains("save without detection"), "{error}");
}

#[tokio::test]
async fn detection_accepts_responses_generation_without_catalog_or_project_context() {
    let (url, captured) = spawn_provider_sequence(vec![TestProviderResponse::sse(
        200,
        include_str!("../fixtures/openai-responses/text-response.sse"),
    )])
    .await;
    let result = detect_provider_protocol(ORDER, &url, "gpt-test", TEST_API_KEY, None, vec![])
        .await
        .unwrap();
    assert_eq!(result, ProviderProtocol::OpenAiResponses);
    let requests = captured.await.unwrap();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].path.ends_with("/responses"));
    assert_eq!(requests[0].body["stream"], true);
    assert_eq!(requests[0].body["input"].as_array().unwrap().len(), 1);
    assert!(requests[0].body.to_string().contains("Reply with OK."));
    assert!(requests[0].body.get("tools").is_none());
}

#[tokio::test]
async fn detection_tries_only_unsupported_endpoints_in_supplied_order() {
    let unsupported = r#"{"error":{"message":"Endpoint unavailable","type":"not_found_error"}}"#;
    let (url, captured) = spawn_provider_sequence(vec![
        TestProviderResponse::json(404, unsupported),
        TestProviderResponse::json(405, unsupported),
        TestProviderResponse::json(415, unsupported),
        TestProviderResponse::sse(
            200,
            include_str!("../fixtures/openai-compatible/stream-response.sse"),
        ),
    ])
    .await;
    let result = detect_provider_protocol(ORDER, &url, "test-model", TEST_API_KEY, None, vec![])
        .await
        .unwrap();
    assert_eq!(result, ProviderProtocol::OpenAiCompatible);
    let requests = captured.await.unwrap();
    assert_eq!(requests.len(), 4);
    assert!(requests[0].path.contains("/responses"));
    assert!(requests[1].path.contains("/messages"));
    assert!(requests[2].path.contains("streamGenerateContent"));
    assert!(requests[3].path.contains("/chat/completions"));
}

#[tokio::test]
async fn detection_stops_on_auth_rate_limit_and_service_errors() {
    for status in [401, 403, 429, 500, 502] {
        let (url, captured) = spawn_provider_sequence(vec![TestProviderResponse::json(
            status,
            r#"{"error":{"message":"test authentication or service failure"}}"#,
        )])
        .await;
        let error = detect_provider_protocol(ORDER, &url, "gpt-test", TEST_API_KEY, None, vec![])
            .await
            .unwrap_err();
        assert!(error.contains("openai-responses"), "{error}");
        assert!(error.contains(&status.to_string()), "{error}");
        assert_eq!(captured.await.unwrap().len(), 1);
    }
}

#[tokio::test]
async fn detection_stops_on_stream_refusal_and_redacts_echoed_key() {
    let body = format!(
        "event: response.failed\ndata: {{\"type\":\"response.failed\",\"response\":{{\"status\":\"failed\",\"error\":{{\"code\":\"invalid_prompt\",\"message\":\"upstream rejected {TEST_API_KEY}\"}}}}}}\n\n"
    );
    let (url, captured) = spawn_provider_sequence(vec![TestProviderResponse::sse(200, body)]).await;
    let error = detect_provider_protocol(ORDER, &url, "gpt-test", TEST_API_KEY, None, vec![])
        .await
        .unwrap_err();
    assert!(!error.contains(TEST_API_KEY), "{error}");
    assert!(error.contains("REDACTED"), "{error}");
    assert_eq!(captured.await.unwrap().len(), 1);
}

#[tokio::test]
async fn fixed_credentials_preserve_first_unauthorized_error_on_every_wire() {
    for protocol in &ORDER[..4] {
        let (url, captured) = spawn_provider_sequence(vec![TestProviderResponse::json(
            401,
            r#"{"error":{"message":"Invalid test credential"}}"#,
        )])
        .await;
        let error =
            detect_provider_protocol(&[*protocol], &url, "test-model", TEST_API_KEY, None, vec![])
                .await
                .unwrap_err();
        assert!(error.contains("401"), "{protocol:?}: {error}");
        assert_eq!(captured.await.unwrap().len(), 1);
    }
}

#[tokio::test]
async fn detection_does_not_confuse_business_errors_with_unsupported_protocols() {
    for (status, body) in [
        (
            404,
            r#"{"error":{"code":"model_not_found","message":"Unknown model"}}"#,
        ),
        (
            404,
            r#"{"error":{"type":"invalid_prompt","message":"Request rejected"}}"#,
        ),
        (
            500,
            r#"{"error":{"code":"unsupported_protocol","message":"Internal failure"}}"#,
        ),
    ] {
        let (url, captured) =
            spawn_provider_sequence(vec![TestProviderResponse::json(status, body)]).await;
        let error = detect_provider_protocol(ORDER, &url, "gpt-test", TEST_API_KEY, None, vec![])
            .await
            .unwrap_err();
        assert!(error.contains("openai-responses"), "{error}");
        assert!(!error.contains("No supported protocol"), "{error}");
        assert_eq!(captured.await.unwrap().len(), 1);
    }
}

#[tokio::test]
async fn detection_does_not_probe_adapters_requiring_other_credentials() {
    let error = detect_provider_protocol(
        &[ProviderProtocol::VertexAi, ProviderProtocol::Genai],
        "http://127.0.0.1:1",
        "model",
        TEST_API_KEY,
        None,
        vec![],
    )
    .await
    .unwrap_err();
    assert!(error.contains("No supported protocol"));
}
