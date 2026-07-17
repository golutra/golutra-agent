//! Deterministic provider retry classification.

use golutra_llm::ProviderError;

pub(crate) fn is_retryable(error: &ProviderError) -> bool {
    matches!(
        error,
        ProviderError::Unavailable { .. }
            | ProviderError::RateLimited { .. }
            | ProviderError::Timeout { .. }
    )
}
