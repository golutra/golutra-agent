//! Deterministic provider retry classification.

use std::time::Duration;

use golutra_agent_llm::ProviderError;

const BASE_BACKOFF_MS: u64 = 250;
const MAX_BACKOFF_MS: u64 = 30_000;

pub(crate) fn is_retryable(error: &ProviderError) -> bool {
    // 明确的请求错误优先于消息关键词；不能因为 400 正文包含 stream 就重试。
    if error
        .http_status()
        .is_some_and(|status| (400..500).contains(&status) && status != 429)
    {
        return false;
    }
    match error {
        ProviderError::ConnectionFailed { .. }
        | ProviderError::Unavailable { .. }
        | ProviderError::RateLimited { .. }
        | ProviderError::Timeout { .. } => true,
        ProviderError::Failed { message } => {
            let message = message.to_ascii_lowercase();
            [
                "stream",
                "connection",
                "connect",
                "disconnect",
                "reset",
                "transport",
                "broken pipe",
                "bad gateway",
                "gateway timeout",
                "service unavailable",
                "temporarily unavailable",
                "server error",
                "server_error",
                "internal error",
                "internal_error",
                "overloaded",
                "502",
                "503",
                "504",
            ]
            .iter()
            .any(|marker| message.contains(marker))
        }
        ProviderError::WithMetadata { error, .. } => is_retryable(error),
        ProviderError::Cancelled
        | ProviderError::NotConfigured { .. }
        | ProviderError::Malformed { .. } => false,
    }
}

pub(crate) fn fallback_eligible(error: &ProviderError) -> bool {
    if error
        .http_status()
        .is_some_and(|status| (400..500).contains(&status) && status != 429)
    {
        return false;
    }
    match error {
        ProviderError::ConnectionFailed { .. }
        | ProviderError::Failed { .. }
        | ProviderError::Unavailable { .. }
        | ProviderError::RateLimited { .. }
        | ProviderError::Timeout { .. } => true,
        ProviderError::WithMetadata { error, .. } => fallback_eligible(error),
        ProviderError::Cancelled
        | ProviderError::NotConfigured { .. }
        | ProviderError::Malformed { .. } => false,
    }
}

pub(crate) fn backoff(attempt: u32) -> Duration {
    let exponent = attempt.saturating_sub(1).min(5);
    Duration::from_millis(
        BASE_BACKOFF_MS
            .saturating_mul(1_u64 << exponent)
            .min(MAX_BACKOFF_MS),
    )
}

/// 本地退避有界并带抖动；服务端明确的等待时间不截短，取消与总截止时间由调用者执行。
pub(crate) fn retry_delay(error: &ProviderError, attempt: u32, request_seed: u64) -> Duration {
    if let Some(server_delay) = error.retry_after() {
        return server_delay;
    }

    let base = backoff(attempt).as_millis() as u64;
    // ProviderRequestId 是随机且时间有序的；确定性分桶能在不引入运行时 RNG 的情况下分散请求。
    let bucket = request_seed.wrapping_add(u64::from(attempt).wrapping_mul(0x9E37_79B9)) % 21;
    let multiplier = 90_u64 + bucket;
    Duration::from_millis(base.saturating_mul(multiplier) / 100)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use golutra_agent_llm::{ProviderError, ProviderErrorMetadata};

    use super::*;

    #[test]
    fn explicit_bad_request_is_not_retried_or_fallen_back_by_message_keywords() {
        for message in ["invalid stream parameter", "upstream connection error"] {
            let error = ProviderError::Failed {
                message: message.to_owned(),
            }
            .with_metadata(ProviderErrorMetadata {
                response_http_status: Some(200),
                http_status: Some(400),
                ..ProviderErrorMetadata::default()
            });
            assert!(!is_retryable(&error));
            assert!(!fallback_eligible(&error));
        }
    }

    #[test]
    fn server_retry_after_is_respected_without_retrying_early() {
        let error = ProviderError::Unavailable {
            message: "busy".to_owned(),
        }
        .with_metadata(ProviderErrorMetadata {
            retry_after: Some(Duration::from_secs(45)),
            ..ProviderErrorMetadata::default()
        });

        assert_eq!(retry_delay(&error, 1, 7), Duration::from_secs(45));
    }

    #[test]
    fn local_backoff_stays_within_the_jitter_window() {
        let error = ProviderError::Unavailable {
            message: "connection reset".to_owned(),
        };
        let delay = retry_delay(&error, 2, 123);
        assert!(delay >= Duration::from_millis(450));
        assert!(delay <= Duration::from_millis(550));
    }
}
