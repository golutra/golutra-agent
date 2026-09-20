//! 恢复边界的故障注入：虚拟时间覆盖数小时，真实时间 soak 单独显式运行。

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use async_trait::async_trait;
use golutra_agent_core::ProviderContract;
use golutra_agent_llm::{MockProvider, ProviderErrorMetadata};

use super::*;

struct FaultProvider {
    calls: AtomicUsize,
    failures: usize,
    error: ProviderError,
    preview: bool,
    requests: Mutex<Vec<ProviderRequest>>,
}

impl FaultProvider {
    fn offline(failures: usize) -> Self {
        Self {
            calls: AtomicUsize::new(0),
            failures,
            error: ProviderError::ConnectionFailed {
                message: "fixture offline".into(),
            },
            preview: false,
            requests: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait]
impl LlmProvider for FaultProvider {
    async fn complete(&self, request: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        self.complete_stream(request, &mut |_| {}).await
    }

    async fn complete_stream(
        &self,
        request: ProviderRequest,
        on_event: &mut (dyn FnMut(ProviderStreamEvent) + Send),
    ) -> Result<ProviderResponse, ProviderError> {
        self.requests.lock().unwrap().push(request.clone());
        if self.calls.fetch_add(1, Ordering::SeqCst) < self.failures {
            if self.preview {
                on_event(ProviderStreamEvent::TextDelta {
                    text: "中文片段\n尚未完成".into(),
                });
                on_event(ProviderStreamEvent::ToolCallDelta {
                    index: 0,
                    tool_call_id: Some("unfinished-write".into()),
                    tool_name: Some("write_file".into()),
                });
            }
            return Err(self.error.clone());
        }
        MockProvider::text_response("中文完整输出\n完成")
            .complete_stream(request, on_event)
            .await
    }

    fn contract(&self) -> ProviderContract {
        MockProvider::text_response("").contract()
    }
}

#[tokio::test(start_paused = true)]
async fn offline_for_hours_recovers_the_same_request_while_background_work_progresses() {
    let provider = FaultProvider::offline(240);
    let session = ProviderSession::new(&provider, None, ProviderSessionPolicy::default())
        .with_deadline(Some(Instant::now() + Duration::from_secs(4 * 3600)));
    let original = super::tests::request();
    let ticks = Arc::new(AtomicUsize::new(0));
    let background_ticks = ticks.clone();
    let background = tokio::spawn(async move {
        loop {
            sleep(Duration::from_secs(30)).await;
            background_ticks.fetch_add(1, Ordering::SeqCst);
        }
    });
    let started = Instant::now();
    let mut waits = Vec::new();
    let (response, completed) = session
        .complete(original.clone(), &CancellationToken::new(), &mut |event| {
            if let ProviderSessionEvent::Recovery(recovery) = event
                && recovery.phase == RecoveryPhase::Waiting
            {
                waits.push(recovery);
            }
        })
        .await
        .expect("network restores without another user turn");
    background.abort();
    assert!(started.elapsed() > Duration::from_secs(3 * 3600));
    assert!(ticks.load(Ordering::SeqCst) > 300);
    assert_eq!(completed.request_id, original.request_id);
    assert_eq!(completed.turn_id, original.turn_id);
    assert_eq!(
        completed
            .messages
            .iter()
            .filter(|message| message.content.starts_with("Runtime recovery:"))
            .count(),
        1
    );
    assert!(response.message.unwrap().content.contains("完成"));
    assert_eq!(waits.len(), 240);
    assert!(
        waits
            .iter()
            .all(|wait| wait.network && wait.delay_ms <= 60_000)
    );
    assert!(
        provider
            .requests
            .lock()
            .unwrap()
            .iter()
            .all(|request| request.request_id == original.request_id)
    );
}

#[test]
fn recovery_reminder_never_evicts_history_or_exceeds_its_budget() {
    let mut request = super::tests::request();
    let before = request.clone();
    add_recovery_reminder(&mut request, Duration::from_secs(120), 0);
    assert_eq!(request, before);
    add_recovery_reminder(&mut request, Duration::from_secs(120), 1024);
    add_recovery_reminder(&mut request, Duration::from_secs(600), 1024);
    assert_eq!(request.messages.len(), 1);
}

#[tokio::test(start_paused = true)]
async fn offline_wait_obeys_absolute_deadline_and_never_completes_a_task() {
    let provider = FaultProvider::offline(usize::MAX);
    let session = ProviderSession::new(&provider, None, ProviderSessionPolicy::default())
        .with_deadline(Some(Instant::now() + Duration::from_secs(4 * 3600)));
    let started = Instant::now();
    let error = session
        .complete(
            super::tests::request(),
            &CancellationToken::new(),
            &mut |_| {},
        )
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        ProviderSessionError::DeadlineExceeded { .. }
    ));
    assert_eq!(started.elapsed(), Duration::from_secs(4 * 3600));
}

#[tokio::test(start_paused = true)]
async fn cancel_during_connection_backoff_is_immediate() {
    let provider = FaultProvider::offline(usize::MAX);
    let cancel = CancellationToken::new();
    let session = ProviderSession::new(&provider, None, ProviderSessionPolicy::default());
    let started = Instant::now();
    let error = session
        .complete(super::tests::request(), &cancel, &mut |event| {
            if matches!(event, ProviderSessionEvent::Recovery(_)) {
                cancel.cancel();
            }
        })
        .await
        .unwrap_err();
    assert_eq!(
        error,
        ProviderSessionError::Provider(ProviderError::Cancelled)
    );
    assert_eq!(started.elapsed(), Duration::ZERO);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn partial_text_and_tool_arguments_have_a_durable_retry_boundary() {
    let mut provider = FaultProvider::offline(1);
    provider.preview = true;
    let session = ProviderSession::new(&provider, None, ProviderSessionPolicy::default());
    let mut events = Vec::new();
    let (response, _) = session
        .complete(
            super::tests::request(),
            &CancellationToken::new(),
            &mut |event| events.push(event),
        )
        .await
        .unwrap();
    assert!(
        response.tool_calls.is_empty(),
        "unfinished tool delta must never become executable"
    );
    let reset = events
        .iter()
        .position(|event| {
            matches!(
                event,
                ProviderSessionEvent::Recovery(ProviderRecovery {
                    reset_stream: true,
                    ..
                })
            )
        })
        .unwrap();
    let final_text = events.iter().position(|event| matches!(event, ProviderSessionEvent::Streamed { event: ProviderStreamEvent::TextDelta { text }, .. } if text.contains("完整"))).unwrap();
    assert!(reset < final_text);
}

#[tokio::test(start_paused = true)]
async fn retry_after_120_seconds_is_not_shortened() {
    let mut provider = FaultProvider::offline(1);
    provider.error = ProviderError::RateLimited {
        message: "busy".into(),
    }
    .with_metadata(ProviderErrorMetadata {
        http_status: Some(429),
        retry_after: Some(Duration::from_secs(120)),
        ..Default::default()
    });
    let started = Instant::now();
    ProviderSession::new(&provider, None, ProviderSessionPolicy::default())
        .complete(
            super::tests::request(),
            &CancellationToken::new(),
            &mut |_| {},
        )
        .await
        .unwrap();
    assert_eq!(started.elapsed(), Duration::from_secs(120));
}

#[tokio::test(start_paused = true)]
async fn auth_protocol_and_explicit_client_errors_fail_without_waiting() {
    for error in [
        ProviderError::Malformed {
            message: "stream JSON invalid".into(),
        },
        ProviderError::NotConfigured {
            message: "missing credential".into(),
        },
        ProviderError::Failed {
            message: "connection invalid model".into(),
        }
        .with_metadata(ProviderErrorMetadata {
            http_status: Some(400),
            ..Default::default()
        }),
        ProviderError::ConnectionFailed {
            message: "unauthorized".into(),
        }
        .with_metadata(ProviderErrorMetadata {
            http_status: Some(401),
            ..Default::default()
        }),
    ] {
        let mut provider = FaultProvider::offline(usize::MAX);
        provider.error = error;
        let started = Instant::now();
        assert!(
            ProviderSession::new(&provider, None, ProviderSessionPolicy::default())
                .complete(
                    super::tests::request(),
                    &CancellationToken::new(),
                    &mut |_| {}
                )
                .await
                .is_err()
        );
        assert_eq!(started.elapsed(), Duration::ZERO);
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test(start_paused = true)]
async fn auxiliary_summary_requests_keep_bounded_connection_retries() {
    let provider = FaultProvider::offline(usize::MAX);
    let policy = ProviderSessionPolicy {
        enable_transport_fallback: false,
        ..Default::default()
    };
    assert!(
        ProviderSession::new(&provider, None, policy)
            .with_connection_wait(false)
            .complete(
                super::tests::request(),
                &CancellationToken::new(),
                &mut |_| {}
            )
            .await
            .is_err()
    );
    assert_eq!(provider.calls.load(Ordering::SeqCst), 3);
}

#[tokio::test(start_paused = true)]
async fn an_http_failure_never_enters_unbounded_connection_wait() {
    let mut provider = FaultProvider::offline(usize::MAX);
    provider.error = provider.error.with_metadata(ProviderErrorMetadata {
        http_status: Some(503),
        ..Default::default()
    });
    let started = Instant::now();
    let mut events = Vec::new();
    assert!(
        ProviderSession::new(&provider, None, ProviderSessionPolicy::default())
            .complete(
                super::tests::request(),
                &CancellationToken::new(),
                &mut |event| events.push(event)
            )
            .await
            .is_err()
    );
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(provider.calls.load(Ordering::SeqCst), 6);
    assert!(events.iter().all(|event| !matches!(
        event,
        ProviderSessionEvent::Recovery(ProviderRecovery { network: true, .. })
    )));
}

fn http_provider(address: std::net::SocketAddr) -> golutra_agent_llm::OpenAiCompatibleProvider {
    use golutra_agent_llm::*;
    OpenAiCompatibleProvider::from_config(OpenAiCompatibleProviderConfig {
        api_key: "fixture-not-a-secret".into(),
        api_key_env: "FIXTURE_KEY".into(),
        provider_id: "offline-fixture".into(),
        base_url: format!("http://{address}/v1"),
        model_id: "fixture".into(),
        protocol: ProviderProtocol::OpenAiCompatible,
        generation_config: Default::default(),
        custom_headers: Default::default(),
        cache_capabilities: None,
    })
}

async fn serve_response(listener: &tokio::net::TcpListener, complete: bool) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let (mut socket, _) = listener.accept().await.unwrap();
    let mut header = Vec::new();
    while !header.ends_with(b"\r\n\r\n") {
        header.push(socket.read_u8().await.unwrap());
        assert!(header.len() < 65536);
    }
    let header = String::from_utf8(header).unwrap();
    let length = header
        .lines()
        .find_map(|line| {
            line.to_ascii_lowercase()
                .strip_prefix("content-length:")
                .and_then(|value| value.trim().parse::<usize>().ok())
        })
        .unwrap();
    assert!(length < 1024 * 1024);
    let mut request = vec![0; length];
    socket.read_exact(&mut request).await.unwrap();
    assert!(header.starts_with("POST /v1/chat/completions"));
    let body = if complete {
        "data: {\"choices\":[{\"delta\":{\"content\":\"恢复后的完整中文。\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n"
    } else {
        "data: {\"choices\":[{\"delta\":{\"content\":\"中断的半句\"},\"finish_reason\":null}]}\n\n"
    };
    socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).as_bytes()).await.unwrap();
    // 分段写入包含中文的 UTF-8 流，接收端必须保持事件顺序。
    for chunk in body.as_bytes().chunks(7) {
        socket.write_all(chunk).await.unwrap();
        tokio::task::yield_now().await;
    }
}

#[tokio::test]
async fn real_http_partial_stream_recovers_without_joining_attempts() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let provider = http_provider(listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        serve_response(&listener, false).await;
        serve_response(&listener, true).await;
    });
    let mut reset_count = 0;
    let result = ProviderSession::new(&provider, None, ProviderSessionPolicy::default())
        .with_deadline(Some(Instant::now() + Duration::from_secs(15)))
        .complete(
            super::tests::request(),
            &CancellationToken::new(),
            &mut |event| {
                if matches!(
                    event,
                    ProviderSessionEvent::Recovery(ProviderRecovery {
                        reset_stream: true,
                        ..
                    })
                ) {
                    reset_count += 1;
                }
            },
        )
        .await
        .unwrap();
    assert_eq!(reset_count, 1);
    assert_eq!(result.0.message.unwrap().content, "恢复后的完整中文。");
    server.await.unwrap();
}

#[tokio::test]
async fn real_connection_refusal_recovers_after_server_returns() {
    real_offline_recovery(Duration::from_millis(100)).await;
}

async fn real_offline_recovery(outage: Duration) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    let provider = http_provider(address);
    let (signal, received) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        received.await.unwrap();
        sleep(outage).await;
        let listener = tokio::net::TcpListener::bind(address).await.unwrap();
        serve_response(&listener, true).await;
    });
    let mut signal = Some(signal);
    let mut waits = 0;
    let request = super::tests::request();
    let id = request.request_id;
    let started = Instant::now();
    let (response, actual) =
        ProviderSession::new(&provider, None, ProviderSessionPolicy::default())
            .with_deadline(Some(Instant::now() + outage + Duration::from_secs(120)))
            .complete(request, &CancellationToken::new(), &mut |event| {
                if let ProviderSessionEvent::Recovery(recovery) = event
                    && recovery.phase == RecoveryPhase::Waiting
                {
                    assert!(recovery.network);
                    waits += 1;
                    if let Some(signal) = signal.take() {
                        let _ = signal.send(());
                    }
                }
            })
            .await
            .unwrap();
    assert!(waits > 0);
    assert_eq!(actual.request_id, id);
    assert_eq!(response.message.unwrap().content, "恢复后的完整中文。");
    assert!(started.elapsed() >= outage);
    server.await.unwrap();
    eprintln!(
        "real TCP outage recovered: elapsed={}s retries={waits} request={id}",
        started.elapsed().as_secs()
    );
}

#[tokio::test]
#[ignore = "real wall-clock outage soak; set GOLUTRA_AGENT_OUTAGE_SOAK_SECONDS"]
async fn real_connection_outage_soak() {
    let seconds = std::env::var("GOLUTRA_AGENT_OUTAGE_SOAK_SECONDS")
        .ok()
        .map(|value| value.parse::<u64>().expect("soak duration in seconds"))
        .unwrap_or(120);
    real_offline_recovery(Duration::from_secs(seconds)).await;
}
