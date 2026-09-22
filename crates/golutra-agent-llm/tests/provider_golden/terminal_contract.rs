//! 从真实 HTTP/SSE 入口验证协议结束契约，防止依赖库归一化掩盖未完成或失败状态。

use super::*;

#[derive(Clone, Copy, Debug)]
enum Wire {
    Chat,
    Anthropic,
    Gemini,
}

fn routes() -> Vec<(ProtocolCase, Wire)> {
    let native = cases();
    let mut routes = vec![
        (native[0], Wire::Anthropic),
        (native[1], Wire::Gemini),
        (native[2], Wire::Gemini),
        (native[3], Wire::Chat),
        (
            ProtocolCase {
                model: "claude-test",
                ..native[2]
            },
            Wire::Anthropic,
        ),
    ];
    for (model, wire) in [
        ("claude-test", Wire::Anthropic),
        ("gemini-test", Wire::Gemini),
        ("gpt-4o-mini", Wire::Chat),
    ] {
        routes.push((ProtocolCase { model, ..native[3] }, wire));
    }
    routes.push((
        ProtocolCase {
            protocol: ProviderProtocol::OpenAiCompatible,
            model: "gpt-test",
            ..native[3]
        },
        Wire::Chat,
    ));
    routes
}

fn endpoint(case: ProtocolCase, base_url: String) -> Box<dyn LlmProvider> {
    if case.protocol == ProviderProtocol::OpenAiCompatible {
        Box::new(OpenAiCompatibleProvider::new(
            TEST_API_KEY,
            base_url,
            case.model,
        ))
    } else if case.protocol == ProviderProtocol::OpenAiResponses {
        Box::new(openai_responses_provider_with_model(base_url, case.model))
    } else {
        Box::new(provider(case, base_url))
    }
}

fn dynamic_request(model: &str) -> ProviderRequest {
    let mut request = simple_request(model);
    let message = |role, content: &str| ProviderMessage {
        role,
        content: content.into(),
        tool_call_id: None,
        tool_name: None,
        tool_calls: Vec::new(),
        metadata: Default::default(),
    };
    request.messages = vec![
        message(ProviderRole::System, "fixed-system-rules"),
        message(ProviderRole::User, "original-user-goal"),
        message(ProviderRole::Assistant, "completed-work-evidence"),
        message(ProviderRole::User, "runtime-recovery-context"),
    ];
    request
}

fn assert_dynamic_context_stays_after_history(body: &Value) {
    let mut static_text = String::new();
    for key in [
        "system",
        "systemInstruction",
        "system_instruction",
        "instructions",
    ] {
        if let Some(value) = body.get(key) {
            static_text.push_str(&value.to_string());
        }
    }
    let messages = body
        .get("messages")
        .or_else(|| body.get("contents"))
        .or_else(|| body.get("input"))
        .and_then(Value::as_array)
        .expect("wire messages");
    for message in messages
        .iter()
        .filter(|message| matches!(message["role"].as_str(), Some("system" | "developer")))
    {
        static_text.push_str(&message.to_string());
    }
    assert!(static_text.contains("fixed-system-rules"), "{body}");
    assert!(!static_text.contains("runtime-recovery-context"), "{body}");
    let history = messages
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        history.find("original-user-goal").unwrap()
            < history.find("completed-work-evidence").unwrap()
    );
    assert!(
        history.find("completed-work-evidence").unwrap()
            < history.find("runtime-recovery-context").unwrap()
    );
    assert_eq!(history.matches("runtime-recovery-context").count(), 1);
}

#[tokio::test]
async fn dynamic_context_preserves_system_prefix_and_history_across_protocols() {
    for (case, wire) in routes() {
        for streaming in [false, true] {
            let response = if streaming {
                TestProviderResponse::sse(200, stream(wire, Some(reasons(wire)[0].0), false, true))
            } else {
                TestProviderResponse::json(
                    200,
                    body(wire, Some(reasons(wire)[0].0), false).to_string(),
                )
            };
            let (url, captured) = spawn_provider_sequence(vec![response]).await;
            let provider = endpoint(case, url);
            if streaming {
                provider
                    .complete_stream(dynamic_request(case.model), &mut |_| {})
                    .await
                    .unwrap();
            } else {
                provider
                    .complete(dynamic_request(case.model))
                    .await
                    .unwrap();
            }
            assert_dynamic_context_stays_after_history(&captured.await.unwrap()[0].body);
        }
    }
    // Responses 原生请求使用 input，与 Chat 的 messages 结构单独验收。
    for streaming in [false, true] {
        let (url, captured) = spawn_provider_sequence(vec![TestProviderResponse::sse(
            200,
            include_str!("../fixtures/openai-responses/text-response.sse"),
        )])
        .await;
        let provider = openai_responses_provider(url);
        if streaming {
            provider
                .complete_stream(dynamic_request("gpt-golden"), &mut |_| {})
                .await
                .unwrap();
        } else {
            provider
                .complete(dynamic_request("gpt-golden"))
                .await
                .unwrap();
        }
        assert_dynamic_context_stays_after_history(&captured.await.unwrap()[0].body);
    }
}

fn body(wire: Wire, reason: Option<&str>, tool: bool) -> Value {
    match wire {
        Wire::Chat => json!({
            "id":"terminal", "model":"test",
            "choices":[{"index":0,"finish_reason":reason,"message": if tool {
                json!({"role":"assistant", "tool_calls":[{"id":"call-1","type":"function",
                    "function":{"name":"read_file","arguments":"{\"path\":\"README.md\"}"}}]})
            } else { json!({"role":"assistant","content":"retained text"}) }}],
            "usage":{"prompt_tokens":12,"completion_tokens":3,"total_tokens":15}
        }),
        Wire::Anthropic => json!({
            "id":"terminal", "type":"message", "role":"assistant", "model":"claude-test",
            "content":[if tool {json!({"type":"tool_use","id":"call-1","name":"read_file","input":{"path":"README.md"}})}
                else {json!({"type":"text","text":"retained text"})}],
            "stop_reason":reason,"stop_sequence":null,
            "usage":{"input_tokens":12,"output_tokens":3}
        }),
        Wire::Gemini => json!({
            "candidates":[{"content":{"role":"model","parts":[if tool {
                json!({"functionCall":{"id":"call-1","name":"read_file","args":{"path":"README.md"}}})
            } else {json!({"text":"retained text"})}]},"finishReason":reason}],
            "usageMetadata":{"promptTokenCount":12,"candidatesTokenCount":3,"totalTokenCount":15},
            "modelVersion":"gemini-test"
        }),
    }
}

fn event(kind: &str, data: Value) -> String {
    format!("event: {kind}\ndata: {data}\n\n")
}

fn stream(wire: Wire, reason: Option<&str>, tool: bool, terminal: bool) -> String {
    let value = body(wire, reason, tool);
    match wire {
        Wire::Chat => {
            let mut delta = value["choices"][0]["message"].clone();
            if tool {
                delta["tool_calls"][0]["index"] = json!(0);
            }
            let mut result = event(
                "message",
                json!({"id":"terminal","choices":[{"index":0,"delta":delta,"finish_reason":null}]}),
            );
            if terminal {
                result += &event(
                    "message",
                    json!({"id":"terminal","choices":[{"index":0,"delta":{},"finish_reason":reason}],"usage":value["usage"]}),
                );
                result += "data: [DONE]\n\n";
            }
            result
        }
        Wire::Gemini => {
            let mut value = value;
            if !terminal {
                value["candidates"][0]["finishReason"] = Value::Null;
            }
            event("message", value)
        }
        Wire::Anthropic => {
            let mut result = event(
                "message_start",
                json!({"type":"message_start","message":{
                    "id":"terminal","role":"assistant","model":"claude-test","content":[],"usage":{"input_tokens":12,"output_tokens":0}
                }}),
            );
            let block = if tool {
                json!({"type":"tool_use","id":"call-1","name":"read_file","input":{}})
            } else {
                json!({"type":"text","text":""})
            };
            result += &event(
                "content_block_start",
                json!({"type":"content_block_start","index":0,"content_block":block}),
            );
            let delta = if tool {
                json!({"type":"input_json_delta","partial_json":"{\"path\":\"README.md\"}"})
            } else {
                json!({"type":"text_delta","text":"retained text"})
            };
            result += &event(
                "content_block_delta",
                json!({"type":"content_block_delta","index":0,"delta":delta}),
            );
            result += &event(
                "content_block_stop",
                json!({"type":"content_block_stop","index":0}),
            );
            if terminal {
                result += &event(
                    "message_delta",
                    json!({"type":"message_delta","delta":{"stop_reason":reason},"usage":{"output_tokens":3}}),
                );
                result += &event("message_stop", json!({"type":"message_stop"}));
            }
            result
        }
    }
}

fn reasons(wire: Wire) -> Vec<(&'static str, ProviderFinishReason)> {
    use ProviderFinishReason::*;
    match wire {
        Wire::Chat => vec![
            ("stop", Stop),
            ("length", Length),
            ("content_filter", ContentFilter),
            ("future_reason", Unknown),
        ],
        Wire::Anthropic => vec![
            ("end_turn", Stop),
            ("stop_sequence", Stop),
            ("pause_turn", Continue),
            ("max_tokens", Length),
            ("refusal", ContentFilter),
            ("future_reason", Unknown),
        ],
        Wire::Gemini => vec![
            ("STOP", Stop),
            ("MAX_TOKENS", Length),
            ("SAFETY", ContentFilter),
            ("RECITATION", ContentFilter),
            ("MALFORMED_FUNCTION_CALL", Error),
            ("UNEXPECTED_TOOL_CALL", Error),
            ("OTHER", Unknown),
        ],
    }
}

#[tokio::test]
async fn all_protocols_preserve_terminal_semantics_in_streamed_and_buffered_responses() {
    for (case, wire) in routes() {
        for (reason, expected) in reasons(wire) {
            for streamed in [false, true] {
                for tool in [false, true] {
                    let fixture = if streamed {
                        TestProviderResponse::sse(200, stream(wire, Some(reason), tool, true))
                    } else {
                        TestProviderResponse::json(200, body(wire, Some(reason), tool).to_string())
                    };
                    let (base_url, _) = spawn_provider_sequence(vec![fixture]).await;
                    let provider = endpoint(case, base_url);
                    let request = simple_request(case.model);
                    let response = if streamed {
                        provider.complete_stream(request, &mut |_| {}).await
                    } else {
                        provider.complete(request).await
                    }
                    .unwrap_or_else(|error| {
                        panic!(
                            "{} {} {wire:?} {reason} streamed={streamed} tool={tool}: {error}",
                            case.protocol.id(),
                            case.model
                        )
                    });
                    let expected = if tool
                        && matches!(
                            expected,
                            ProviderFinishReason::Stop | ProviderFinishReason::Continue
                        ) {
                        ProviderFinishReason::ToolCalls
                    } else {
                        expected
                    };
                    assert_eq!(
                        response.finish_reason,
                        expected,
                        "{} {} {wire:?} {reason} streamed={streamed} tool={tool}",
                        case.protocol.id(),
                        case.model
                    );
                    assert_eq!(response.tool_calls.len(), usize::from(tool));
                    if !tool {
                        assert_eq!(response.message.unwrap().content, "retained text");
                    }
                    assert_eq!(response.usage.input_tokens, Some(12));
                    assert_eq!(response.usage.output_tokens, Some(3));
                }
            }
        }
    }
}

#[tokio::test]
async fn absent_reason_or_interrupted_stream_never_certifies_tool_calls() {
    for (case, wire) in routes() {
        for (streamed, terminal) in [(false, true), (true, true), (true, false)] {
            let fixture = if streamed {
                TestProviderResponse::sse(200, stream(wire, None, true, terminal))
            } else {
                TestProviderResponse::json(200, body(wire, None, true).to_string())
            };
            let (base_url, _) = spawn_provider_sequence(vec![fixture]).await;
            let provider = endpoint(case, base_url);
            let request = simple_request(case.model);
            let result = if streamed {
                provider.complete_stream(request, &mut |_| {}).await
            } else {
                provider.complete(request).await
            };
            if let Ok(response) = result {
                assert_eq!(
                    response.finish_reason,
                    ProviderFinishReason::Unknown,
                    "{} {} streamed={streamed} terminal={terminal}",
                    case.protocol.id(),
                    case.model
                );
            }
        }
    }
}

#[tokio::test]
async fn genai_responses_route_reuses_dedicated_terminal_contract() {
    for protocol in [ProviderProtocol::OpenAiResponses, ProviderProtocol::Genai] {
        let case = ProtocolCase {
            protocol,
            model: "gpt-6-astra",
            ..cases()[3]
        };
        for (status, reason, end_turn, expected) in [
            (
                "completed",
                Value::Null,
                json!(true),
                ProviderFinishReason::Stop,
            ),
            (
                "completed",
                Value::Null,
                json!(false),
                ProviderFinishReason::Continue,
            ),
            (
                "incomplete",
                json!("max_output_tokens"),
                Value::Null,
                ProviderFinishReason::Length,
            ),
            (
                "incomplete",
                json!("content_filter"),
                Value::Null,
                ProviderFinishReason::ContentFilter,
            ),
            (
                "incomplete",
                json!("future_reason"),
                Value::Null,
                ProviderFinishReason::Error,
            ),
        ] {
            for streamed in [false, true] {
                let mut fixture = event(
                    "response.output_text.delta",
                    json!({"type":"response.output_text.delta","delta":"retained text"}),
                );
                let kind = format!("response.{status}");
                fixture += &event(
                    &kind,
                    json!({"type":kind,"response":{"id":"terminal","status":status,"model":case.model,"output":[],"end_turn":end_turn,"incomplete_details":{"reason":reason}}}),
                );
                let (base_url, captured) =
                    spawn_provider_sequence(vec![TestProviderResponse::sse(200, fixture)]).await;
                let provider = endpoint(case, base_url);
                assert!(!provider.supports_buffered_transport());
                let request = simple_request(case.model);
                let response = if streamed {
                    provider.complete_stream(request, &mut |_| {}).await
                } else {
                    provider.complete(request).await
                }
                .unwrap();
                assert_eq!(response.finish_reason, expected);
                assert_eq!(response.message.unwrap().content, "retained text");
                let requests = captured.await.unwrap();
                assert_eq!(requests[0].path, "/v1/responses");
                assert_eq!(requests[0].body["stream"], true);
            }
        }
    }
}

#[tokio::test]
async fn unclosed_anthropic_tool_blocks_cannot_disappear_into_text_continuation() {
    for (case, _) in routes()
        .into_iter()
        .filter(|(_, wire)| matches!(wire, Wire::Anthropic))
    {
        for reason in ["end_turn", "max_tokens", "pause_turn"] {
            // 第一项完整调用之后，第二项只收到部分参数；不能执行第一项后静默漏掉第二项。
            let mut fixture = stream(Wire::Anthropic, None, true, false);
            fixture += &event(
                "content_block_start",
                json!({"type":"content_block_start","index":1,
                "content_block":{"type":"tool_use","id":"call-2","name":"read_file","input":{}}}),
            );
            fixture += &event(
                "content_block_delta",
                json!({"type":"content_block_delta","index":1,
                "delta":{"type":"input_json_delta","partial_json":"{\"path\":"}}),
            );
            fixture += &event(
                "message_delta",
                json!({"type":"message_delta","delta":{"stop_reason":reason},"usage":{"output_tokens":3}}),
            );
            fixture += &event("message_stop", json!({"type":"message_stop"}));
            let (base_url, _) =
                spawn_provider_sequence(vec![TestProviderResponse::sse(200, fixture)]).await;
            let error = endpoint(case, base_url)
                .complete_stream(simple_request(case.model), &mut |_| {})
                .await
                .unwrap_err();
            assert!(
                error.to_string().contains("uncaptured tool calls"),
                "{error}"
            );
        }
    }
}

#[tokio::test]
async fn complete_native_responses_retain_invalid_arguments_for_tool_feedback() {
    for (case, wire) in routes() {
        let reason = match wire {
            Wire::Chat => "tool_calls",
            Wire::Anthropic => "tool_use",
            Wire::Gemini => "STOP",
        };
        let mut value = body(wire, Some(reason), true);
        match wire {
            Wire::Chat => {
                value["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"] =
                    json!("\"partial\"")
            }
            Wire::Anthropic => value["content"][0]["input"] = json!("partial"),
            Wire::Gemini => {
                value["candidates"][0]["content"]["parts"][0]["functionCall"]["args"] =
                    json!("partial")
            }
        }
        let (base_url, _) = spawn_provider(200, value.to_string()).await;
        let response = endpoint(case, base_url)
            .complete(simple_request(case.model))
            .await
            .unwrap();
        assert_eq!(response.finish_reason, ProviderFinishReason::ToolCalls);
        assert_eq!(response.tool_calls[0].arguments, json!("partial"));
    }
}

#[tokio::test]
async fn completed_chat_and_responses_streams_preserve_malformed_json_for_schema_feedback() {
    for protocol in [
        ProviderProtocol::OpenAiCompatible,
        ProviderProtocol::OpenAiResponses,
        ProviderProtocol::Genai,
    ] {
        for streamed in [false, true] {
            let case = ProtocolCase {
                protocol,
                model: "gpt-5",
                ..cases()[3]
            };
            let fixture = if protocol == ProviderProtocol::OpenAiCompatible {
                stream(Wire::Chat, Some("tool_calls"), true, true)
                    .replace("{\\\"path\\\":\\\"README.md\\\"}", "{\\\"path\\\":")
            } else {
                include_str!("../fixtures/openai-responses/tool-response.sse")
                    .lines()
                    .filter(|line| !line.contains("README.md"))
                    .collect::<Vec<_>>()
                    .join("\n")
                    + "\n"
            };
            let (base_url, _) =
                spawn_provider_sequence(vec![TestProviderResponse::sse(200, fixture)]).await;
            let provider = endpoint(case, base_url);
            let response = if streamed || protocol == ProviderProtocol::OpenAiCompatible {
                provider
                    .complete_stream(simple_request(case.model), &mut |_| {})
                    .await
            } else {
                provider.complete(simple_request(case.model)).await
            }
            .unwrap();
            assert_eq!(response.finish_reason, ProviderFinishReason::ToolCalls);
            assert_eq!(response.tool_calls[0].arguments, json!("{\"path\":"));
        }
    }
}
