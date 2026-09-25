//! 用独立短请求确认推理协议；复用实际适配器，不把目录可读或 HTTP 200 当作推理成功。

use super::*;

const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(20);
const TOTAL_TIMEOUT: Duration = Duration::from_secs(60);
const UNCONFIRMED_HINT: &str = "protocol remains unconfirmed, not unsupported. Retry or select a protocol manually to save without detection";

/// 按调用方的共享顺序测试协议，返回第一个完成有效响应的协议。
/// 只对明确不支持的接口继续；认证、拒绝、限流、网络和服务错误原样结束。
/// Vertex 需要独立项目/OAuth 配置，Genai 是路由适配器，二者须手动选择。
pub async fn detect_provider_protocol(
    order: &[ProviderProtocol],
    base_url: &str,
    model: &str,
    api_key: &str,
    generation: Option<ProviderGenerationConfig>,
    headers: Vec<ProviderHeaderConfig>,
) -> Result<ProviderProtocol, String> {
    if api_key.trim().is_empty() || model.trim().is_empty() {
        return Err("Protocol detection requires an API key and model".to_owned());
    }
    tokio::time::timeout(TOTAL_TIMEOUT, async {
        let mut failures = Vec::new();
        for &protocol in order {
            if !matches!(protocol, ProviderProtocol::OpenAiResponses | ProviderProtocol::Anthropic
                | ProviderProtocol::Gemini | ProviderProtocol::OpenAiCompatible) {
                continue;
            }
            let result = tokio::time::timeout(ATTEMPT_TIMEOUT,
                probe(protocol, base_url, model, api_key, generation.clone(), headers.clone())
            ).await.map_err(|_| format!("{} detection timed out; {UNCONFIRMED_HINT}", protocol.id()))?;
            match result {
                Ok(()) => return Ok(protocol),
                Err(error) => {
                    let detail = format!("{}: {}", protocol.id(), detection_error(&error, api_key));
                    if !unsupported_endpoint(&error) {
                        return Err(detail);
                    }
                    failures.push(detail);
                }
            }
        }
        Err(format!("No supported protocol detected. {}. Select a protocol manually if this service requires special configuration.", failures.join("; ")))
    }).await.map_err(|_| format!("Protocol detection timed out; {UNCONFIRMED_HINT}"))?
}

async fn probe(
    protocol: ProviderProtocol,
    base: &str,
    model: &str,
    key: &str,
    generation: Option<ProviderGenerationConfig>,
    headers: Vec<ProviderHeaderConfig>,
) -> Result<(), ProviderError> {
    let resolved_headers = headers
        .iter()
        .map(|header| {
            header
                .validate()
                .map_err(|message| ProviderError::NotConfigured { message })?;
            let value = match &header.value {
                ProviderHeaderValue::Literal { value } => value.clone(),
                ProviderHeaderValue::Environment { key } => std::env::var(key)
                    .ok()
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| ProviderError::NotConfigured {
                        message: format!("Provider header environment variable {key} is not set"),
                    })?,
            };
            Ok((header.name.clone(), value))
        })
        .collect::<Result<BTreeMap<_, _>, ProviderError>>()?;
    let values = HashMap::from([
        (GOLUTRA_AGENT_PROVIDER_PROTOCOL, protocol.id().to_owned()),
        (GOLUTRA_AGENT_PROVIDER_BASE_URL, base.to_owned()),
        (GOLUTRA_AGENT_PROVIDER_MODEL, model.to_owned()),
        (GOLUTRA_AGENT_PROVIDER_API_KEY, key.to_owned()),
        (
            GOLUTRA_AGENT_PROVIDER_GENERATION_CONFIG,
            serde_json::to_string(&generation.unwrap_or_default()).expect("generation serializes"),
        ),
        (
            GOLUTRA_AGENT_PROVIDER_CUSTOM_HEADERS,
            serde_json::to_string(&resolved_headers).expect("headers serialize"),
        ),
    ]);
    // SDK 初始化可能读取系统代理，不能阻塞终端事件循环。闭包完成前不发送请求。
    let provider = tokio::task::spawn_blocking(move || {
        ConfiguredProvider::resolve_from_reader(MockProvider::text_response(""), |name| {
            values.get(name).cloned()
        })
    })
    .await
    .map_err(|_| ProviderError::Failed {
        message: "Protocol client initialization interrupted".to_owned(),
    })??;
    let request = ProviderRequest {
        request_id: ProviderRequestId::new(),
        task_id: TaskId::new(),
        turn_id: TurnId::new(),
        session_id: None,
        cache_scope: None,
        provider_id: "protocol-detection".to_owned(),
        model_id: model.to_owned(),
        messages: vec![ProviderMessage {
            role: ProviderRole::User,
            content: "Reply with OK.".to_owned(),
            tool_call_id: None,
            tool_name: None,
            tool_calls: Vec::new(),
            metadata: Default::default(),
        }],
        tools: Vec::new(),
        cache_policy: PromptCachePolicy::None,
        max_output_tokens: Some(1024),
    };
    let response = provider.complete_stream(request, &mut |_| {}).await?;
    if matches!(
        response.finish_reason,
        ProviderFinishReason::Stop | ProviderFinishReason::Length
    ) && response
        .message
        .as_ref()
        .is_some_and(|message| !message.content.trim().is_empty())
    {
        Ok(())
    } else {
        Err(ProviderError::Failed {
            message: "Protocol detection did not receive a completed text response".to_owned(),
        })
    }
}

fn unsupported_endpoint(error: &ProviderError) -> bool {
    let metadata = error.metadata();
    let code = metadata
        .and_then(|m| m.provider_code.as_deref())
        .unwrap_or_default();
    // 明确业务拒绝不能因外层状态码或上游措辞被当作协议不支持。
    if matches!(error.http_status(), Some(401 | 403 | 429 | 500 | 502..=599))
        || [
            code,
            metadata
                .and_then(|m| m.error_type.as_deref())
                .unwrap_or_default(),
        ]
        .iter()
        .any(|marker| {
            matches!(
                *marker,
                "invalid_prompt"
                    | "model_not_found"
                    | "bio_policy"
                    | "misalignment_policy_violation"
                    | "cyber_policy_violation"
                    | "authentication_error"
                    | "permission_denied"
            )
        })
    {
        return false;
    }
    matches!(error.http_status(), Some(404 | 405 | 415 | 501))
        || matches!(
            code,
            "unsupported_protocol" | "unsupported_endpoint" | "unsupported_api"
        )
}

fn detection_error(error: &ProviderError, key: &str) -> String {
    // 部分适配器的 Display 只有正文；显式保留状态及请求身份，便于修正配置。
    let mut message = error.to_string();
    let semantic_error = match error {
        ProviderError::WithMetadata { error, .. } => error.as_ref(),
        error => error,
    };
    if matches!(semantic_error, ProviderError::Timeout { .. }) {
        message.push_str(&format!("; {UNCONFIRMED_HINT}"));
    }
    if let Some(status) = error.http_status() {
        message.push_str(&format!(" (status: {status})"));
    }
    if let Some(metadata) = error.metadata() {
        if let Some(code) = &metadata.provider_code {
            message.push_str(&format!(" (code: {code})"));
        }
        if let Some(id) = &metadata.request_id {
            message.push_str(&format!(" (request ID: {id})"));
        }
    }
    sanitize_provider_error(&message.replace(key, "[REDACTED]"))
        .chars()
        .take(2048)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adapter_timeout_preserves_diagnostics_and_remains_unconfirmed() {
        let error = ProviderError::Timeout {
            message: "test-key read deadline".to_owned(),
        }
        .with_metadata(ProviderErrorMetadata {
            request_id: Some("request-1".to_owned()),
            ..Default::default()
        });
        let detail = detection_error(&error, "test-key");
        assert!(detail.contains(UNCONFIRMED_HINT));
        assert!(detail.contains("request-1"));
        assert!(!detail.contains("test-key"));
        assert!(!unsupported_endpoint(&error));
    }
}
