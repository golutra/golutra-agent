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
    ConfiguredProvider, ModelCatalog, ProviderGenerationConfig, ProviderProtocol,
    validate_native_base_url, validate_openai_base_url,
};
use secrecy::SecretString;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

mod provider_auth;

pub use provider_auth::{
    BuiltinOAuthMethod, builtin_oauth_method, builtin_oauth_methods,
    builtin_oauth_methods_for_provider,
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
        let mut settings = load_or_migrate_provider_settings_unlocked(paths, store.as_ref())?;
        persist_profile_in_settings(&mut settings, self.profile.clone(), self.activate)?;
        settings.save_unlocked(path)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProviderSettingsSnapshot {
    existed: bool,
    settings: ProviderSettings,
}

#[derive(Clone)]
enum SecretMutationAction {
    Set(SecretString),
    Delete,
}

#[derive(Clone)]
struct SecretMutation {
    reference: CredentialRef,
    action: SecretMutationAction,
}

struct SecretSnapshot {
    reference: CredentialRef,
    value: Option<SecretString>,
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
                let redacted = if ["API_KEY", "TOKEN", "SECRET", "PASSWORD", "AUTHORIZATION"]
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
    run_provider_settings_transaction(paths, workspace_root, store, |user| {
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

pub fn provider_auth_service(paths: &ProviderConfigPaths) -> Result<AuthService, ConfigError> {
    let store = default_secret_store(paths)?;
    AuthService::new(paths.home.clone(), store)
        .map_err(|error| ConfigError::Validation(error.to_string()))
}

#[must_use]
pub fn generate_custom_provider_api_key_env(
    protocol: ProviderProtocol,
    base_url: impl AsRef<str>,
) -> String {
    let canonical_base_url = strip_trailing_slashes(base_url.as_ref().trim());
    // 协议和 endpoint 共同参与 hash，避免规范化后的可读字段碰撞覆盖凭据。
    let mut hasher = Sha256::new();
    hasher.update(protocol.id().as_bytes());
    hasher.update([0]);
    hasher.update(canonical_base_url.as_bytes());
    let digest = hasher.finalize();
    let suffix = digest
        .iter()
        .take(6)
        .map(|byte| format!("{byte:02X}"))
        .collect::<String>();

    format!(
        "{}{}_{}_{}",
        CUSTOM_PROVIDER_API_KEY_ENV_PREFIX,
        normalize_env_segment(protocol.id()),
        normalize_env_segment(base_url.as_ref()),
        suffix
    )
}

pub fn golutra_home() -> Result<PathBuf, ConfigError> {
    if let Some(value) = std::env::var_os(GOLUTRA_HOME_ENV) {
        if value.is_empty() {
            return Err(ConfigError::Validation(
                "GOLUTRA_HOME cannot be empty".to_owned(),
            ));
        }
        return Ok(PathBuf::from(value));
    }
    let home = std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .ok_or_else(|| ConfigError::Validation("HOME is not set".to_owned()))?;
    Ok(PathBuf::from(home).join(".golutra"))
}

fn default_secret_store(paths: &ProviderConfigPaths) -> Result<Arc<dyn SecretStore>, ConfigError> {
    DefaultSecretStore::new(paths.home.clone())
        .map(|store| Arc::new(store) as Arc<dyn SecretStore>)
        .map_err(|error| ConfigError::Validation(error.to_string()))
}

#[derive(Deserialize)]
struct LegacyProviderSettings {
    version: u32,
    active_profile: Option<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    profiles: Vec<LegacyProviderProfile>,
}

#[derive(Deserialize)]
struct LegacyProviderProfile {
    name: String,
    protocol: ProviderProtocol,
    model_id: Option<String>,
    base_url: Option<String>,
    api_key_env: Option<String>,
    #[serde(default)]
    generation_config: Option<ProviderGenerationConfig>,
    #[serde(default = "default_true")]
    enabled: bool,
}

fn load_or_migrate_provider_settings_unlocked(
    paths: &ProviderConfigPaths,
    store: &dyn SecretStore,
) -> Result<ProviderSettings, ConfigError> {
    let content = match fs::read_to_string(&paths.user_config) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ProviderSettings::default());
        }
        Err(error) => return Err(ConfigError::Io(error.to_string())),
    };
    let version = serde_json::from_str::<serde_json::Value>(&content)
        .map_err(|error| ConfigError::Json(error.to_string()))?
        .get("version")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| {
            ConfigError::Validation("provider settings version is missing".to_owned())
        })?;
    match u32::try_from(version).ok() {
        Some(PROVIDER_SETTINGS_VERSION) => {
            let settings: ProviderSettings = serde_json::from_str(&content)
                .map_err(|error| ConfigError::Json(error.to_string()))?;
            settings.validate()?;
            Ok(settings)
        }
        Some(1) => {
            let legacy: LegacyProviderSettings = serde_json::from_str(&content)
                .map_err(|error| ConfigError::Json(error.to_string()))?;
            migrate_legacy_provider_settings(paths, store, legacy)
        }
        _ => Err(ConfigError::Validation(format!(
            "unsupported provider settings version {version}"
        ))),
    }
}

fn migrate_legacy_provider_settings(
    paths: &ProviderConfigPaths,
    store: &dyn SecretStore,
    legacy: LegacyProviderSettings,
) -> Result<ProviderSettings, ConfigError> {
    if legacy.version != 1 {
        return Err(ConfigError::Validation(format!(
            "unsupported legacy provider settings version {}",
            legacy.version
        )));
    }
    let mut secret_snapshots = Vec::new();
    let migration_result = (|| {
        let profiles = legacy
            .profiles
            .into_iter()
            .map(|profile| {
                let credential_ref = if profile.protocol == ProviderProtocol::Mock {
                    None
                } else {
                    let env_key = profile.api_key_env.as_deref().ok_or_else(|| {
                        ConfigError::Validation(format!(
                            "legacy provider profile `{}` has no api_key_env",
                            profile.name
                        ))
                    })?;
                    validate_env_key(env_key)?;
                    if let Some(value) = legacy
                        .env
                        .get(env_key)
                        .filter(|value| !value.trim().is_empty())
                    {
                        let reference =
                            migrated_credential_ref(&paths.home, &profile.name, env_key)?;
                        secret_snapshots.push(SecretSnapshot {
                            value: store
                                .get(&reference)
                                .map_err(|error| ConfigError::Validation(error.to_string()))?,
                            reference: reference.clone(),
                        });
                        store
                            .set(&reference, &SecretString::from(value.clone()))
                            .map_err(|error| ConfigError::Validation(error.to_string()))?;
                        Some(reference)
                    } else {
                        Some(
                            CredentialRef::environment(env_key, SecretKind::ApiKey)
                                .map_err(|error| ConfigError::Validation(error.to_string()))?,
                        )
                    }
                };
                Ok(ProviderProfile {
                    name: profile.name,
                    protocol: profile.protocol,
                    model_id: profile.model_id,
                    base_url: profile.base_url,
                    credential_ref,
                    oauth: None,
                    generation_config: profile.generation_config,
                    enabled: profile.enabled,
                })
            })
            .collect::<Result<Vec<_>, ConfigError>>()?;
        let settings = ProviderSettings {
            version: PROVIDER_SETTINGS_VERSION,
            active_profile: legacy.active_profile,
            profiles,
        };
        settings.validate()?;
        settings.save_unlocked(&paths.user_config)?;
        Ok(settings)
    })();

    if migration_result.is_err() {
        restore_secret_snapshots_sync(store, &secret_snapshots)?;
    }
    migration_result
}

fn migrated_credential_ref(
    home: &Path,
    profile_name: &str,
    env_key: &str,
) -> Result<CredentialRef, ConfigError> {
    let mut hasher = Sha256::new();
    hasher.update(home.to_string_lossy().as_bytes());
    hasher.update([0]);
    hasher.update(profile_name.as_bytes());
    hasher.update([0]);
    hasher.update(env_key.as_bytes());
    let suffix = hasher
        .finalize()
        .iter()
        .take(8)
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    CredentialRef::with_id(
        format!("cred_migrated_{suffix}"),
        CredentialSource::Disk,
        SecretKind::ApiKey,
    )
    .map_err(|error| ConfigError::Validation(error.to_string()))
}

async fn snapshot_secrets(
    store: Arc<dyn SecretStore>,
    mutations: &[SecretMutation],
) -> Result<Vec<SecretSnapshot>, ProviderInstallError> {
    let references = mutations
        .iter()
        .map(|mutation| mutation.reference.clone())
        .collect::<Vec<_>>();
    tokio::task::spawn_blocking(move || {
        let mut seen = BTreeSet::new();
        let mut snapshots = Vec::new();
        for reference in references {
            if seen.insert(reference.id.clone()) {
                snapshots.push(SecretSnapshot {
                    value: store.get(&reference).map_err(|error| {
                        provider_install_error("secret-backup", error.to_string())
                    })?,
                    reference,
                });
            }
        }
        Ok(snapshots)
    })
    .await
    .map_err(|error| provider_install_error("secret-backup", error.to_string()))?
}

async fn apply_secret_mutations(
    store: Arc<dyn SecretStore>,
    mutations: &[SecretMutation],
) -> Result<(), ProviderInstallError> {
    let mutations = mutations.to_vec();
    tokio::task::spawn_blocking(move || {
        for mutation in mutations {
            match mutation.action {
                SecretMutationAction::Set(secret) => store
                    .set(&mutation.reference, &secret)
                    .map_err(|error| provider_install_error("secret-write", error.to_string()))?,
                SecretMutationAction::Delete => {
                    store.delete(&mutation.reference).map_err(|error| {
                        provider_install_error("secret-delete", error.to_string())
                    })?;
                }
            }
        }
        Ok(())
    })
    .await
    .map_err(|error| provider_install_error("secret-write", error.to_string()))?
}

async fn restore_secret_snapshots(
    store: Arc<dyn SecretStore>,
    snapshots: &[SecretSnapshot],
) -> Result<(), ConfigError> {
    let snapshots = snapshots
        .iter()
        .map(|snapshot| SecretSnapshot {
            reference: snapshot.reference.clone(),
            value: snapshot.value.clone(),
        })
        .collect::<Vec<_>>();
    tokio::task::spawn_blocking(move || restore_secret_snapshots_sync(store.as_ref(), &snapshots))
        .await
        .map_err(|error| ConfigError::Io(error.to_string()))?
}

fn restore_secret_snapshots_sync(
    store: &dyn SecretStore,
    snapshots: &[SecretSnapshot],
) -> Result<(), ConfigError> {
    for snapshot in snapshots.iter().rev() {
        match &snapshot.value {
            Some(secret) => store
                .set(&snapshot.reference, secret)
                .map_err(|error| ConfigError::Validation(error.to_string()))?,
            None => {
                store
                    .delete(&snapshot.reference)
                    .map_err(|error| ConfigError::Validation(error.to_string()))?;
            }
        }
    }
    Ok(())
}

async fn run_provider_settings_transaction<F>(
    paths: &ProviderConfigPaths,
    workspace_root: impl AsRef<Path>,
    store: Arc<dyn SecretStore>,
    mutate: F,
) -> Result<(), ProviderInstallError>
where
    F: FnOnce(&mut ProviderSettings) -> Result<Vec<SecretMutation>, ConfigError>,
{
    let workspace_root = workspace_root.as_ref().to_path_buf();
    let _lock = acquire_provider_settings_lock_async(paths.user_config.clone()).await?;
    let initial_settings = load_or_migrate_provider_settings_unlocked(paths, store.as_ref())
        .map_err(|error| provider_install_error("load", error.to_string()))?;
    let user_snapshot = snapshot_provider_settings_file(&paths.user_config, initial_settings)
        .map_err(|error| provider_install_error("backup", error.to_string()))?;
    let mut user_settings = user_snapshot.settings.clone();
    let secret_mutations = mutate(&mut user_settings)
        .map_err(|error| provider_install_error("mutate", error.to_string()))?;
    user_settings
        .validate()
        .map_err(|error| provider_install_error("mutate", error.to_string()))?;
    let secret_snapshots = snapshot_secrets(Arc::clone(&store), &secret_mutations).await?;

    let transaction_result = async {
        apply_secret_mutations(Arc::clone(&store), &secret_mutations).await?;
        persist_provider_settings_file(&paths.user_config, &user_snapshot, &user_settings)
            .map_err(|error| provider_install_error("persist", error.to_string()))?;

        probe_provider_after_settings_update(
            &workspace_root,
            &paths.home,
            &user_settings,
            Arc::clone(&store),
        )
        .await
    }
    .await;

    if let Err(error) = transaction_result {
        let config_restore = restore_provider_settings_file(&paths.user_config, &user_snapshot);
        let secret_restore = restore_secret_snapshots(Arc::clone(&store), &secret_snapshots).await;
        if let Err(restore) = config_restore {
            return Err(provider_install_error(
                "rollback",
                format!("{}; config rollback failed: {restore}", error.message),
            ));
        }
        if let Err(restore) = secret_restore {
            return Err(provider_install_error(
                "rollback",
                format!("{}; secret rollback failed: {restore}", error.message),
            ));
        }
        return Err(error);
    }

    Ok(())
}

fn snapshot_provider_settings_file(
    path: &Path,
    settings: ProviderSettings,
) -> Result<ProviderSettingsSnapshot, ConfigError> {
    Ok(ProviderSettingsSnapshot {
        existed: path.exists(),
        settings,
    })
}

fn persist_provider_settings_file(
    path: &Path,
    snapshot: &ProviderSettingsSnapshot,
    settings: &ProviderSettings,
) -> Result<(), ConfigError> {
    if snapshot.settings == *settings {
        return Ok(());
    }
    if !snapshot.existed && *settings == ProviderSettings::default() {
        return Ok(());
    }
    if snapshot.existed && *settings == ProviderSettings::default() {
        fs::remove_file(path).map_err(|error| ConfigError::Io(error.to_string()))?;
        sync_directory(normalized_parent(path))?;
        return Ok(());
    }
    settings.save_unlocked(path)
}

fn restore_provider_settings_file(
    path: &Path,
    snapshot: &ProviderSettingsSnapshot,
) -> Result<(), ConfigError> {
    if !snapshot.existed {
        if path.exists() {
            fs::remove_file(path).map_err(|error| ConfigError::Io(error.to_string()))?;
            sync_directory(normalized_parent(path))?;
        }
        return Ok(());
    }
    snapshot.settings.save_unlocked(path)
}

async fn acquire_provider_settings_lock_async(path: PathBuf) -> Result<File, ProviderInstallError> {
    tokio::task::spawn_blocking(move || acquire_provider_settings_lock(&path))
        .await
        .map_err(|error| provider_install_error("lock", error.to_string()))?
        .map_err(|error| provider_install_error("lock", error.to_string()))
}

fn acquire_provider_settings_lock(path: &Path) -> Result<File, ConfigError> {
    let parent = normalized_parent(path);
    fs::create_dir_all(parent).map_err(|error| ConfigError::Io(error.to_string()))?;
    set_owner_only_dir(parent)?;
    let lock_path = path.with_extension("lock");
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)
        .map_err(|error| ConfigError::Io(error.to_string()))?;
    set_owner_only_file(&lock_path)?;
    file.lock_exclusive()
        .map_err(|error| ConfigError::Io(error.to_string()))?;
    Ok(file)
}

async fn probe_provider_after_settings_update(
    workspace_root: &Path,
    home: &Path,
    user_settings: &ProviderSettings,
    store: Arc<dyn SecretStore>,
) -> Result<(), ProviderInstallError> {
    let Some(active_profile) = user_settings.active_profile() else {
        return Ok(());
    };
    if active_profile.protocol == ProviderProtocol::Mock {
        return Ok(());
    }
    let auth = AuthService::new(home.to_path_buf(), store)
        .map_err(|error| provider_install_error("resolve", error.to_string()))?;
    let runtime_env = runtime_env_from_settings(user_settings, &auth)
        .map_err(|error| provider_install_error("resolve", error.to_string()))?;
    ConfiguredProvider::probe_from_reader_with_credential(
        |key| runtime_env.get(key),
        runtime_env.credential_provider(),
    )
    .await
    .map_err(|error| {
        provider_install_error(
            "probe",
            format!(
                "provider probe failed for workspace {}: {error}",
                workspace_root.display()
            ),
        )
    })?;
    Ok(())
}

fn provider_install_error(step: &'static str, message: impl Into<String>) -> ProviderInstallError {
    ProviderInstallError {
        step,
        message: message.into(),
    }
}

fn persist_profile_in_settings(
    settings: &mut ProviderSettings,
    profile: ProviderProfile,
    activate: bool,
) -> Result<(), ConfigError> {
    profile.validate()?;
    settings.upsert_profile(profile, activate);
    Ok(())
}

fn replaced_credential(
    previous: Option<CredentialRef>,
    current: Option<&CredentialRef>,
) -> Result<Option<CredentialRef>, ConfigError> {
    let Some(previous) = previous else {
        return Ok(None);
    };
    let Some(current) = current else {
        return Ok(Some(previous));
    };
    if previous.id != current.id {
        return Ok(Some(previous));
    }
    if previous != *current {
        return Err(ConfigError::Validation(format!(
            "credential `{}` metadata cannot change in place",
            previous.id
        )));
    }
    Ok(None)
}

fn write_json_owner_only<T: Serialize>(path: &Path, value: &T) -> Result<(), ConfigError> {
    let parent = normalized_parent(path);
    fs::create_dir_all(parent).map_err(|error| ConfigError::Io(error.to_string()))?;
    set_owner_only_dir(parent)?;
    let content = serde_json::to_string_pretty(value)
        .map_err(|error| ConfigError::Json(error.to_string()))?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)
        .map_err(|error| ConfigError::Io(error.to_string()))?;
    temporary
        .write_all(format!("{content}\n").as_bytes())
        .map_err(|error| ConfigError::Io(error.to_string()))?;
    temporary
        .as_file()
        .sync_all()
        .map_err(|error| ConfigError::Io(error.to_string()))?;
    set_owner_only_file(temporary.path())?;
    temporary
        .as_file()
        .sync_all()
        .map_err(|error| ConfigError::Io(error.to_string()))?;
    temporary
        .persist(path)
        .map_err(|error| ConfigError::Io(error.error.to_string()))?;
    set_owner_only_file(path)?;
    File::open(path)
        .and_then(|file| file.sync_all())
        .map_err(|error| ConfigError::Io(error.to_string()))?;
    sync_directory(parent)
}

fn normalized_parent(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<(), ConfigError> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| ConfigError::Io(error.to_string()))
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> Result<(), ConfigError> {
    Ok(())
}

#[cfg(unix)]
fn set_owner_only_file(path: &Path) -> Result<(), ConfigError> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .map_err(|error| ConfigError::Io(error.to_string()))
}

#[cfg(not(unix))]
fn set_owner_only_file(_path: &Path) -> Result<(), ConfigError> {
    Ok(())
}

#[cfg(unix)]
fn set_owner_only_dir(path: &Path) -> Result<(), ConfigError> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(|error| ConfigError::Io(error.to_string()))
}

#[cfg(not(unix))]
fn set_owner_only_dir(_path: &Path) -> Result<(), ConfigError> {
    Ok(())
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
mod tests {
    use std::{
        ffi::OsString,
        sync::{Mutex, MutexGuard},
    };

    use secrecy::ExposeSecret;
    use tempfile::{TempDir, tempdir};
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    use super::*;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn lock_test_env() -> MutexGuard<'static, ()> {
        ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    struct IsolatedGolutraHome {
        previous: Option<OsString>,
        _dir: TempDir,
        _guard: MutexGuard<'static, ()>,
    }

    impl IsolatedGolutraHome {
        fn new() -> Self {
            let guard = lock_test_env();
            let dir = tempdir().expect("home");
            let previous = std::env::var_os(GOLUTRA_HOME_ENV);
            unsafe {
                std::env::set_var(GOLUTRA_HOME_ENV, dir.path());
            }
            Self {
                previous,
                _dir: dir,
                _guard: guard,
            }
        }
    }

    struct ScopedEnvVar {
        key: &'static str,
        previous: Option<OsString>,
    }

    impl ScopedEnvVar {
        fn set(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
            let previous = std::env::var_os(key);
            unsafe {
                std::env::set_var(key, value);
            }
            Self { key, previous }
        }
    }

    impl Drop for ScopedEnvVar {
        fn drop(&mut self) {
            match &self.previous {
                Some(value) => unsafe {
                    std::env::set_var(self.key, value);
                },
                None => unsafe {
                    std::env::remove_var(self.key);
                },
            }
        }
    }

    impl Drop for IsolatedGolutraHome {
        fn drop(&mut self) {
            match &self.previous {
                Some(value) => unsafe {
                    std::env::set_var(GOLUTRA_HOME_ENV, value);
                },
                None => unsafe {
                    std::env::remove_var(GOLUTRA_HOME_ENV);
                },
            }
        }
    }

    async fn spawn_probe_server(body: &'static str) -> String {
        spawn_probe_server_response("200 OK", body).await
    }

    async fn spawn_probe_server_response(status: &'static str, body: &'static str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            let mut buffer = [0_u8; 2048];
            let _ = stream.read(&mut buffer).await.expect("read request");
            let response = format!(
                "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream
                .write_all(response.as_bytes())
                .await
                .expect("write response");
        });
        format!("http://{address}/v1")
    }

    fn env_credential(key: &str) -> CredentialRef {
        CredentialRef::environment(key, SecretKind::ApiKey).expect("credential")
    }

    fn disk_credential() -> CredentialRef {
        CredentialRef::disk(SecretKind::ApiKey)
    }

    fn oauth_credential() -> CredentialRef {
        CredentialRef::ephemeral(SecretKind::OAuthTokenSet)
    }

    fn oauth_descriptor() -> OAuthProviderDescriptor {
        OAuthProviderDescriptor {
            provider_id: "test-provider".to_owned(),
            client_id: "test-client".to_owned(),
            authorization_endpoint: "https://auth.example.com/authorize".to_owned(),
            token_endpoint: "https://auth.example.com/token".to_owned(),
            device_authorization_endpoint: None,
            revocation_endpoint: None,
            scopes: vec!["model.invoke".to_owned()],
            audience: None,
            browser_redirect_uri: None,
            authorization_params: std::collections::BTreeMap::new(),
            authorization_nonce: false,
            openai_device_authorization: None,
            flows: vec![golutra_auth::OAuthFlow::BrowserPkce],
        }
    }

    fn store_test_oauth_credential(
        store: &dyn SecretStore,
        reference: &CredentialRef,
        access_token: &str,
    ) {
        let secret = SecretString::from(
            serde_json::json!({
                "revision": "oauth_test_revision",
                "refresh_token": "refresh-test",
                "access_token": access_token,
                "expires_at": null,
                "token_type": "Bearer",
                "scopes": ["model.invoke"]
            })
            .to_string(),
        );
        store.set(reference, &secret).expect("OAuth token set");
    }

    #[test]
    fn runtime_config_roundtrips() {
        let dir = tempdir().expect("dir");
        let path = dir.path().join("golutra.json");
        let config = RuntimeConfig::p1_default();

        config.save(&path).expect("save");
        let loaded = RuntimeConfig::load(&path).expect("load");

        assert_eq!(loaded.protocol_version, "v0.1");
        assert!(loaded.model_catalog.route_default().is_some());
    }

    #[test]
    fn empty_golutra_home_is_rejected() {
        let _guard = lock_test_env();
        let _home = ScopedEnvVar::set(GOLUTRA_HOME_ENV, "");

        let error = golutra_home().expect_err("empty home must be rejected");

        assert!(error.to_string().contains("cannot be empty"));
    }

    #[test]
    fn provider_settings_v2_persists_only_credential_reference() {
        let _home = IsolatedGolutraHome::new();
        let paths = ProviderConfigPaths::global().expect("paths");
        let profile = ProviderProfile::openai_compatible(
            "golutra",
            "api.golutra.cn",
            "gpt-test",
            env_credential("GOLUTRA_PROVIDER_API_KEY"),
        )
        .expect("profile");
        ProviderInstallPlan {
            scope: ProviderConfigScope::User,
            profile,
            activate: true,
            pending_secret: None,
        }
        .apply(&paths)
        .expect("install");

        let loaded = ProviderSettings::load(&paths.user_config).expect("load");
        let serialized = fs::read_to_string(&paths.user_config).expect("serialized");

        assert_eq!(loaded.version, PROVIDER_SETTINGS_VERSION);
        assert!(serialized.contains("credential_ref"));
        assert!(!serialized.contains("secret-value"));
    }

    #[test]
    fn provider_settings_reject_shared_credential_ids() {
        let reference = disk_credential();
        let first = ProviderProfile::openai_compatible(
            "first",
            "https://api.example.com/v1",
            "model-first",
            reference.clone(),
        )
        .expect("first profile");
        let second = ProviderProfile::openai_compatible(
            "second",
            "https://api.example.com/v1",
            "model-second",
            reference,
        )
        .expect("second profile");
        let settings = ProviderSettings {
            version: PROVIDER_SETTINGS_VERSION,
            active_profile: Some("first".to_owned()),
            profiles: vec![first, second],
        };

        let error = settings.validate().expect_err("shared ref rejected");

        assert!(error.to_string().contains("multiple provider profiles"));
    }

    #[test]
    fn concurrent_global_provider_updates_preserve_both_profiles() {
        let _home = IsolatedGolutraHome::new();
        let paths = ProviderConfigPaths::global().expect("paths");
        let plan = |name: &str| {
            let mut profile = ProviderProfile::mock();
            profile.name = name.to_owned();
            ProviderInstallPlan {
                scope: ProviderConfigScope::User,
                profile,
                activate: false,
                pending_secret: None,
            }
        };
        let first_paths = paths.clone();
        let second_paths = paths.clone();
        std::thread::scope(|scope| {
            scope.spawn(move || plan("first").apply(&first_paths).expect("first update"));
            scope.spawn(move || plan("second").apply(&second_paths).expect("second update"));
        });

        let settings = ProviderSettings::load(&paths.user_config).expect("settings");
        let names = settings
            .profiles
            .iter()
            .map(|profile| profile.name.as_str())
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(names, std::collections::HashSet::from(["first", "second"]));
    }

    #[test]
    fn legacy_env_map_migrates_shared_keys_to_disk_secret_store_and_v2_references() {
        let dir = tempdir().expect("dir");
        let paths = ProviderConfigPaths::from_home(dir.path()).expect("paths");
        fs::write(
            &paths.user_config,
            r#"{
  "version": 1,
  "active_profile": "golutra",
  "env": {"GOLUTRA_PROVIDER_API_KEY": "legacy-secret"},
  "profiles": [
    {
      "name": "golutra",
      "protocol": "openai-compatible",
      "model_id": "gpt-test",
      "base_url": "https://api.golutra.cn/v1",
      "api_key_env": "GOLUTRA_PROVIDER_API_KEY",
      "enabled": true
    },
    {
      "name": "backup",
      "protocol": "openai-compatible",
      "model_id": "gpt-backup",
      "base_url": "https://backup.golutra.cn/v1",
      "api_key_env": "GOLUTRA_PROVIDER_API_KEY",
      "enabled": true
    }
  ]
}
"#,
        )
        .expect("write");

        let loaded = load_provider_settings(&paths).expect("migrate");
        let references = loaded
            .profiles
            .iter()
            .map(|profile| profile.credential_ref.as_ref().expect("credential ref"))
            .collect::<Vec<_>>();
        let store = DefaultSecretStore::new(&paths.home).expect("disk store");

        assert_eq!(loaded.version, PROVIDER_SETTINGS_VERSION);
        assert_eq!(references.len(), 2);
        assert_ne!(references[0].id, references[1].id);
        for reference in references {
            assert_eq!(reference.source, CredentialSource::Disk);
            assert_eq!(
                store
                    .get(reference)
                    .expect("read")
                    .expect("secret")
                    .expose_secret(),
                "legacy-secret"
            );
        }
        assert!(
            paths
                .home
                .join(golutra_auth::CREDENTIALS_FILE_NAME)
                .is_file()
        );
        assert!(
            !fs::read_to_string(&paths.user_config)
                .expect("v2")
                .contains("legacy-secret")
        );
    }

    #[test]
    fn provider_profile_serialization_contains_no_secret_field() {
        let profile = ProviderProfile::openai_compatible(
            "golutra",
            "api.golutra.cn",
            "gpt-test",
            disk_credential(),
        )
        .expect("profile");

        let serialized = serde_json::to_string(&profile).expect("serialize");
        assert!(!serialized.contains("api_key"));
        assert!(serialized.contains("credential_ref"));
    }

    #[test]
    fn workspace_provider_config_scope_is_rejected() {
        let _home = IsolatedGolutraHome::new();
        let paths = ProviderConfigPaths::global().expect("paths");
        let profile = ProviderProfile::openai_compatible(
            "golutra",
            "api.golutra.cn",
            "gpt-test",
            env_credential("GOLUTRA_PROVIDER_API_KEY"),
        )
        .expect("profile");
        let plan = ProviderInstallPlan {
            scope: ProviderConfigScope::Workspace,
            profile,
            activate: true,
            pending_secret: None,
        };

        let error = plan.apply(&paths).expect_err("workspace scope rejected");

        assert!(matches!(error, ConfigError::Validation(_)));
        assert!(
            error
                .to_string()
                .contains("workspace provider config is no longer supported")
        );
    }

    #[test]
    fn custom_provider_api_key_env_is_stable_and_disambiguated() {
        let without_trailing_slash = generate_custom_provider_api_key_env(
            ProviderProtocol::OpenAiCompatible,
            "https://api.example.com/v1",
        );
        let with_trailing_slash = generate_custom_provider_api_key_env(
            ProviderProtocol::OpenAiCompatible,
            "https://api.example.com/v1/",
        );
        let different_protocol = generate_custom_provider_api_key_env(
            ProviderProtocol::Anthropic,
            "https://api.example.com/v1",
        );
        let different_url = generate_custom_provider_api_key_env(
            ProviderProtocol::OpenAiCompatible,
            "https://other.example.com/v1",
        );

        assert_eq!(without_trailing_slash, with_trailing_slash);
        assert!(without_trailing_slash.starts_with(CUSTOM_PROVIDER_API_KEY_ENV_PREFIX));
        assert!(without_trailing_slash.contains("OPENAI_COMPATIBLE_HTTPS_API_EXAMPLE_COM_V1"));
        assert_ne!(without_trailing_slash, different_protocol);
        assert_ne!(without_trailing_slash, different_url);
    }

    #[test]
    fn provider_install_plan_accepts_native_live_protocols() {
        let _home = IsolatedGolutraHome::new();
        let paths = ProviderConfigPaths::global().expect("paths");
        let profile = ProviderProfile {
            name: "anthropic".to_owned(),
            protocol: ProviderProtocol::Anthropic,
            model_id: Some("claude-sonnet-4".to_owned()),
            base_url: Some("https://api.anthropic.com/v1".to_owned()),
            credential_ref: Some(env_credential("GOLUTRA_PROVIDER_API_KEY")),
            oauth: None,
            generation_config: None,
            enabled: true,
        };
        let plan = ProviderInstallPlan {
            scope: ProviderConfigScope::User,
            profile,
            activate: true,
            pending_secret: None,
        };

        plan.apply(&paths).expect("native protocol installed");
        let settings = ProviderSettings::load(&paths.user_config).expect("settings");
        assert_eq!(
            settings.active_profile().expect("profile").protocol,
            ProviderProtocol::Anthropic
        );
    }

    #[test]
    fn provider_profile_rejects_an_invalid_or_credentialed_base_url() {
        for base_url in [
            "",
            "file:///tmp/provider",
            "https://user:secret@api.example.com/v1",
            "https://api.example.com/v1?token=secret",
        ] {
            let error = ProviderProfile::openai_compatible(
                "invalid",
                base_url,
                "model",
                env_credential("GOLUTRA_PROVIDER_API_KEY"),
            )
            .expect_err("invalid provider URL");
            assert!(error.to_string().contains("base URL"));
        }
    }

    #[test]
    fn onboarding_requires_explicit_provider_profile() {
        let _home = IsolatedGolutraHome::new();
        let state = provider_onboarding_state().expect("onboarding");

        assert!(!state.configured);
        assert_eq!(state.source, "none");
        assert_eq!(state.missing_fields, vec!["active_profile"]);
        assert!(state.active_profile.is_none());
    }

    #[test]
    fn onboarding_does_not_implicitly_activate_the_first_enabled_profile() {
        let _home = IsolatedGolutraHome::new();
        let paths = ProviderConfigPaths::global().expect("paths");
        ProviderInstallPlan {
            scope: ProviderConfigScope::User,
            profile: ProviderProfile::mock(),
            activate: false,
            pending_secret: None,
        }
        .apply(&paths)
        .expect("install inactive profile");

        let state = provider_onboarding_state().expect("onboarding");

        assert!(!state.configured);
        assert_eq!(state.missing_fields, vec!["active_profile"]);
        assert!(state.active_profile.is_none());
        assert!(
            load_provider_runtime_env()
                .expect("runtime env")
                .get("GOLUTRA_PROVIDER_PROTOCOL")
                .is_none()
        );
    }

    #[test]
    fn disabled_profile_cannot_be_active() {
        let mut profile = ProviderProfile::mock();
        profile.enabled = false;
        let mut settings = ProviderSettings {
            profiles: vec![profile],
            ..ProviderSettings::default()
        };

        let error = settings
            .set_active_profile("mock")
            .expect_err("disabled profile cannot be selected");
        assert!(error.to_string().contains("disabled"));

        settings.active_profile = Some("mock".to_owned());
        let error = settings
            .validate()
            .expect_err("disabled active profile is invalid");
        assert!(error.to_string().contains("disabled"));
    }

    #[test]
    fn onboarding_accepts_explicit_mock_provider() {
        let _home = IsolatedGolutraHome::new();
        let paths = ProviderConfigPaths::global().expect("paths");
        ProviderInstallPlan {
            scope: ProviderConfigScope::User,
            profile: ProviderProfile::mock(),
            activate: true,
            pending_secret: None,
        }
        .apply(&paths)
        .expect("install mock");

        let state = provider_onboarding_state().expect("onboarding");

        assert!(state.configured);
        assert_eq!(state.source, "user");
        assert!(state.missing_fields.is_empty());
        assert_eq!(
            state.active_profile.expect("profile").protocol,
            ProviderProtocol::Mock
        );
    }

    #[test]
    fn onboarding_accepts_disk_backed_provider_without_exposing_secret() {
        let _home = IsolatedGolutraHome::new();
        let paths = ProviderConfigPaths::global().expect("paths");
        let store = Arc::new(golutra_auth::MemorySecretStore::default());
        let reference = disk_credential();
        store
            .set(&reference, &SecretString::from("secret".to_owned()))
            .expect("secret");
        let profile =
            ProviderProfile::openai_compatible("golutra", "api.golutra.cn", "gpt-test", reference)
                .expect("profile");
        ProviderSettings {
            version: PROVIDER_SETTINGS_VERSION,
            active_profile: Some("golutra".to_owned()),
            profiles: vec![profile],
        }
        .save(&paths.user_config)
        .expect("settings");

        let state = provider_onboarding_state_with_store(&paths, store).expect("onboarding");

        assert!(state.configured);
        assert_eq!(state.source, "user");
        assert!(state.missing_fields.is_empty());
        assert!(
            serde_json::to_string(&state)
                .expect("serialize")
                .contains("credential_ref")
        );
        assert!(
            !serde_json::to_string(&state)
                .expect("serialize")
                .contains("profile-secret")
        );
    }

    #[test]
    fn onboarding_accepts_key_from_configured_env_var() {
        let _home = IsolatedGolutraHome::new();
        let paths = ProviderConfigPaths::global().expect("paths");
        let _api_key = ScopedEnvVar::set("GOLUTRA_PROVIDER_API_KEY", "secret-from-env");
        let profile = ProviderProfile::openai_compatible(
            "golutra",
            "api.golutra.cn",
            "gpt-test",
            env_credential("GOLUTRA_PROVIDER_API_KEY"),
        )
        .expect("profile");
        ProviderInstallPlan {
            scope: ProviderConfigScope::User,
            profile,
            activate: true,
            pending_secret: None,
        }
        .apply(&paths)
        .expect("install provider");

        let state = provider_onboarding_state().expect("onboarding");

        assert!(state.configured);
        assert!(state.missing_fields.is_empty());
    }

    #[tokio::test]
    async fn runtime_env_uses_dynamic_secret_provider_instead_of_serializing_key() {
        let home = tempdir().expect("home");
        let store = Arc::new(golutra_auth::MemorySecretStore::default());
        let reference = disk_credential();
        store
            .set(&reference, &SecretString::from("profile-secret".to_owned()))
            .expect("secret");
        let profile = ProviderProfile::openai_compatible(
            "custom",
            "https://api.golutra.cn/v1",
            "gpt-5.5",
            reference,
        )
        .expect("profile");
        let settings = ProviderSettings {
            version: PROVIDER_SETTINGS_VERSION,
            active_profile: Some("custom".to_owned()),
            profiles: vec![profile],
        };
        let auth = AuthService::new(home.path(), store).expect("auth");

        let runtime_env = runtime_env_from_settings(&settings, &auth).expect("runtime env");
        let resolved = runtime_env
            .credential_provider()
            .expect("provider")
            .credential(false)
            .await
            .expect("credential");

        assert_eq!(
            runtime_env.get("GOLUTRA_PROVIDER_API_KEY"),
            Some("<resolved-credential>".to_owned())
        );
        assert_eq!(resolved.expose_secret(), "profile-secret");
    }

    #[test]
    fn runtime_env_includes_generation_config_json() {
        let mut profile = ProviderProfile::openai_compatible(
            "custom",
            "https://api.golutra.cn/v1",
            "gpt-5.5",
            env_credential("GOLUTRA_CUSTOM_PROVIDER_API_KEY_TEST"),
        )
        .expect("profile");
        profile.generation_config = Some(ProviderGenerationConfig {
            enable_thinking: true,
            reasoning_effort: Some(golutra_llm::ProviderReasoningEffort::High),
            context_window_size: Some(128_000),
            max_tokens: Some(512),
        });
        let settings = ProviderSettings {
            version: PROVIDER_SETTINGS_VERSION,
            active_profile: Some("custom".to_owned()),
            profiles: vec![profile],
        };
        let home = tempdir().expect("home");
        let auth = AuthService::new(
            home.path(),
            Arc::new(golutra_auth::MemorySecretStore::default()),
        )
        .expect("auth");

        let runtime_env = runtime_env_from_settings(&settings, &auth).expect("runtime env");
        let value = runtime_env
            .get(GOLUTRA_PROVIDER_GENERATION_CONFIG)
            .expect("generation config");

        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&value).expect("json"),
            serde_json::json!({
                "enable_thinking": true,
                "reasoning_effort": "high",
                "context_window_size": 128000,
                "max_tokens": 512
            })
        );
    }

    #[test]
    fn redacted_runtime_env_hides_hashed_custom_provider_keys() {
        let key = "GOLUTRA_CUSTOM_PROVIDER_API_KEY_OPENAI_COMPATIBLE_EXAMPLE_1234";
        let environment = ProviderRuntimeEnv {
            values: BTreeMap::from([
                (key.to_owned(), "private-value".to_owned()),
                ("GOLUTRA_PROVIDER_MODEL".to_owned(), "model".to_owned()),
            ]),
            credential: None,
        };

        let redacted = environment.redacted_values();

        assert_eq!(redacted.get(key).map(String::as_str), Some("<redacted>"));
        assert_eq!(
            redacted.get("GOLUTRA_PROVIDER_MODEL").map(String::as_str),
            Some("model")
        );
    }

    #[tokio::test]
    async fn verified_provider_install_rolls_back_on_probe_failure() {
        let _home = IsolatedGolutraHome::new();
        let dir = tempdir().expect("dir");
        let paths = ProviderConfigPaths::global().expect("paths");
        let store = Arc::new(golutra_auth::MemorySecretStore::default());
        let reference = disk_credential();
        let profile = ProviderProfile::openai_compatible(
            "custom",
            "http://127.0.0.1:9/v1",
            "gpt-5.5",
            reference.clone(),
        )
        .expect("profile");

        let error = apply_provider_install_plan_verified_with_store(
            &paths,
            dir.path(),
            &ProviderInstallPlan {
                scope: ProviderConfigScope::User,
                profile,
                activate: true,
                pending_secret: Some(SecretString::from("bad-key".to_owned())),
            },
            store.clone(),
        )
        .await
        .expect_err("probe should fail");

        assert_eq!(error.step, "probe");
        assert!(!paths.user_config.exists());
        assert!(!store.contains(&reference));
    }

    #[tokio::test]
    async fn verified_provider_install_persists_when_probe_succeeds() {
        let _home = IsolatedGolutraHome::new();
        let dir = tempdir().expect("dir");
        let paths = ProviderConfigPaths::global().expect("paths");
        let store = Arc::new(golutra_auth::MemorySecretStore::default());
        let reference = disk_credential();
        let base_url = spawn_probe_server(r#"{"data":[{"id":"gpt-5.5"}]}"#).await;
        let profile =
            ProviderProfile::openai_compatible("custom", base_url, "gpt-5.5", reference.clone())
                .expect("profile");

        apply_provider_install_plan_verified_with_store(
            &paths,
            dir.path(),
            &ProviderInstallPlan {
                scope: ProviderConfigScope::User,
                profile,
                activate: true,
                pending_secret: Some(SecretString::from("good-key".to_owned())),
            },
            store.clone(),
        )
        .await
        .expect("probe should pass");

        let settings = ProviderSettings::load(&paths.user_config).expect("settings");
        let active = settings.active_profile().expect("active");
        assert_eq!(active.name, "custom");
        assert_eq!(active.model_id.as_deref(), Some("gpt-5.5"));
        assert!(store.contains(&reference));
        assert!(
            !fs::read_to_string(&paths.user_config)
                .expect("config")
                .contains("good-key")
        );
    }

    #[tokio::test]
    async fn verified_provider_install_persists_secret_in_the_disk_store() {
        let _home = IsolatedGolutraHome::new();
        let workspace = tempdir().expect("workspace");
        let paths = ProviderConfigPaths::global().expect("paths");
        let reference = disk_credential();
        let base_url = spawn_probe_server(r#"{"data":[{"id":"gpt-5.5"}]}"#).await;
        let profile =
            ProviderProfile::openai_compatible("custom", base_url, "gpt-5.5", reference.clone())
                .expect("profile");

        apply_provider_install_plan_verified(
            &paths,
            workspace.path(),
            &ProviderInstallPlan {
                scope: ProviderConfigScope::User,
                profile,
                activate: true,
                pending_secret: Some(SecretString::from("disk-secret".to_owned())),
            },
        )
        .await
        .expect("verified install");

        let store = golutra_auth::DefaultSecretStore::new(&paths.home).expect("disk store");
        assert_eq!(
            store
                .get(&reference)
                .expect("stored credential")
                .expect("secret")
                .expose_secret(),
            "disk-secret"
        );
        assert!(
            paths
                .home
                .join(golutra_auth::CREDENTIALS_FILE_NAME)
                .is_file()
        );
        assert!(
            !fs::read_to_string(&paths.user_config)
                .expect("provider config")
                .contains("disk-secret")
        );
    }

    #[tokio::test]
    async fn verified_provider_replacement_deletes_superseded_secret() {
        let _home = IsolatedGolutraHome::new();
        let dir = tempdir().expect("dir");
        let paths = ProviderConfigPaths::global().expect("paths");
        let store = Arc::new(golutra_auth::MemorySecretStore::default());
        let previous_reference = disk_credential();
        store
            .set(
                &previous_reference,
                &SecretString::from("previous-key".to_owned()),
            )
            .expect("previous secret");
        let previous_profile = ProviderProfile::openai_compatible(
            "custom",
            "https://api.example.com/v1",
            "old-model",
            previous_reference.clone(),
        )
        .expect("previous profile");
        ProviderSettings {
            version: PROVIDER_SETTINGS_VERSION,
            active_profile: Some("custom".to_owned()),
            profiles: vec![previous_profile],
        }
        .save(&paths.user_config)
        .expect("previous settings");

        let current_reference = disk_credential();
        let base_url = spawn_probe_server(r#"{"data":[{"id":"new-model"}]}"#).await;
        let current_profile = ProviderProfile::openai_compatible(
            "custom",
            base_url,
            "new-model",
            current_reference.clone(),
        )
        .expect("current profile");

        apply_provider_install_plan_verified_with_store(
            &paths,
            dir.path(),
            &ProviderInstallPlan {
                scope: ProviderConfigScope::User,
                profile: current_profile,
                activate: true,
                pending_secret: Some(SecretString::from("current-key".to_owned())),
            },
            store.clone(),
        )
        .await
        .expect("replacement");

        assert!(!store.contains(&previous_reference));
        assert!(store.contains(&current_reference));
        assert_eq!(
            ProviderSettings::load(&paths.user_config)
                .expect("settings")
                .active_profile()
                .and_then(|profile| profile.credential_ref.as_ref())
                .map(|reference| reference.id.as_str()),
            Some(current_reference.id.as_str())
        );
    }

    #[tokio::test]
    async fn failed_provider_replacement_restores_previous_secret_and_profile() {
        let _home = IsolatedGolutraHome::new();
        let dir = tempdir().expect("dir");
        let paths = ProviderConfigPaths::global().expect("paths");
        let store = Arc::new(golutra_auth::MemorySecretStore::default());
        let previous_reference = disk_credential();
        store
            .set(
                &previous_reference,
                &SecretString::from("previous-key".to_owned()),
            )
            .expect("previous secret");
        let previous_profile = ProviderProfile::openai_compatible(
            "custom",
            "https://api.example.com/v1",
            "old-model",
            previous_reference.clone(),
        )
        .expect("previous profile");
        ProviderSettings {
            version: PROVIDER_SETTINGS_VERSION,
            active_profile: Some("custom".to_owned()),
            profiles: vec![previous_profile],
        }
        .save(&paths.user_config)
        .expect("previous settings");

        let current_reference = disk_credential();
        let base_url = spawn_probe_server_response(
            "401 Unauthorized",
            r#"{"error":{"message":"invalid credential"}}"#,
        )
        .await;
        let current_profile = ProviderProfile::openai_compatible(
            "custom",
            base_url,
            "new-model",
            current_reference.clone(),
        )
        .expect("current profile");

        let error = apply_provider_install_plan_verified_with_store(
            &paths,
            dir.path(),
            &ProviderInstallPlan {
                scope: ProviderConfigScope::User,
                profile: current_profile,
                activate: true,
                pending_secret: Some(SecretString::from("current-key".to_owned())),
            },
            store.clone(),
        )
        .await
        .expect_err("probe failure");

        assert_eq!(error.step, "probe");
        assert!(store.contains(&previous_reference));
        assert!(!store.contains(&current_reference));
        assert_eq!(
            ProviderSettings::load(&paths.user_config)
                .expect("settings")
                .active_profile()
                .and_then(|profile| profile.credential_ref.as_ref())
                .map(|reference| reference.id.as_str()),
            Some(previous_reference.id.as_str())
        );
    }

    #[tokio::test]
    async fn verified_oauth_install_and_logout_keep_tokens_out_of_config() {
        let _home = IsolatedGolutraHome::new();
        let dir = tempdir().expect("dir");
        let paths = ProviderConfigPaths::global().expect("paths");
        let store = Arc::new(golutra_auth::MemorySecretStore::default());
        let reference = oauth_credential();
        store_test_oauth_credential(store.as_ref(), &reference, "oauth-access-token");
        let base_url = spawn_probe_server(r#"{"data":[{"id":"gpt-oauth"}]}"#).await;
        let mut profile =
            ProviderProfile::openai_compatible("oauth", base_url, "gpt-oauth", reference.clone())
                .expect("profile");
        profile.oauth = Some(oauth_descriptor());

        apply_oauth_provider_install_plan_verified_with_store(
            &paths,
            dir.path(),
            &ProviderInstallPlan {
                scope: ProviderConfigScope::User,
                profile,
                activate: true,
                pending_secret: None,
            },
            store.clone(),
        )
        .await
        .expect("OAuth install");

        let persisted = fs::read_to_string(&paths.user_config).expect("config");
        assert!(!persisted.contains("oauth-access-token"));
        assert!(!persisted.contains("refresh-test"));
        let settings = ProviderSettings::load(&paths.user_config).expect("settings");
        assert!(settings.active_profile().expect("active").oauth.is_some());

        logout_provider_profile_verified_with_store(&paths, dir.path(), "oauth", store.clone())
            .await
            .expect("logout");

        let settings = ProviderSettings::load(&paths.user_config).expect("settings");
        let profile = settings
            .profiles
            .iter()
            .find(|profile| profile.name == "oauth")
            .expect("profile");
        assert!(settings.active_profile.is_none());
        assert!(profile.credential_ref.is_none());
        assert!(profile.oauth.is_none());
        assert!(!profile.enabled);
        assert!(!store.contains(&reference));
    }

    #[tokio::test]
    async fn failed_oauth_probe_rolls_back_profile_and_deletes_token_set() {
        let _home = IsolatedGolutraHome::new();
        let dir = tempdir().expect("dir");
        let paths = ProviderConfigPaths::global().expect("paths");
        let store = Arc::new(golutra_auth::MemorySecretStore::default());
        let reference = oauth_credential();
        store_test_oauth_credential(store.as_ref(), &reference, "oauth-access-token");
        let mut profile = ProviderProfile::openai_compatible(
            "oauth",
            "http://127.0.0.1:9/v1",
            "gpt-oauth",
            reference.clone(),
        )
        .expect("profile");
        profile.oauth = Some(oauth_descriptor());

        let error = apply_oauth_provider_install_plan_verified_with_store(
            &paths,
            dir.path(),
            &ProviderInstallPlan {
                scope: ProviderConfigScope::User,
                profile,
                activate: true,
                pending_secret: None,
            },
            store.clone(),
        )
        .await
        .expect_err("probe must fail");

        assert_eq!(error.step, "probe");
        assert!(!paths.user_config.exists());
        assert!(!store.contains(&reference));
    }

    #[tokio::test]
    async fn verified_workspace_provider_install_is_rejected() {
        let _home = IsolatedGolutraHome::new();
        let dir = tempdir().expect("dir");
        let paths = ProviderConfigPaths::global().expect("paths");
        let store = Arc::new(golutra_auth::MemorySecretStore::default());
        let base_url = spawn_probe_server(r#"{"data":[{"id":"gpt-5.5"}]}"#).await;
        let profile =
            ProviderProfile::openai_compatible("custom", base_url, "gpt-5.5", disk_credential())
                .expect("profile");

        let error = apply_provider_install_plan_verified_with_store(
            &paths,
            dir.path(),
            &ProviderInstallPlan {
                scope: ProviderConfigScope::Workspace,
                profile,
                activate: true,
                pending_secret: Some(SecretString::from("good-key".to_owned())),
            },
            store,
        )
        .await
        .expect_err("workspace scope rejected");

        assert_eq!(error.step, "mutate");
        assert!(
            error
                .message
                .contains("workspace provider config is no longer supported")
        );
        assert!(!paths.user_config.exists());
    }

    #[test]
    fn provider_env_rejects_dangerous_names() {
        let error = ProviderProfile::openai_compatible(
            "golutra",
            "api.golutra.cn",
            "gpt-test",
            CredentialRef::environment("PATH", SecretKind::ApiKey).expect("reference"),
        )
        .expect_err("dangerous env rejected");

        assert!(matches!(error, ConfigError::Validation(_)));
    }
}
