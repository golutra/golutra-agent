//! 单次逻辑请求的可取消恢复：断网等待与有限重试分离，并共同服从任务截止时间。
//! 流式增量仅作预览；只有完整响应才能交给 AgentLoop 执行工具。

use std::time::Duration;

use golutra_agent_llm::{
    LlmProvider, ProviderError, ProviderRequest, ProviderResponse, ProviderStreamEvent,
};
use tokio::sync::mpsc;
use tokio::time::{Instant, sleep, timeout_at};
use tokio_util::sync::CancellationToken;

use super::provider_recovery::{ProviderRecovery, RecoveryPhase, RetryState, duration_ms};
use super::provider_retry;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderTransport {
    #[default]
    Streaming,
    Buffered,
}

impl ProviderTransport {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Streaming => "streaming",
            Self::Buffered => "buffered",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderSessionPolicy {
    /// 普通流错误的重试额度；已确认的连接故障另行等待，仍受任务截止时间约束。
    pub max_stream_retries: u32,
    /// 非流式请求的普通瞬态错误额度，不包含连接等待。
    pub max_request_retries: u32,
    /// Maximum time without a stream event before the attempt is considered lost.
    pub stream_idle_timeout: Duration,
    /// Total deadline for one buffered request attempt.
    pub request_timeout: Duration,
    /// Try the provider's non-streaming transport after streaming retries are exhausted.
    pub enable_transport_fallback: bool,
}

impl Default for ProviderSessionPolicy {
    fn default() -> Self {
        Self {
            max_stream_retries: 2,
            max_request_retries: 2,
            stream_idle_timeout: Duration::from_secs(300),
            request_timeout: Duration::from_secs(300),
            enable_transport_fallback: true,
        }
    }
}

impl ProviderSessionPolicy {
    #[must_use]
    pub fn bounded(mut self) -> Self {
        self.max_stream_retries = self.max_stream_retries.min(8);
        self.max_request_retries = self.max_request_retries.min(8);
        self.stream_idle_timeout = self.stream_idle_timeout.max(Duration::from_millis(1));
        self.request_timeout = self.request_timeout.max(Duration::from_millis(1));
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderSessionEvent {
    Streamed {
        provider_id: String,
        model_id: String,
        event: ProviderStreamEvent,
    },
    Recovery(ProviderRecovery),
    TransportFallback {
        provider_id: String,
        from: ProviderTransport,
        to: ProviderTransport,
        reason: String,
    },
    ProviderFallback {
        from_provider: String,
        to_provider: String,
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ProviderSessionError {
    Provider(ProviderError),
    DeadlineExceeded { reason: String },
}

struct StreamAttemptFailure {
    error: ProviderError,
    preview_seen: bool,
}

pub(crate) struct ProviderSession<'a, P> {
    primary: &'a P,
    fallback: Option<&'a P>,
    policy: ProviderSessionPolicy,
    deadline: Option<Instant>,
    allow_connection_wait: bool,
    input_budget: u64,
}

impl<'a, P> ProviderSession<'a, P>
where
    P: LlmProvider,
{
    pub(crate) fn new(
        primary: &'a P,
        fallback: Option<&'a P>,
        policy: ProviderSessionPolicy,
    ) -> Self {
        Self {
            primary,
            fallback,
            policy: policy.bounded(),
            deadline: None,
            allow_connection_wait: true,
            input_budget: u64::MAX,
        }
    }

    #[must_use]
    pub(crate) fn with_deadline(mut self, deadline: Option<Instant>) -> Self {
        self.deadline = deadline;
        self
    }

    pub(crate) fn with_connection_wait(mut self, enabled: bool) -> Self {
        self.allow_connection_wait = enabled;
        self
    }

    pub(crate) fn with_input_budget(mut self, budget: u64) -> Self {
        self.input_budget = budget;
        self
    }

    pub(crate) async fn complete<E>(
        &self,
        request: ProviderRequest,
        cancellation: &CancellationToken,
        on_event: &mut E,
    ) -> Result<(ProviderResponse, ProviderRequest), ProviderSessionError>
    where
        E: FnMut(ProviderSessionEvent) + Send,
    {
        let Some(deadline) = self.deadline else {
            return self
                .complete_without_deadline(request, cancellation, on_event)
                .await
                .map_err(ProviderSessionError::Provider);
        };
        match timeout_at(
            deadline,
            self.complete_without_deadline(request, cancellation, on_event),
        )
        .await
        {
            Ok(result) => result.map_err(ProviderSessionError::Provider),
            Err(_) => Err(ProviderSessionError::DeadlineExceeded {
                reason: "provider session exceeded the runtime wall-clock deadline".to_owned(),
            }),
        }
    }

    async fn complete_without_deadline<E>(
        &self,
        mut request: ProviderRequest,
        cancellation: &CancellationToken,
        on_event: &mut E,
    ) -> Result<(ProviderResponse, ProviderRequest), ProviderError>
    where
        E: FnMut(ProviderSessionEvent) + Send,
    {
        match self
            .complete_provider(self.primary, &mut request, cancellation, on_event)
            .await
        {
            Ok(response) => Ok((response, request)),
            Err(primary_failure) => {
                let Some(fallback) = self.fallback else {
                    return Err(primary_failure);
                };
                if !provider_retry::fallback_eligible(&primary_failure) {
                    return Err(primary_failure);
                };
                let from_provider = self.primary.contract().provider_id;
                let to_provider = fallback.contract().provider_id;
                on_event(ProviderSessionEvent::Recovery(ProviderRecovery {
                    phase: RecoveryPhase::Retrying,
                    attempt: 0,
                    delay_ms: 0,
                    waited_ms: 0,
                    network: false,
                    reset_stream: true,
                    reason: "switching provider".to_owned(),
                    transport: ProviderTransport::Streaming,
                    error_metadata: primary_failure.metadata().cloned(),
                }));
                on_event(ProviderSessionEvent::ProviderFallback {
                    from_provider,
                    to_provider: to_provider.clone(),
                    reason: retry_reason(&primary_failure),
                });
                let mut fallback_request = request;
                fallback_request.provider_id = to_provider;
                fallback_request.model_id = fallback.contract().model_id;
                self.complete_provider(fallback, &mut fallback_request, cancellation, on_event)
                    .await
                    .map(|response| (response, fallback_request))
            }
        }
    }

    async fn complete_provider<E>(
        &self,
        provider: &P,
        request: &mut ProviderRequest,
        cancellation: &CancellationToken,
        on_event: &mut E,
    ) -> Result<ProviderResponse, ProviderError>
    where
        E: FnMut(ProviderSessionEvent) + Send,
    {
        let mut retries = RetryState::default();
        let error = loop {
            if self.allow_connection_wait {
                add_recovery_reminder(request, retries.waited, self.input_budget);
            }
            match self
                .complete_stream_attempt(provider, request.clone(), cancellation, on_event)
                .await
            {
                Ok(response) => return Ok(response),
                Err(failure) => {
                    let Some((delay, recovery)) = retries.schedule(
                        &failure.error,
                        self.policy.max_stream_retries,
                        self.allow_connection_wait,
                        request.request_id.0.as_u128() as u64,
                        failure.preview_seen,
                    ) else {
                        // 换传输之前也必须结束旧预览，不能将 buffered 结果接到半句话后。
                        if failure.preview_seen
                            && self.policy.enable_transport_fallback
                            && provider.supports_buffered_transport()
                            && provider_retry::is_retryable(&failure.error)
                        {
                            on_event(ProviderSessionEvent::Recovery(ProviderRecovery {
                                phase: RecoveryPhase::Retrying,
                                attempt: 0,
                                delay_ms: 0,
                                waited_ms: duration_ms(retries.waited),
                                network: false,
                                reset_stream: true,
                                reason: "switching to buffered transport".to_owned(),
                                transport: ProviderTransport::Buffered,
                                error_metadata: failure.error.metadata().cloned(),
                            }));
                        }
                        break failure.error;
                    };
                    if !wait_for_recovery(delay, recovery, &mut retries, cancellation, on_event)
                        .await
                    {
                        return Err(ProviderError::Cancelled);
                    }
                }
            }
        };
        if self.policy.enable_transport_fallback
            && provider.supports_buffered_transport()
            && provider_retry::is_retryable(&error)
        {
            let provider_id = provider.contract().provider_id;
            on_event(ProviderSessionEvent::TransportFallback {
                provider_id,
                from: ProviderTransport::Streaming,
                to: ProviderTransport::Buffered,
                reason: retry_reason(&error),
            });
            return self
                .complete_buffered(provider, request, cancellation, on_event, &mut retries)
                .await;
        }
        Err(error)
    }

    async fn complete_stream_attempt<E>(
        &self,
        provider: &P,
        request: ProviderRequest,
        cancellation: &CancellationToken,
        on_event: &mut E,
    ) -> Result<ProviderResponse, StreamAttemptFailure>
    where
        E: FnMut(ProviderSessionEvent) + Send,
    {
        let contract = provider.contract();
        let provider_id = contract.provider_id;
        let model_id = contract.model_id;
        let (event_sender, mut event_receiver) = mpsc::unbounded_channel();
        let mut callback = move |event| {
            let _ = event_sender.send(event);
        };
        let future = provider.complete_stream(request, &mut callback);
        tokio::pin!(future);
        let mut idle_deadline = Box::pin(sleep(self.policy.stream_idle_timeout));
        let mut preview_seen = false;

        loop {
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => return Err(StreamAttemptFailure {
                    error: ProviderError::Cancelled,
                    preview_seen,
                }),
                result = &mut future => {
                    while let Ok(event) = event_receiver.try_recv() {
                        preview_seen |= is_preview_event(&event);
                        on_event(ProviderSessionEvent::Streamed {
                            provider_id: provider_id.clone(),
                            model_id: model_id.clone(),
                            event,
                        });
                    }
                    return result.map_err(|error| StreamAttemptFailure {
                        error,
                        preview_seen,
                    });
                }
                event = event_receiver.recv() => {
                    let Some(event) = event else {
                        return Err(StreamAttemptFailure {
                            error: ProviderError::Failed {
                                message: "provider stream event channel closed".to_owned(),
                            },
                            preview_seen,
                        });
                    };
                    preview_seen |= is_preview_event(&event);
                    on_event(ProviderSessionEvent::Streamed {
                        provider_id: provider_id.clone(),
                        model_id: model_id.clone(),
                        event,
                    });
                    idle_deadline
                        .as_mut()
                        .reset(Instant::now() + self.policy.stream_idle_timeout);
                }
                _ = &mut idle_deadline => {
                    return Err(StreamAttemptFailure {
                        error: ProviderError::Timeout {
                            message: format!(
                                "provider stream idle for {} ms",
                                self.policy.stream_idle_timeout.as_millis()
                            ),
                        },
                        preview_seen,
                    });
                }
            }
        }
    }

    async fn complete_buffered<E>(
        &self,
        provider: &P,
        request: &mut ProviderRequest,
        cancellation: &CancellationToken,
        on_event: &mut E,
        retries: &mut RetryState,
    ) -> Result<ProviderResponse, ProviderError>
    where
        E: FnMut(ProviderSessionEvent) + Send,
    {
        // 传输降级有独立的普通重试额度；累计等待时间仍属于同一逻辑请求。
        retries.reset_transport_budget();
        loop {
            if self.allow_connection_wait {
                add_recovery_reminder(request, retries.waited, self.input_budget);
            }
            let result = self
                .complete_buffered_attempt(provider, request.clone(), cancellation)
                .await;
            match result {
                Ok(response) => {
                    emit_response_events(provider, &response, on_event);
                    return Ok(response);
                }
                Err(error) => {
                    let Some((delay, recovery)) = retries.schedule(
                        &error,
                        self.policy.max_request_retries,
                        self.allow_connection_wait,
                        request.request_id.0.as_u128() as u64,
                        false,
                    ) else {
                        return Err(error);
                    };
                    if !wait_for_recovery(delay, recovery, retries, cancellation, on_event).await {
                        return Err(ProviderError::Cancelled);
                    }
                }
            }
        }
    }

    async fn complete_buffered_attempt(
        &self,
        provider: &P,
        request: ProviderRequest,
        cancellation: &CancellationToken,
    ) -> Result<ProviderResponse, ProviderError> {
        let future = provider.complete(request);
        tokio::pin!(future);
        let timeout = sleep(self.policy.request_timeout);
        tokio::pin!(timeout);
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => Err(ProviderError::Cancelled),
            result = &mut future => result,
            _ = &mut timeout => Err(ProviderError::Timeout {
                message: format!(
                    "buffered provider request exceeded {} ms",
                    self.policy.request_timeout.as_millis()
                ),
            }),
        }
    }
}

async fn wait_backoff(delay: Duration, cancellation: &CancellationToken) -> bool {
    let Some(deadline) = Instant::now().checked_add(delay) else {
        // 极大的 Retry-After 不能溢出或提前重试；外层任务截止时间仍然生效。
        cancellation.cancelled().await;
        return false;
    };
    tokio::select! {
        _ = cancellation.cancelled() => false,
        _ = tokio::time::sleep_until(deadline) => true,
    }
}

fn add_recovery_reminder(request: &mut ProviderRequest, waited: Duration, input_budget: u64) {
    const REMINDER: &str = "Runtime recovery: this request waited at least 60 seconds. Continue the existing task using completed tool results. Before changing files or external state, recheck only the facts that may have changed during the wait. Do not repeat completed side effects.";
    if waited < Duration::from_secs(60)
        || request
            .messages
            .iter()
            .any(|message| message.content == REMINDER)
    {
        return;
    }
    // 动态提示使用 user 段，避免 genai 把末尾 system 消息提升到静态前缀。
    // 仅在长等待后追加一次；保留完整工具配对，返回的 completed_request 包含该提示。
    let reminder = golutra_agent_llm::ProviderMessage {
        role: golutra_agent_llm::ProviderRole::User,
        content: REMINDER.to_owned(),
        tool_call_id: None,
        tool_name: None,
        tool_calls: Vec::new(),
        metadata: Default::default(),
    };
    let input_tokens = golutra_agent_context::estimate_message_tokens(&request.messages)
        .saturating_add(golutra_agent_llm::estimate_provider_tool_tokens(
            &request.tools,
        ))
        .saturating_add(golutra_agent_context::estimate_message_tokens(
            std::slice::from_ref(&reminder),
        ));
    // 恢复提示不能挤掉原始事实或绕过上下文预算；紧贴预算时保留原请求。
    if input_tokens <= input_budget {
        request.messages.push(reminder);
    }
}

async fn wait_for_recovery<E: FnMut(ProviderSessionEvent)>(
    delay: Duration,
    mut recovery: ProviderRecovery,
    retries: &mut RetryState,
    cancellation: &CancellationToken,
    on_event: &mut E,
) -> bool {
    on_event(ProviderSessionEvent::Recovery(recovery.clone()));
    let started = Instant::now();
    if !wait_backoff(delay, cancellation).await {
        return false;
    }
    retries.waited += started.elapsed();
    recovery.phase = RecoveryPhase::Retrying;
    recovery.waited_ms = duration_ms(retries.waited);
    recovery.reset_stream = false;
    on_event(ProviderSessionEvent::Recovery(recovery));
    true
}

fn is_preview_event(event: &ProviderStreamEvent) -> bool {
    match event {
        // 推理不进入正文；正文与工具增量也仅供展示，不表示工具已执行。
        ProviderStreamEvent::ReasoningDelta { .. } => false,
        ProviderStreamEvent::TextDelta { text } => !text.is_empty(),
        ProviderStreamEvent::ToolCallDelta {
            tool_call_id,
            tool_name,
            ..
        } => {
            tool_call_id.as_ref().is_some_and(|value| !value.is_empty())
                || tool_name.as_ref().is_some_and(|value| !value.is_empty())
        }
    }
}

fn retry_reason(error: &ProviderError) -> String {
    let Some(metadata) = error.metadata() else {
        return error.to_string();
    };
    let mut reason = error.to_string();
    if let Some(status) = metadata.http_status {
        reason = format!("HTTP {status}: {reason}");
    }
    if let Some(code) = metadata.provider_code.as_deref() {
        reason = format!("{code}: {reason}");
    }
    reason
}

fn emit_response_events<P, E>(provider: &P, response: &ProviderResponse, on_event: &mut E)
where
    P: LlmProvider,
    E: FnMut(ProviderSessionEvent),
{
    let contract = provider.contract();
    if let Some(message) = response
        .message
        .as_ref()
        .filter(|message| !message.content.is_empty())
    {
        on_event(ProviderSessionEvent::Streamed {
            provider_id: contract.provider_id.clone(),
            model_id: contract.model_id.clone(),
            event: ProviderStreamEvent::TextDelta {
                text: message.content.clone(),
            },
        });
    }
    for (index, call) in response.tool_calls.iter().enumerate() {
        on_event(ProviderSessionEvent::Streamed {
            provider_id: contract.provider_id.clone(),
            model_id: contract.model_id.clone(),
            event: ProviderStreamEvent::ToolCallDelta {
                index,
                tool_call_id: Some(call.tool_call_id.clone()),
                tool_name: Some(call.tool_name.clone()),
            },
        });
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use async_trait::async_trait;
    use golutra_agent_core::{ProviderContract, ProviderRequestId, TaskId, TurnId};
    use golutra_agent_llm::{MockProvider, ProviderRequest};

    use super::*;

    #[derive(Debug, Clone)]
    struct FlakyStreamProvider {
        success: MockProvider,
        stream_calls: Arc<AtomicUsize>,
        buffered_calls: Arc<AtomicUsize>,
        failures_before_success: usize,
        always_idle: bool,
    }

    #[derive(Debug, Clone)]
    struct ProgressingStreamProvider {
        success: MockProvider,
        event_interval: Duration,
        event_count: usize,
    }

    #[derive(Debug, Clone)]
    struct PartialThenFailProvider {
        success: MockProvider,
        stream_calls: Arc<AtomicUsize>,
    }

    #[derive(Debug, Clone)]
    struct ReasoningThenFailProvider {
        success: MockProvider,
        stream_calls: Arc<AtomicUsize>,
    }

    impl FlakyStreamProvider {
        fn new(failures_before_success: usize) -> Self {
            Self {
                success: MockProvider::text_response("done"),
                stream_calls: Arc::new(AtomicUsize::new(0)),
                buffered_calls: Arc::new(AtomicUsize::new(0)),
                failures_before_success,
                always_idle: false,
            }
        }

        fn idle() -> Self {
            Self {
                always_idle: true,
                ..Self::new(0)
            }
        }
    }

    #[async_trait]
    impl LlmProvider for PartialThenFailProvider {
        async fn complete(
            &self,
            request: ProviderRequest,
        ) -> Result<ProviderResponse, ProviderError> {
            self.success.complete(request).await
        }

        async fn complete_stream(
            &self,
            request: ProviderRequest,
            on_event: &mut (dyn FnMut(ProviderStreamEvent) + Send),
        ) -> Result<ProviderResponse, ProviderError> {
            let call = self.stream_calls.fetch_add(1, Ordering::SeqCst);
            if call == 0 {
                on_event(ProviderStreamEvent::TextDelta {
                    text: "partial".to_owned(),
                });
                return Err(ProviderError::Unavailable {
                    message: "stream disconnected after output".to_owned(),
                });
            }
            self.success.complete_stream(request, on_event).await
        }

        fn contract(&self) -> ProviderContract {
            self.success.contract()
        }
    }

    #[async_trait]
    impl LlmProvider for ReasoningThenFailProvider {
        async fn complete(
            &self,
            request: ProviderRequest,
        ) -> Result<ProviderResponse, ProviderError> {
            self.success.complete(request).await
        }

        async fn complete_stream(
            &self,
            request: ProviderRequest,
            on_event: &mut (dyn FnMut(ProviderStreamEvent) + Send),
        ) -> Result<ProviderResponse, ProviderError> {
            let call = self.stream_calls.fetch_add(1, Ordering::SeqCst);
            if call == 0 {
                on_event(ProviderStreamEvent::ReasoningDelta {
                    text: "planning before transient disconnect".to_owned(),
                });
                return Err(ProviderError::Unavailable {
                    message: "stream disconnected during reasoning".to_owned(),
                });
            }
            self.success.complete_stream(request, on_event).await
        }

        fn contract(&self) -> ProviderContract {
            self.success.contract()
        }
    }

    #[async_trait]
    impl LlmProvider for ProgressingStreamProvider {
        async fn complete(
            &self,
            request: ProviderRequest,
        ) -> Result<ProviderResponse, ProviderError> {
            self.success.complete(request).await
        }

        async fn complete_stream(
            &self,
            request: ProviderRequest,
            on_event: &mut (dyn FnMut(ProviderStreamEvent) + Send),
        ) -> Result<ProviderResponse, ProviderError> {
            for _ in 0..self.event_count {
                sleep(self.event_interval).await;
                on_event(ProviderStreamEvent::TextDelta {
                    text: ".".to_owned(),
                });
            }
            self.success.complete_stream(request, on_event).await
        }

        fn contract(&self) -> ProviderContract {
            self.success.contract()
        }
    }

    #[async_trait]
    impl LlmProvider for FlakyStreamProvider {
        async fn complete(
            &self,
            request: ProviderRequest,
        ) -> Result<ProviderResponse, ProviderError> {
            self.buffered_calls.fetch_add(1, Ordering::SeqCst);
            self.success.complete(request).await
        }

        async fn complete_stream(
            &self,
            request: ProviderRequest,
            on_event: &mut (dyn FnMut(ProviderStreamEvent) + Send),
        ) -> Result<ProviderResponse, ProviderError> {
            let call = self.stream_calls.fetch_add(1, Ordering::SeqCst);
            if self.always_idle {
                sleep(Duration::from_secs(60)).await;
            }
            if call < self.failures_before_success {
                return Err(ProviderError::Unavailable {
                    message: "connection reset by fixture".to_owned(),
                });
            }
            self.success.complete_stream(request, on_event).await
        }

        fn contract(&self) -> ProviderContract {
            self.success.contract()
        }
    }

    #[test]
    fn default_retry_budget_is_bounded_for_one_logical_turn() {
        let policy = ProviderSessionPolicy::default();
        assert_eq!(policy.max_stream_retries, 2);
        assert_eq!(policy.max_request_retries, 2);
        assert_eq!(policy.bounded().max_stream_retries, 2);
    }

    #[tokio::test]
    async fn reconnects_a_dropped_stream_inside_the_same_provider_session() {
        let provider = FlakyStreamProvider::new(2);
        let policy = ProviderSessionPolicy {
            max_stream_retries: 2,
            max_request_retries: 0,
            enable_transport_fallback: false,
            stream_idle_timeout: Duration::from_secs(1),
            request_timeout: Duration::from_secs(1),
        };
        let session = ProviderSession::new(&provider, None, policy);
        let mut events = Vec::new();

        let (response, _) = session
            .complete(request(), &CancellationToken::new(), &mut |event| {
                events.push(event)
            })
            .await
            .expect("reconnected response");

        assert_eq!(response.message.expect("message").content, "done");
        assert_eq!(provider.stream_calls.load(Ordering::SeqCst), 3);
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(
                    event,
                    ProviderSessionEvent::Recovery(ProviderRecovery {
                        phase: RecoveryPhase::Waiting,
                        ..
                    })
                ))
                .count(),
            2
        );
        assert!(events.iter().any(|event| matches!(
            event,
            ProviderSessionEvent::Streamed {
                event: ProviderStreamEvent::TextDelta { text },
                ..
            } if text == "done"
        )));
    }

    #[tokio::test]
    async fn retries_partial_output_with_an_explicit_preview_boundary() {
        let provider = PartialThenFailProvider {
            success: MockProvider::text_response("replayed"),
            stream_calls: Arc::new(AtomicUsize::new(0)),
        };
        let policy = ProviderSessionPolicy {
            max_stream_retries: 2,
            max_request_retries: 0,
            enable_transport_fallback: true,
            stream_idle_timeout: Duration::from_secs(1),
            request_timeout: Duration::from_secs(1),
        };
        let session = ProviderSession::new(&provider, None, policy);
        let mut events = Vec::new();

        let (response, _) = session
            .complete(request(), &CancellationToken::new(), &mut |event| {
                events.push(event)
            })
            .await
            .expect("partial preview can be retried before tool execution");

        assert_eq!(response.message.unwrap().content, "replayed");
        assert_eq!(provider.stream_calls.load(Ordering::SeqCst), 2);
        assert!(events.iter().any(|event| matches!(
            event,
            ProviderSessionEvent::Recovery(ProviderRecovery {
                reset_stream: true,
                ..
            })
        )));
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(
                    event,
                    ProviderSessionEvent::Streamed {
                        event: ProviderStreamEvent::TextDelta { .. },
                        ..
                    }
                ))
                .count(),
            2
        );
    }

    #[tokio::test]
    async fn replays_a_stream_after_reasoning_only_transient_failure() {
        let provider = ReasoningThenFailProvider {
            success: MockProvider::text_response("recovered"),
            stream_calls: Arc::new(AtomicUsize::new(0)),
        };
        let policy = ProviderSessionPolicy {
            max_stream_retries: 1,
            max_request_retries: 0,
            enable_transport_fallback: false,
            stream_idle_timeout: Duration::from_secs(1),
            request_timeout: Duration::from_secs(1),
        };
        let session = ProviderSession::new(&provider, None, policy);
        let mut events = Vec::new();

        let (response, _) = session
            .complete(request(), &CancellationToken::new(), &mut |event| {
                events.push(event)
            })
            .await
            .expect("reasoning-only failure should be replayable");

        assert_eq!(response.message.expect("message").content, "recovered");
        assert_eq!(provider.stream_calls.load(Ordering::SeqCst), 2);
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(
                    event,
                    ProviderSessionEvent::Recovery(ProviderRecovery {
                        phase: RecoveryPhase::Waiting,
                        ..
                    })
                ))
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(
                    event,
                    ProviderSessionEvent::Streamed {
                        event: ProviderStreamEvent::ReasoningDelta { .. },
                        ..
                    }
                ))
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(
                    event,
                    ProviderSessionEvent::Streamed {
                        event: ProviderStreamEvent::TextDelta { text },
                        ..
                    } if text == "recovered"
                ))
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn fails_an_attempt_after_the_stream_idle_deadline() {
        let provider = FlakyStreamProvider::idle();
        let policy = ProviderSessionPolicy {
            max_stream_retries: 0,
            max_request_retries: 0,
            enable_transport_fallback: false,
            stream_idle_timeout: Duration::from_millis(10),
            request_timeout: Duration::from_secs(1),
        };
        let session = ProviderSession::new(&provider, None, policy);

        let error = session
            .complete(request(), &CancellationToken::new(), &mut |_| {})
            .await
            .expect_err("idle timeout");

        assert!(matches!(
            error,
            ProviderSessionError::Provider(ProviderError::Timeout { .. })
        ));
        assert_eq!(provider.stream_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn stream_events_reset_the_idle_deadline() {
        let provider = ProgressingStreamProvider {
            success: MockProvider::text_response("done"),
            event_interval: Duration::from_millis(60),
            event_count: 2,
        };
        let policy = ProviderSessionPolicy {
            max_stream_retries: 0,
            max_request_retries: 0,
            enable_transport_fallback: false,
            stream_idle_timeout: Duration::from_millis(100),
            request_timeout: Duration::from_secs(1),
        };
        let session = ProviderSession::new(&provider, None, policy);
        let mut deltas = 0;

        let (response, _) = session
            .complete(request(), &CancellationToken::new(), &mut |event| {
                if matches!(event, ProviderSessionEvent::Streamed { .. }) {
                    deltas += 1;
                }
            })
            .await
            .expect("active stream must outlive its original idle deadline");

        assert_eq!(response.message.expect("message").content, "done");
        assert_eq!(deltas, 3);
    }

    #[tokio::test]
    async fn cancellation_interrupts_an_idle_stream_without_waiting_for_timeout() {
        let provider = FlakyStreamProvider::idle();
        let policy = ProviderSessionPolicy {
            max_stream_retries: 0,
            max_request_retries: 0,
            enable_transport_fallback: false,
            stream_idle_timeout: Duration::from_secs(60),
            request_timeout: Duration::from_secs(60),
        };
        let session = ProviderSession::new(&provider, None, policy);
        let cancellation = CancellationToken::new();
        let cancel = cancellation.clone();
        tokio::spawn(async move {
            sleep(Duration::from_millis(10)).await;
            cancel.cancel();
        });

        let result = tokio::time::timeout(
            Duration::from_millis(250),
            session.complete(request(), &cancellation, &mut |_| {}),
        )
        .await
        .expect("cancellation must not wait for the idle deadline");

        assert!(matches!(
            result,
            Err(ProviderSessionError::Provider(ProviderError::Cancelled))
        ));
    }

    #[tokio::test]
    async fn absolute_deadline_stops_a_stream_that_keeps_resetting_idle_timeout() {
        let provider = ProgressingStreamProvider {
            success: MockProvider::text_response("done"),
            event_interval: Duration::from_millis(10),
            event_count: 100,
        };
        let policy = ProviderSessionPolicy {
            max_stream_retries: 10,
            max_request_retries: 10,
            enable_transport_fallback: true,
            stream_idle_timeout: Duration::from_millis(50),
            request_timeout: Duration::from_secs(1),
        };
        let session = ProviderSession::new(&provider, None, policy)
            .with_deadline(Some(Instant::now() + Duration::from_millis(45)));
        let mut events = Vec::new();

        let error = session
            .complete(request(), &CancellationToken::new(), &mut |event| {
                events.push(event)
            })
            .await
            .expect_err("absolute deadline");

        assert!(matches!(
            error,
            ProviderSessionError::DeadlineExceeded { .. }
        ));
        assert!(
            events
                .iter()
                .any(|event| matches!(event, ProviderSessionEvent::Streamed { .. }))
        );
    }

    #[tokio::test]
    async fn falls_back_to_buffered_transport_after_stream_retries_are_exhausted() {
        let provider = FlakyStreamProvider::new(usize::MAX);
        let policy = ProviderSessionPolicy {
            max_stream_retries: 0,
            max_request_retries: 0,
            enable_transport_fallback: true,
            stream_idle_timeout: Duration::from_secs(1),
            request_timeout: Duration::from_secs(1),
        };
        let session = ProviderSession::new(&provider, None, policy);
        let mut events = Vec::new();

        let (response, _) = session
            .complete(request(), &CancellationToken::new(), &mut |event| {
                events.push(event)
            })
            .await
            .expect("buffered fallback");

        assert_eq!(response.message.expect("message").content, "done");
        assert_eq!(provider.stream_calls.load(Ordering::SeqCst), 1);
        assert_eq!(provider.buffered_calls.load(Ordering::SeqCst), 1);
        assert!(events.iter().any(|event| matches!(
            event,
            ProviderSessionEvent::TransportFallback {
                from: ProviderTransport::Streaming,
                to: ProviderTransport::Buffered,
                ..
            }
        )));
    }

    pub(super) fn request() -> ProviderRequest {
        ProviderRequest {
            request_id: ProviderRequestId::new(),
            task_id: TaskId::new(),
            turn_id: TurnId::new(),
            session_id: None,
            cache_scope: None,
            provider_id: "mock".to_owned(),
            model_id: "mock-model".to_owned(),
            messages: Vec::new(),
            tools: Vec::new(),
            cache_policy: Default::default(),
            max_output_tokens: None,
        }
    }
}

#[cfg(test)]
#[path = "provider_recovery_tests.rs"]
mod recovery_tests;
