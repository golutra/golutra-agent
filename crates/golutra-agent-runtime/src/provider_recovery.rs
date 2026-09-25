//! 一次逻辑请求的恢复事实；连接等待与普通重试分别计数，等待不消耗工具或执行步数。

use std::time::Duration;

use golutra_agent_llm::{ProviderAttemptError, ProviderError, ProviderErrorMetadata};
use serde::Serialize;

use super::{ProviderTransport, provider_retry};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryPhase {
    Waiting,
    Retrying,
}

/// 前端原地展示该状态；完整事件仍用于诊断和历史重建。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProviderRecovery {
    pub phase: RecoveryPhase,
    pub attempt: u32,
    pub delay_ms: u64,
    pub waited_ms: u64,
    pub network: bool,
    pub reset_stream: bool,
    pub reason: String,
    pub transport: ProviderTransport,
    pub error_metadata: Option<ProviderErrorMetadata>,
}

#[derive(Default)]
pub(super) struct RetryState {
    ordinary_retries: u32,
    connection_retries: u32,
    attempts: u32,
    pub waited: Duration,
    transport: ProviderTransport,
    failures: Vec<ProviderAttemptError>,
    failed_attempts: u32,
}

impl RetryState {
    pub fn switch_to_buffered(&mut self) {
        self.transport = ProviderTransport::Buffered;
    }

    pub fn record_failure(&mut self, error: &ProviderError, elapsed: Duration) {
        self.failed_attempts = self.failed_attempts.saturating_add(1);
        let metadata = error.metadata().cloned().unwrap_or_default();
        // 保留第一因和最近七次错误；长时间断网不能让诊断记录无限增长。
        if self.failures.len() == 8 {
            self.failures.remove(1);
        }
        self.failures.push(ProviderAttemptError {
            attempt: self.failed_attempts,
            transport: self.transport.label().to_owned(),
            elapsed_ms: duration_ms(elapsed),
            message: golutra_agent_tools::redact_sensitive_text(&error.to_string())
                .0
                .chars()
                .take(512)
                .collect(),
            response_http_status: metadata.response_http_status,
            http_status: metadata.http_status,
            error_type: metadata.error_type,
            request_id: metadata.request_id,
            upstream_response_id: metadata.upstream_response_id,
        });
    }

    pub fn with_failures(&self, error: ProviderError) -> ProviderError {
        let mut metadata = error.metadata().cloned().unwrap_or_default();
        metadata.attempts = self.failures.clone();
        error.with_metadata(metadata)
    }

    pub fn schedule(
        &mut self,
        error: &ProviderError,
        max_retries: u32,
        allow_connection_wait: bool,
        seed: u64,
        reset_stream: bool,
    ) -> Option<(Duration, ProviderRecovery)> {
        if !provider_retry::is_retryable(error) {
            return None;
        }
        let network = allow_connection_wait && is_connection_failure(error);
        let delay = if network {
            self.connection_retries = self.connection_retries.saturating_add(1);
            let seconds = (5_u64 << self.connection_retries.saturating_sub(1).min(4)).min(60);
            // 按请求分散并发恢复，但不超过一分钟，避免所有子任务同时重连。
            let jitter = 90 + seed.wrapping_add(u64::from(self.connection_retries)) % 11;
            Duration::from_millis(seconds * 1_000 * jitter / 100)
        } else {
            if self.ordinary_retries >= max_retries {
                return None;
            }
            self.ordinary_retries += 1;
            provider_retry::retry_delay(error, self.ordinary_retries, seed)
        };
        self.attempts = self.attempts.saturating_add(1);
        Some((
            delay,
            ProviderRecovery {
                phase: RecoveryPhase::Waiting,
                attempt: self.attempts,
                delay_ms: duration_ms(delay),
                waited_ms: duration_ms(self.waited),
                network,
                reset_stream,
                reason: error.to_string(),
                transport: self.transport,
                error_metadata: error.metadata().cloned(),
            },
        ))
    }
}

fn is_connection_failure(error: &ProviderError) -> bool {
    if error.http_status().is_some() {
        return false;
    }
    match error {
        ProviderError::ConnectionFailed { .. } => true,
        ProviderError::WithMetadata { error, .. } => is_connection_failure(error),
        _ => false,
    }
}

pub(super) fn duration_ms(value: Duration) -> u64 {
    value.as_millis().min(u128::from(u64::MAX)) as u64
}
