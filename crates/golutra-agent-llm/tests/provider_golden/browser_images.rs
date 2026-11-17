use super::*;
use golutra_agent_llm::ProviderImage;

const PNG: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+a8l8AAAAASUVORK5CYII=";

fn image_request(model: &str) -> ProviderRequest {
    let mut request = simple_request(model);
    request.messages.last_mut().unwrap().metadata.images = vec![ProviderImage {
        mime_type: "image/png".to_owned(),
        base64: PNG.to_owned(),
    }];
    request
}

fn assert_image_wire(body: &Value) {
    let wire = body.to_string();
    assert!(wire.contains(PNG), "image bytes missing: {body}");
    assert!(wire.contains("image/png"), "MIME missing: {body}");
    assert!(
        wire.contains("\"image_url\"")
            || wire.contains("\"inlineData\"")
            || wire.contains("\"inline_data\"")
            || wire.contains("\"type\":\"image\""),
        "expected a native image content block: {body}"
    );
    assert!(!wire.contains("encoded_bytes"));
}

#[tokio::test]
async fn browser_images_reach_native_http_wire_for_every_supported_protocol() {
    for case in cases() {
        let (url, captured) = spawn_provider(200, case.text_response_fixture).await;
        provider(case, url)
            .complete(image_request(case.model))
            .await
            .unwrap();
        assert_image_wire(&captured.await.unwrap().body);
    }
    let (url, captured) = spawn_provider(
        200,
        include_str!("../fixtures/openai-compatible/text_response.json"),
    )
    .await;
    OpenAiCompatibleProvider::new(TEST_API_KEY, url, "gpt-golden")
        .complete(image_request("gpt-golden"))
        .await
        .unwrap();
    assert_image_wire(&captured.await.unwrap().body);
    let (url, captured) = spawn_provider_sequence(vec![TestProviderResponse::sse(
        200,
        include_str!("../fixtures/openai-responses/text-response.sse"),
    )])
    .await;
    openai_responses_provider(url)
        .complete_stream(image_request("gpt-golden"), &mut |_| {})
        .await
        .unwrap();
    let body = &captured.await.unwrap()[0].body;
    assert_image_wire(body);
    assert!(body.to_string().contains("\"type\":\"input_image\""));
}
