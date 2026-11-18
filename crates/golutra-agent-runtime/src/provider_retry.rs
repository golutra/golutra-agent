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
        ProviderError::Failed { .. } => false,
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

/// 只有适配器确认流传输不完整时才换传输；HTTP/SSE 业务错误沿原传输有限重试。
pub(crate) fn transport_fallback_eligible(error: &ProviderError) -> bool {
    is_retryable(error)
        && error
            .metadata()
            .is_some_and(|metadata| metadata.stream_interrupted)
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
    let capacity_pressure = error
        .metadata()
        .is_some_and(|metadata| metadata.capacity_pressure);
    let base = if capacity_pressure {
        // 容量不足通常会持续一段时间；短退避会立刻打到同一拥塞窗口。
        if attempt <= 1 { 5_000 } else { 15_000 }
    } else {
        backoff(attempt).as_millis() as u64
    };
    // ProviderRequestId 是随机且时间有序的；确定性分桶能在不引入运行时 RNG 的情况下分散请求。
    let bucket = request_seed.wrapping_add(u64::from(attempt).wrapping_mul(0x9E37_79B9)) % 21;
    // Capacity waits start at 5/15 seconds and add up to 10% jitter; parallel
    // children should neither retry early nor all re-enter the same window.
    let multiplier = if capacity_pressure {
        100 + bucket / 2
    } else {
        90 + bucket
    };
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

    #[test]
    fn capacity_backoff_is_bounded_and_spreads_parallel_requests() {
        let error = ProviderError::Unavailable {
            message: "Try later".into(),
        }
        .with_metadata(ProviderErrorMetadata {
            capacity_pressure: true,
            ..Default::default()
        });
        for (attempt, minimum, maximum) in [(1, 5_000, 5_500), (2, 15_000, 16_500)] {
            let delays = (0..42)
                .map(|seed| {
                    let delay = retry_delay(&error, attempt, seed);
                    assert_eq!(delay, retry_delay(&error, attempt, seed));
                    assert!(
                        (Duration::from_millis(minimum)..=Duration::from_millis(maximum))
                            .contains(&delay)
                    );
                    delay
                })
                .collect::<std::collections::BTreeSet<_>>();
            assert!(delays.len() > 1, "parallel requests need distinct waits");
        }
    }

    #[test]
    fn capacity_backoff_uses_classified_metadata_not_display_wording() {
        let error = ProviderError::Unavailable {
            message: "at capacity".into(),
        };
        assert!(retry_delay(&error, 1, 7) < Duration::from_secs(1));
    }
}
