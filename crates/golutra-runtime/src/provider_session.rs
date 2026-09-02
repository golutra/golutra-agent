//! A bounded, cancellable provider session for one logical model request.
//!
//! The session owns retry and transport policy, while `AgentLoop` only maps
//! the resulting facts into its runtime trace.  This keeps provider recovery
//! independent from context, tool, and verification logic.

use std::time::Duration;

use golutra_llm::{
    LlmProvider, ProviderError, ProviderRequest, ProviderResponse, ProviderStreamEvent,
};
use tokio::sync::mpsc;
use tokio::time::{Instant, sleep, timeout_at};
use tokio_util::sync::CancellationToken;

use super::provider_retry;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderTransport {
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
    /// Number of reconnects after a dropped streaming attempt.
    pub max_stream_retries: u32,
    /// Number of retries for a buffered request after a transient failure.
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
    RetryScheduled {
        attempt: u32,
        max_retries: u32,
        transport: ProviderTransport,
        reason: String,
    },
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
    replay_barrier_seen: bool,
}

struct ProviderAttemptFailure {
    error: ProviderError,
    replay_safe: bool,
}

pub(crate) struct ProviderSession<'a, P> {
    primary: &'a P,
    fallback: Option<&'a P>,
    policy: ProviderSessionPolicy,
    deadline: Option<Instant>,
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
        }
    }

    #[must_use]
    pub(crate) fn with_deadline(mut self, deadline: Option<Instant>) -> Self {
        self.deadline = deadline;
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
        request: ProviderRequest,
        cancellation: &CancellationToken,
        on_event: &mut E,
    ) -> Result<(ProviderResponse, ProviderRequest), ProviderError>
    where
        E: FnMut(ProviderSessionEvent) + Send,
    {
        match self
            .complete_provider(self.primary, request.clone(), cancellation, on_event)
            .await
        {
            Ok(response) => Ok((response, request)),
            Err(primary_failure) => {
                let Some(fallback) = self.fallback else {
                    return Err(primary_failure.error);
                };
                if !primary_failure.replay_safe
                    || !provider_retry::fallback_eligible(&primary_failure.error)
                {
                    return Err(primary_failure.error);
                };
                let from_provider = self.primary.contract().provider_id;
                let to_provider = fallback.contract().provider_id;
                on_event(ProviderSessionEvent::ProviderFallback {
                    from_provider,
                    to_provider: to_provider.clone(),
                    reason: retry_reason(&primary_failure.error),
                });
                let mut fallback_request = request;
                fallback_request.provider_id = to_provider;
                fallback_request.model_id = fallback.contract().model_id;
                self.complete_provider(fallback, fallback_request.clone(), cancellation, on_event)
                    .await
                    .map(|response| (response, fallback_request))
                    .map_err(|failure| failure.error)
            }
        }
    }

    async fn complete_provider<E>(
        &self,
        provider: &P,
        request: ProviderRequest,
        cancellation: &CancellationToken,
        on_event: &mut E,
    ) -> Result<ProviderResponse, ProviderAttemptFailure>
    where
        E: FnMut(ProviderSessionEvent) + Send,
    {
        let mut last_error = None;
        let mut replay_safe = true;
        for retry_index in 0..=self.policy.max_stream_retries {
            match self
                .complete_stream_attempt(provider, request.clone(), cancellation, on_event)
                .await
            {
                Ok(response) => return Ok(response),
                Err(failure)
                    if !failure.replay_barrier_seen
                        && provider_retry::is_retryable(&failure.error)
                        && retry_index < self.policy.max_stream_retries =>
                {
                    let attempt = retry_index.saturating_add(1);
                    let delay = provider_retry::retry_delay(
                        &failure.error,
                        attempt,
                        request.request_id.0.as_u128() as u64,
                    );
                    on_event(ProviderSessionEvent::RetryScheduled {
                        attempt,
                        max_retries: self.policy.max_stream_retries,
                        transport: ProviderTransport::Streaming,
                        reason: format!(
                            "{} (retry_delay_ms={})",
                            retry_reason(&failure.error),
                            delay.as_millis()
                        ),
                    });
                    if !wait_backoff(delay, cancellation).await {
                        return Err(ProviderAttemptFailure {
                            error: ProviderError::Cancelled,
                            replay_safe: true,
                        });
                    }
                    last_error = Some(failure.error);
                }
                Err(failure) => {
                    replay_safe = !failure.replay_barrier_seen;
                    last_error = Some(failure.error);
                    break;
                }
            }
        }

        let error = last_error.unwrap_or_else(|| ProviderError::Failed {
            message: "provider stream ended without a result".to_owned(),
        });
        if replay_safe
            && self.policy.enable_transport_fallback
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
                .complete_buffered(provider, request, cancellation, on_event)
                .await;
        }
        Err(ProviderAttemptFailure { error, replay_safe })
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
        let mut replay_barrier_seen = false;

        loop {
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => return Err(StreamAttemptFailure {
                    error: ProviderError::Cancelled,
                    replay_barrier_seen,
                }),
                result = &mut future => {
                    while let Ok(event) = event_receiver.try_recv() {
                        replay_barrier_seen |= is_replay_barrier_event(&event);
                        on_event(ProviderSessionEvent::Streamed {
                            provider_id: provider_id.clone(),
                            model_id: model_id.clone(),
                            event,
                        });
                    }
                    return result.map_err(|error| StreamAttemptFailure {
                        error,
                        replay_barrier_seen,
                    });
                }
                event = event_receiver.recv() => {
                    let Some(event) = event else {
                        return Err(StreamAttemptFailure {
                            error: ProviderError::Failed {
                                message: "provider stream event channel closed".to_owned(),
                            },
                            replay_barrier_seen,
                        });
                    };
                    replay_barrier_seen |= is_replay_barrier_event(&event);
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
                        replay_barrier_seen,
                    });
                }
            }
        }
    }

    async fn complete_buffered<E>(
        &self,
        provider: &P,
        request: ProviderRequest,
        cancellation: &CancellationToken,
        on_event: &mut E,
    ) -> Result<ProviderResponse, ProviderAttemptFailure>
    where
        E: FnMut(ProviderSessionEvent) + Send,
    {
        for retry_index in 0..=self.policy.max_request_retries {
            let result = self
                .complete_buffered_attempt(provider, request.clone(), cancellation)
                .await;
            match result {
                Ok(response) => {
                    emit_response_events(provider, &response, on_event);
                    return Ok(response);
                }
                Err(error)
                    if provider_retry::is_retryable(&error)
                        && retry_index < self.policy.max_request_retries =>
                {
                    let attempt = retry_index.saturating_add(1);
                    let delay = provider_retry::retry_delay(
                        &error,
                        attempt,
                        request.request_id.0.as_u128() as u64,
                    );
                    on_event(ProviderSessionEvent::RetryScheduled {
                        attempt,
                        max_retries: self.policy.max_request_retries,
                        transport: ProviderTransport::Buffered,
                        reason: format!(
                            "{} (retry_delay_ms={})",
                            retry_reason(&error),
                            delay.as_millis()
                        ),
                    });
                    if !wait_backoff(delay, cancellation).await {
                        return Err(ProviderAttemptFailure {
                            error: ProviderError::Cancelled,
                            replay_safe: true,
                        });
                    }
                }
                Err(error) => {
                    return Err(ProviderAttemptFailure {
                        error,
                        replay_safe: true,
                    });
                }
            }
        }
        unreachable!("buffered retry loop always returns")
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
    tokio::select! {
        _ = cancellation.cancelled() => false,
        _ = sleep(delay) => true,
    }
}

fn is_replay_barrier_event(event: &ProviderStreamEvent) -> bool {
    match event {
        // reasoning 只用于诊断/UI，不包含可执行副作用；不能阻止安全重放。
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
    use golutra_core::{ProviderContract, ProviderRequestId, TaskId, TurnId};
    use golutra_llm::{MockProvider, ProviderRequest};

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
                .filter(|event| matches!(event, ProviderSessionEvent::RetryScheduled { .. }))
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
    async fn does_not_replay_a_stream_after_meaningful_output() {
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

        let error = session
            .complete(request(), &CancellationToken::new(), &mut |event| {
                events.push(event)
            })
            .await
            .expect_err("partial stream must not be replayed");

        assert!(matches!(
            error,
            ProviderSessionError::Provider(ProviderError::Unavailable { .. })
        ));
        assert_eq!(provider.stream_calls.load(Ordering::SeqCst), 1);
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, ProviderSessionEvent::RetryScheduled { .. }))
        );
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
            1
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
                .filter(|event| matches!(event, ProviderSessionEvent::RetryScheduled { .. }))
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

    fn request() -> ProviderRequest {
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
