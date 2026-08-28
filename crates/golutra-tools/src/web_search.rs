use std::{
    collections::HashMap,
    fmt,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use async_trait::async_trait;
use futures_util::StreamExt;
use reqwest::{Client, Url, header};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::{ExternalToolOutput, ToolError, ToolRequest, WebSearchBackend};

const DEFAULT_MAX_RESULTS: usize = 5;
const MAX_RESULTS: usize = 20;
const MAX_QUERY_CHARS: usize = 2_048;
const MAX_SNIPPET_CHARS: usize = 1_024;
const MAX_RESULT_URL_CHARS: usize = 2_048;
const MAX_RESPONSE_BYTES: usize = 512 * 1024;
const MAX_CACHE_ENTRIES: usize = 64;
const CACHE_TTL: Duration = Duration::from_secs(30);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, Clone)]
struct CachedSearch {
    expires_at: Instant,
    output: ExternalToolOutput,
}

/// 可替换的 HTTP 搜索适配器，兼容 Brave 风格和通用 JSON endpoint。
#[derive(Clone)]
pub struct HttpWebSearchBackend {
    endpoint: Url,
    api_key: Option<String>,
    client: Client,
    cache: Arc<Mutex<HashMap<String, CachedSearch>>>,
}

impl fmt::Debug for HttpWebSearchBackend {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HttpWebSearchBackend")
            .field("endpoint_host", &self.endpoint.host_str())
            .field("endpoint_path", &self.endpoint.path())
            .field("api_key_configured", &self.api_key.is_some())
            .field(
                "cache_entries",
                &self.cache.lock().map(|cache| cache.len()).unwrap_or(0),
            )
            .finish()
    }
}

impl HttpWebSearchBackend {
    pub fn new(endpoint: impl AsRef<str>, api_key: Option<String>) -> Result<Self, ToolError> {
        let endpoint = Url::parse(endpoint.as_ref()).map_err(|error| {
            ToolError::InvalidArguments(format!("invalid web search endpoint: {error}"))
        })?;
        if !matches!(endpoint.scheme(), "http" | "https") {
            return Err(ToolError::InvalidArguments(
                "web search endpoint must use http or https".to_owned(),
            ));
        }
        let client = Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .user_agent("golutra-agent/web-search")
            .build()
            .map_err(|error| {
                ToolError::Execution(format!("web search client setup failed: {error}"))
            })?;
        Ok(Self {
            endpoint,
            api_key: api_key.filter(|value| !value.trim().is_empty()),
            client,
            cache: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    /// 从显式环境变量构造；未配置 endpoint 时不创建网络适配器。
    pub fn from_env() -> Result<Option<Self>, ToolError> {
        let Some(endpoint) = std::env::var("GOLUTRA_WEB_SEARCH_ENDPOINT")
            .ok()
            .filter(|value| !value.trim().is_empty())
        else {
            return Ok(None);
        };
        Self::new(endpoint, std::env::var("GOLUTRA_WEB_SEARCH_API_KEY").ok()).map(Some)
    }

    fn cache_key(&self, query: &str, max_results: usize, domains: &[String]) -> String {
        let domains = domains.join(",");
        format!("{}\n{}\n{}\n{}", self.endpoint, query, max_results, domains)
    }

    fn cached(&self, key: &str) -> Option<ExternalToolOutput> {
        let mut cache = self.cache.lock().ok()?;
        let entry = cache.get(key)?;
        if entry.expires_at <= Instant::now() {
            cache.remove(key);
            return None;
        }
        let mut output = entry.output.clone();
        if let Some(facts) = output.structured_facts.as_object_mut() {
            facts.insert("cached".to_owned(), Value::Bool(true));
        }
        Some(output)
    }

    fn store(&self, key: String, output: ExternalToolOutput) {
        let Ok(mut cache) = self.cache.lock() else {
            return;
        };
        if cache.len() >= MAX_CACHE_ENTRIES
            && let Some(oldest) = cache
                .iter()
                .min_by_key(|(_, entry)| entry.expires_at)
                .map(|(key, _)| key.clone())
        {
            cache.remove(&oldest);
        }
        cache.insert(
            key,
            CachedSearch {
                expires_at: Instant::now() + CACHE_TTL,
                output,
            },
        );
    }

    async fn request(
        &self,
        query: &str,
        max_results: usize,
        domains: &[String],
        cancellation: CancellationToken,
    ) -> Result<Value, ToolError> {
        let mut url = self.endpoint.clone();
        {
            let mut pairs = url.query_pairs_mut();
            pairs.append_pair("q", query);
            pairs.append_pair("count", &max_results.to_string());
            if !domains.is_empty() {
                pairs.append_pair("domains", &domains.join(","));
            }
        }
        let mut request = self.client.get(url);
        if let Some(api_key) = self.api_key.as_deref() {
            if self.is_brave_endpoint() {
                request = request.header("X-Subscription-Token", api_key);
            } else {
                request = request.header(header::AUTHORIZATION, format!("Bearer {api_key}"));
            }
        }
        let response = tokio::select! {
            () = cancellation.cancelled() => {
                return Err(ToolError::Execution("web search cancelled".to_owned()));
            }
            response = request.send() => response
                .map_err(|error| ToolError::Execution(format!("web search request failed: {error}")))?,
        };
        let status = response.status();
        // 同时限制声明长度和实际流量，避免异常 endpoint 占满 runtime 内存。
        if response
            .content_length()
            .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
        {
            return Err(ToolError::Execution(format!(
                "web search response exceeds {MAX_RESPONSE_BYTES} bytes"
            )));
        }
        let mut stream = response.bytes_stream();
        let mut body = Vec::new();
        while let Some(chunk) = tokio::select! {
            () = cancellation.cancelled() => {
                return Err(ToolError::Execution("web search cancelled".to_owned()));
            }
            chunk = stream.next() => chunk,
        } {
            let chunk = chunk.map_err(|error| {
                ToolError::Execution(format!("web search response read failed: {error}"))
            })?;
            if body.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
                return Err(ToolError::Execution(format!(
                    "web search response exceeds {MAX_RESPONSE_BYTES} bytes"
                )));
            }
            body.extend_from_slice(&chunk);
        }
        let body = String::from_utf8(body).map_err(|error| {
            ToolError::Execution(format!("web search response was not UTF-8: {error}"))
        })?;
        if !status.is_success() {
            return Err(ToolError::Execution(format!(
                "web search returned HTTP {}: {}",
                status.as_u16(),
                bounded_text(&body, 512)
            )));
        }
        serde_json::from_str(&body).map_err(|error| {
            ToolError::Execution(format!("web search returned invalid JSON: {error}"))
        })
    }

    fn is_brave_endpoint(&self) -> bool {
        self.endpoint.path().contains("/res/v1/web/search")
    }
}

#[async_trait]
impl WebSearchBackend for HttpWebSearchBackend {
    async fn search(
        &self,
        request: &ToolRequest,
        cancellation: CancellationToken,
    ) -> Result<ExternalToolOutput, ToolError> {
        let (query, max_results, domains) = parse_request(&request.arguments)?;
        let key = self.cache_key(&query, max_results, &domains);
        if let Some(output) = self.cached(&key) {
            return Ok(output);
        }
        let payload = self
            .request(&query, max_results, &domains, cancellation)
            .await?;
        let results = parse_results(&payload, max_results);
        let output = ExternalToolOutput {
            summary: format!("web search returned {} results", results.len()),
            content: serde_json::to_string(&results).unwrap_or_else(|_| "[]".to_owned()),
            structured_facts: json!({
                "query": query,
                "results": results,
                "result_count": results.len(),
                "cached": false,
                "source": self.endpoint.host_str().unwrap_or("configured-search"),
            }),
            is_error: false,
        };
        self.store(key, output.clone());
        Ok(output)
    }
}

fn parse_request(arguments: &Value) -> Result<(String, usize, Vec<String>), ToolError> {
    let query = arguments
        .get("query")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| ToolError::InvalidArguments("web search query is required".to_owned()))?;
    if query.chars().count() > MAX_QUERY_CHARS {
        return Err(ToolError::InvalidArguments(
            "web search query is too long".to_owned(),
        ));
    }
    let query = query.split_whitespace().collect::<Vec<_>>().join(" ");
    let max_results =
        arguments
            .get("max_results")
            .and_then(Value::as_u64)
            .map_or(DEFAULT_MAX_RESULTS, |value| {
                usize::try_from(value)
                    .unwrap_or(MAX_RESULTS)
                    .clamp(1, MAX_RESULTS)
            });
    let mut domains = arguments
        .get("domains")
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ToOwned::to_owned)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    domains.sort_unstable();
    domains.dedup();
    Ok((query, max_results, domains))
}

fn parse_results(payload: &Value, max_results: usize) -> Vec<Value> {
    let candidates = [
        payload.pointer("/web/results"),
        payload.get("results"),
        payload.get("items"),
        payload.get("organic_results"),
    ]
    .into_iter()
    .flatten()
    .find_map(Value::as_array);
    candidates
        .into_iter()
        .flatten()
        .take(max_results)
        .filter_map(|item| {
            let title = item
                .get("title")
                .or_else(|| item.get("name"))
                .and_then(Value::as_str)
                .map(|value| bounded_text(value, 256))?;
            let url = item
                .get("url")
                .or_else(|| item.get("link"))
                .and_then(Value::as_str)
                .map(|value| bounded_text(value, MAX_RESULT_URL_CHARS))?;
            let snippet = item
                .get("description")
                .or_else(|| item.get("snippet"))
                .and_then(Value::as_str)
                .map(|value| bounded_text(value, MAX_SNIPPET_CHARS))
                .unwrap_or_default();
            Some(json!({"title": title, "url": url, "snippet": snippet}))
        })
        .collect()
}

fn bounded_text(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use golutra_core::{SessionId, ToolCallId, TurnId};
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    #[test]
    fn request_normalization_is_stable() {
        let arguments = json!({
            "query": "  Rust   async  ",
            "max_results": 4,
            "domains": ["b.example", "a.example", "a.example"]
        });
        assert_eq!(
            parse_request(&arguments).expect("request"),
            (
                "Rust async".to_owned(),
                4,
                vec!["a.example".to_owned(), "b.example".to_owned()]
            )
        );
    }

    #[test]
    fn result_parser_projects_only_bounded_fields() {
        let payload = json!({"web": {"results": [{
            "title": "Rust",
            "url": "https://example.com",
            "description": "useful"
        }]}});
        assert_eq!(
            parse_results(&payload, 5),
            vec![json!({
                "title": "Rust",
                "url": "https://example.com",
                "snippet": "useful"
            })]
        );
    }

    #[tokio::test]
    async fn search_cache_reuses_normalized_request_and_redacts_debug_credentials() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
        let address = listener.local_addr().expect("listener address");
        let calls = Arc::new(AtomicUsize::new(0));
        let server_calls = calls.clone();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("request");
            server_calls.fetch_add(1, Ordering::SeqCst);
            let mut request = [0_u8; 2048];
            let _ = stream.read(&mut request).await.expect("request bytes");
            let body =
                r#"{"results":[{"title":"Rust","url":"https://example.com","snippet":"async"}]}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream
                .write_all(response.as_bytes())
                .await
                .expect("response");
        });
        let backend = HttpWebSearchBackend::new(
            format!("http://{address}/search"),
            Some("secret-search-key".to_owned()),
        )
        .expect("backend");
        let request = ToolRequest {
            tool_call_id: ToolCallId::new(),
            provider_tool_call_id: None,
            session_id: SessionId::new(),
            turn_id: Some(TurnId::new()),
            tool_name: "web_search".to_owned(),
            arguments: json!({
                "query": "  Rust   async ",
                "domains": ["example.com", "example.com"]
            }),
        };
        let first = backend
            .search(&request, CancellationToken::new())
            .await
            .expect("first search");
        let second = backend
            .search(&request, CancellationToken::new())
            .await
            .expect("cached search");
        server.await.expect("server");

        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(first.structured_facts["cached"], false);
        assert_eq!(second.structured_facts["cached"], true);
        assert!(!format!("{backend:?}").contains("secret-search-key"));
    }
}
