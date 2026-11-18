//! Capacity classification through real local Responses HTTP/SSE frames.
use super::*;

async fn observe(payload: Value) -> ProviderError {
    let body = format!("event: error\ndata: {payload}\n\n");
    let (base_url, captured) =
        spawn_provider_sequence(vec![TestProviderResponse::sse(200, body)]).await;
    let error = openai_responses_provider(base_url)
        .complete_stream(simple_request("gpt-golden"), &mut |_| {})
        .await
        .expect_err("fixture returns a business error");
    assert_eq!(captured.await.unwrap().len(), 1);
    error
}

fn transient(error: &ProviderError) -> bool {
    match error {
        ProviderError::Unavailable { .. } | ProviderError::RateLimited { .. } => true,
        ProviderError::WithMetadata { error, .. } => transient(error),
        _ => false,
    }
}

#[tokio::test]
async fn original_capacity_message_becomes_retryable() {
    let error = observe(json!({"error": {
        "message": "The model is currently at capacity due to high demand."
    }}))
    .await;
    assert!(transient(&error), "{error:?}");
}

#[tokio::test]
async fn nested_positive_flags_are_retryable() {
    for key in ["at_capacity", "high_demand", "insufficient_capacity"] {
        let error = observe(json!({"error": {key: true}})).await;
        assert!(transient(&error), "{key}: {error:?}");
    }
}

#[tokio::test]
async fn permanent_error_wins_over_capacity_wording() {
    for code in [
        "invalid_request",
        "invalid_prompt",
        "authentication_error",
        "permission_denied",
    ] {
        let error = observe(json!({"error": {
            "code": code,
            "message": "The model is currently at capacity due to high demand."
        }}))
        .await;
        assert!(!transient(&error), "{code}: {error:?}");
    }
}

#[tokio::test]
async fn unknown_error_remains_non_retryable() {
    let error = observe(json!({"error": {"message": "Something went wrong."}})).await;
    assert!(!transient(&error), "{error:?}");
}

#[tokio::test]
async fn false_capacity_flag_must_not_enable_retry() {
    let error = observe(json!({"error": {
        "message": "Something went wrong.", "at_capacity": false
    }}))
    .await;
    assert!(
        !transient(&error),
        "negative capacity flag enabled retry: {error:?}"
    );
}

#[tokio::test]
async fn null_capacity_flag_must_not_enable_retry() {
    let error = observe(json!({"error": {
        "message": "Something went wrong.", "at_capacity": null
    }}))
    .await;
    assert!(
        !transient(&error),
        "null capacity flag enabled retry: {error:?}"
    );
}

#[tokio::test]
async fn details_must_not_hide_primary_capacity_message() {
    let error = observe(json!({"error": {
        "details": "x".repeat(2_100),
        "message": "The model is currently at capacity due to high demand."
    }}))
    .await;
    assert!(
        transient(&error),
        "primary capacity message lost behind details: {error:?}"
    );
}

#[tokio::test]
async fn top_level_capacity_flag_must_reach_classifier() {
    let error = observe(json!({"type": "error", "at_capacity": true})).await;
    assert!(
        transient(&error),
        "top-level capacity marker discarded: {error:?}"
    );
}

#[tokio::test]
async fn status_503_with_field_only_capacity_keeps_capacity_metadata() {
    let error = observe(json!({"error": {
        "status": 503, "at_capacity": true, "message": "Try later."
    }}))
    .await;
    assert!(transient(&error));
    let metadata = serde_json::to_value(error.metadata().unwrap()).unwrap();
    assert_eq!(
        metadata["capacity_pressure"], true,
        "capacity backoff signal lost: {error:?}"
    );
}

#[tokio::test]
async fn capacity_retry_after_is_preserved() {
    let error = observe(json!({"error": {
        "message": "The model is currently at capacity due to high demand.",
        "retry_after": 7
    }}))
    .await;
    assert!(transient(&error));
    assert_eq!(error.retry_after(), Some(std::time::Duration::from_secs(7)));
}

#[tokio::test]
async fn capacity_markers_accept_code_variants_and_affirmative_flags() {
    for value in [
        json!({"code": "AT_CAPACITY"}),
        json!({"type": "insufficient-capacity"}),
        json!({"message": "High\tDemand"}),
        json!({"AT-CAPACITY": "true"}),
        json!({"high_demand": 1}),
        json!({"insufficient_capacity": "1"}),
        json!({"details": {"reason": "at capacity"}}),
    ] {
        let error = observe(json!({"error": value})).await;
        assert!(transient(&error), "{error:?}");
        assert!(error.metadata().unwrap().capacity_pressure);
    }
}

#[tokio::test]
async fn capacity_negative_fields_and_unrelated_diagnostics_stay_terminal() {
    for value in [
        json!({"at_capacity": "false"}),
        json!({"at_capacity": "0"}),
        json!({"at_capacity": 0}),
        json!({"at_capacity": ""}),
        json!({"at_capacity": {}}),
        json!({"not_at_capacity": true}),
        json!({"message": "invalid format capacity parameter"}),
        json!({"metadata": {"at_capacity": true}}),
        json!({"input": "The model is at capacity"}),
    ] {
        let error = observe(json!({"error": value})).await;
        assert!(!transient(&error), "{error:?}");
        assert!(!error.metadata().unwrap().capacity_pressure);
    }
}

#[tokio::test]
async fn top_level_capacity_frames_preserve_status_and_retry_advice() {
    let error = observe(json!({
        "type": "error", "at_capacity": true, "status": 503,
        "retry_after": 7, "request_id": "capacity-top-level"
    }))
    .await;
    assert!(transient(&error));
    assert!(error.metadata().unwrap().capacity_pressure);
    assert_eq!(error.http_status(), Some(503));
    assert_eq!(error.retry_after(), Some(std::time::Duration::from_secs(7)));
    assert_eq!(
        error.metadata().unwrap().request_id.as_deref(),
        Some("capacity-top-level")
    );
    for payload in [
        json!({"type": "error", "at_capacity": false}),
        json!({"type": "error", "at_capacity": null}),
        json!({"type": "error", "at_capacity": true, "status": 400}),
        json!({"type": "error", "at_capacity": true, "error_type": "invalid_request_error", "retry_after": 7}),
    ] {
        let error = observe(payload).await;
        assert!(!transient(&error), "{error:?}");
        assert!(!error.metadata().unwrap().capacity_pressure);
    }
}

#[tokio::test]
async fn capacity_metadata_survives_all_native_http_error_adapters() {
    for case in cases() {
        for status in [429, 503] {
            let (base_url, captured) = spawn_provider_sequence(vec![TestProviderResponse::json(
                status,
                json!({"error": {"at_capacity": true, "message": "Try later."}}).to_string(),
            )])
            .await;
            let error = provider(case, base_url)
                .complete_stream(simple_request(case.model), &mut |_| {})
                .await
                .expect_err("capacity HTTP error");
            assert_eq!(captured.await.unwrap().len(), 1);
            assert!(transient(&error), "{}: {error:?}", case.protocol.id());
            assert!(
                error.metadata().unwrap().capacity_pressure,
                "{}: {error:?}",
                case.protocol.id()
            );
            assert_eq!(error.http_status(), Some(status));
        }
    }
}

#[tokio::test]
async fn chat_completions_uses_the_same_capacity_metadata_and_hard_error_boundary() {
    for status in [200, 400, 429, 503] {
        let payload = json!({"error": {"at_capacity": true, "message": "Try later."}});
        let response = if status == 200 {
            TestProviderResponse::sse(status, format!("event: error\ndata: {payload}\n\n"))
        } else {
            TestProviderResponse::json(status, payload.to_string())
        }
        .header("retry-after", "7");
        let (base_url, captured) = spawn_provider_sequence(vec![response]).await;
        let error = OpenAiCompatibleProvider::new(TEST_API_KEY, base_url, "gpt-golden")
            .complete_stream(simple_request("gpt-golden"), &mut |_| {})
            .await
            .unwrap_err();
        assert_eq!(captured.await.unwrap().len(), 1);
        assert_eq!(transient(&error), status != 400);
        assert_eq!(error.metadata().unwrap().capacity_pressure, status != 400);
        assert_eq!(error.retry_after(), Some(std::time::Duration::from_secs(7)));
    }
}
