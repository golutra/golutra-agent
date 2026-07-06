use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use golutra_llm::{ModelCatalog, ProviderProtocol, normalize_openai_base_url};
use serde::{Deserialize, Serialize};
use thiserror::Error;

const GOLUTRA_HOME: &str = "GOLUTRA_HOME";
const PROVIDER_FILE: &str = "provider.json";
const WORKSPACE_DIR: &str = ".golutra";
const USER_KEY_SENTINEL: &str = "<stored:user-provider-key>";
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
    pub workspace_config: PathBuf,
}

impl ProviderConfigPaths {
    pub fn for_workspace(workspace_root: impl AsRef<Path>) -> Result<Self, ConfigError> {
        Ok(Self {
            user_config: golutra_home()?.join(PROVIDER_FILE),
            workspace_config: workspace_root
                .as_ref()
                .join(WORKSPACE_DIR)
                .join(PROVIDER_FILE),
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
            version: 1,
            active_profile: None,
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
        let profile = Self {
            name: name.into(),
            protocol: ProviderProtocol::OpenAiCompatible,
            model_id: Some(model_id.into()),
            base_url: Some(normalize_openai_base_url(&base_url.into())),
            api_key_env: Some(api_key_env.into()),
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
        if self.protocol == ProviderProtocol::OpenAiCompatible && self.enabled {
            require_non_empty(self.model_id.as_deref(), "model_id")?;
            require_non_empty(self.base_url.as_deref(), "base_url")?;
            if self.api_key.is_none() {
                require_non_empty(self.api_key_env.as_deref(), "api_key_env")?;
            }
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
        let path = match self.scope {
            ProviderConfigScope::User => &paths.user_config,
            ProviderConfigScope::Workspace => &paths.workspace_config,
        };
        if self.scope == ProviderConfigScope::Workspace && self.profile.api_key.is_some() {
            return Err(ConfigError::Validation(
                "workspace provider config must not store api_key".to_owned(),
            ));
        }
        let mut settings = ProviderSettings::load(path)?;
        settings.upsert_profile(self.profile.clone(), self.activate);
        settings.save(path)
    }
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
        std::env::var(key)
            .ok()
            .filter(|value| !value.trim().is_empty())
            .or_else(|| self.values.get(key).cloned())
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
    let workspace = ProviderSettings::load(&paths.workspace_config)?;
    let merged = merge_provider_settings(user, workspace);
    let source = if paths.workspace_config.exists() {
        "workspace"
    } else if paths.user_config.exists() {
        "user"
    } else {
        "none"
    }
    .to_owned();
    let active_profile = merged.active_profile().map(ProviderProfile::redacted);
    let missing_fields = active_profile
        .as_ref()
        .map(missing_fields)
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
    let user = ProviderSettings::load(&paths.user_config)?;
    let workspace = ProviderSettings::load(&paths.workspace_config)?;
    Ok(merge_provider_settings(user, workspace))
}

#[must_use]
pub fn merge_provider_settings(
    user: ProviderSettings,
    workspace: ProviderSettings,
) -> ProviderSettings {
    let mut by_name = BTreeMap::<String, ProviderProfile>::new();
    for profile in user.profiles {
        by_name.insert(profile.name.clone(), profile);
    }
    for mut profile in workspace.profiles {
        if let Some(user_profile) = by_name.get(&profile.name)
            && profile.api_key.is_none()
        {
            profile.api_key = user_profile.api_key.clone();
        }
        by_name.insert(profile.name.clone(), profile);
    }
    ProviderSettings {
        version: 1,
        active_profile: workspace.active_profile.or(user.active_profile),
        profiles: by_name.into_values().collect(),
    }
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
        if let Some(api_key_env) = &profile.api_key_env {
            values.insert(
                "GOLUTRA_PROVIDER_API_KEY_ENV".to_owned(),
                api_key_env.clone(),
            );
            if let Ok(value) = std::env::var(api_key_env) {
                values.insert("GOLUTRA_PROVIDER_API_KEY".to_owned(), value);
            }
        }
        if let Some(api_key) = &profile.api_key {
            values.insert("GOLUTRA_PROVIDER_API_KEY".to_owned(), api_key.clone());
        }
    }
    ProviderRuntimeEnv { values }
}

fn golutra_home() -> Result<PathBuf, ConfigError> {
    if let Ok(value) = std::env::var(GOLUTRA_HOME) {
        return Ok(PathBuf::from(value));
    }
    let home =
        std::env::var("HOME").map_err(|_| ConfigError::Validation("HOME is not set".to_owned()))?;
    Ok(PathBuf::from(home).join(".golutra"))
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

fn missing_fields(profile: &ProviderProfile) -> Vec<String> {
    let mut fields = Vec::new();
    if profile.protocol == ProviderProtocol::OpenAiCompatible {
        if profile.model_id.as_deref().is_none_or(str::is_empty) {
            fields.push("model_id".to_owned());
        }
        if profile.base_url.as_deref().is_none_or(str::is_empty) {
            fields.push("base_url".to_owned());
        }
        let api_key_ready = profile
            .api_key
            .as_deref()
            .is_some_and(|value| !value.is_empty())
            || profile
                .api_key_env
                .as_ref()
                .and_then(|key| std::env::var(key).ok())
                .is_some_and(|value| !value.trim().is_empty());
        if !api_key_ready {
            fields.push("api_key".to_owned());
        }
    }
    fields
}

#[cfg(test)]
mod tests {
    use std::{
        ffi::OsString,
        sync::{Mutex, MutexGuard},
    };

    use tempfile::{TempDir, tempdir};

    use super::*;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct IsolatedGolutraHome {
        previous: Option<OsString>,
        _dir: TempDir,
        _guard: MutexGuard<'static, ()>,
    }

    impl IsolatedGolutraHome {
        fn new() -> Self {
            let guard = ENV_LOCK.lock().expect("env lock");
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
    fn provider_settings_roundtrip_redacts_user_key() {
        let dir = tempdir().expect("dir");
        let path = dir.path().join("provider.json");
        let mut profile = ProviderProfile::openai_compatible(
            "golutra",
            "api.golutra.cn",
            "gpt-test",
            "GOLUTRA_PROVIDER_API_KEY",
        )
        .expect("profile");
        profile.api_key = Some("secret".to_owned());
        let settings = ProviderSettings {
            version: 1,
            active_profile: Some("golutra".to_owned()),
            profiles: vec![profile],
        };

        settings.save(&path).expect("save");
        let loaded = ProviderSettings::load(&path).expect("load");

        assert_eq!(
            loaded.active_profile().expect("profile").api_key,
            Some("secret".to_owned())
        );
        assert_eq!(
            loaded.active_profile().expect("profile").redacted().api_key,
            Some(USER_KEY_SENTINEL.to_owned())
        );
    }

    #[test]
    fn workspace_config_rejects_stored_api_key() {
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
        let plan = ProviderInstallPlan {
            scope: ProviderConfigScope::Workspace,
            profile,
            activate: true,
        };

        assert!(matches!(
            plan.apply(&paths),
            Err(ConfigError::Validation(_))
        ));
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
            scope: ProviderConfigScope::Workspace,
            profile: ProviderProfile::mock(),
            activate: true,
        }
        .apply(&paths)
        .expect("install mock");

        let state = provider_onboarding_state(dir.path()).expect("onboarding");

        assert!(state.configured);
        assert_eq!(state.source, "workspace");
        assert!(state.missing_fields.is_empty());
        assert_eq!(
            state.active_profile.expect("profile").protocol,
            ProviderProtocol::Mock
        );
    }

    #[test]
    fn onboarding_accepts_user_openai_key() {
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
    fn merged_provider_env_prefers_workspace_non_secret_fields() {
        let mut user_profile = ProviderProfile::openai_compatible(
            "golutra",
            "https://user.example/v1",
            "user-model",
            "GOLUTRA_PROVIDER_API_KEY",
        )
        .expect("user");
        user_profile.api_key = Some("secret".to_owned());
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
                profiles: vec![user_profile],
            },
            ProviderSettings {
                version: 1,
                active_profile: Some("golutra".to_owned()),
                profiles: vec![workspace_profile],
            },
        );
        let runtime_env = runtime_env_from_settings(&merged);

        assert_eq!(
            runtime_env.get("GOLUTRA_PROVIDER_MODEL"),
            Some("workspace-model".to_owned())
        );
        assert_eq!(
            runtime_env.get("GOLUTRA_PROVIDER_API_KEY"),
            Some("secret".to_owned())
        );
    }

    #[test]
    fn provider_env_rejects_dangerous_names() {
        let error =
            ProviderProfile::openai_compatible("golutra", "api.golutra.cn", "gpt-test", "PATH")
                .expect_err("dangerous env rejected");

        assert!(matches!(error, ConfigError::Validation(_)));
    }
}
