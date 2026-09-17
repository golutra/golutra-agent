mod credential;
mod oauth;

pub use credential::{
    CREDENTIALS_FILE_NAME, CredentialMetadata, CredentialProvider, CredentialRef, CredentialSource,
    DefaultSecretStore, FixedCredentialProvider, MemorySecretStore, SecretKind, SecretStore,
    StoredCredentialProvider,
};
pub use oauth::{
    AuthService, BrowserOAuthLogin, DeviceOAuthLogin, OAuthFlow, OAuthLoginResult,
    OAuthProviderDescriptor, OAuthTokenMetadata, OpenAiDeviceAuthorizationDescriptor,
    OpenAiDeviceOAuthLogin,
};

use thiserror::Error;

#[derive(Debug, Error)]
pub enum AuthError {
    #[error("auth validation failed: {0}")]
    Validation(String),
    #[error("credential store failed: {0}")]
    SecretStore(String),
    #[error("credential `{0}` was not found")]
    SecretNotFound(String),
    #[error("oauth request failed: {0}")]
    OAuth(String),
    #[error("oauth authorization was cancelled")]
    Cancelled,
    #[error("oauth authorization timed out")]
    Timeout,
    #[error("oauth credential requires authentication again: {0}")]
    ReauthenticationRequired(String),
    #[error("auth io failed: {0}")]
    Io(String),
}
