use std::{
    collections::{BTreeMap, HashMap},
    fmt,
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, Utc};
use fs2::FileExt;
use futures_util::StreamExt;
use oauth2::{
    AuthType, AuthUrl, AuthorizationCode, ClientId, CsrfToken, DeviceAuthorizationUrl,
    ExtraTokenFields, PkceCodeChallenge, PkceCodeVerifier, RedirectUrl, RefreshToken,
    RevocationUrl, Scope, StandardDeviceAuthorizationResponse, StandardRevocableToken,
    StandardTokenResponse, TokenResponse, TokenUrl,
    basic::{
        BasicErrorResponse, BasicRevocationErrorResponse, BasicTokenIntrospectionResponse,
        BasicTokenType,
    },
};
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::TcpListener,
    sync::{Mutex as AsyncMutex, OnceCell},
    task::JoinHandle,
};

use crate::{
    AuthError, CredentialMetadata, CredentialProvider, CredentialRef, CredentialSource, SecretKind,
    SecretStore, StoredCredentialProvider,
};

const BROWSER_LOGIN_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const DEVICE_LOGIN_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const OAUTH_HTTP_CLIENT_INIT_TIMEOUT: Duration = Duration::from_secs(10);
const OAUTH_HTTP_TIMEOUT: Duration = Duration::from_secs(30);
const REFRESH_EARLY_SECONDS: i64 = 5 * 60;
const MAX_CALLBACK_REQUEST_LINE_BYTES: u64 = 8 * 1024;
const MAX_CALLBACK_ATTEMPTS: usize = 16;
const MAX_OAUTH_RESPONSE_BYTES: usize = 1024 * 1024;
const DEVICE_POLL_SAFETY_MARGIN: Duration = Duration::from_secs(3);
static OAUTH_HTTP_CLIENT: OnceCell<oauth2::reqwest::Client> = OnceCell::const_new();

async fn oauth_http_client() -> Result<oauth2::reqwest::Client, AuthError> {
    let client = tokio::time::timeout(
        OAUTH_HTTP_CLIENT_INIT_TIMEOUT,
        OAUTH_HTTP_CLIENT.get_or_try_init(|| async {
            tokio::task::spawn_blocking(|| {
                oauth2::reqwest::ClientBuilder::new()
                    .redirect(oauth2::reqwest::redirect::Policy::none())
                    .connect_timeout(Duration::from_secs(10))
                    .timeout(OAUTH_HTTP_TIMEOUT)
                    .build()
            })
            .await
            .map_err(|error| AuthError::OAuth(error.to_string()))?
            .map_err(|error| AuthError::OAuth(error.to_string()))
        }),
    )
    .await
    .map_err(|_| AuthError::Timeout)??;
    Ok(client.clone())
}

fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct OAuthExtraTokenFields {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    id_token: Option<String>,
}

impl ExtraTokenFields for OAuthExtraTokenFields {}

type OAuthTokenResponse = StandardTokenResponse<OAuthExtraTokenFields, BasicTokenType>;

type OAuthClient<HasAuthUrl, HasDeviceAuthUrl, HasIntrospectionUrl, HasRevocationUrl, HasTokenUrl> =
    oauth2::Client<
        BasicErrorResponse,
        OAuthTokenResponse,
        BasicTokenIntrospectionResponse,
        StandardRevocableToken,
        BasicRevocationErrorResponse,
        HasAuthUrl,
        HasDeviceAuthUrl,
        HasIntrospectionUrl,
        HasRevocationUrl,
        HasTokenUrl,
    >;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OAuthFlow {
    BrowserPkce,
    DeviceCode,
    #[serde(rename = "openai-device-auth")]
    OpenAiDeviceAuth,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenAiDeviceAuthorizationDescriptor {
    pub user_code_endpoint: String,
    pub token_poll_endpoint: String,
    pub verification_uri: String,
    pub redirect_uri: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthProviderDescriptor {
    pub provider_id: String,
    pub client_id: String,
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_authorization_endpoint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revocation_endpoint: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scopes: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audience: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub browser_redirect_uri: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub authorization_params: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub authorization_nonce: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub openai_device_authorization: Option<OpenAiDeviceAuthorizationDescriptor>,
    pub flows: Vec<OAuthFlow>,
}

impl OAuthProviderDescriptor {
    pub fn validate(&self) -> Result<(), AuthError> {
        validate_non_empty(&self.provider_id, "oauth provider id")?;
        validate_non_empty(&self.client_id, "oauth client id")?;
        validate_endpoint(&self.authorization_endpoint, "authorization endpoint")?;
        validate_endpoint(&self.token_endpoint, "token endpoint")?;
        if let Some(endpoint) = &self.device_authorization_endpoint {
            validate_endpoint(endpoint, "device authorization endpoint")?;
        }
        if let Some(endpoint) = &self.revocation_endpoint {
            validate_endpoint(endpoint, "revocation endpoint")?;
        }
        if let Some(redirect_uri) = &self.browser_redirect_uri {
            validate_browser_redirect_uri(redirect_uri)?;
        }
        if let Some(device) = &self.openai_device_authorization {
            validate_endpoint(
                &device.user_code_endpoint,
                "OpenAI device user-code endpoint",
            )?;
            validate_endpoint(
                &device.token_poll_endpoint,
                "OpenAI device token-poll endpoint",
            )?;
            validate_endpoint(&device.verification_uri, "OpenAI device verification URI")?;
            validate_endpoint(&device.redirect_uri, "OpenAI device redirect URI")?;
        }
        if self.flows.is_empty() {
            return Err(AuthError::Validation(
                "oauth descriptor must enable at least one flow".to_owned(),
            ));
        }
        if self.flows.contains(&OAuthFlow::DeviceCode)
            && self.device_authorization_endpoint.is_none()
        {
            return Err(AuthError::Validation(
                "device-code flow requires device_authorization_endpoint".to_owned(),
            ));
        }
        if self.flows.contains(&OAuthFlow::OpenAiDeviceAuth)
            && self.openai_device_authorization.is_none()
        {
            return Err(AuthError::Validation(
                "OpenAI device-auth flow requires openai_device_authorization".to_owned(),
            ));
        }
        if self.scopes.iter().any(|scope| scope.trim().is_empty()) {
            return Err(AuthError::Validation(
                "oauth scopes cannot contain empty values".to_owned(),
            ));
        }
        for (key, value) in &self.authorization_params {
            validate_non_empty(key, "oauth authorization parameter name")?;
            validate_non_empty(value, "oauth authorization parameter value")?;
            if matches!(
                key.as_str(),
                "client_id"
                    | "redirect_uri"
                    | "response_type"
                    | "scope"
                    | "state"
                    | "code_challenge"
                    | "code_challenge_method"
                    | "nonce"
            ) {
                return Err(AuthError::Validation(format!(
                    "oauth authorization parameter `{key}` is managed by Golutra"
                )));
            }
        }
        Ok(())
    }

    fn supports(&self, flow: OAuthFlow) -> Result<(), AuthError> {
        self.validate()?;
        if self.flows.contains(&flow) {
            Ok(())
        } else {
            Err(AuthError::Validation(format!(
                "oauth provider `{}` does not support {}",
                self.provider_id,
                match flow {
                    OAuthFlow::BrowserPkce => "browser-pkce",
                    OAuthFlow::DeviceCode => "device-code",
                    OAuthFlow::OpenAiDeviceAuth => "openai-device-auth",
                }
            )))
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthTokenMetadata {
    pub expires_at: Option<DateTime<Utc>>,
    pub scopes: Vec<String>,
    pub token_type: String,
    pub refreshable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthLoginResult {
    pub credential_ref: CredentialRef,
    pub metadata: OAuthTokenMetadata,
}

#[derive(Clone)]
pub struct AuthService {
    inner: Arc<AuthServiceInner>,
}

struct AuthServiceInner {
    home: PathBuf,
    store: Arc<dyn SecretStore>,
    access_cache: Mutex<HashMap<String, CachedAccessToken>>,
    refresh_locks: Mutex<HashMap<String, Arc<AsyncMutex<()>>>>,
}

impl fmt::Debug for AuthService {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AuthService")
            .field("home", &self.inner.home)
            .finish_non_exhaustive()
    }
}

#[derive(Clone)]
struct CachedAccessToken {
    token: SecretString,
    expires_at: Option<DateTime<Utc>>,
    revision: String,
}

#[derive(Serialize, Deserialize)]
struct StoredOAuthCredential {
    revision: String,
    refresh_token: Option<String>,
    access_token: Option<String>,
    expires_at: Option<DateTime<Utc>>,
    token_type: String,
    scopes: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    account_id: Option<String>,
}

impl AuthService {
    pub fn new(home: impl Into<PathBuf>, store: Arc<dyn SecretStore>) -> Result<Self, AuthError> {
        let home = home.into();
        if home.as_os_str().is_empty() {
            return Err(AuthError::Validation(
                "auth service home cannot be empty".to_owned(),
            ));
        }
        Ok(Self {
            inner: Arc::new(AuthServiceInner {
                home,
                store,
                access_cache: Mutex::new(HashMap::new()),
                refresh_locks: Mutex::new(HashMap::new()),
            }),
        })
    }

    #[must_use]
    pub fn credential_provider(
        &self,
        reference: CredentialRef,
        oauth: Option<OAuthProviderDescriptor>,
    ) -> Arc<dyn CredentialProvider> {
        if reference.secret_kind == SecretKind::OAuthTokenSet {
            Arc::new(AuthServiceCredentialProvider {
                service: self.clone(),
                reference,
                oauth,
            })
        } else {
            Arc::new(StoredCredentialProvider::new(
                Arc::clone(&self.inner.store),
                reference,
            ))
        }
    }

    pub async fn begin_browser_login(
        &self,
        descriptor: OAuthProviderDescriptor,
        source: CredentialSource,
    ) -> Result<BrowserOAuthLogin, AuthError> {
        descriptor.supports(OAuthFlow::BrowserPkce)?;
        require_writable_source(&source)?;
        let callback = bind_browser_callback(&descriptor).await?;
        let redirect_url = callback.redirect_url;
        let client = oauth_client(&descriptor, Some(&redirect_url))?;
        let (pkce_challenge, pkce_verifier) = PkceCodeChallenge::new_random_sha256();
        let mut authorization = client
            .authorize_url(CsrfToken::new_random)
            .set_pkce_challenge(pkce_challenge);
        for scope in &descriptor.scopes {
            authorization = authorization.add_scope(Scope::new(scope.clone()));
        }
        if let Some(audience) = &descriptor.audience {
            authorization = authorization.add_extra_param("audience", audience);
        }
        for (key, value) in &descriptor.authorization_params {
            authorization = authorization.add_extra_param(key, value);
        }
        if descriptor.authorization_nonce {
            authorization =
                authorization.add_extra_param("nonce", uuid::Uuid::new_v4().simple().to_string());
        }
        let (authorization_url, expected_state) = authorization.url();
        let expected_state_value = expected_state.secret().to_owned();
        let callback = tokio::spawn(wait_for_browser_callback(
            callback.listener,
            callback.callback_path,
            expected_state_value.clone(),
        ));
        let reference = new_reference(source, SecretKind::OAuthTokenSet)?;

        Ok(BrowserOAuthLogin {
            service: self.clone(),
            descriptor,
            reference,
            authorization_url: authorization_url.to_string(),
            redirect_url,
            pkce_verifier: Some(pkce_verifier),
            callback: Some(callback),
        })
    }

    pub async fn begin_device_login(
        &self,
        descriptor: OAuthProviderDescriptor,
        source: CredentialSource,
    ) -> Result<DeviceOAuthLogin, AuthError> {
        descriptor.supports(OAuthFlow::DeviceCode)?;
        require_writable_source(&source)?;
        let client = oauth_device_client(&descriptor)?;
        let mut request = client.exchange_device_code();
        for scope in &descriptor.scopes {
            request = request.add_scope(Scope::new(scope.clone()));
        }
        if let Some(audience) = &descriptor.audience {
            request = request.add_extra_param("audience", audience);
        }
        let http_client = oauth_http_client().await?;
        let details: StandardDeviceAuthorizationResponse =
            tokio::time::timeout(OAUTH_HTTP_TIMEOUT, request.request_async(&http_client))
                .await
                .map_err(|_| AuthError::Timeout)?
                .map_err(oauth_request_error)?;
        let reference = new_reference(source, SecretKind::OAuthTokenSet)?;

        Ok(DeviceOAuthLogin {
            service: self.clone(),
            descriptor,
            reference,
            details,
        })
    }

    pub async fn begin_openai_device_login(
        &self,
        descriptor: OAuthProviderDescriptor,
        source: CredentialSource,
    ) -> Result<OpenAiDeviceOAuthLogin, AuthError> {
        descriptor.supports(OAuthFlow::OpenAiDeviceAuth)?;
        require_writable_source(&source)?;
        let device = descriptor
            .openai_device_authorization
            .clone()
            .ok_or_else(|| {
                AuthError::Validation(
                    "OpenAI device-auth configuration is missing from descriptor".to_owned(),
                )
            })?;
        let http_client = oauth_http_client().await?;
        let response = tokio::time::timeout(
            OAUTH_HTTP_TIMEOUT,
            http_client
                .post(&device.user_code_endpoint)
                .header(
                    reqwest::header::USER_AGENT,
                    format!("golutra/{}", env!("CARGO_PKG_VERSION")),
                )
                .json(&serde_json::json!({"client_id": descriptor.client_id}))
                .send(),
        )
        .await
        .map_err(|_| AuthError::Timeout)?
        .map_err(|error| AuthError::OAuth(sanitize_oauth_error(&error.to_string())))?;
        let details: OpenAiDeviceAuthorizationResponse = oauth_json_response(response).await?;
        validate_non_empty(&details.device_auth_id, "OpenAI device auth id")?;
        validate_non_empty(&details.user_code, "OpenAI device user code")?;
        let reference = new_reference(source, SecretKind::OAuthTokenSet)?;

        Ok(OpenAiDeviceOAuthLogin {
            service: self.clone(),
            descriptor,
            device,
            reference,
            device_auth_id: details.device_auth_id,
            user_code: details.user_code,
            poll_interval: device_poll_interval(&details.interval),
        })
    }

    pub async fn logout(
        &self,
        reference: &CredentialRef,
        descriptor: Option<&OAuthProviderDescriptor>,
    ) -> Result<bool, AuthError> {
        let revoke_result = self.revoke(reference, descriptor).await;
        let delete_result = self.delete_secret(reference).await;
        self.inner
            .access_cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&reference.id);
        let deleted = delete_result?;
        revoke_result?;
        Ok(deleted)
    }

    pub async fn revoke(
        &self,
        reference: &CredentialRef,
        descriptor: Option<&OAuthProviderDescriptor>,
    ) -> Result<(), AuthError> {
        if reference.secret_kind != SecretKind::OAuthTokenSet {
            return Ok(());
        }
        let Some(descriptor) = descriptor.filter(|value| value.revocation_endpoint.is_some())
        else {
            return Ok(());
        };
        let Some(secret) = self.read_secret(reference).await? else {
            return Ok(());
        };
        let credential = decode_oauth_secret(&secret)?;
        self.revoke_oauth_credential(descriptor, &credential).await
    }

    async fn persist_login_token(
        &self,
        reference: CredentialRef,
        token: OAuthTokenResponse,
    ) -> Result<OAuthLoginResult, AuthError> {
        let (stored, cached, metadata) = token_parts(&token);
        self.write_oauth_secret(&reference, &stored).await?;
        self.cache_token(reference.id.as_str(), cached);
        Ok(OAuthLoginResult {
            credential_ref: reference,
            metadata,
        })
    }

    async fn resolve_credential(
        &self,
        reference: &CredentialRef,
        descriptor: Option<&OAuthProviderDescriptor>,
        force_refresh: bool,
    ) -> Result<SecretString, AuthError> {
        if reference.secret_kind != SecretKind::OAuthTokenSet {
            return self
                .read_secret(reference)
                .await?
                .ok_or_else(|| AuthError::SecretNotFound(reference.id.clone()));
        }
        let descriptor = descriptor.ok_or_else(|| {
            AuthError::Validation("oauth credential is missing provider descriptor".to_owned())
        })?;
        descriptor.validate()?;
        let observed_cache_revision = self.cached_revision(reference);
        if !force_refresh && let Some(token) = self.cached_token(reference) {
            return Ok(token);
        }
        let refresh_lock = self.refresh_lock(&reference.id);
        let _in_process = refresh_lock.lock().await;
        if !force_refresh && let Some(token) = self.cached_token(reference) {
            return Ok(token);
        }
        if force_refresh
            && let Some(token) =
                self.cached_token_after_revision(reference, observed_cache_revision.as_deref())
        {
            return Ok(token);
        }

        let observed_secret = self
            .read_secret(reference)
            .await?
            .ok_or_else(|| AuthError::SecretNotFound(reference.id.clone()))?;
        let observed = decode_oauth_secret(&observed_secret)?;
        let _cross_process =
            acquire_refresh_file_lock(self.inner.home.clone(), reference.id.clone()).await?;
        let latest_secret = self
            .read_secret(reference)
            .await?
            .ok_or_else(|| AuthError::SecretNotFound(reference.id.clone()))?;
        let stored = decode_oauth_secret(&latest_secret)?;
        let another_process_refreshed = stored.revision != observed.revision;
        if (!force_refresh || another_process_refreshed)
            && let Some(token) = valid_persisted_access_token(&stored)
        {
            self.cache_token(
                reference.id.as_str(),
                CachedAccessToken {
                    token: token.clone(),
                    expires_at: stored.expires_at,
                    revision: stored.revision.clone(),
                },
            );
            return Ok(token);
        }
        let refresh_token = stored.refresh_token.as_deref().ok_or_else(|| {
            AuthError::ReauthenticationRequired(
                "credential expired and has no refresh token".to_owned(),
            )
        })?;
        let client = oauth_client(descriptor, None)?;
        let refresh_token = RefreshToken::new(refresh_token.to_owned());
        let mut request = client.exchange_refresh_token(&refresh_token);
        for scope in &descriptor.scopes {
            request = request.add_scope(Scope::new(scope.clone()));
        }
        self.clear_cached_token(reference);
        let http_client = oauth_http_client().await?;
        let token_result =
            tokio::time::timeout(OAUTH_HTTP_TIMEOUT, request.request_async(&http_client))
                .await
                .map_err(|_| AuthError::Timeout)?;
        let token = match token_result {
            Ok(token) => token,
            Err(error) => {
                let error = oauth_refresh_error(error);
                if matches!(error, AuthError::ReauthenticationRequired(_)) {
                    self.delete_secret(reference).await?;
                }
                return Err(error);
            }
        };
        let (mut updated, cached, _) = token_parts(&token);
        if updated.refresh_token.is_none() {
            updated.refresh_token = stored.refresh_token;
        }
        if updated.account_id.is_none() {
            updated.account_id = stored.account_id;
        }
        self.write_oauth_secret(reference, &updated).await?;
        self.cache_token(reference.id.as_str(), cached.clone());
        Ok(cached.token)
    }

    async fn credential_metadata(
        &self,
        reference: &CredentialRef,
    ) -> Result<CredentialMetadata, AuthError> {
        if reference.secret_kind != SecretKind::OAuthTokenSet {
            return Ok(CredentialMetadata::default());
        }
        let Some(secret) = self.read_secret(reference).await? else {
            return Err(AuthError::SecretNotFound(reference.id.clone()));
        };
        let credential = decode_oauth_secret(&secret)?;
        Ok(CredentialMetadata {
            account_id: credential.account_id,
        })
    }

    async fn revoke_oauth_credential(
        &self,
        descriptor: &OAuthProviderDescriptor,
        credential: &StoredOAuthCredential,
    ) -> Result<(), AuthError> {
        let client = oauth_client(descriptor, None)?;
        let token: StandardRevocableToken = if let Some(refresh_token) = &credential.refresh_token {
            RefreshToken::new(refresh_token.clone()).into()
        } else if let Some(access_token) = &credential.access_token {
            oauth2::AccessToken::new(access_token.clone()).into()
        } else {
            return Ok(());
        };
        let http_client = oauth_http_client().await?;
        tokio::time::timeout(
            OAUTH_HTTP_TIMEOUT,
            client
                .revoke_token(token)
                .map_err(|error| AuthError::OAuth(error.to_string()))?
                .request_async(&http_client),
        )
        .await
        .map_err(|_| AuthError::Timeout)?
        .map_err(oauth_request_error)
    }

    async fn read_secret(
        &self,
        reference: &CredentialRef,
    ) -> Result<Option<SecretString>, AuthError> {
        let store = Arc::clone(&self.inner.store);
        let reference = reference.clone();
        tokio::task::spawn_blocking(move || store.get(&reference))
            .await
            .map_err(|error| AuthError::SecretStore(error.to_string()))?
    }

    async fn write_oauth_secret(
        &self,
        reference: &CredentialRef,
        credential: &StoredOAuthCredential,
    ) -> Result<(), AuthError> {
        let serialized = serde_json::to_string(credential)
            .map_err(|error| AuthError::OAuth(error.to_string()))?;
        let secret = SecretString::from(serialized);
        let store = Arc::clone(&self.inner.store);
        let reference = reference.clone();
        tokio::task::spawn_blocking(move || store.set(&reference, &secret))
            .await
            .map_err(|error| AuthError::SecretStore(error.to_string()))?
    }

    async fn delete_secret(&self, reference: &CredentialRef) -> Result<bool, AuthError> {
        let store = Arc::clone(&self.inner.store);
        let reference = reference.clone();
        tokio::task::spawn_blocking(move || store.delete(&reference))
            .await
            .map_err(|error| AuthError::SecretStore(error.to_string()))?
    }

    fn cached_token(&self, reference: &CredentialRef) -> Option<SecretString> {
        self.inner
            .access_cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&reference.id)
            .filter(|cached| token_is_fresh(cached.expires_at))
            .map(|cached| cached.token.clone())
    }

    fn cached_revision(&self, reference: &CredentialRef) -> Option<String> {
        self.inner
            .access_cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&reference.id)
            .map(|cached| cached.revision.clone())
    }

    fn cached_token_after_revision(
        &self,
        reference: &CredentialRef,
        previous_revision: Option<&str>,
    ) -> Option<SecretString> {
        self.inner
            .access_cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&reference.id)
            .filter(|cached| Some(cached.revision.as_str()) != previous_revision)
            .filter(|cached| token_is_fresh(cached.expires_at))
            .map(|cached| cached.token.clone())
    }

    fn cache_token(&self, credential_id: &str, cached: CachedAccessToken) {
        self.inner
            .access_cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(credential_id.to_owned(), cached);
    }

    fn clear_cached_token(&self, reference: &CredentialRef) {
        self.inner
            .access_cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&reference.id);
    }

    fn refresh_lock(&self, credential_id: &str) -> Arc<AsyncMutex<()>> {
        let mut locks = self
            .inner
            .refresh_locks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Arc::clone(
            locks
                .entry(credential_id.to_owned())
                .or_insert_with(|| Arc::new(AsyncMutex::new(()))),
        )
    }
}

pub struct BrowserOAuthLogin {
    service: AuthService,
    descriptor: OAuthProviderDescriptor,
    reference: CredentialRef,
    authorization_url: String,
    redirect_url: String,
    pkce_verifier: Option<PkceCodeVerifier>,
    callback: Option<JoinHandle<Result<BrowserCallback, AuthError>>>,
}

impl fmt::Debug for BrowserOAuthLogin {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BrowserOAuthLogin")
            .field("provider_id", &self.descriptor.provider_id)
            .field("redirect_url", &self.redirect_url)
            .finish_non_exhaustive()
    }
}

impl BrowserOAuthLogin {
    #[must_use]
    pub fn authorization_url(&self) -> &str {
        &self.authorization_url
    }

    #[must_use]
    pub fn redirect_url(&self) -> &str {
        &self.redirect_url
    }

    pub async fn open_browser(&self) -> Result<(), AuthError> {
        let url = self.authorization_url.clone();
        tokio::task::spawn_blocking(move || webbrowser::open(&url))
            .await
            .map_err(|error| AuthError::OAuth(error.to_string()))?
            .map(|_| ())
            .map_err(|error| AuthError::OAuth(error.to_string()))
    }

    pub async fn complete(mut self) -> Result<OAuthLoginResult, AuthError> {
        self.complete_with_timeout(BROWSER_LOGIN_TIMEOUT).await
    }

    pub async fn complete_with_timeout(
        &mut self,
        timeout: Duration,
    ) -> Result<OAuthLoginResult, AuthError> {
        let callback = self.callback.take().ok_or_else(|| {
            AuthError::Validation("oauth callback was already consumed".to_owned())
        })?;
        let mut callback = callback;
        let callback = match tokio::time::timeout(timeout, &mut callback).await {
            Ok(result) => result.map_err(|error| AuthError::OAuth(error.to_string()))??,
            Err(_) => {
                callback.abort();
                let _ = callback.await;
                return Err(AuthError::Timeout);
            }
        };
        let verifier = self.pkce_verifier.take().ok_or_else(|| {
            AuthError::Validation("oauth PKCE verifier was already consumed".to_owned())
        })?;
        let client = oauth_client(&self.descriptor, Some(&self.redirect_url))?;
        let http_client = oauth_http_client().await?;
        let token = tokio::time::timeout(
            OAUTH_HTTP_TIMEOUT,
            client
                .exchange_code(AuthorizationCode::new(callback.code))
                .set_pkce_verifier(verifier)
                .request_async(&http_client),
        )
        .await
        .map_err(|_| AuthError::Timeout)?
        .map_err(oauth_request_error)?;
        self.service
            .persist_login_token(self.reference.clone(), token)
            .await
    }
}

impl Drop for BrowserOAuthLogin {
    fn drop(&mut self) {
        if let Some(callback) = self.callback.take() {
            callback.abort();
        }
    }
}

pub struct DeviceOAuthLogin {
    service: AuthService,
    descriptor: OAuthProviderDescriptor,
    reference: CredentialRef,
    details: StandardDeviceAuthorizationResponse,
}

impl fmt::Debug for DeviceOAuthLogin {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DeviceOAuthLogin")
            .field("provider_id", &self.descriptor.provider_id)
            .field(
                "verification_uri",
                &self.details.verification_uri().to_string(),
            )
            .finish_non_exhaustive()
    }
}

impl DeviceOAuthLogin {
    #[must_use]
    pub fn verification_uri(&self) -> String {
        self.details.verification_uri().to_string()
    }

    #[must_use]
    pub fn verification_uri_complete(&self) -> Option<String> {
        self.details
            .verification_uri_complete()
            .map(|value| value.secret().to_owned())
    }

    #[must_use]
    pub fn user_code(&self) -> String {
        self.details.user_code().secret().to_owned()
    }

    pub async fn complete(self) -> Result<OAuthLoginResult, AuthError> {
        let client = oauth_device_client(&self.descriptor)?;
        let http_client = oauth_http_client().await?;
        let token = tokio::time::timeout(
            DEVICE_LOGIN_TIMEOUT,
            client
                .exchange_device_access_token(&self.details)
                .request_async(&http_client, tokio::time::sleep, None),
        )
        .await
        .map_err(|_| AuthError::Timeout)?
        .map_err(oauth_request_error)?;
        self.service
            .persist_login_token(self.reference, token)
            .await
    }
}

#[derive(Debug, Deserialize)]
struct OpenAiDeviceAuthorizationResponse {
    device_auth_id: String,
    user_code: String,
    #[serde(default)]
    interval: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct OpenAiDeviceAuthorizationGrant {
    authorization_code: String,
    code_verifier: String,
}

pub struct OpenAiDeviceOAuthLogin {
    service: AuthService,
    descriptor: OAuthProviderDescriptor,
    device: OpenAiDeviceAuthorizationDescriptor,
    reference: CredentialRef,
    device_auth_id: String,
    user_code: String,
    poll_interval: Duration,
}

impl fmt::Debug for OpenAiDeviceOAuthLogin {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpenAiDeviceOAuthLogin")
            .field("provider_id", &self.descriptor.provider_id)
            .field("verification_uri", &self.device.verification_uri)
            .finish_non_exhaustive()
    }
}

impl OpenAiDeviceOAuthLogin {
    #[must_use]
    pub fn verification_uri(&self) -> &str {
        &self.device.verification_uri
    }

    #[must_use]
    pub fn user_code(&self) -> &str {
        &self.user_code
    }

    pub async fn open_browser(&self) -> Result<(), AuthError> {
        let url = self.device.verification_uri.clone();
        tokio::task::spawn_blocking(move || webbrowser::open(&url))
            .await
            .map_err(|error| AuthError::OAuth(error.to_string()))?
            .map(|_| ())
            .map_err(|error| AuthError::OAuth(error.to_string()))
    }

    pub async fn complete(self) -> Result<OAuthLoginResult, AuthError> {
        let grant = tokio::time::timeout(DEVICE_LOGIN_TIMEOUT, self.poll_authorization())
            .await
            .map_err(|_| AuthError::Timeout)??;
        validate_non_empty(
            &grant.authorization_code,
            "OpenAI device authorization code",
        )?;
        validate_non_empty(&grant.code_verifier, "OpenAI device code verifier")?;
        let client = oauth_client(&self.descriptor, Some(&self.device.redirect_uri))?;
        let http_client = oauth_http_client().await?;
        let token = tokio::time::timeout(
            OAUTH_HTTP_TIMEOUT,
            client
                .exchange_code(AuthorizationCode::new(grant.authorization_code))
                .set_pkce_verifier(PkceCodeVerifier::new(grant.code_verifier))
                .request_async(&http_client),
        )
        .await
        .map_err(|_| AuthError::Timeout)?
        .map_err(oauth_request_error)?;
        self.service
            .persist_login_token(self.reference, token)
            .await
    }

    async fn poll_authorization(&self) -> Result<OpenAiDeviceAuthorizationGrant, AuthError> {
        let http_client = oauth_http_client().await?;
        loop {
            let response = tokio::time::timeout(
                OAUTH_HTTP_TIMEOUT,
                http_client
                    .post(&self.device.token_poll_endpoint)
                    .header(
                        reqwest::header::USER_AGENT,
                        format!("golutra/{}", env!("CARGO_PKG_VERSION")),
                    )
                    .json(&serde_json::json!({
                        "device_auth_id": self.device_auth_id,
                        "user_code": self.user_code,
                    }))
                    .send(),
            )
            .await
            .map_err(|_| AuthError::Timeout)?
            .map_err(|error| AuthError::OAuth(sanitize_oauth_error(&error.to_string())))?;
            match response.status().as_u16() {
                200 => return oauth_json_response(response).await,
                403 | 404 => {
                    tokio::time::sleep(openai_device_poll_delay(self.poll_interval)).await;
                }
                _ => {
                    let _: serde_json::Value = oauth_json_response(response).await?;
                    return Err(AuthError::OAuth(
                        "OpenAI device authorization polling failed".to_owned(),
                    ));
                }
            }
        }
    }
}

#[derive(Clone)]
struct AuthServiceCredentialProvider {
    service: AuthService,
    reference: CredentialRef,
    oauth: Option<OAuthProviderDescriptor>,
}

impl fmt::Debug for AuthServiceCredentialProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AuthServiceCredentialProvider")
            .field("credential_id", &self.reference.id)
            .field("source", &self.reference.source_label())
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl CredentialProvider for AuthServiceCredentialProvider {
    async fn credential(&self, force_refresh: bool) -> Result<SecretString, AuthError> {
        self.service
            .resolve_credential(&self.reference, self.oauth.as_ref(), force_refresh)
            .await
    }

    async fn metadata(&self) -> Result<CredentialMetadata, AuthError> {
        self.service.credential_metadata(&self.reference).await
    }

    fn source_label(&self) -> String {
        self.reference.source_label()
    }
}

#[derive(Debug)]
struct BrowserCallback {
    code: String,
}

struct BrowserCallbackBinding {
    listener: TcpListener,
    redirect_url: String,
    callback_path: String,
}

async fn bind_browser_callback(
    descriptor: &OAuthProviderDescriptor,
) -> Result<BrowserCallbackBinding, AuthError> {
    if let Some(redirect_uri) = &descriptor.browser_redirect_uri {
        let parsed = oauth2::url::Url::parse(redirect_uri)
            .map_err(|error| AuthError::Validation(error.to_string()))?;
        let host = parsed.host_str().ok_or_else(|| {
            AuthError::Validation("oauth browser redirect URI has no host".to_owned())
        })?;
        let ip = if host == "localhost" {
            std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
        } else {
            host.parse::<std::net::IpAddr>().map_err(|_| {
                AuthError::Validation(
                    "oauth browser redirect URI must use a loopback host".to_owned(),
                )
            })?
        };
        let port = parsed.port().ok_or_else(|| {
            AuthError::Validation("oauth browser redirect URI must include a port".to_owned())
        })?;
        let listener = TcpListener::bind(std::net::SocketAddr::new(ip, port))
            .await
            .map_err(|error| AuthError::Io(error.to_string()))?;
        return Ok(BrowserCallbackBinding {
            listener,
            redirect_url: redirect_uri.clone(),
            callback_path: parsed.path().to_owned(),
        });
    }

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|error| AuthError::Io(error.to_string()))?;
    let address = listener
        .local_addr()
        .map_err(|error| AuthError::Io(error.to_string()))?;
    Ok(BrowserCallbackBinding {
        listener,
        redirect_url: format!("http://127.0.0.1:{}/oauth/callback", address.port()),
        callback_path: "/oauth/callback".to_owned(),
    })
}

async fn wait_for_browser_callback(
    listener: TcpListener,
    expected_path: String,
    expected_state: String,
) -> Result<BrowserCallback, AuthError> {
    for _ in 0..MAX_CALLBACK_ATTEMPTS {
        let (mut stream, _) = listener
            .accept()
            .await
            .map_err(|error| AuthError::Io(error.to_string()))?;
        let mut request_line = String::new();
        BufReader::new(&mut stream)
            .take(MAX_CALLBACK_REQUEST_LINE_BYTES)
            .read_line(&mut request_line)
            .await
            .map_err(|error| AuthError::Io(error.to_string()))?;
        let mut parts = request_line.split_whitespace();
        let method = parts.next().unwrap_or_default();
        let target = parts.next().unwrap_or_default();
        let expected_target_prefix = format!("{expected_path}?");
        if method != "GET" || !target.starts_with(&expected_target_prefix) {
            write_callback_response(&mut stream, 400, "OAuth callback rejected").await?;
            continue;
        }
        let Ok(url) = oauth2::url::Url::parse(&format!("http://127.0.0.1{target}")) else {
            write_callback_response(&mut stream, 400, "OAuth callback rejected").await?;
            continue;
        };
        let parameters = url.query_pairs().collect::<HashMap<_, _>>();
        if let Some(error) = parameters.get("error") {
            write_callback_response(&mut stream, 400, "OAuth authorization failed").await?;
            return if error.as_ref() == "access_denied" {
                Err(AuthError::Cancelled)
            } else {
                Err(AuthError::OAuth(sanitize_oauth_error(error)))
            };
        }
        let code = parameters
            .get("code")
            .filter(|value| !value.is_empty())
            .map(|value| value.to_string());
        let state = parameters
            .get("state")
            .filter(|value| !value.is_empty())
            .map(|value| value.to_string());
        let (Some(code), Some(state)) = (code, state) else {
            write_callback_response(&mut stream, 400, "OAuth callback rejected").await?;
            continue;
        };
        if state != expected_state {
            write_callback_response(&mut stream, 400, "OAuth callback rejected").await?;
            continue;
        }
        write_callback_response(&mut stream, 200, "OAuth authorization completed").await?;
        return Ok(BrowserCallback { code });
    }
    Err(AuthError::OAuth(
        "oauth callback rejected too many invalid requests".to_owned(),
    ))
}

async fn write_callback_response(
    stream: &mut tokio::net::TcpStream,
    status: u16,
    message: &str,
) -> Result<(), AuthError> {
    let reason = if status == 200 { "OK" } else { "Bad Request" };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\ncontent-type: text/plain; charset=utf-8\r\ncontent-length: {}\r\ncache-control: no-store\r\nconnection: close\r\n\r\n{message}",
        message.len()
    );
    stream
        .write_all(response.as_bytes())
        .await
        .map_err(|error| AuthError::Io(error.to_string()))
}

fn oauth_client(
    descriptor: &OAuthProviderDescriptor,
    redirect_url: Option<&str>,
) -> Result<
    OAuthClient<
        oauth2::EndpointSet,
        oauth2::EndpointNotSet,
        oauth2::EndpointNotSet,
        oauth2::EndpointMaybeSet,
        oauth2::EndpointSet,
    >,
    AuthError,
> {
    descriptor.validate()?;
    let mut client = OAuthClient::new(ClientId::new(descriptor.client_id.clone()))
        .set_auth_uri(
            AuthUrl::new(descriptor.authorization_endpoint.clone())
                .map_err(|error| AuthError::Validation(error.to_string()))?,
        )
        .set_token_uri(
            TokenUrl::new(descriptor.token_endpoint.clone())
                .map_err(|error| AuthError::Validation(error.to_string()))?,
        )
        .set_auth_type(AuthType::RequestBody);
    if let Some(redirect_url) = redirect_url {
        client = client.set_redirect_uri(
            RedirectUrl::new(redirect_url.to_owned())
                .map_err(|error| AuthError::Validation(error.to_string()))?,
        );
    }
    let revocation_url = descriptor
        .revocation_endpoint
        .as_ref()
        .map(|endpoint| {
            RevocationUrl::new(endpoint.clone())
                .map_err(|error| AuthError::Validation(error.to_string()))
        })
        .transpose()?;
    Ok(client.set_revocation_url_option(revocation_url))
}

fn oauth_device_client(
    descriptor: &OAuthProviderDescriptor,
) -> Result<
    OAuthClient<
        oauth2::EndpointSet,
        oauth2::EndpointSet,
        oauth2::EndpointNotSet,
        oauth2::EndpointNotSet,
        oauth2::EndpointSet,
    >,
    AuthError,
> {
    descriptor.validate()?;
    OAuthClient::new(ClientId::new(descriptor.client_id.clone()))
        .set_auth_uri(
            AuthUrl::new(descriptor.authorization_endpoint.clone())
                .map_err(|error| AuthError::Validation(error.to_string()))?,
        )
        .set_token_uri(
            TokenUrl::new(descriptor.token_endpoint.clone())
                .map_err(|error| AuthError::Validation(error.to_string()))?,
        )
        .set_device_authorization_url(
            DeviceAuthorizationUrl::new(
                descriptor
                    .device_authorization_endpoint
                    .clone()
                    .ok_or_else(|| {
                        AuthError::Validation("device authorization endpoint is missing".to_owned())
                    })?,
            )
            .map_err(|error| AuthError::Validation(error.to_string()))?,
        )
        .set_auth_type(AuthType::RequestBody)
        .pipe(Ok)
}

trait Pipe: Sized {
    fn pipe<T>(self, function: impl FnOnce(Self) -> T) -> T {
        function(self)
    }
}

impl<T> Pipe for T {}

fn token_parts(
    token: &OAuthTokenResponse,
) -> (StoredOAuthCredential, CachedAccessToken, OAuthTokenMetadata) {
    let expires_at = token
        .expires_in()
        .and_then(|duration| chrono::Duration::from_std(duration).ok())
        .map(|duration| Utc::now() + duration);
    let refresh_token = token.refresh_token().map(|token| token.secret().to_owned());
    let scopes: Vec<String> = token
        .scopes()
        .map(|scopes| {
            scopes
                .iter()
                .map(|scope| scope.as_ref().to_owned())
                .collect()
        })
        .unwrap_or_default();
    let token_type = token.token_type().as_ref().to_owned();
    let access_token = SecretString::from(token.access_token().secret().to_owned());
    let account_id = token
        .extra_fields()
        .id_token
        .as_deref()
        .and_then(oauth_account_id)
        .or_else(|| oauth_account_id(token.access_token().secret()));
    let revision = format!("oauth_{}", uuid::Uuid::now_v7().simple());
    let stored = StoredOAuthCredential {
        revision: revision.clone(),
        access_token: Some(token.access_token().secret().to_owned()),
        refresh_token: refresh_token.clone(),
        expires_at,
        token_type: token_type.clone(),
        scopes: scopes.clone(),
        account_id: account_id.clone(),
    };
    let cached = CachedAccessToken {
        token: access_token,
        expires_at,
        revision,
    };
    let metadata = OAuthTokenMetadata {
        expires_at,
        scopes,
        token_type,
        refreshable: refresh_token.is_some(),
        account_id,
    };
    (stored, cached, metadata)
}

async fn oauth_json_response<T: DeserializeOwned>(
    response: reqwest::Response,
) -> Result<T, AuthError> {
    let status = response.status();
    if response
        .content_length()
        .is_some_and(|length| length > MAX_OAUTH_RESPONSE_BYTES as u64)
    {
        return Err(AuthError::OAuth(
            "OAuth server response exceeded the size limit".to_owned(),
        ));
    }
    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk =
            chunk.map_err(|error| AuthError::OAuth(sanitize_oauth_error(&error.to_string())))?;
        if bytes.len().saturating_add(chunk.len()) > MAX_OAUTH_RESPONSE_BYTES {
            return Err(AuthError::OAuth(
                "OAuth server response exceeded the size limit".to_owned(),
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    if !status.is_success() {
        return Err(AuthError::OAuth(sanitize_oauth_error(
            &String::from_utf8_lossy(&bytes),
        )));
    }
    serde_json::from_slice(&bytes)
        .map_err(|_| AuthError::OAuth("OAuth server returned invalid JSON".to_owned()))
}

fn device_poll_interval(value: &serde_json::Value) -> Duration {
    let seconds = value
        .as_u64()
        .or_else(|| value.as_str().and_then(|value| value.parse::<u64>().ok()))
        .filter(|seconds| *seconds > 0)
        .unwrap_or(5);
    Duration::from_secs(seconds)
}

fn openai_device_poll_delay(interval: Duration) -> Duration {
    if interval >= DEVICE_LOGIN_TIMEOUT {
        DEVICE_LOGIN_TIMEOUT.saturating_add(DEVICE_POLL_SAFETY_MARGIN)
    } else {
        interval.saturating_add(DEVICE_POLL_SAFETY_MARGIN)
    }
}

fn oauth_account_id(token: &str) -> Option<String> {
    let payload = token.split('.').nth(1)?;
    let decoded = URL_SAFE_NO_PAD.decode(payload).ok()?;
    let claims: serde_json::Value = serde_json::from_slice(&decoded).ok()?;
    claims
        .get("chatgpt_account_id")
        .and_then(serde_json::Value::as_str)
        .or_else(|| {
            claims
                .get("https://api.openai.com/auth")
                .and_then(|auth| auth.get("chatgpt_account_id"))
                .and_then(serde_json::Value::as_str)
        })
        .or_else(|| {
            claims
                .get("organizations")
                .and_then(serde_json::Value::as_array)
                .and_then(|organizations| organizations.first())
                .and_then(|organization| organization.get("id"))
                .and_then(serde_json::Value::as_str)
        })
        .filter(|value| !value.trim().is_empty() && value.len() <= 512)
        .map(ToOwned::to_owned)
}

fn decode_oauth_secret(secret: &SecretString) -> Result<StoredOAuthCredential, AuthError> {
    serde_json::from_str(secret.expose_secret())
        .map_err(|error| AuthError::SecretStore(format!("oauth credential is invalid: {error}")))
}

fn valid_persisted_access_token(credential: &StoredOAuthCredential) -> Option<SecretString> {
    credential
        .access_token
        .as_ref()
        .filter(|_| token_is_fresh(credential.expires_at))
        .map(|token| SecretString::from(token.clone()))
}

fn token_is_fresh(expires_at: Option<DateTime<Utc>>) -> bool {
    expires_at.is_none_or(|expires_at| {
        expires_at > Utc::now() + chrono::Duration::seconds(REFRESH_EARLY_SECONDS)
    })
}

fn new_reference(
    source: CredentialSource,
    secret_kind: SecretKind,
) -> Result<CredentialRef, AuthError> {
    match source {
        CredentialSource::Disk => Ok(CredentialRef::disk(secret_kind)),
        CredentialSource::Ephemeral => Ok(CredentialRef::ephemeral(secret_kind)),
        CredentialSource::Environment { .. } => Err(AuthError::Validation(
            "OAuth login cannot write to an environment credential reference".to_owned(),
        )),
    }
}

fn require_writable_source(source: &CredentialSource) -> Result<(), AuthError> {
    if matches!(source, CredentialSource::Environment { .. }) {
        Err(AuthError::Validation(
            "OAuth login requires disk or ephemeral credential storage".to_owned(),
        ))
    } else {
        Ok(())
    }
}

fn validate_non_empty(value: &str, label: &str) -> Result<(), AuthError> {
    if value.trim().is_empty() || value.len() > 512 {
        Err(AuthError::Validation(format!("{label} is invalid")))
    } else {
        Ok(())
    }
}

fn validate_endpoint(value: &str, label: &str) -> Result<(), AuthError> {
    let url = oauth2::url::Url::parse(value)
        .map_err(|error| AuthError::Validation(format!("{label} is invalid: {error}")))?;
    let loopback = url.host_str().is_some_and(|host| {
        host == "localhost"
            || host
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
    });
    if url.scheme() != "https" && !(url.scheme() == "http" && loopback) {
        return Err(AuthError::Validation(format!(
            "{label} must use HTTPS or loopback HTTP"
        )));
    }
    if !url.username().is_empty() || url.password().is_some() || url.fragment().is_some() {
        return Err(AuthError::Validation(format!(
            "{label} must not contain credentials or a fragment"
        )));
    }
    Ok(())
}

fn validate_browser_redirect_uri(value: &str) -> Result<(), AuthError> {
    let url = oauth2::url::Url::parse(value).map_err(|error| {
        AuthError::Validation(format!("browser redirect URI is invalid: {error}"))
    })?;
    if url.scheme() != "http" {
        return Err(AuthError::Validation(
            "oauth browser redirect URI must use HTTP on loopback".to_owned(),
        ));
    }
    let loopback = url.host_str().is_some_and(|host| {
        host == "localhost"
            || host
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
    });
    if !loopback {
        return Err(AuthError::Validation(
            "oauth browser redirect URI must use a loopback host".to_owned(),
        ));
    }
    if url.port().is_none()
        || url.path().is_empty()
        || url.path() == "/"
        || url.query().is_some()
        || url.fragment().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(AuthError::Validation(
            "oauth browser redirect URI must include a port and callback path without credentials, query, or fragment"
                .to_owned(),
        ));
    }
    Ok(())
}

fn oauth_request_error(error: impl fmt::Display) -> AuthError {
    AuthError::OAuth(sanitize_oauth_error(&error.to_string()))
}

fn oauth_refresh_error(error: impl fmt::Display) -> AuthError {
    let message = sanitize_oauth_error(&error.to_string());
    if message.to_ascii_lowercase().contains("invalid_grant") {
        AuthError::ReauthenticationRequired(message)
    } else {
        AuthError::OAuth(message)
    }
}

fn sanitize_oauth_error(message: &str) -> String {
    let normalized = message.to_ascii_lowercase();
    let known_code = [
        "invalid_grant",
        "invalid_client",
        "invalid_request",
        "access_denied",
        "authorization_pending",
        "slow_down",
        "expired_token",
        "unsupported_grant_type",
        "unsupported_token_type",
    ]
    .into_iter()
    .find(|code| normalized.contains(code));
    if let Some(code) = known_code {
        format!("OAuth server returned {code}")
    } else if normalized.contains("timed out") || normalized.contains("timeout") {
        "OAuth server request timed out".to_owned()
    } else if normalized.contains("connect")
        || normalized.contains("dns")
        || normalized.contains("request")
    {
        "OAuth transport request failed".to_owned()
    } else {
        "OAuth server request failed".to_owned()
    }
}

async fn acquire_refresh_file_lock(
    home: PathBuf,
    credential_id: String,
) -> Result<File, AuthError> {
    tokio::task::spawn_blocking(move || {
        let directory = home.join("auth").join("refresh");
        fs::create_dir_all(&directory).map_err(|error| AuthError::Io(error.to_string()))?;
        set_owner_only_dir(&directory)?;
        let path = directory.join(format!("{}.lock", refresh_lock_id(&credential_id)));
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|error| AuthError::Io(error.to_string()))?;
        set_owner_only_file(&path)?;
        file.lock_exclusive()
            .map_err(|error| AuthError::Io(error.to_string()))?;
        Ok(file)
    })
    .await
    .map_err(|error| AuthError::Io(error.to_string()))?
}

fn refresh_lock_id(credential_id: &str) -> String {
    use sha2::{Digest, Sha256};

    Sha256::digest(credential_id.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(unix)]
fn set_owner_only_dir(path: &Path) -> Result<(), AuthError> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(|error| AuthError::Io(error.to_string()))
}

#[cfg(not(unix))]
fn set_owner_only_dir(_path: &Path) -> Result<(), AuthError> {
    Ok(())
}

#[cfg(unix)]
fn set_owner_only_file(path: &Path) -> Result<(), AuthError> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .map_err(|error| AuthError::Io(error.to_string()))
}

#[cfg(not(unix))]
fn set_owner_only_file(_path: &Path) -> Result<(), AuthError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use secrecy::ExposeSecret;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpStream,
        sync::Notify,
        task::JoinHandle,
    };

    use super::*;

    #[derive(Debug, Clone)]
    struct FakeOAuthOptions {
        access_token_expires_in: u64,
        refresh_delay: Duration,
        reject_refresh: bool,
    }

    impl Default for FakeOAuthOptions {
        fn default() -> Self {
            Self {
                access_token_expires_in: 3_600,
                refresh_delay: Duration::ZERO,
                reject_refresh: false,
            }
        }
    }

    #[derive(Debug, Default)]
    struct FakeOAuthState {
        requests: Mutex<Vec<CapturedRequest>>,
        refresh_count: AtomicUsize,
        revoke_count: AtomicUsize,
        refresh_started: Notify,
    }

    #[derive(Debug, Clone)]
    struct CapturedRequest {
        path: String,
        body: String,
    }

    struct FakeOAuthServer {
        base_url: String,
        state: Arc<FakeOAuthState>,
        task: JoinHandle<()>,
    }

    impl FakeOAuthServer {
        async fn spawn(options: FakeOAuthOptions) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
            let address = listener.local_addr().expect("address");
            let state = Arc::new(FakeOAuthState::default());
            let server_state = Arc::clone(&state);
            let task = tokio::spawn(async move {
                loop {
                    let Ok((mut stream, _)) = listener.accept().await else {
                        break;
                    };
                    let request = read_test_request(&mut stream).await;
                    server_state
                        .requests
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .push(request.clone());
                    let (status, body) =
                        fake_oauth_response(&request, &server_state, &options).await;
                    write_test_response(&mut stream, status, &body).await;
                }
            });
            Self {
                base_url: format!("http://{address}"),
                state,
                task,
            }
        }

        fn descriptor(&self) -> OAuthProviderDescriptor {
            OAuthProviderDescriptor {
                provider_id: "test-provider".to_owned(),
                client_id: "test-client".to_owned(),
                authorization_endpoint: format!("{}/authorize", self.base_url),
                token_endpoint: format!("{}/token", self.base_url),
                device_authorization_endpoint: Some(format!("{}/device", self.base_url)),
                revocation_endpoint: Some(format!("{}/revoke", self.base_url)),
                scopes: vec!["profile".to_owned(), "model.invoke".to_owned()],
                audience: Some("golutra-test".to_owned()),
                browser_redirect_uri: None,
                authorization_params: BTreeMap::new(),
                authorization_nonce: false,
                openai_device_authorization: None,
                flows: vec![OAuthFlow::BrowserPkce, OAuthFlow::DeviceCode],
            }
        }

        fn refresh_count(&self) -> usize {
            self.state.refresh_count.load(Ordering::SeqCst)
        }

        fn requests(&self) -> Vec<CapturedRequest> {
            self.state
                .requests
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone()
        }
    }

    impl Drop for FakeOAuthServer {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    async fn read_test_request(stream: &mut TcpStream) -> CapturedRequest {
        let mut bytes = Vec::new();
        let mut chunk = [0_u8; 2_048];
        let mut expected_len = None;
        loop {
            let read = stream.read(&mut chunk).await.expect("read request");
            if read == 0 {
                break;
            }
            bytes.extend_from_slice(&chunk[..read]);
            assert!(bytes.len() <= 64 * 1_024, "test request exceeded limit");
            if expected_len.is_none()
                && let Some(header_end) = find_header_end(&bytes)
            {
                let headers = String::from_utf8_lossy(&bytes[..header_end]);
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        line.split_once(':').and_then(|(name, value)| {
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().ok())
                                .flatten()
                        })
                    })
                    .unwrap_or_default();
                expected_len = Some(header_end + 4 + content_length);
            }
            if expected_len.is_some_and(|length| bytes.len() >= length) {
                break;
            }
        }
        let header_end = find_header_end(&bytes).expect("request headers");
        let head = String::from_utf8_lossy(&bytes[..header_end]);
        let path = head
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .expect("request path")
            .to_owned();
        CapturedRequest {
            path,
            body: String::from_utf8_lossy(&bytes[header_end + 4..]).into_owned(),
        }
    }

    fn find_header_end(bytes: &[u8]) -> Option<usize> {
        bytes.windows(4).position(|window| window == b"\r\n\r\n")
    }

    async fn fake_oauth_response(
        request: &CapturedRequest,
        state: &FakeOAuthState,
        options: &FakeOAuthOptions,
    ) -> (u16, String) {
        if request.path == "/device" {
            return (
                200,
                serde_json::json!({
                    "device_code": "device-secret",
                    "user_code": "GOLUTRA-123",
                    "verification_uri": "https://example.com/device",
                    "verification_uri_complete": "https://example.com/device?code=GOLUTRA-123",
                    "expires_in": 600,
                    "interval": 1
                })
                .to_string(),
            );
        }
        if request.path == "/openai-device/usercode" {
            return (
                200,
                serde_json::json!({
                    "device_auth_id": "openai-device-secret",
                    "user_code": "OPENAI-123",
                    "interval": "1"
                })
                .to_string(),
            );
        }
        if request.path == "/openai-device/token" {
            return (
                200,
                serde_json::json!({
                    "authorization_code": "openai-device-code",
                    "code_verifier": "openai-device-verifier"
                })
                .to_string(),
            );
        }
        if request.path == "/revoke" {
            state.revoke_count.fetch_add(1, Ordering::SeqCst);
            return (200, String::new());
        }
        if request.path != "/token" {
            return (404, serde_json::json!({"error": "not_found"}).to_string());
        }
        if request.body.contains("grant_type=refresh_token") {
            let refresh_number = state.refresh_count.fetch_add(1, Ordering::SeqCst) + 1;
            state.refresh_started.notify_waiters();
            tokio::time::sleep(options.refresh_delay).await;
            if options.reject_refresh {
                return (
                    400,
                    serde_json::json!({
                        "error": "invalid_grant",
                        "error_description": "refresh token expired",
                        "refresh_token": "must-not-appear"
                    })
                    .to_string(),
                );
            }
            return (
                200,
                serde_json::json!({
                    "access_token": format!("access-refresh-{refresh_number}"),
                    "refresh_token": format!("refresh-{}", refresh_number + 1),
                    "token_type": "Bearer",
                    "expires_in": options.access_token_expires_in,
                    "scope": "profile model.invoke"
                })
                .to_string(),
            );
        }
        let access_token = if request.body.contains("code=openai-device-code") {
            "access-openai-device"
        } else if request.body.contains("device_code=device-secret") {
            "access-device"
        } else {
            "access-browser"
        };
        (
            200,
            serde_json::json!({
                "access_token": access_token,
                "refresh_token": "refresh-1",
                "id_token": fake_id_token("account-from-id-token"),
                "token_type": "Bearer",
                "expires_in": options.access_token_expires_in,
                "scope": "profile model.invoke"
            })
            .to_string(),
        )
    }

    fn fake_id_token(account_id: &str) -> String {
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"none"}"#);
        let payload = URL_SAFE_NO_PAD.encode(
            serde_json::json!({"chatgpt_account_id": account_id})
                .to_string()
                .as_bytes(),
        );
        format!("{header}.{payload}.signature")
    }

    async fn write_test_response(stream: &mut TcpStream, status: u16, body: &str) {
        let reason = if status == 200 { "OK" } else { "Not Found" };
        let response = format!(
            "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        stream
            .write_all(response.as_bytes())
            .await
            .expect("write response");
    }

    async fn send_browser_callback(redirect_url: &str, code: &str, state: &str) -> String {
        let url = oauth2::url::Url::parse(redirect_url).expect("redirect url");
        let port = url.port().expect("redirect port");
        let mut stream = TcpStream::connect(("127.0.0.1", port))
            .await
            .expect("connect callback");
        let request = format!(
            "GET {}?code={code}&state={state} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nconnection: close\r\n\r\n",
            url.path()
        );
        stream
            .write_all(request.as_bytes())
            .await
            .expect("write callback");
        let mut response = String::new();
        stream
            .read_to_string(&mut response)
            .await
            .expect("read callback response");
        response
    }

    async fn browser_login(
        service: &AuthService,
        descriptor: OAuthProviderDescriptor,
    ) -> OAuthLoginResult {
        let login = service
            .begin_browser_login(descriptor, CredentialSource::Ephemeral)
            .await
            .expect("begin browser login");
        let authorization_url =
            oauth2::url::Url::parse(login.authorization_url()).expect("authorization url");
        let parameters = authorization_url.query_pairs().collect::<HashMap<_, _>>();
        let state = parameters.get("state").expect("state").to_string();
        let redirect_url = login.redirect_url().to_owned();
        let (result, callback) = tokio::join!(
            login.complete(),
            send_browser_callback(&redirect_url, "browser-code", &state)
        );
        assert!(callback.starts_with("HTTP/1.1 200"));
        result.expect("complete browser login")
    }

    #[test]
    fn descriptor_rejects_remote_plaintext_endpoints() {
        let descriptor = OAuthProviderDescriptor {
            provider_id: "custom".to_owned(),
            client_id: "client".to_owned(),
            authorization_endpoint: "http://example.com/authorize".to_owned(),
            token_endpoint: "https://example.com/token".to_owned(),
            device_authorization_endpoint: None,
            revocation_endpoint: None,
            scopes: vec!["openid".to_owned()],
            audience: None,
            browser_redirect_uri: None,
            authorization_params: BTreeMap::new(),
            authorization_nonce: false,
            openai_device_authorization: None,
            flows: vec![OAuthFlow::BrowserPkce],
        };

        assert!(descriptor.validate().is_err());
    }

    #[test]
    fn oauth_flow_serializes_stable_wire_ids() {
        assert_eq!(
            serde_json::to_value(OAuthFlow::OpenAiDeviceAuth).expect("flow JSON"),
            serde_json::json!("openai-device-auth")
        );
    }

    #[test]
    fn auth_service_debug_does_not_expose_store_contents() {
        let home = tempfile::tempdir().expect("home");
        let store = Arc::new(crate::MemorySecretStore::default());
        let service = AuthService::new(home.path(), store).expect("service");

        assert!(!format!("{service:?}").contains("secret-value"));
    }

    #[test]
    fn oauth_errors_preserve_actionable_codes_without_response_secrets() {
        let message = sanitize_oauth_error(
            r#"server response: {"error":"invalid_grant","refresh_token":"refresh-secret"}"#,
        );

        assert_eq!(message, "OAuth server returned invalid_grant");
        assert!(!message.contains("refresh-secret"));
    }

    #[test]
    fn openai_device_polling_respects_server_interval_and_total_deadline() {
        let server_interval = device_poll_interval(&serde_json::json!(300));
        let beyond_deadline = device_poll_interval(&serde_json::json!(1_800));

        assert_eq!(server_interval, Duration::from_secs(300));
        assert_eq!(
            openai_device_poll_delay(server_interval),
            Duration::from_secs(303)
        );
        assert!(openai_device_poll_delay(beyond_deadline) > DEVICE_LOGIN_TIMEOUT);
    }

    #[tokio::test]
    async fn browser_pkce_flow_validates_state_and_persists_token_set() {
        let server = FakeOAuthServer::spawn(FakeOAuthOptions::default()).await;
        let home = tempfile::tempdir().expect("home");
        let store = Arc::new(crate::MemorySecretStore::default());
        let service = AuthService::new(home.path(), store.clone()).expect("service");
        let descriptor = server.descriptor();
        let login = service
            .begin_browser_login(descriptor.clone(), CredentialSource::Ephemeral)
            .await
            .expect("begin browser login");
        let authorization_url =
            oauth2::url::Url::parse(login.authorization_url()).expect("authorization url");
        let parameters = authorization_url.query_pairs().collect::<HashMap<_, _>>();
        assert_eq!(
            parameters
                .get("code_challenge_method")
                .map(|value| value.as_ref()),
            Some("S256")
        );
        assert!(parameters.contains_key("code_challenge"));
        assert_eq!(
            parameters.get("audience").map(|value| value.as_ref()),
            Some("golutra-test")
        );
        let state = parameters.get("state").expect("state").to_string();
        let redirect_url = login.redirect_url().to_owned();

        let (result, callback) = tokio::join!(
            login.complete(),
            send_browser_callback(&redirect_url, "browser-code", &state)
        );
        assert!(callback.starts_with("HTTP/1.1 200"));
        let result = result.expect("complete login");
        assert!(result.metadata.refreshable);
        assert_eq!(
            result.metadata.account_id.as_deref(),
            Some("account-from-id-token")
        );
        assert!(store.contains(&result.credential_ref));
        let provider = service.credential_provider(result.credential_ref.clone(), Some(descriptor));
        assert_eq!(
            provider
                .credential(false)
                .await
                .expect("access token")
                .expose_secret(),
            "access-browser"
        );
        assert_eq!(
            provider
                .metadata()
                .await
                .expect("credential metadata")
                .account_id
                .as_deref(),
            Some("account-from-id-token")
        );
        let token_request = server
            .requests()
            .into_iter()
            .find(|request| request.path == "/token")
            .expect("token request");
        assert!(token_request.body.contains("grant_type=authorization_code"));
        assert!(token_request.body.contains("code_verifier="));
        assert!(!token_request.body.contains("access-browser"));
    }

    #[tokio::test]
    async fn browser_flow_uses_registered_callback_params_and_nonce() {
        let server = FakeOAuthServer::spawn(FakeOAuthOptions::default()).await;
        let home = tempfile::tempdir().expect("home");
        let store = Arc::new(crate::MemorySecretStore::default());
        let service = AuthService::new(home.path(), store).expect("service");
        let reserved = std::net::TcpListener::bind("127.0.0.1:0").expect("reserve callback");
        let port = reserved.local_addr().expect("callback address").port();
        drop(reserved);
        let redirect_url = format!("http://127.0.0.1:{port}/auth/callback");
        let mut descriptor = server.descriptor();
        descriptor.browser_redirect_uri = Some(redirect_url.clone());
        descriptor.authorization_params = BTreeMap::from([
            ("originator".to_owned(), "golutra".to_owned()),
            ("plan".to_owned(), "generic".to_owned()),
        ]);
        descriptor.authorization_nonce = true;

        let login = service
            .begin_browser_login(descriptor, CredentialSource::Ephemeral)
            .await
            .expect("begin fixed callback login");
        assert_eq!(login.redirect_url(), redirect_url);
        let authorization_url =
            oauth2::url::Url::parse(login.authorization_url()).expect("authorization url");
        let parameters = authorization_url.query_pairs().collect::<HashMap<_, _>>();
        assert_eq!(
            parameters.get("redirect_uri").map(|value| value.as_ref()),
            Some(redirect_url.as_str())
        );
        assert_eq!(
            parameters.get("originator").map(|value| value.as_ref()),
            Some("golutra")
        );
        assert_eq!(
            parameters.get("plan").map(|value| value.as_ref()),
            Some("generic")
        );
        assert!(
            parameters
                .get("nonce")
                .is_some_and(|value| !value.is_empty())
        );
        let state = parameters.get("state").expect("state").to_string();

        let (result, callback) = tokio::join!(
            login.complete(),
            send_browser_callback(&redirect_url, "fixed-code", &state)
        );

        assert!(callback.starts_with("HTTP/1.1 200"));
        assert!(result.is_ok());
        let token_request = server
            .requests()
            .into_iter()
            .find(|request| request.path == "/token")
            .expect("token request");
        let token_parameters = oauth2::url::form_urlencoded::parse(token_request.body.as_bytes())
            .collect::<HashMap<_, _>>();
        assert_eq!(
            token_parameters
                .get("redirect_uri")
                .map(|value| value.as_ref()),
            Some(redirect_url.as_str())
        );
    }

    #[tokio::test]
    async fn browser_flow_ignores_invalid_state_until_valid_callback_arrives() {
        let server = FakeOAuthServer::spawn(FakeOAuthOptions::default()).await;
        let home = tempfile::tempdir().expect("home");
        let store = Arc::new(crate::MemorySecretStore::default());
        let service = AuthService::new(home.path(), store).expect("service");
        let login = service
            .begin_browser_login(server.descriptor(), CredentialSource::Ephemeral)
            .await
            .expect("begin browser login");
        let authorization_url =
            oauth2::url::Url::parse(login.authorization_url()).expect("authorization url");
        let state = authorization_url
            .query_pairs()
            .find_map(|(key, value)| (key == "state").then(|| value.into_owned()))
            .expect("state");
        let redirect_url = login.redirect_url().to_owned();

        let (result, callbacks) = tokio::join!(login.complete(), async {
            let rejected =
                send_browser_callback(&redirect_url, "browser-code", "wrong-state").await;
            let accepted = send_browser_callback(&redirect_url, "browser-code", &state).await;
            (rejected, accepted)
        });

        assert!(callbacks.0.starts_with("HTTP/1.1 400"));
        assert!(callbacks.1.starts_with("HTTP/1.1 200"));
        assert!(result.is_ok());
        assert_eq!(server.requests().len(), 1);
    }

    #[tokio::test]
    async fn browser_flow_timeout_closes_callback_listener() {
        let server = FakeOAuthServer::spawn(FakeOAuthOptions::default()).await;
        let home = tempfile::tempdir().expect("home");
        let store = Arc::new(crate::MemorySecretStore::default());
        let service = AuthService::new(home.path(), store).expect("service");
        let mut login = service
            .begin_browser_login(server.descriptor(), CredentialSource::Ephemeral)
            .await
            .expect("begin browser login");
        let redirect_url = oauth2::url::Url::parse(login.redirect_url()).expect("redirect url");
        let port = redirect_url.port().expect("redirect port");

        let result = login.complete_with_timeout(Duration::from_millis(10)).await;

        assert!(matches!(result, Err(AuthError::Timeout)));
        assert!(TcpStream::connect(("127.0.0.1", port)).await.is_err());
    }

    #[tokio::test]
    async fn device_flow_persists_token_set_to_disk_and_resolves_after_restart() {
        let server = FakeOAuthServer::spawn(FakeOAuthOptions::default()).await;
        let home = tempfile::tempdir().expect("home");
        let store = Arc::new(crate::DefaultSecretStore::new(home.path()).expect("disk store"));
        let service = AuthService::new(home.path(), store).expect("service");
        let descriptor = server.descriptor();
        let login = service
            .begin_device_login(descriptor.clone(), CredentialSource::Disk)
            .await
            .expect("begin device login");
        assert_eq!(login.user_code(), "GOLUTRA-123");
        assert_eq!(login.verification_uri(), "https://example.com/device");
        assert_eq!(
            login.verification_uri_complete().as_deref(),
            Some("https://example.com/device?code=GOLUTRA-123")
        );

        let result = login.complete().await.expect("complete device login");
        assert!(home.path().join(crate::CREDENTIALS_FILE_NAME).is_file());
        let restarted = AuthService::new(
            home.path(),
            Arc::new(crate::DefaultSecretStore::new(home.path()).expect("restarted disk store")),
        )
        .expect("restarted service");
        let provider = restarted.credential_provider(result.credential_ref, Some(descriptor));
        assert_eq!(
            provider
                .credential(false)
                .await
                .expect("access token")
                .expose_secret(),
            "access-device"
        );
        let requests = server.requests();
        let device_request = requests
            .iter()
            .find(|request| request.path == "/device")
            .expect("device authorization request");
        assert!(device_request.body.contains("client_id=test-client"));
        assert!(device_request.body.contains("scope=profile"));
        assert!(device_request.body.contains("model.invoke"));
        assert!(device_request.body.contains("audience=golutra-test"));
        let token_request = requests
            .iter()
            .find(|request| request.path == "/token")
            .expect("device token request");
        assert!(token_request.body.contains("device_code=device-secret"));
        assert!(
            token_request
                .body
                .contains("grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code")
        );
    }

    #[tokio::test]
    async fn openai_headless_flow_exchanges_device_authorization_code() {
        let server = FakeOAuthServer::spawn(FakeOAuthOptions::default()).await;
        let home = tempfile::tempdir().expect("home");
        let store = Arc::new(crate::MemorySecretStore::default());
        let service = AuthService::new(home.path(), store).expect("service");
        let mut descriptor = server.descriptor();
        descriptor.flows.push(OAuthFlow::OpenAiDeviceAuth);
        descriptor.openai_device_authorization = Some(OpenAiDeviceAuthorizationDescriptor {
            user_code_endpoint: format!("{}/openai-device/usercode", server.base_url),
            token_poll_endpoint: format!("{}/openai-device/token", server.base_url),
            verification_uri: format!("{}/openai-device/verify", server.base_url),
            redirect_uri: format!("{}/openai-device/callback", server.base_url),
        });
        let login = service
            .begin_openai_device_login(descriptor.clone(), CredentialSource::Ephemeral)
            .await
            .expect("begin OpenAI device auth");
        assert_eq!(login.user_code(), "OPENAI-123");
        assert_eq!(
            login.verification_uri(),
            format!("{}/openai-device/verify", server.base_url)
        );

        let result = login.complete().await.expect("complete OpenAI device auth");
        let provider = service.credential_provider(result.credential_ref, Some(descriptor));
        assert_eq!(
            provider
                .credential(false)
                .await
                .expect("access token")
                .expose_secret(),
            "access-openai-device"
        );
        let requests = server.requests();
        let user_code = requests
            .iter()
            .find(|request| request.path == "/openai-device/usercode")
            .expect("OpenAI user-code request");
        assert!(user_code.body.contains("\"client_id\":\"test-client\""));
        let poll = requests
            .iter()
            .find(|request| request.path == "/openai-device/token")
            .expect("OpenAI token poll request");
        assert!(
            poll.body
                .contains("\"device_auth_id\":\"openai-device-secret\"")
        );
        assert!(poll.body.contains("\"user_code\":\"OPENAI-123\""));
        let exchange = requests
            .iter()
            .find(|request| {
                request.path == "/token" && request.body.contains("code=openai-device-code")
            })
            .expect("OpenAI authorization-code exchange");
        assert!(
            exchange
                .body
                .contains("code_verifier=openai-device-verifier")
        );
        assert!(exchange.body.contains("grant_type=authorization_code"));
        assert!(exchange.body.contains("redirect_uri=http%3A%2F%2F"));
    }

    #[tokio::test]
    async fn concurrent_forced_refresh_is_single_flight_and_rotates_refresh_token() {
        let server = FakeOAuthServer::spawn(FakeOAuthOptions {
            refresh_delay: Duration::from_millis(50),
            ..FakeOAuthOptions::default()
        })
        .await;
        let home = tempfile::tempdir().expect("home");
        let store = Arc::new(crate::MemorySecretStore::default());
        let service = AuthService::new(home.path(), store).expect("service");
        let descriptor = server.descriptor();
        let login = browser_login(&service, descriptor.clone()).await;
        let provider = service.credential_provider(login.credential_ref, Some(descriptor));

        let mut tasks = Vec::new();
        for _ in 0..8 {
            let provider = Arc::clone(&provider);
            tasks.push(tokio::spawn(async move {
                provider.credential(true).await.expect("refresh")
            }));
        }
        for task in tasks {
            assert_eq!(
                task.await.expect("join").expose_secret(),
                "access-refresh-1"
            );
        }
        assert_eq!(server.refresh_count(), 1);

        assert_eq!(
            provider
                .credential(true)
                .await
                .expect("second refresh")
                .expose_secret(),
            "access-refresh-2"
        );
        let refresh_requests = server
            .requests()
            .into_iter()
            .filter(|request| request.body.contains("grant_type=refresh_token"))
            .collect::<Vec<_>>();
        assert_eq!(refresh_requests.len(), 2);
        assert!(refresh_requests[0].body.contains("refresh_token=refresh-1"));
        assert!(refresh_requests[1].body.contains("refresh_token=refresh-2"));
    }

    #[tokio::test]
    async fn invalid_grant_deletes_stale_token_set_and_requires_reauthentication() {
        let server = FakeOAuthServer::spawn(FakeOAuthOptions {
            reject_refresh: true,
            ..FakeOAuthOptions::default()
        })
        .await;
        let home = tempfile::tempdir().expect("home");
        let store = Arc::new(crate::MemorySecretStore::default());
        let service = AuthService::new(home.path(), store.clone()).expect("service");
        let descriptor = server.descriptor();
        let login = browser_login(&service, descriptor.clone()).await;
        let reference = login.credential_ref.clone();
        let provider = service.credential_provider(reference.clone(), Some(descriptor));

        let error = provider
            .credential(true)
            .await
            .expect_err("refresh must require login");

        assert!(matches!(
            error,
            AuthError::ReauthenticationRequired(message)
                if message == "OAuth server returned invalid_grant"
        ));
        assert!(!store.contains(&reference));
        assert!(matches!(
            provider.credential(false).await,
            Err(AuthError::SecretNotFound(id)) if id == reference.id
        ));
    }

    #[tokio::test]
    async fn separate_auth_services_reuse_cross_process_refresh_result() {
        let server = FakeOAuthServer::spawn(FakeOAuthOptions {
            refresh_delay: Duration::from_millis(100),
            ..FakeOAuthOptions::default()
        })
        .await;
        let home = tempfile::tempdir().expect("home");
        let store = Arc::new(crate::MemorySecretStore::default());
        let first_service = AuthService::new(home.path(), store.clone()).expect("first service");
        let second_service = AuthService::new(home.path(), store).expect("second service");
        let descriptor = server.descriptor();
        let login = browser_login(&first_service, descriptor.clone()).await;
        let first_provider = first_service
            .credential_provider(login.credential_ref.clone(), Some(descriptor.clone()));
        let second_provider =
            second_service.credential_provider(login.credential_ref, Some(descriptor));

        let first = tokio::spawn(async move {
            first_provider
                .credential(true)
                .await
                .expect("first refresh")
        });
        server.state.refresh_started.notified().await;
        let second = tokio::spawn(async move {
            second_provider
                .credential(true)
                .await
                .expect("second refresh")
        });

        assert_eq!(
            first.await.expect("join").expose_secret(),
            "access-refresh-1"
        );
        assert_eq!(
            second.await.expect("join").expose_secret(),
            "access-refresh-1"
        );
        assert_eq!(server.refresh_count(), 1);
    }

    #[tokio::test]
    async fn logout_deletes_local_secret_even_when_remote_revoke_fails() {
        let server = FakeOAuthServer::spawn(FakeOAuthOptions::default()).await;
        let home = tempfile::tempdir().expect("home");
        let store = Arc::new(crate::MemorySecretStore::default());
        let service = AuthService::new(home.path(), store.clone()).expect("service");
        let descriptor = server.descriptor();
        let login = browser_login(&service, descriptor.clone()).await;

        let result = service
            .logout(&login.credential_ref, Some(&descriptor))
            .await;

        assert!(matches!(result, Err(AuthError::OAuth(message)) if message.contains("HTTPS")));
        assert!(!store.contains(&login.credential_ref));
        assert_eq!(server.state.revoke_count.load(Ordering::SeqCst), 0);
    }
}
