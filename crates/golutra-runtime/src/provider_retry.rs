//! Deterministic provider retry classification.

use std::time::Duration;

use golutra_llm::ProviderError;

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
            ]
            .iter()
            .any(|marker| message.contains(marker))
        }
        ProviderError::Cancelled
        | ProviderError::NotConfigured { .. }
        | ProviderError::Malformed { .. } => false,
    }
}

pub(crate) fn backoff(attempt: u32) -> Duration {
    let exponent = attempt.saturating_sub(1).min(5);
    Duration::from_millis(100_u64.saturating_mul(1_u64 << exponent))
}
