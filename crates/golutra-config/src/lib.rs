use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
};

use fs2::FileExt;
use golutra_auth::{
    AuthService, CredentialProvider, CredentialRef, CredentialSource, DefaultSecretStore,
    OAuthProviderDescriptor, SecretKind, SecretStore,
};
use golutra_llm::{
    ConfiguredProvider, GOLUTRA_PROVIDER_CUSTOM_HEADERS, ModelCatalog, ProviderGenerationConfig,
    ProviderHeaderConfig, ProviderHeaderValue, ProviderProtocol, validate_native_base_url,
    validate_openai_base_url,
};
use secrecy::SecretString;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

mod provider_auth;
mod provider_storage;

pub use provider_auth::{
    BuiltinOAuthMethod, builtin_oauth_method, builtin_oauth_methods,
    builtin_oauth_methods_for_provider,
};
pub(crate) use provider_storage::{
    SecretMutation, SecretMutationAction, acquire_provider_settings_lock, default_secret_store,
    load_or_migrate_provider_settings_unlocked, load_provider_settings_for_install_unlocked,
    persist_profile_in_settings, provider_install_error, replaced_credential,
    run_provider_install_transaction, run_provider_settings_transaction, write_json_owner_only,
};
pub use provider_storage::{
    generate_custom_provider_api_key_env, golutra_home, provider_auth_service,
};

pub const GOLUTRA_HOME_ENV: &str = "GOLUTRA_HOME";
const PROVIDER_FILE: &str = "provider.json";
const PROVIDER_SETTINGS_VERSION: u32 = 2;
const GOLUTRA_PROVIDER_GENERATION_CONFIG: &str = "GOLUTRA_PROVIDER_GENERATION_CONFIG";
pub const CUSTOM_PROVIDER_API_KEY_ENV_PREFIX: &str = "GOLUTRA_CUSTOM_PROVIDER_API_KEY_";
const DENIED_ENV_KEYS: &[&str] = &[
    "NODE_OPTIONS",
    "LD_PRELOAD",
    "LD_LIBRARY_PATH",
    "DYLD_INSERT_LIBRARIES",
    "PATH",
    "HOME",
    "SHELL",
    "PWD",
];

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("config io failed: {0}")]
    Io(String),
    #[error("config json failed: {0}")]
    Json(String),
    #[error("config validation failed: {0}")]
    Validation(String),
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
#[error("{message}")]
pub struct ProviderInstallError {
    pub step: &'static str,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeConfig {
    pub data_dir: String,
    pub event_log_layout: String,
    pub checkpoint_strategy: String,
    pub sandbox_profile: String,
    pub protocol_version: String,
    pub model_catalog: ModelCatalog,
}

impl RuntimeConfig {
    #[must_use]
    pub fn p1_default() -> Self {
        Self {
            data_dir: "${GOLUTRA_HOME:-~/.golutra}/state".to_owned(),
            event_log_layout: "sqlite".to_owned(),
            checkpoint_strategy: "snapshot".to_owned(),
            sandbox_profile: "p0_workspace_guard".to_owned(),
            protocol_version: "v0.1".to_owned(),
            model_catalog: ModelCatalog::p1_default(),
        }
    }

    pub fn load(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let content =
            fs::read_to_string(path).map_err(|error| ConfigError::Io(error.to_string()))?;
        serde_json::from_str(&content).map_err(|error| ConfigError::Json(error.to_string()))
    }

    pub fn save(&self, path: impl AsRef<Path>) -> Result<(), ConfigError> {
        let content = serde_json::to_string_pretty(self)
            .map_err(|error| ConfigError::Json(error.to_string()))?;
        fs::write(path, content).map_err(|error| ConfigError::Io(error.to_string()))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderConfigPaths {
    pub home: PathBuf,
    pub user_config: PathBuf,
}

impl ProviderConfigPaths {
    pub fn global() -> Result<Self, ConfigError> {
        Self::from_home(golutra_home()?)
    }

    pub fn from_home(home: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let home = home.as_ref();
        if home.as_os_str().is_empty() {
            return Err(ConfigError::Validation(
                "provider config home cannot be empty".to_owned(),
            ));
        }
        Ok(Self {
            home: home.to_path_buf(),
            user_config: home.join(PROVIDER_FILE),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderConfigScope {
    User,
    Workspace,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderSettings {
    pub version: u32,
    pub active_profile: Option<String>,
    pub profiles: Vec<ProviderProfile>,
}

impl Default for ProviderSettings {
    fn default() -> Self {
        Self {
            version: PROVIDER_SETTINGS_VERSION,
            active_profile: None,
            profiles: Vec::new(),
        }
    }
}

impl ProviderSettings {
    pub fn load(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let path = path.as_ref();
        match fs::read_to_string(path) {
            Ok(content) => {
                let settings: Self = serde_json::from_str(&content)
                    .map_err(|error| ConfigError::Json(error.to_string()))?;
                settings.validate()?;
                Ok(settings)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(ConfigError::Io(error.to_string())),
        }
    }

    pub fn save(&self, path: impl AsRef<Path>) -> Result<(), ConfigError> {
        self.validate()?;
        let path = path.as_ref();
        let _lock = acquire_provider_settings_lock(path)?;
        self.save_unlocked(path)
    }

    fn save_unlocked(&self, path: &Path) -> Result<(), ConfigError> {
        write_json_owner_only(path, self)
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.version != PROVIDER_SETTINGS_VERSION {
            return Err(ConfigError::Validation(format!(
                "unsupported provider settings version {}",
                self.version
            )));
        }
        let mut profile_names = BTreeSet::new();
        let mut credential_ids = BTreeSet::new();
        for profile in &self.profiles {
            profile.validate()?;
            if !profile_names.insert(profile.name.as_str()) {
                return Err(ConfigError::Validation(format!(
                    "provider profile `{}` is duplicated",
                    profile.name
                )));
            }
            if let Some(reference) = &profile.credential_ref
                && !credential_ids.insert(reference.id.as_str())
            {
                return Err(ConfigError::Validation(format!(
                    "credential `{}` is referenced by multiple provider profiles",
                    reference.id
                )));
            }
        }
        if let Some(active_profile) = &self.active_profile {
            let profile = self
                .profiles
                .iter()
                .find(|profile| &profile.name == active_profile)
                .ok_or_else(|| {
                    ConfigError::Validation(format!(
                        "active provider profile `{active_profile}` does not exist"
                    ))
                })?;
            if !profile.enabled {
                return Err(ConfigError::Validation(format!(
                    "active provider profile `{active_profile}` is disabled"
                )));
            }
        }
        Ok(())
    }

    #[must_use]
    pub fn active_profile(&self) -> Option<&ProviderProfile> {
        self.active_profile
            .as_ref()
            .and_then(|name| self.profiles.iter().find(|profile| &profile.name == name))
    }

    pub fn upsert_profile(&mut self, profile: ProviderProfile, activate: bool) {
        let profile_name = profile.name.clone();
        if let Some(existing) = self
            .profiles
            .iter_mut()
            .find(|existing| existing.name == profile.name)
        {
            *existing = profile;
        } else {
            self.profiles.push(profile);
        }
        if activate {
            self.active_profile = Some(profile_name);
        }
    }

    pub fn set_active_profile(&mut self, name: impl Into<String>) -> Result<(), ConfigError> {
        let name = name.into();
        if self
            .profiles
            .iter()
            .any(|profile| profile.name == name && profile.enabled)
        {
            self.active_profile = Some(name);
            Ok(())
        } else {
            Err(ConfigError::Validation(format!(
                "provider profile `{name}` does not exist or is disabled"
            )))
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderProfile {
    pub name: String,
    pub protocol: ProviderProtocol,
    pub model_id: Option<String>,
    pub base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_ref: Option<CredentialRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oauth: Option<OAuthProviderDescriptor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation_config: Option<ProviderGenerationConfig>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub custom_headers: Vec<ProviderHeaderConfig>,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

impl ProviderProfile {
    #[must_use]
    pub fn mock() -> Self {
        Self {
            name: "mock".to_owned(),
            protocol: ProviderProtocol::Mock,
            model_id: Some("mock-model".to_owned()),
            base_url: None,
            credential_ref: None,
            oauth: None,
            generation_config: None,
            custom_headers: Vec::new(),
            enabled: true,
        }
    }

    pub fn openai_compatible(
        name: impl Into<String>,
        base_url: impl Into<String>,
        model_id: impl Into<String>,
        credential_ref: CredentialRef,
    ) -> Result<Self, ConfigError> {
        Self::live_profile(
            name,
            ProviderProtocol::OpenAiCompatible,
            base_url,
            model_id,
            credential_ref,
        )
    }

    pub fn live_profile(
        name: impl Into<String>,
        protocol: ProviderProtocol,
        base_url: impl Into<String>,
        model_id: impl Into<String>,
        credential_ref: CredentialRef,
    ) -> Result<Self, ConfigError> {
        let base_url = normalize_provider_base_url(protocol, &base_url.into())?;
        let profile = Self {
            name: name.into(),
            protocol,
            model_id: Some(model_id.into()),
            base_url: Some(base_url),
            credential_ref: Some(credential_ref),
            oauth: None,
            generation_config: None,
            custom_headers: Vec::new(),
            enabled: true,
        };
        profile.validate()?;
        Ok(profile)
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        validate_profile_name(&self.name)?;
        if let Some(credential_ref) = &self.credential_ref {
            credential_ref
                .validate()
                .map_err(|error| ConfigError::Validation(error.to_string()))?;
            if let CredentialSource::Environment { key } = &credential_ref.source {
                validate_env_key(key)?;
            }
        }
        if let Some(oauth) = &self.oauth {
            oauth
                .validate()
                .map_err(|error| ConfigError::Validation(error.to_string()))?;
            if self
                .credential_ref
                .as_ref()
                .is_none_or(|reference| reference.secret_kind != SecretKind::OAuthTokenSet)
            {
                return Err(ConfigError::Validation(
                    "oauth provider profile must reference an oauth-token-set credential"
                        .to_owned(),
                ));
            }
        }
        if self.enabled {
            validate_provider_protocol_runtime_supported(self.protocol)?;
        }
        if live_profile_requires_connection_fields(self.protocol) && self.enabled {
            require_non_empty(self.model_id.as_deref(), "model_id")?;
            require_non_empty(self.base_url.as_deref(), "base_url")?;
            if self.credential_ref.is_none() {
                return Err(ConfigError::Validation(
                    "provider profile requires credential_ref".to_owned(),
                ));
            }
            if self.protocol == ProviderProtocol::OpenAiCompatible {
                validate_openai_base_url(self.base_url.as_deref().unwrap_or_default())
                    .map_err(ConfigError::Validation)?;
            }
        }
        if let Some(generation_config) = &self.generation_config {
            generation_config
                .validate()
                .map_err(ConfigError::Validation)?;
        }
        let mut header_names = BTreeSet::new();
        for header in &self.custom_headers {
            header.validate().map_err(ConfigError::Validation)?;
            let normalized_name = header.name.to_ascii_lowercase();
            if !header_names.insert(normalized_name) {
                return Err(ConfigError::Validation(format!(
                    "provider header `{}` is configured more than once",
                    header.name
                )));
            }
            if let ProviderHeaderValue::Environment { key } = &header.value {
                validate_env_key(key)?;
            }
        }
        Ok(())
    }

    #[must_use]
    pub fn redacted(&self) -> Self {
        self.clone()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderInstallPlan {
    pub scope: ProviderConfigScope,
    pub profile: ProviderProfile,
    pub activate: bool,
    #[serde(skip, default)]
    pub pending_secret: Option<SecretString>,
}

impl ProviderInstallPlan {
    pub fn apply(&self, paths: &ProviderConfigPaths) -> Result<(), ConfigError> {
        self.profile.validate()?;
        if self.scope == ProviderConfigScope::Workspace {
            return Err(ConfigError::Validation(
                "workspace provider config is no longer supported; use global user provider config"
                    .to_owned(),
            ));
        }
        if self.pending_secret.is_some() {
            return Err(ConfigError::Validation(
                "provider plans containing a secret require verified async installation".to_owned(),
            ));
        }
        let path = &paths.user_config;
        let _lock = acquire_provider_settings_lock(path)?;
        let store = default_secret_store(paths)?;
        let mut settings =
            load_provider_settings_for_install_unlocked(paths, store.as_ref())?.settings;
        persist_profile_in_settings(&mut settings, self.profile.clone(), self.activate)?;
        settings.save_unlocked(path)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderOnboardingState {
    pub configured: bool,
    pub active_profile: Option<ProviderProfile>,
    pub missing_fields: Vec<String>,
    pub source: String,
}

#[derive(Clone)]
pub struct ProviderRuntimeEnv {
    values: BTreeMap<String, String>,
    credential: Option<Arc<dyn CredentialProvider>>,
}

impl std::fmt::Debug for ProviderRuntimeEnv {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProviderRuntimeEnv")
            .field("values", &self.redacted_values())
            .field("has_credential", &self.credential.is_some())
            .finish()
    }
}

impl ProviderRuntimeEnv {
    #[must_use]
    pub fn get(&self, key: &str) -> Option<String> {
        self.values.get(key).cloned().or_else(|| {
            std::env::var(key)
                .ok()
                .filter(|value| !value.trim().is_empty())
        })
    }

    #[must_use]
    pub fn redacted_values(&self) -> BTreeMap<String, String> {
        self.values
            .iter()
            .map(|(key, value)| {
                let normalized = key.to_ascii_uppercase();
                let redacted = if normalized == GOLUTRA_PROVIDER_CUSTOM_HEADERS
                    || ["API_KEY", "TOKEN", "SECRET", "PASSWORD", "AUTHORIZATION"]
                        .iter()
                        .any(|marker| normalized.contains(marker))
                {
                    "<redacted>".to_owned()
                } else {
                    value.clone()
                };
                (key.clone(), redacted)
            })
            .collect()
    }

    #[must_use]
    pub fn credential_provider(&self) -> Option<Arc<dyn CredentialProvider>> {
        self.credential.clone()
    }

    /// Applies a non-secret, per-run provider setting without mutating the
    /// persisted provider profile or replacing its credential provider.
    #[must_use]
    pub fn with_runtime_override(
        mut self,
        key: impl Into<String>,
        value: impl Into<String>,
    ) -> Self {
        self.values.insert(key.into(), value.into());
        self
    }
}

pub fn load_provider_runtime_env() -> Result<ProviderRuntimeEnv, ConfigError> {
    let paths = ProviderConfigPaths::global()?;
    load_provider_runtime_env_from_paths(&paths)
}

pub fn load_provider_runtime_env_from_paths(
    paths: &ProviderConfigPaths,
) -> Result<ProviderRuntimeEnv, ConfigError> {
    let store = default_secret_store(paths)?;
    load_provider_runtime_env_from_paths_with_store(paths, store)
}

pub fn load_provider_runtime_env_for_profile_from_paths(
    paths: &ProviderConfigPaths,
    profile_name: &str,
) -> Result<ProviderRuntimeEnv, ConfigError> {
    let store = default_secret_store(paths)?;
    let mut settings = load_provider_settings_with_store(paths, Arc::clone(&store))?;
    settings.set_active_profile(profile_name.to_owned())?;
    let auth = AuthService::new(paths.home.clone(), store)
        .map_err(|error| ConfigError::Validation(error.to_string()))?;
    runtime_env_from_settings(&settings, &auth)
}

pub fn load_provider_runtime_env_from_paths_with_store(
    paths: &ProviderConfigPaths,
    store: Arc<dyn SecretStore>,
) -> Result<ProviderRuntimeEnv, ConfigError> {
    let merged = load_provider_settings_with_store(paths, Arc::clone(&store))?;
    let auth = AuthService::new(paths.home.clone(), store)
        .map_err(|error| ConfigError::Validation(error.to_string()))?;
    runtime_env_from_settings(&merged, &auth)
}

pub fn provider_onboarding_state() -> Result<ProviderOnboardingState, ConfigError> {
    let paths = ProviderConfigPaths::global()?;
    let store = default_secret_store(&paths)?;
    provider_onboarding_state_with_store(&paths, store)
}

pub fn provider_onboarding_state_with_store(
    paths: &ProviderConfigPaths,
    store: Arc<dyn SecretStore>,
) -> Result<ProviderOnboardingState, ConfigError> {
    let user = load_provider_settings_with_store(paths, Arc::clone(&store))?;
    let source = if paths.user_config.exists() {
        "user"
    } else {
        "none"
    }
    .to_owned();
    let active_profile = user.active_profile().cloned();
    let missing_fields = match active_profile.as_ref() {
        Some(profile) => missing_fields(profile, store.as_ref())?,
        None => vec!["active_profile".to_owned()],
    };
    Ok(ProviderOnboardingState {
        configured: missing_fields.is_empty(),
        active_profile,
        missing_fields,
        source,
    })
}

pub fn load_merged_provider_settings(
    paths: &ProviderConfigPaths,
) -> Result<ProviderSettings, ConfigError> {
    load_provider_settings(paths)
}

pub fn load_provider_settings(
    paths: &ProviderConfigPaths,
) -> Result<ProviderSettings, ConfigError> {
    let store = default_secret_store(paths)?;
    load_provider_settings_with_store(paths, store)
}

pub fn load_provider_settings_with_store(
    paths: &ProviderConfigPaths,
    store: Arc<dyn SecretStore>,
) -> Result<ProviderSettings, ConfigError> {
    let _lock = acquire_provider_settings_lock(&paths.user_config)?;
    load_or_migrate_provider_settings_unlocked(paths, store.as_ref())
}

pub fn runtime_env_from_settings(
    settings: &ProviderSettings,
    auth: &AuthService,
) -> Result<ProviderRuntimeEnv, ConfigError> {
    let mut values = BTreeMap::new();
    let mut credential = None;
    if let Some(profile) = settings.active_profile() {
        values.insert(
            "GOLUTRA_PROVIDER_PROTOCOL".to_owned(),
            profile.protocol.id().to_owned(),
        );
        values.insert(
            "GOLUTRA_PROVIDER_MODE".to_owned(),
            if profile.protocol == ProviderProtocol::Mock {
                "mock".to_owned()
            } else {
                "live".to_owned()
            },
        );
        if let Some(model_id) = &profile.model_id {
            values.insert("GOLUTRA_PROVIDER_MODEL".to_owned(), model_id.clone());
        }
        if let Some(base_url) = &profile.base_url {
            values.insert("GOLUTRA_PROVIDER_BASE_URL".to_owned(), base_url.clone());
        }
        if let Some(generation_config) = &profile.generation_config
            && !generation_config.is_empty()
            && let Ok(value) = serde_json::to_string(generation_config)
        {
            values.insert(GOLUTRA_PROVIDER_GENERATION_CONFIG.to_owned(), value);
        }
        if !profile.custom_headers.is_empty() {
            let resolved_headers = profile
                .custom_headers
                .iter()
                .map(|header| {
                    let value = match &header.value {
                        ProviderHeaderValue::Literal { value } => value.clone(),
                        ProviderHeaderValue::Environment { key } => std::env::var(key)
                            .ok()
                            .filter(|value| !value.is_empty())
                            .ok_or_else(|| {
                                ConfigError::Validation(format!(
                                    "provider header environment key `{key}` is not set"
                                ))
                            })?,
                    };
                    Ok((header.name.clone(), value))
                })
                .collect::<Result<BTreeMap<_, _>, ConfigError>>()?;
            values.insert(
                GOLUTRA_PROVIDER_CUSTOM_HEADERS.to_owned(),
                serde_json::to_string(&resolved_headers)
                    .map_err(|error| ConfigError::Json(error.to_string()))?,
            );
        }
        if let Some(oauth) = &profile.oauth {
            values.insert(
                "GOLUTRA_PROVIDER_AUTH_PROVIDER".to_owned(),
                oauth.provider_id.clone(),
            );
        }
        if let Some(credential_ref) = &profile.credential_ref {
            let source_label = credential_ref.source_label();
            values.insert("GOLUTRA_PROVIDER_API_KEY_ENV".to_owned(), source_label);
            values.insert(
                "GOLUTRA_PROVIDER_API_KEY".to_owned(),
                "<resolved-credential>".to_owned(),
            );
            if let CredentialSource::Environment { key } = &credential_ref.source {
                values.insert(key.clone(), "<resolved-credential>".to_owned());
            }
            credential =
                Some(auth.credential_provider(credential_ref.clone(), profile.oauth.clone()));
        }
    }
    Ok(ProviderRuntimeEnv { values, credential })
}

#[must_use]
pub fn provider_protocol_has_runtime_adapter(protocol: ProviderProtocol) -> bool {
    matches!(
        protocol,
        ProviderProtocol::Mock
            | ProviderProtocol::OpenAiCompatible
            | ProviderProtocol::OpenAiResponses
            | ProviderProtocol::Anthropic
            | ProviderProtocol::Gemini
            | ProviderProtocol::VertexAi
            | ProviderProtocol::Genai
    )
}

pub fn validate_provider_protocol_runtime_supported(
    protocol: ProviderProtocol,
) -> Result<(), ConfigError> {
    if provider_protocol_has_runtime_adapter(protocol) {
        Ok(())
    } else {
        Err(ConfigError::Validation(format!(
            "provider protocol `{}` has no live adapter",
            protocol.id()
        )))
    }
}

pub async fn apply_provider_install_plan_verified(
    paths: &ProviderConfigPaths,
    workspace_root: impl AsRef<Path>,
    plan: &ProviderInstallPlan,
) -> Result<(), ProviderInstallError> {
    let store = default_secret_store(paths)
        .map_err(|error| provider_install_error("secret-store", error.to_string()))?;
    apply_provider_install_plan_verified_with_store(paths, workspace_root, plan, store).await
}

pub async fn apply_oauth_provider_install_plan_verified(
    paths: &ProviderConfigPaths,
    workspace_root: impl AsRef<Path>,
    plan: &ProviderInstallPlan,
) -> Result<(), ProviderInstallError> {
    let store = default_secret_store(paths)
        .map_err(|error| provider_install_error("secret-store", error.to_string()))?;
    apply_oauth_provider_install_plan_verified_with_store(paths, workspace_root, plan, store).await
}

pub async fn apply_oauth_provider_install_plan_verified_with_store(
    paths: &ProviderConfigPaths,
    workspace_root: impl AsRef<Path>,
    plan: &ProviderInstallPlan,
    store: Arc<dyn SecretStore>,
) -> Result<(), ProviderInstallError> {
    let descriptor = plan.profile.oauth.as_ref().ok_or_else(|| {
        provider_install_error("oauth", "OAuth provider plan requires a descriptor")
    })?;
    descriptor
        .validate()
        .map_err(|error| provider_install_error("oauth", error.to_string()))?;
    let reference = plan.profile.credential_ref.as_ref().ok_or_else(|| {
        provider_install_error(
            "oauth",
            "OAuth provider plan requires a credential reference",
        )
    })?;
    if reference.secret_kind != SecretKind::OAuthTokenSet {
        return Err(provider_install_error(
            "oauth",
            "OAuth provider plan requires an oauth-token-set credential",
        ));
    }
    if plan.pending_secret.is_some() {
        return Err(provider_install_error(
            "oauth",
            "OAuth provider plan cannot contain a pending API key",
        ));
    }
    let auth = AuthService::new(paths.home.clone(), Arc::clone(&store))
        .map_err(|error| provider_install_error("oauth", error.to_string()))?;
    let result = apply_provider_install_plan_verified_with_store(
        paths,
        workspace_root,
        plan,
        Arc::clone(&store),
    )
    .await;
    if let Err(error) = result {
        let cleanup = auth.logout(reference, Some(descriptor)).await;
        return match cleanup {
            Ok(_) => Err(error),
            Err(cleanup_error) => Err(provider_install_error(
                "rollback",
                format!(
                    "{}; OAuth credential cleanup failed: {cleanup_error}",
                    error.message
                ),
            )),
        };
    }
    Ok(())
}

pub async fn apply_provider_install_plan_verified_with_store(
    paths: &ProviderConfigPaths,
    workspace_root: impl AsRef<Path>,
    plan: &ProviderInstallPlan,
    store: Arc<dyn SecretStore>,
) -> Result<(), ProviderInstallError> {
    if plan.scope == ProviderConfigScope::Workspace {
        return Err(provider_install_error(
            "mutate",
            "workspace provider config is no longer supported; use global user provider config",
        ));
    }
    run_provider_install_transaction(paths, workspace_root, store, |user| {
        let previous_reference = user
            .profiles
            .iter()
            .find(|profile| profile.name == plan.profile.name)
            .and_then(|profile| profile.credential_ref.clone());
        plan.profile.validate()?;
        persist_profile_in_settings(user, plan.profile.clone(), plan.activate)?;
        let mut mutations = Vec::new();
        if let Some(secret) = &plan.pending_secret {
            let reference = plan.profile.credential_ref.clone().ok_or_else(|| {
                ConfigError::Validation(
                    "provider plan with pending secret requires credential_ref".to_owned(),
                )
            })?;
            mutations.push(SecretMutation {
                reference,
                action: SecretMutationAction::Set(secret.clone()),
            });
        }
        if let Some(reference) =
            replaced_credential(previous_reference, plan.profile.credential_ref.as_ref())?
        {
            mutations.push(SecretMutation {
                reference,
                action: SecretMutationAction::Delete,
            });
        }
        Ok(mutations)
    })
    .await
}

pub async fn update_provider_settings_verified<F>(
    paths: &ProviderConfigPaths,
    workspace_root: impl AsRef<Path>,
    mutate: F,
) -> Result<(), ProviderInstallError>
where
    F: FnOnce(&mut ProviderSettings) -> Result<(), ConfigError>,
{
    let store = default_secret_store(paths)
        .map_err(|error| provider_install_error("secret-store", error.to_string()))?;
    update_provider_settings_verified_with_store(paths, workspace_root, store, mutate).await
}

pub async fn update_provider_settings_verified_with_store<F>(
    paths: &ProviderConfigPaths,
    workspace_root: impl AsRef<Path>,
    store: Arc<dyn SecretStore>,
    mutate: F,
) -> Result<(), ProviderInstallError>
where
    F: FnOnce(&mut ProviderSettings) -> Result<(), ConfigError>,
{
    run_provider_settings_transaction(paths, workspace_root, store, move |settings| {
        mutate(settings)?;
        Ok(Vec::new())
    })
    .await
}

pub async fn replace_provider_credential_verified(
    paths: &ProviderConfigPaths,
    workspace_root: impl AsRef<Path>,
    profile_name: impl Into<String>,
    credential_ref: CredentialRef,
    secret: Option<SecretString>,
) -> Result<(), ProviderInstallError> {
    let store = default_secret_store(paths)
        .map_err(|error| provider_install_error("secret-store", error.to_string()))?;
    replace_provider_credential_verified_with_store(
        paths,
        workspace_root,
        profile_name,
        credential_ref,
        secret,
        store,
    )
    .await
}

pub async fn replace_provider_credential_verified_with_store(
    paths: &ProviderConfigPaths,
    workspace_root: impl AsRef<Path>,
    profile_name: impl Into<String>,
    credential_ref: CredentialRef,
    secret: Option<SecretString>,
    store: Arc<dyn SecretStore>,
) -> Result<(), ProviderInstallError> {
    let profile_name = profile_name.into();
    run_provider_settings_transaction(paths, workspace_root, store, move |settings| {
        let profile = settings
            .profiles
            .iter_mut()
            .find(|profile| profile.name == profile_name)
            .ok_or_else(|| {
                ConfigError::Validation(format!("provider profile `{profile_name}` does not exist"))
            })?;
        let previous_reference = profile.credential_ref.clone();
        profile.credential_ref = Some(credential_ref);
        profile.oauth = None;
        profile.enabled = true;
        let mut mutations = secret
            .map(|secret| SecretMutation {
                reference: profile
                    .credential_ref
                    .clone()
                    .expect("credential was assigned above"),
                action: SecretMutationAction::Set(secret),
            })
            .into_iter()
            .collect::<Vec<_>>();
        if let Some(reference) =
            replaced_credential(previous_reference, profile.credential_ref.as_ref())?
        {
            mutations.push(SecretMutation {
                reference,
                action: SecretMutationAction::Delete,
            });
        }
        Ok(mutations)
    })
    .await
}

pub async fn remove_provider_credential_verified(
    paths: &ProviderConfigPaths,
    workspace_root: impl AsRef<Path>,
    profile_name: impl Into<String>,
) -> Result<(), ProviderInstallError> {
    let store = default_secret_store(paths)
        .map_err(|error| provider_install_error("secret-store", error.to_string()))?;
    remove_provider_credential_verified_with_store(paths, workspace_root, profile_name, store).await
}

pub async fn remove_provider_credential_verified_with_store(
    paths: &ProviderConfigPaths,
    workspace_root: impl AsRef<Path>,
    profile_name: impl Into<String>,
    store: Arc<dyn SecretStore>,
) -> Result<(), ProviderInstallError> {
    let profile_name = profile_name.into();
    let settings = load_provider_settings_with_store(paths, Arc::clone(&store))
        .map_err(|error| provider_install_error("load", error.to_string()))?;
    let reference = settings
        .profiles
        .iter()
        .find(|profile| profile.name == profile_name)
        .and_then(|profile| profile.credential_ref.clone())
        .ok_or_else(|| {
            provider_install_error(
                "mutate",
                format!("provider profile `{profile_name}` has no credential"),
            )
        })?;
    remove_specific_provider_credential_verified_with_store(
        paths,
        workspace_root,
        profile_name,
        reference,
        store,
    )
    .await
}

async fn remove_specific_provider_credential_verified_with_store(
    paths: &ProviderConfigPaths,
    workspace_root: impl AsRef<Path>,
    profile_name: String,
    reference: CredentialRef,
    store: Arc<dyn SecretStore>,
) -> Result<(), ProviderInstallError> {
    let expected_reference = reference.clone();
    run_provider_settings_transaction(paths, workspace_root, store, move |settings| {
        let profile = settings
            .profiles
            .iter_mut()
            .find(|profile| profile.name == profile_name)
            .ok_or_else(|| {
                ConfigError::Validation(format!("provider profile `{profile_name}` does not exist"))
            })?;
        if profile.credential_ref.as_ref() != Some(&expected_reference) {
            return Err(ConfigError::Validation(format!(
                "provider profile `{profile_name}` credential changed during logout"
            )));
        }
        profile.credential_ref = None;
        profile.oauth = None;
        profile.enabled = false;
        if settings.active_profile.as_deref() == Some(profile_name.as_str()) {
            settings.active_profile = None;
        }
        Ok(vec![SecretMutation {
            reference,
            action: SecretMutationAction::Delete,
        }])
    })
    .await
}

pub async fn logout_provider_profile_verified(
    paths: &ProviderConfigPaths,
    workspace_root: impl AsRef<Path>,
    profile_name: impl Into<String>,
) -> Result<(), ProviderInstallError> {
    let store = default_secret_store(paths)
        .map_err(|error| provider_install_error("secret-store", error.to_string()))?;
    logout_provider_profile_verified_with_store(paths, workspace_root, profile_name, store).await
}

pub async fn logout_provider_profile_verified_with_store(
    paths: &ProviderConfigPaths,
    workspace_root: impl AsRef<Path>,
    profile_name: impl Into<String>,
    store: Arc<dyn SecretStore>,
) -> Result<(), ProviderInstallError> {
    let workspace_root = workspace_root.as_ref().to_path_buf();
    let profile_name = profile_name.into();
    let settings = load_provider_settings_with_store(paths, Arc::clone(&store))
        .map_err(|error| provider_install_error("load", error.to_string()))?;
    let profile = settings
        .profiles
        .iter()
        .find(|profile| profile.name == profile_name)
        .ok_or_else(|| {
            provider_install_error(
                "logout",
                format!("provider profile `{profile_name}` does not exist"),
            )
        })?;
    let reference = profile.credential_ref.clone().ok_or_else(|| {
        provider_install_error(
            "logout",
            format!("provider profile `{profile_name}` has no credential"),
        )
    })?;
    let descriptor = profile.oauth.clone();
    let auth = AuthService::new(paths.home.clone(), Arc::clone(&store))
        .map_err(|error| provider_install_error("oauth", error.to_string()))?;
    let revoke_result = auth.revoke(&reference, descriptor.as_ref()).await;
    remove_specific_provider_credential_verified_with_store(
        paths,
        workspace_root,
        profile_name,
        reference,
        store,
    )
    .await?;
    revoke_result.map_err(|error| provider_install_error("revoke", error.to_string()))
}

fn default_true() -> bool {
    true
}

fn validate_profile_name(value: &str) -> Result<(), ConfigError> {
    let valid = !value.trim().is_empty()
        && value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'));
    if valid {
        Ok(())
    } else {
        Err(ConfigError::Validation(
            "provider profile name must use ascii letters, numbers, dash or underscore".to_owned(),
        ))
    }
}

fn validate_env_key(value: &str) -> Result<(), ConfigError> {
    let normalized = value.trim();
    if DENIED_ENV_KEYS.contains(&normalized) {
        return Err(ConfigError::Validation(format!(
            "env key `{normalized}` is denied for provider credentials"
        )));
    }
    let valid = !normalized.is_empty()
        && normalized.chars().all(|character| {
            character.is_ascii_uppercase() || character.is_ascii_digit() || character == '_'
        });
    if valid {
        Ok(())
    } else {
        Err(ConfigError::Validation(
            "provider env key must be uppercase ascii with underscores".to_owned(),
        ))
    }
}

fn require_non_empty(value: Option<&str>, field: &str) -> Result<(), ConfigError> {
    if value.is_some_and(|value| !value.trim().is_empty()) {
        Ok(())
    } else {
        Err(ConfigError::Validation(format!(
            "provider profile requires `{field}`"
        )))
    }
}

fn live_profile_requires_connection_fields(protocol: ProviderProtocol) -> bool {
    protocol != ProviderProtocol::Mock
}

fn normalize_provider_base_url(
    protocol: ProviderProtocol,
    value: &str,
) -> Result<String, ConfigError> {
    if protocol == ProviderProtocol::OpenAiCompatible {
        validate_openai_base_url(value).map_err(ConfigError::Validation)
    } else {
        validate_native_base_url(value).map_err(ConfigError::Validation)
    }
}

fn missing_fields(
    profile: &ProviderProfile,
    store: &dyn SecretStore,
) -> Result<Vec<String>, ConfigError> {
    let mut fields = Vec::new();
    if live_profile_requires_connection_fields(profile.protocol) {
        if profile.model_id.as_deref().is_none_or(str::is_empty) {
            fields.push("model_id".to_owned());
        }
        if profile.base_url.as_deref().is_none_or(str::is_empty) {
            fields.push("base_url".to_owned());
        }
        let api_key_ready = profile
            .credential_ref
            .as_ref()
            .map(|reference| {
                store
                    .get(reference)
                    .map(|value| value.is_some())
                    .map_err(|error| ConfigError::Validation(error.to_string()))
            })
            .transpose()?
            .unwrap_or(false);
        if !api_key_ready {
            fields.push("api_key".to_owned());
        }
        for header in &profile.custom_headers {
            if let ProviderHeaderValue::Environment { key } = &header.value
                && std::env::var(key).ok().is_none_or(|value| value.is_empty())
            {
                fields.push(format!("header_env:{key}"));
            }
        }
    }
    Ok(fields)
}

fn normalize_env_segment(value: &str) -> String {
    let mut result = String::new();
    let mut previous_was_underscore = false;
    for character in value.trim().chars().flat_map(char::to_uppercase) {
        if character.is_ascii_alphanumeric() {
            result.push(character);
            previous_was_underscore = false;
        } else if !previous_was_underscore {
            result.push('_');
            previous_was_underscore = true;
        }
    }
    result.trim_matches('_').to_owned()
}

fn strip_trailing_slashes(value: &str) -> &str {
    let mut end = value.len();
    while end > 0 && value.as_bytes()[end - 1] == b'/' {
        end -= 1;
    }
    &value[..end]
}

#[cfg(test)]
mod tests;
