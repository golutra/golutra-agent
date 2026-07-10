use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use golutra_llm::{
    ConfiguredProvider, ModelCatalog, ProviderGenerationConfig, ProviderProtocol,
    normalize_openai_base_url,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

const GOLUTRA_HOME: &str = "GOLUTRA_HOME";
const PROVIDER_FILE: &str = "provider.json";
const USER_KEY_SENTINEL: &str = "<stored:user-provider-key>";
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
            data_dir: "${GOLUTRA_HOME:-.golutra}/state".to_owned(),
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
    pub user_config: PathBuf,
}

impl ProviderConfigPaths {
    pub fn for_workspace(_workspace_root: impl AsRef<Path>) -> Result<Self, ConfigError> {
        Ok(Self {
            user_config: golutra_home()?.join(PROVIDER_FILE),
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
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    pub profiles: Vec<ProviderProfile>,
}

impl Default for ProviderSettings {
    fn default() -> Self {
        Self {
            version: 1,
            active_profile: None,
            env: BTreeMap::new(),
            profiles: Vec::new(),
        }
    }
}

impl ProviderSettings {
    pub fn load(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let path = path.as_ref();
        if !path.exists() {
            return Ok(Self::default());
        }
        let content =
            fs::read_to_string(path).map_err(|error| ConfigError::Io(error.to_string()))?;
        serde_json::from_str(&content).map_err(|error| ConfigError::Json(error.to_string()))
    }

    pub fn save(&self, path: impl AsRef<Path>) -> Result<(), ConfigError> {
        self.validate()?;
        write_json_owner_only(path.as_ref(), self)
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        for key in self.env.keys() {
            validate_env_key(key)?;
        }
        for profile in &self.profiles {
            profile.validate()?;
        }
        Ok(())
    }

    #[must_use]
    pub fn active_profile(&self) -> Option<&ProviderProfile> {
        self.active_profile
            .as_ref()
            .and_then(|name| self.profiles.iter().find(|profile| &profile.name == name))
            .or_else(|| self.profiles.iter().find(|profile| profile.enabled))
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
        if self.profiles.iter().any(|profile| profile.name == name) {
            self.active_profile = Some(name);
            Ok(())
        } else {
            Err(ConfigError::Validation(format!(
                "provider profile `{name}` does not exist"
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
    pub api_key_env: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation_config: Option<ProviderGenerationConfig>,
    #[serde(skip, default)]
    pub api_key: Option<String>,
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
            api_key_env: None,
            generation_config: None,
            api_key: None,
            enabled: true,
        }
    }

    pub fn openai_compatible(
        name: impl Into<String>,
        base_url: impl Into<String>,
        model_id: impl Into<String>,
        api_key_env: impl Into<String>,
    ) -> Result<Self, ConfigError> {
        Self::live_profile(
            name,
            ProviderProtocol::OpenAiCompatible,
            base_url,
            model_id,
            api_key_env,
        )
    }

    pub fn live_profile(
        name: impl Into<String>,
        protocol: ProviderProtocol,
        base_url: impl Into<String>,
        model_id: impl Into<String>,
        api_key_env: impl Into<String>,
    ) -> Result<Self, ConfigError> {
        let base_url = normalize_provider_base_url(protocol, &base_url.into());
        let profile = Self {
            name: name.into(),
            protocol,
            model_id: Some(model_id.into()),
            base_url: Some(base_url),
            api_key_env: Some(api_key_env.into()),
            generation_config: None,
            api_key: None,
            enabled: true,
        };
        profile.validate()?;
        Ok(profile)
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        validate_profile_name(&self.name)?;
        if let Some(api_key_env) = &self.api_key_env {
            validate_env_key(api_key_env)?;
        }
        if self.enabled {
            validate_provider_protocol_runtime_supported(self.protocol)?;
        }
        if live_profile_requires_connection_fields(self.protocol) && self.enabled {
            require_non_empty(self.model_id.as_deref(), "model_id")?;
            require_non_empty(self.base_url.as_deref(), "base_url")?;
            require_non_empty(self.api_key_env.as_deref(), "api_key_env")?;
        }
        Ok(())
    }

    #[must_use]
    pub fn redacted(&self) -> Self {
        let mut clone = self.clone();
        if clone.api_key.is_some() {
            clone.api_key = Some(USER_KEY_SENTINEL.to_owned());
        }
        clone
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderInstallPlan {
    pub scope: ProviderConfigScope,
    pub profile: ProviderProfile,
    pub activate: bool,
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
        let path = &paths.user_config;
        let mut settings = ProviderSettings::load(path)?;
        persist_profile_in_settings(&mut settings, self.profile.clone(), self.activate)?;
        settings.save(path)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProviderSettingsSnapshot {
    existed: bool,
    settings: ProviderSettings,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderOnboardingState {
    pub configured: bool,
    pub active_profile: Option<ProviderProfile>,
    pub missing_fields: Vec<String>,
    pub source: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderRuntimeEnv {
    values: BTreeMap<String, String>,
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
                let redacted = if key.ends_with("API_KEY") || key.ends_with("TOKEN") {
                    "<redacted>".to_owned()
                } else {
                    value.clone()
                };
                (key.clone(), redacted)
            })
            .collect()
    }
}

pub fn load_provider_runtime_env(
    workspace_root: impl AsRef<Path>,
) -> Result<ProviderRuntimeEnv, ConfigError> {
    let paths = ProviderConfigPaths::for_workspace(workspace_root)?;
    let merged = load_merged_provider_settings(&paths)?;
    Ok(runtime_env_from_settings(&merged))
}

pub fn provider_onboarding_state(
    workspace_root: impl AsRef<Path>,
) -> Result<ProviderOnboardingState, ConfigError> {
    let paths = ProviderConfigPaths::for_workspace(workspace_root)?;
    let user = ProviderSettings::load(&paths.user_config)?;
    let source = if paths.user_config.exists() {
        "user"
    } else {
        "none"
    }
    .to_owned();
    let active_profile = user
        .active_profile()
        .map(|profile| redacted_profile_with_credentials(&user, profile));
    let missing_fields = active_profile
        .as_ref()
        .map(|profile| missing_fields(&user, profile))
        .unwrap_or_else(|| vec!["active_profile".to_owned()]);
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
    ProviderSettings::load(&paths.user_config)
}

#[must_use]
pub fn merge_provider_settings(
    user: ProviderSettings,
    _workspace: ProviderSettings,
) -> ProviderSettings {
    user
}

#[must_use]
pub fn runtime_env_from_settings(settings: &ProviderSettings) -> ProviderRuntimeEnv {
    let mut values = BTreeMap::new();
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
        if let Some(api_key_env) = &profile.api_key_env {
            values.insert(
                "GOLUTRA_PROVIDER_API_KEY_ENV".to_owned(),
                api_key_env.clone(),
            );
            if let Some(value) = settings
                .env
                .get(api_key_env)
                .filter(|value| !value.trim().is_empty())
                .cloned()
            {
                values.insert(api_key_env.clone(), value.clone());
                values.insert("GOLUTRA_PROVIDER_API_KEY".to_owned(), value);
            } else if let Ok(value) = std::env::var(api_key_env) {
                values.insert(api_key_env.clone(), value.clone());
                values.insert("GOLUTRA_PROVIDER_API_KEY".to_owned(), value);
            }
        }
    }
    ProviderRuntimeEnv { values }
}

#[must_use]
pub fn provider_protocol_has_runtime_adapter(protocol: ProviderProtocol) -> bool {
    matches!(
        protocol,
        ProviderProtocol::Mock | ProviderProtocol::OpenAiCompatible
    )
}

pub fn validate_provider_protocol_runtime_supported(
    protocol: ProviderProtocol,
) -> Result<(), ConfigError> {
    if provider_protocol_has_runtime_adapter(protocol) {
        Ok(())
    } else {
        Err(ConfigError::Validation(format!(
            "provider protocol `{}` is catalog-only and has no live adapter yet",
            protocol.id()
        )))
    }
}

pub async fn apply_provider_install_plan_verified(
    paths: &ProviderConfigPaths,
    workspace_root: impl AsRef<Path>,
    plan: &ProviderInstallPlan,
) -> Result<(), ProviderInstallError> {
    if plan.scope == ProviderConfigScope::Workspace {
        return Err(provider_install_error(
            "mutate",
            "workspace provider config is no longer supported; use global user provider config",
        ));
    }
    run_provider_settings_transaction(paths, workspace_root, |user, workspace| {
        plan.profile.validate()?;
        match plan.scope {
            ProviderConfigScope::User => {
                persist_profile_in_settings(user, plan.profile.clone(), plan.activate)?;
            }
            ProviderConfigScope::Workspace => {
                persist_profile_in_settings(workspace, plan.profile.clone(), plan.activate)?;
            }
        }
        Ok(())
    })
    .await
}

pub async fn update_provider_settings_verified<F>(
    paths: &ProviderConfigPaths,
    workspace_root: impl AsRef<Path>,
    mutate: F,
) -> Result<(), ProviderInstallError>
where
    F: FnOnce(&mut ProviderSettings, &mut ProviderSettings) -> Result<(), ConfigError>,
{
    run_provider_settings_transaction(paths, workspace_root, mutate).await
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

fn golutra_home() -> Result<PathBuf, ConfigError> {
    if let Ok(value) = std::env::var(GOLUTRA_HOME) {
        return Ok(PathBuf::from(value));
    }
    let home =
        std::env::var("HOME").map_err(|_| ConfigError::Validation("HOME is not set".to_owned()))?;
    Ok(PathBuf::from(home).join(".golutra"))
}

async fn run_provider_settings_transaction<F>(
    paths: &ProviderConfigPaths,
    workspace_root: impl AsRef<Path>,
    mutate: F,
) -> Result<(), ProviderInstallError>
where
    F: FnOnce(&mut ProviderSettings, &mut ProviderSettings) -> Result<(), ConfigError>,
{
    let workspace_root = workspace_root.as_ref().to_path_buf();
    let user_snapshot = snapshot_provider_settings_file(&paths.user_config)
        .map_err(|error| provider_install_error("backup", error.to_string()))?;

    let mut user_settings = user_snapshot.settings.clone();
    let mut workspace_settings = ProviderSettings::default();
    let transaction_result = async {
        mutate(&mut user_settings, &mut workspace_settings)
            .map_err(|error| provider_install_error("mutate", error.to_string()))?;
        if workspace_settings != ProviderSettings::default() {
            return Err(provider_install_error(
                "mutate",
                "workspace provider config is no longer supported; use global user provider config",
            ));
        }

        persist_provider_settings_file(&paths.user_config, &user_snapshot, &user_settings)
            .map_err(|error| provider_install_error("persist", error.to_string()))?;

        probe_provider_after_settings_update(&workspace_root, &user_settings).await
    }
    .await;

    if let Err(error) = transaction_result {
        restore_provider_settings_file(&paths.user_config, &user_snapshot).map_err(|restore| {
            provider_install_error(
                "rollback",
                format!("{}; rollback failed: {restore}", error.message),
            )
        })?;
        return Err(error);
    }

    Ok(())
}

fn snapshot_provider_settings_file(path: &Path) -> Result<ProviderSettingsSnapshot, ConfigError> {
    Ok(ProviderSettingsSnapshot {
        existed: path.exists(),
        settings: ProviderSettings::load(path)?,
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
        return Ok(());
    }
    settings.save(path)
}

fn restore_provider_settings_file(
    path: &Path,
    snapshot: &ProviderSettingsSnapshot,
) -> Result<(), ConfigError> {
    if !snapshot.existed {
        if path.exists() {
            fs::remove_file(path).map_err(|error| ConfigError::Io(error.to_string()))?;
        }
        return Ok(());
    }
    snapshot.settings.save(path)
}

async fn probe_provider_after_settings_update(
    workspace_root: &Path,
    user_settings: &ProviderSettings,
) -> Result<(), ProviderInstallError> {
    let Some(active_profile) = user_settings.active_profile() else {
        return Ok(());
    };
    if active_profile.protocol == ProviderProtocol::Mock {
        return Ok(());
    }
    let runtime_env = runtime_env_from_settings(user_settings);
    ConfiguredProvider::probe_from_reader(|key| runtime_env.get(key))
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
    mut profile: ProviderProfile,
    activate: bool,
) -> Result<(), ConfigError> {
    if let Some(api_key) = profile.api_key.take() {
        let env_key = profile.api_key_env.clone().ok_or_else(|| {
            ConfigError::Validation(
                "provider profile with inline api_key must declare api_key_env".to_owned(),
            )
        })?;
        settings.env.insert(env_key, api_key);
    }
    settings.upsert_profile(profile, activate);
    Ok(())
}

fn redacted_profile_with_credentials(
    settings: &ProviderSettings,
    profile: &ProviderProfile,
) -> ProviderProfile {
    let mut redacted = profile.redacted();
    if redacted.api_key.is_none()
        && profile
            .api_key_env
            .as_ref()
            .and_then(|key| resolve_api_key_value(settings, key))
            .is_some()
    {
        redacted.api_key = Some(USER_KEY_SENTINEL.to_owned());
    }
    redacted
}

fn write_json_owner_only<T: Serialize>(path: &Path, value: &T) -> Result<(), ConfigError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| ConfigError::Io(error.to_string()))?;
        set_owner_only_dir(parent)?;
    }
    let temp_path = path.with_extension("json.tmp");
    let content = serde_json::to_string_pretty(value)
        .map_err(|error| ConfigError::Json(error.to_string()))?;
    fs::write(&temp_path, format!("{content}\n"))
        .map_err(|error| ConfigError::Io(error.to_string()))?;
    set_owner_only_file(&temp_path)?;
    fs::rename(&temp_path, path).map_err(|error| ConfigError::Io(error.to_string()))?;
    set_owner_only_file(path)
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

fn normalize_provider_base_url(protocol: ProviderProtocol, value: &str) -> String {
    if protocol == ProviderProtocol::OpenAiCompatible {
        normalize_openai_base_url(value)
    } else {
        value.trim().trim_end_matches('/').to_owned()
    }
}

fn missing_fields(settings: &ProviderSettings, profile: &ProviderProfile) -> Vec<String> {
    let mut fields = Vec::new();
    if live_profile_requires_connection_fields(profile.protocol) {
        if profile.model_id.as_deref().is_none_or(str::is_empty) {
            fields.push("model_id".to_owned());
        }
        if profile.base_url.as_deref().is_none_or(str::is_empty) {
            fields.push("base_url".to_owned());
        }
        let api_key_ready = profile
            .api_key_env
            .as_ref()
            .and_then(|key| resolve_api_key_value(settings, key))
            .is_some_and(|value| !value.trim().is_empty());
        if !api_key_ready {
            fields.push("api_key".to_owned());
        }
    }
    fields
}

fn resolve_api_key_value(settings: &ProviderSettings, env_key: &str) -> Option<String> {
    settings
        .env
        .get(env_key)
        .filter(|value| !value.trim().is_empty())
        .cloned()
        .or_else(|| {
            std::env::var(env_key)
                .ok()
                .filter(|value| !value.trim().is_empty())
        })
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
            let previous = std::env::var_os(GOLUTRA_HOME);
            unsafe {
                std::env::set_var(GOLUTRA_HOME, dir.path());
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
                    std::env::set_var(GOLUTRA_HOME, value);
                },
                None => unsafe {
                    std::env::remove_var(GOLUTRA_HOME);
                },
            }
        }
    }

    async fn spawn_probe_server(body: &'static str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            let mut buffer = [0_u8; 2048];
            let _ = stream.read(&mut buffer).await.expect("read request");
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
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
    fn provider_settings_persists_env_map_and_omits_inline_api_key() {
        let _home = IsolatedGolutraHome::new();
        let dir = tempdir().expect("dir");
        let paths = ProviderConfigPaths::for_workspace(dir.path()).expect("paths");
        let mut profile = ProviderProfile::openai_compatible(
            "golutra",
            "api.golutra.cn",
            "gpt-test",
            "GOLUTRA_PROVIDER_API_KEY",
        )
        .expect("profile");
        profile.api_key = Some("secret".to_owned());
        ProviderInstallPlan {
            scope: ProviderConfigScope::User,
            profile,
            activate: true,
        }
        .apply(&paths)
        .expect("install");

        let loaded = ProviderSettings::load(&paths.user_config).expect("load");

        assert_eq!(
            loaded
                .env
                .get("GOLUTRA_PROVIDER_API_KEY")
                .map(String::as_str),
            Some("secret")
        );
        assert!(loaded.active_profile().expect("profile").api_key.is_none());
    }

    #[test]
    fn legacy_inline_api_key_is_ignored_on_load() {
        let dir = tempdir().expect("dir");
        let path = dir.path().join("provider.json");
        fs::write(
            &path,
            r#"{
  "version": 1,
  "active_profile": "golutra",
  "profiles": [
    {
      "name": "golutra",
      "protocol": "openai-compatible",
      "model_id": "gpt-test",
      "base_url": "https://api.golutra.cn/v1",
      "api_key_env": "GOLUTRA_PROVIDER_API_KEY",
      "api_key": "legacy-secret",
      "enabled": true
    }
  ]
}
"#,
        )
        .expect("write");

        let loaded = ProviderSettings::load(&path).expect("load");

        assert!(loaded.env.is_empty());
        assert!(loaded.active_profile().expect("profile").api_key.is_none());
    }

    #[test]
    fn redacted_profile_marks_env_backed_credentials() {
        let settings = ProviderSettings {
            version: 1,
            active_profile: Some("golutra".to_owned()),
            env: BTreeMap::from([("GOLUTRA_PROVIDER_API_KEY".to_owned(), "secret".to_owned())]),
            profiles: vec![
                ProviderProfile::openai_compatible(
                    "golutra",
                    "api.golutra.cn",
                    "gpt-test",
                    "GOLUTRA_PROVIDER_API_KEY",
                )
                .expect("profile"),
            ],
        };
        let redacted = redacted_profile_with_credentials(
            &settings,
            settings.active_profile().expect("profile"),
        );

        assert_eq!(redacted.api_key, Some(USER_KEY_SENTINEL.to_owned()));
    }

    #[test]
    fn workspace_provider_config_scope_is_rejected() {
        let _home = IsolatedGolutraHome::new();
        let dir = tempdir().expect("dir");
        let paths = ProviderConfigPaths::for_workspace(dir.path()).expect("paths");
        let profile = ProviderProfile::openai_compatible(
            "golutra",
            "api.golutra.cn",
            "gpt-test",
            "GOLUTRA_PROVIDER_API_KEY",
        )
        .expect("profile");
        let plan = ProviderInstallPlan {
            scope: ProviderConfigScope::Workspace,
            profile,
            activate: true,
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
    fn provider_install_plan_rejects_catalog_only_live_protocols() {
        let _home = IsolatedGolutraHome::new();
        let dir = tempdir().expect("dir");
        let paths = ProviderConfigPaths::for_workspace(dir.path()).expect("paths");
        let profile = ProviderProfile {
            name: "anthropic".to_owned(),
            protocol: ProviderProtocol::Anthropic,
            model_id: Some("claude-sonnet-4".to_owned()),
            base_url: Some("https://api.anthropic.com/v1".to_owned()),
            api_key_env: Some("GOLUTRA_PROVIDER_API_KEY".to_owned()),
            generation_config: None,
            api_key: Some("secret".to_owned()),
            enabled: true,
        };
        let plan = ProviderInstallPlan {
            scope: ProviderConfigScope::User,
            profile,
            activate: true,
        };

        let error = plan
            .apply(&paths)
            .expect_err("unsupported protocol rejected");

        assert!(matches!(error, ConfigError::Validation(_)));
        assert!(error.to_string().contains("has no live adapter yet"));
        let settings = ProviderSettings::load(&paths.user_config).expect("settings");
        assert!(settings.profiles.is_empty());
    }

    #[test]
    fn onboarding_requires_explicit_provider_profile() {
        let _home = IsolatedGolutraHome::new();
        let dir = tempdir().expect("dir");
        let state = provider_onboarding_state(dir.path()).expect("onboarding");

        assert!(!state.configured);
        assert_eq!(state.source, "none");
        assert_eq!(state.missing_fields, vec!["active_profile"]);
        assert!(state.active_profile.is_none());
    }

    #[test]
    fn onboarding_accepts_explicit_mock_provider() {
        let _home = IsolatedGolutraHome::new();
        let dir = tempdir().expect("dir");
        let paths = ProviderConfigPaths::for_workspace(dir.path()).expect("paths");
        ProviderInstallPlan {
            scope: ProviderConfigScope::User,
            profile: ProviderProfile::mock(),
            activate: true,
        }
        .apply(&paths)
        .expect("install mock");

        let state = provider_onboarding_state(dir.path()).expect("onboarding");

        assert!(state.configured);
        assert_eq!(state.source, "user");
        assert!(state.missing_fields.is_empty());
        assert_eq!(
            state.active_profile.expect("profile").protocol,
            ProviderProtocol::Mock
        );
    }

    #[test]
    fn onboarding_accepts_user_openai_key_from_env_map() {
        let _home = IsolatedGolutraHome::new();
        let dir = tempdir().expect("dir");
        let paths = ProviderConfigPaths::for_workspace(dir.path()).expect("paths");
        let mut profile = ProviderProfile::openai_compatible(
            "golutra",
            "api.golutra.cn",
            "gpt-test",
            "GOLUTRA_PROVIDER_API_KEY",
        )
        .expect("profile");
        profile.api_key = Some("secret".to_owned());
        ProviderInstallPlan {
            scope: ProviderConfigScope::User,
            profile,
            activate: true,
        }
        .apply(&paths)
        .expect("install provider");

        let state = provider_onboarding_state(dir.path()).expect("onboarding");

        assert!(state.configured);
        assert_eq!(state.source, "user");
        assert!(state.missing_fields.is_empty());
        assert_eq!(
            state.active_profile.expect("profile").api_key,
            Some(USER_KEY_SENTINEL.to_owned())
        );
    }

    #[test]
    fn onboarding_accepts_key_from_configured_env_var() {
        let _home = IsolatedGolutraHome::new();
        let dir = tempdir().expect("dir");
        let paths = ProviderConfigPaths::for_workspace(dir.path()).expect("paths");
        let _api_key = ScopedEnvVar::set("GOLUTRA_PROVIDER_API_KEY", "secret-from-env");
        let profile = ProviderProfile::openai_compatible(
            "golutra",
            "api.golutra.cn",
            "gpt-test",
            "GOLUTRA_PROVIDER_API_KEY",
        )
        .expect("profile");
        ProviderInstallPlan {
            scope: ProviderConfigScope::User,
            profile,
            activate: true,
        }
        .apply(&paths)
        .expect("install provider");

        let state = provider_onboarding_state(dir.path()).expect("onboarding");

        assert!(state.configured);
        assert!(state.missing_fields.is_empty());
    }

    #[test]
    fn merged_provider_settings_ignores_workspace_provider_config() {
        let user_profile = ProviderProfile::openai_compatible(
            "golutra",
            "https://user.example/v1",
            "user-model",
            "GOLUTRA_PROVIDER_API_KEY",
        )
        .expect("user");
        let workspace_profile = ProviderProfile::openai_compatible(
            "golutra",
            "https://workspace.example/v1",
            "workspace-model",
            "GOLUTRA_PROVIDER_API_KEY",
        )
        .expect("workspace");

        let merged = merge_provider_settings(
            ProviderSettings {
                version: 1,
                active_profile: Some("golutra".to_owned()),
                env: BTreeMap::from([("GOLUTRA_PROVIDER_API_KEY".to_owned(), "secret".to_owned())]),
                profiles: vec![user_profile],
            },
            ProviderSettings {
                version: 1,
                active_profile: Some("golutra".to_owned()),
                env: BTreeMap::new(),
                profiles: vec![workspace_profile],
            },
        );
        let runtime_env = runtime_env_from_settings(&merged);

        assert_eq!(
            runtime_env.get("GOLUTRA_PROVIDER_MODEL"),
            Some("user-model".to_owned())
        );
        assert_eq!(
            runtime_env.get("GOLUTRA_PROVIDER_API_KEY"),
            Some("secret".to_owned())
        );
    }

    #[test]
    fn runtime_env_prefers_active_profile_key_over_process_env() {
        let _guard = lock_test_env();
        let _api_key = ScopedEnvVar::set("GOLUTRA_PROVIDER_API_KEY", "process-secret");
        let profile = ProviderProfile::openai_compatible(
            "custom",
            "https://api.golutra.cn/v1",
            "gpt-5.5",
            "GOLUTRA_CUSTOM_PROVIDER_API_KEY_TEST",
        )
        .expect("profile");
        let settings = ProviderSettings {
            version: 1,
            active_profile: Some("custom".to_owned()),
            env: BTreeMap::from([(
                "GOLUTRA_CUSTOM_PROVIDER_API_KEY_TEST".to_owned(),
                "profile-secret".to_owned(),
            )]),
            profiles: vec![profile],
        };

        let runtime_env = runtime_env_from_settings(&settings);

        assert_eq!(
            runtime_env.get("GOLUTRA_PROVIDER_API_KEY"),
            Some("profile-secret".to_owned())
        );
        assert_eq!(
            runtime_env.get("GOLUTRA_CUSTOM_PROVIDER_API_KEY_TEST"),
            Some("profile-secret".to_owned())
        );
    }

    #[test]
    fn runtime_env_includes_generation_config_json() {
        let mut profile = ProviderProfile::openai_compatible(
            "custom",
            "https://api.golutra.cn/v1",
            "gpt-5.5",
            "GOLUTRA_CUSTOM_PROVIDER_API_KEY_TEST",
        )
        .expect("profile");
        profile.generation_config = Some(ProviderGenerationConfig {
            enable_thinking: true,
            reasoning_effort: Some(golutra_llm::ProviderReasoningEffort::High),
            context_window_size: Some(128_000),
            max_tokens: Some(512),
        });
        let settings = ProviderSettings {
            version: 1,
            active_profile: Some("custom".to_owned()),
            env: BTreeMap::from([(
                "GOLUTRA_CUSTOM_PROVIDER_API_KEY_TEST".to_owned(),
                "profile-secret".to_owned(),
            )]),
            profiles: vec![profile],
        };

        let runtime_env = runtime_env_from_settings(&settings);
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

    #[tokio::test]
    async fn verified_provider_install_rolls_back_on_probe_failure() {
        let _home = IsolatedGolutraHome::new();
        let dir = tempdir().expect("dir");
        let paths = ProviderConfigPaths::for_workspace(dir.path()).expect("paths");
        let mut profile = ProviderProfile::openai_compatible(
            "custom",
            "http://127.0.0.1:9/v1",
            "gpt-5.5",
            "GOLUTRA_CUSTOM_PROVIDER_API_KEY_TEST",
        )
        .expect("profile");
        profile.api_key = Some("bad-key".to_owned());

        let error = apply_provider_install_plan_verified(
            &paths,
            dir.path(),
            &ProviderInstallPlan {
                scope: ProviderConfigScope::User,
                profile,
                activate: true,
            },
        )
        .await
        .expect_err("probe should fail");

        assert_eq!(error.step, "probe");
        assert!(!paths.user_config.exists());
    }

    #[tokio::test]
    async fn verified_provider_install_persists_when_probe_succeeds() {
        let _home = IsolatedGolutraHome::new();
        let dir = tempdir().expect("dir");
        let paths = ProviderConfigPaths::for_workspace(dir.path()).expect("paths");
        let base_url = spawn_probe_server(r#"{"data":[{"id":"gpt-5.5"}]}"#).await;
        let mut profile = ProviderProfile::openai_compatible(
            "custom",
            base_url,
            "gpt-5.5",
            "GOLUTRA_CUSTOM_PROVIDER_API_KEY_TEST",
        )
        .expect("profile");
        profile.api_key = Some("good-key".to_owned());

        apply_provider_install_plan_verified(
            &paths,
            dir.path(),
            &ProviderInstallPlan {
                scope: ProviderConfigScope::User,
                profile,
                activate: true,
            },
        )
        .await
        .expect("probe should pass");

        let settings = ProviderSettings::load(&paths.user_config).expect("settings");
        let active = settings.active_profile().expect("active");
        assert_eq!(active.name, "custom");
        assert_eq!(active.model_id.as_deref(), Some("gpt-5.5"));
    }

    #[tokio::test]
    async fn verified_workspace_provider_install_is_rejected() {
        let _home = IsolatedGolutraHome::new();
        let dir = tempdir().expect("dir");
        let paths = ProviderConfigPaths::for_workspace(dir.path()).expect("paths");
        let base_url = spawn_probe_server(r#"{"data":[{"id":"gpt-5.5"}]}"#).await;
        let mut profile = ProviderProfile::openai_compatible(
            "custom",
            base_url,
            "gpt-5.5",
            "GOLUTRA_CUSTOM_PROVIDER_API_KEY_TEST",
        )
        .expect("profile");
        profile.api_key = Some("good-key".to_owned());

        let error = apply_provider_install_plan_verified(
            &paths,
            dir.path(),
            &ProviderInstallPlan {
                scope: ProviderConfigScope::Workspace,
                profile,
                activate: true,
            },
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
        let error =
            ProviderProfile::openai_compatible("golutra", "api.golutra.cn", "gpt-test", "PATH")
                .expect_err("dangerous env rejected");

        assert!(matches!(error, ConfigError::Validation(_)));
    }
}
