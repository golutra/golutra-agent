//! Deterministic provider retry classification.

use std::time::Duration;

use golutra_llm::ProviderError;

const BASE_BACKOFF_MS: u64 = 250;
const MAX_BACKOFF_MS: u64 = 30_000;

pub(crate) fn is_retryable(error: &ProviderError) -> bool {
    match error {
        ProviderError::Unavailable { .. }
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
        ProviderError::Failed { .. }
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

/// 计算有界退避，并按请求种子分散相邻请求的重试时间。
pub(crate) fn retry_delay(error: &ProviderError, attempt: u32, request_seed: u64) -> Duration {
    if let Some(server_delay) = error.retry_after() {
        return server_delay.min(Duration::from_millis(MAX_BACKOFF_MS));
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

    use golutra_llm::{ProviderError, ProviderErrorMetadata};

    use super::*;

    #[test]
    fn server_retry_after_is_bounded_and_preferred() {
        let error = ProviderError::Unavailable {
            message: "busy".to_owned(),
        }
        .with_metadata(ProviderErrorMetadata {
            retry_after: Some(Duration::from_secs(45)),
            ..ProviderErrorMetadata::default()
        });

        assert_eq!(retry_delay(&error, 1, 7), Duration::from_secs(30));
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
