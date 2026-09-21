//! 按已选协议读取模型目录；只做可跳过的发现，不推断模型能力或验证推理请求。

use std::{collections::HashSet, time::Duration};

use serde::Deserialize;

use crate::{ProviderProtocol, validate_provider_base_url};

// 模型发现是可跳过的向导步骤，不能像长程推理一样无限等待或读取无限响应。
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_CATALOG_BYTES: usize = 1024 * 1024;

/// 按 OpenAI Chat/Responses、Anthropic 或 Gemini 协议读取 `/models` 返回的模型 ID。
/// 不支持目录的协议返回错误，调用方应允许手动填写；不跟随重定向或泄露凭据。
pub async fn discover_provider_models(
    protocol: ProviderProtocol,
    base_url: &str,
    api_key: &str,
) -> Result<Vec<String>, String> {
    discover_provider_models_with_client_builder(
        protocol,
        base_url,
        api_key,
        reqwest::Client::builder(),
    )
    .await
}

/// 使用调用方的代理/TLS 配置读取目录；仍强制限制总时长、响应大小和重定向。
/// 本地集成测试可传入 `Client::builder().no_proxy()`，避免依赖宿主系统代理。
pub async fn discover_provider_models_with_client_builder(
    protocol: ProviderProtocol,
    base_url: &str,
    api_key: &str,
    client_builder: reqwest::ClientBuilder,
) -> Result<Vec<String>, String> {
    if matches!(
        protocol,
        ProviderProtocol::VertexAi | ProviderProtocol::Genai
    ) {
        // Vertex 的项目模型资源与生成模型 ID 不等价；Genai 在选择模型前没有确定协议。
        return Err("Automatic model discovery is unavailable for this protocol".to_owned());
    }
    let base = validate_provider_base_url(protocol, base_url)
        .map_err(|_| "Invalid model catalog base URL".to_owned())?;
    if api_key.trim().is_empty() {
        return Err("API key is empty".to_owned());
    }
    tokio::time::timeout(DISCOVERY_TIMEOUT, async {
        // macOS 系统代理读取可能同步阻塞。隔离客户端初始化，并将其纳入总时限；
        // 取消后即使系统读取尚未结束，也不会发送 Key（闭包只创建无凭据客户端）。
        let client = tokio::task::spawn_blocking(move || {
            client_builder
                .timeout(DISCOVERY_TIMEOUT)
                .redirect(reqwest::redirect::Policy::none())
                .build()
        })
        .await
        .map_err(|_| "Model catalog client initialization was interrupted".to_owned())?
        .map_err(|_| "Cannot initialize model catalog client".to_owned())?;
        fetch_models(&client, protocol, &base, api_key).await
    })
    .await
    .map_err(|_| "Model catalog request timed out".to_owned())?
}

async fn fetch_models(
    client: &reqwest::Client,
    protocol: ProviderProtocol,
    base: &str,
    api_key: &str,
) -> Result<Vec<String>, String> {
    let request = client
        .get(format!("{base}/models"))
        .header(reqwest::header::ACCEPT, "application/json");
    let request = match protocol {
        ProviderProtocol::Anthropic => request
            .header("x-api-key", api_key)
            .header("anthropic-version", "2023-06-01"),
        ProviderProtocol::Gemini => request.header("x-goog-api-key", api_key),
        _ => request.bearer_auth(api_key),
    };
    let mut response = request.send().await.map_err(discovery_http_error)?;
    if !response.status().is_success() {
        return Err(format!("Model catalog returned HTTP {}", response.status()));
    }
    let too_large = || "Model catalog exceeds 1 MiB".to_owned();
    if response
        .content_length()
        .is_some_and(|size| size > MAX_CATALOG_BYTES as u64)
    {
        return Err(too_large());
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(discovery_http_error)? {
        if bytes.len().saturating_add(chunk.len()) > MAX_CATALOG_BYTES {
            return Err(too_large());
        }
        bytes.extend_from_slice(&chunk);
    }
    parse_models(protocol, &bytes)
}

fn discovery_http_error(error: reqwest::Error) -> String {
    if error.is_timeout() {
        "Model catalog request timed out".to_owned()
    } else if error.is_connect() {
        "Cannot connect to model catalog; check network and proxy".to_owned()
    } else {
        "Model catalog request failed".to_owned()
    }
}

fn parse_models(protocol: ProviderProtocol, bytes: &[u8]) -> Result<Vec<String>, String> {
    #[derive(Deserialize)]
    struct Catalog {
        data: Vec<Model>,
    }
    #[derive(Deserialize)]
    struct Model {
        id: String,
    }
    #[derive(Deserialize)]
    struct GeminiCatalog {
        models: Vec<GeminiModel>,
    }
    #[derive(Deserialize)]
    struct GeminiModel {
        name: String,
    }
    let ids: Vec<String> = if protocol == ProviderProtocol::Gemini {
        let catalog: GeminiCatalog = serde_json::from_slice(bytes)
            .map_err(|_| "Model catalog is not valid Gemini models JSON".to_owned())?;
        catalog
            .models
            .into_iter()
            .map(|model| {
                model
                    .name
                    .trim()
                    .strip_prefix("models/")
                    .unwrap_or(model.name.trim())
                    .to_owned()
            })
            .collect()
    } else {
        let catalog: Catalog = serde_json::from_slice(bytes).map_err(|_| {
            "Model catalog is not valid models JSON (expected data[].id)".to_owned()
        })?;
        catalog.data.into_iter().map(|model| model.id).collect()
    };
    let mut seen = HashSet::new();
    Ok(ids
        .into_iter()
        .filter_map(|model| {
            let id = model.trim();
            if id.is_empty() || id.chars().any(char::is_control) || !seen.insert(id.to_owned()) {
                None
            } else {
                Some(id.to_owned())
            }
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    async fn server(response: String) -> (String, tokio::task::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                let mut chunk = [0; 2048];
                let n = stream.read(&mut chunk).await.unwrap();
                assert!(n > 0);
                request.extend_from_slice(&chunk[..n]);
            }
            // 大响应可能被大小上限提前中断，这是预期的客户端行为。
            let _ = stream.write_all(response.as_bytes()).await;
            String::from_utf8(request).unwrap()
        });
        (base, task)
    }

    fn response(status: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    #[tokio::test]
    async fn model_catalog_uses_normalized_base_and_bearer_and_preserves_upstream_ids() {
        for (suffix, expected_path) in [
            ("", "/v1/models"),
            ("/proxy/v2/", "/proxy/v2/models"),
            ("/v1/chat/completions", "/v1/models"),
        ] {
            let (base, task) = server(response("200 OK", r#"{"data":[{"id":"z-model"},{"id":" a-model "},{"id":"z-model"},{"id":""},{"id":"bad\u001b[31m"}]}"#)).await;
            let models = discover_local_models(&format!("{base}{suffix}"))
                .await
                .unwrap();
            assert_eq!(models, ["z-model", "a-model"]);
            let request = task.await.unwrap();
            assert!(request.starts_with(&format!("GET {expected_path} HTTP/1.1\r\n")));
            assert!(
                request
                    .to_ascii_lowercase()
                    .contains("authorization: bearer fake-secret\r\n")
            );
        }
    }

    #[tokio::test]
    async fn model_catalog_uses_each_protocol_endpoint_headers_and_ids() {
        for (protocol, suffix, path, header, body, expected) in [
            (
                ProviderProtocol::OpenAiResponses,
                "/v1/responses",
                "/v1/models",
                "authorization: bearer fake-secret\r\n",
                r#"{"data":[{"id":"response-model"}]}"#,
                "response-model",
            ),
            (
                ProviderProtocol::Anthropic,
                "/v1/messages",
                "/v1/models",
                "x-api-key: fake-secret\r\n",
                r#"{"data":[{"id":"anthropic-model"}]}"#,
                "anthropic-model",
            ),
            (
                ProviderProtocol::Gemini,
                "",
                "/v1beta/models",
                "x-goog-api-key: fake-secret\r\n",
                r#"{"models":[{"name":"models/gemini-model"},{"name":"models/gemini-model"}]}"#,
                "gemini-model",
            ),
        ] {
            let (base, task) = server(response("200 OK", body)).await;
            let models = discover_provider_models_with_client_builder(
                protocol,
                &format!("{base}{suffix}"),
                "fake-secret",
                reqwest::Client::builder().no_proxy(),
            )
            .await
            .unwrap();
            assert_eq!(models, [expected]);
            let request = task.await.unwrap().to_ascii_lowercase();
            assert!(
                request.starts_with(&format!("get {path} http/1.1\r\n")),
                "{request}"
            );
            assert!(request.contains(header));
            if protocol == ProviderProtocol::Anthropic {
                assert!(request.contains("anthropic-version: 2023-06-01\r\n"));
            }
            if protocol != ProviderProtocol::OpenAiResponses {
                assert!(!request.contains("authorization:"));
            }
        }
    }

    #[tokio::test]
    async fn model_catalog_unsupported_protocols_return_without_network() {
        for protocol in [ProviderProtocol::VertexAi, ProviderProtocol::Genai] {
            let error = discover_provider_models(protocol, "http://127.0.0.1:1", "fake-secret")
                .await
                .unwrap_err();
            assert_eq!(
                error,
                "Automatic model discovery is unavailable for this protocol"
            );
        }
    }

    #[tokio::test]
    async fn model_catalog_reports_http_and_format_failures_without_echoing_secrets() {
        for (status, body, expected) in [
            ("401 Unauthorized", "fake-secret", "HTTP 401"),
            ("500 Internal Server Error", "fake-secret", "HTTP 500"),
            (
                "200 OK",
                "<html>fake-secret</html>",
                "not valid models JSON",
            ),
            (
                "200 OK",
                r#"{"models":["fake-secret"]}"#,
                "not valid models JSON",
            ),
        ] {
            let (base, task) = server(response(status, body)).await;
            let error = discover_local_models(&base).await.unwrap_err();
            assert!(error.contains(expected), "{error}");
            assert!(!error.contains("fake-secret"));
            task.await.unwrap();
        }
        assert!(
            parse_models(ProviderProtocol::OpenAiCompatible, br#"{"data":[]}"#)
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn model_catalog_rejects_redirects_and_oversized_declared_or_streamed_bodies() {
        for response in [
            "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:1/secret\r\nContent-Length: 0\r\n\r\n".to_owned(),
            format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", MAX_CATALOG_BYTES + 1),
            format!("HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n{}", "x".repeat(MAX_CATALOG_BYTES + 1)),
        ] {
            let redirect = response.contains("302");
            let (base, task) = server(response).await;
            let error = discover_local_models(&base).await.unwrap_err();
            assert!(error.contains(if redirect { "HTTP 302" } else { "exceeds 1 MiB" }), "{error}");
            task.await.unwrap();
        }
    }

    #[tokio::test]
    async fn model_catalog_times_out_even_after_response_headers() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = [0; 4096];
            assert!(socket.read(&mut bytes).await.unwrap() > 0);
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 500\r\n\r\n")
                .await
                .unwrap();
            std::future::pending::<()>().await;
        });
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_millis(100))
            .build()
            .unwrap();
        let error = fetch_models(
            &client,
            ProviderProtocol::OpenAiCompatible,
            &base,
            "fake-secret",
        )
        .await
        .unwrap_err();
        task.abort();
        let _ = task.await;
        assert!(error.contains("timed out"));
    }

    // 公开入口的本地网络测试显式直连，避免依赖系统代理扫描耗时。
    async fn discover_local_models(base: &str) -> Result<Vec<String>, String> {
        discover_provider_models_with_client_builder(
            ProviderProtocol::OpenAiCompatible,
            base,
            "fake-secret",
            reqwest::Client::builder().no_proxy(),
        )
        .await
    }
}
