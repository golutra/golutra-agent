//! Provider 配置、凭据与磁盘状态的事务持久化。

use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InvalidProviderSettingsPolicy {
    Reject,
    ReplaceJson,
}

pub(crate) struct LoadedProviderSettings {
    pub(crate) settings: ProviderSettings,
    pub(crate) rollback_bytes: Option<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProviderSettingsSnapshot {
    pub(crate) existed: bool,
    pub(crate) settings: ProviderSettings,
    pub(crate) rollback_bytes: Option<Vec<u8>>,
}

#[derive(Clone)]
pub(crate) enum SecretMutationAction {
    Set(SecretString),
    Delete,
}

#[derive(Clone)]
pub(crate) struct SecretMutation {
    pub(crate) reference: CredentialRef,
    pub(crate) action: SecretMutationAction,
}

pub(crate) struct SecretSnapshot {
    pub(crate) reference: CredentialRef,
    pub(crate) value: Option<SecretString>,
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

pub(crate) fn default_secret_store(
    paths: &ProviderConfigPaths,
) -> Result<Arc<dyn SecretStore>, ConfigError> {
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

pub(crate) fn load_or_migrate_provider_settings_unlocked(
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

pub(crate) fn load_provider_settings_for_install_unlocked(
    paths: &ProviderConfigPaths,
    store: &dyn SecretStore,
) -> Result<LoadedProviderSettings, ConfigError> {
    match load_or_migrate_provider_settings_unlocked(paths, store) {
        Ok(settings) => Ok(LoadedProviderSettings {
            settings,
            rollback_bytes: None,
        }),
        Err(ConfigError::Json(_)) => {
            let rollback_bytes =
                fs::read(&paths.user_config).map_err(|error| ConfigError::Io(error.to_string()))?;
            Ok(LoadedProviderSettings {
                settings: ProviderSettings::default(),
                rollback_bytes: Some(rollback_bytes),
            })
        }
        Err(error) => Err(error),
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

pub(crate) fn migrated_credential_ref(
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

pub(crate) async fn snapshot_secrets(
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

pub(crate) async fn apply_secret_mutations(
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

pub(crate) async fn restore_secret_snapshots(
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

pub(crate) fn restore_secret_snapshots_sync(
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

pub(crate) async fn run_provider_settings_transaction<F>(
    paths: &ProviderConfigPaths,
    workspace_root: impl AsRef<Path>,
    store: Arc<dyn SecretStore>,
    mutate: F,
) -> Result<(), ProviderInstallError>
where
    F: FnOnce(&mut ProviderSettings) -> Result<Vec<SecretMutation>, ConfigError>,
{
    run_provider_settings_transaction_with_policy(
        paths,
        workspace_root,
        store,
        InvalidProviderSettingsPolicy::Reject,
        mutate,
    )
    .await
}

pub(crate) async fn run_provider_install_transaction<F>(
    paths: &ProviderConfigPaths,
    workspace_root: impl AsRef<Path>,
    store: Arc<dyn SecretStore>,
    mutate: F,
) -> Result<(), ProviderInstallError>
where
    F: FnOnce(&mut ProviderSettings) -> Result<Vec<SecretMutation>, ConfigError>,
{
    run_provider_settings_transaction_with_policy(
        paths,
        workspace_root,
        store,
        InvalidProviderSettingsPolicy::ReplaceJson,
        mutate,
    )
    .await
}

pub(crate) async fn run_provider_settings_transaction_with_policy<F>(
    paths: &ProviderConfigPaths,
    workspace_root: impl AsRef<Path>,
    store: Arc<dyn SecretStore>,
    invalid_settings_policy: InvalidProviderSettingsPolicy,
    mutate: F,
) -> Result<(), ProviderInstallError>
where
    F: FnOnce(&mut ProviderSettings) -> Result<Vec<SecretMutation>, ConfigError>,
{
    let workspace_root = workspace_root.as_ref().to_path_buf();
    let _lock = acquire_provider_settings_lock_async(paths.user_config.clone()).await?;
    let loaded = match invalid_settings_policy {
        InvalidProviderSettingsPolicy::Reject => LoadedProviderSettings {
            settings: load_or_migrate_provider_settings_unlocked(paths, store.as_ref())
                .map_err(|error| provider_install_error("load", error.to_string()))?,
            rollback_bytes: None,
        },
        InvalidProviderSettingsPolicy::ReplaceJson => {
            load_provider_settings_for_install_unlocked(paths, store.as_ref())
                .map_err(|error| provider_install_error("load", error.to_string()))?
        }
    };
    let user_snapshot =
        snapshot_provider_settings_file(&paths.user_config, loaded.settings, loaded.rollback_bytes)
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

pub(crate) fn snapshot_provider_settings_file(
    path: &Path,
    settings: ProviderSettings,
    rollback_bytes: Option<Vec<u8>>,
) -> Result<ProviderSettingsSnapshot, ConfigError> {
    Ok(ProviderSettingsSnapshot {
        existed: path.exists(),
        settings,
        rollback_bytes,
    })
}

pub(crate) fn persist_provider_settings_file(
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

pub(crate) fn restore_provider_settings_file(
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
    if let Some(rollback_bytes) = &snapshot.rollback_bytes {
        return write_owner_only(path, rollback_bytes);
    }
    snapshot.settings.save_unlocked(path)
}

pub(crate) async fn acquire_provider_settings_lock_async(
    path: PathBuf,
) -> Result<File, ProviderInstallError> {
    tokio::task::spawn_blocking(move || acquire_provider_settings_lock(&path))
        .await
        .map_err(|error| provider_install_error("lock", error.to_string()))?
        .map_err(|error| provider_install_error("lock", error.to_string()))
}

pub(crate) fn acquire_provider_settings_lock(path: &Path) -> Result<File, ConfigError> {
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

pub(crate) async fn probe_provider_after_settings_update(
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

pub(crate) fn provider_install_error(
    step: &'static str,
    message: impl Into<String>,
) -> ProviderInstallError {
    ProviderInstallError {
        step,
        message: message.into(),
    }
}

pub(crate) fn persist_profile_in_settings(
    settings: &mut ProviderSettings,
    profile: ProviderProfile,
    activate: bool,
) -> Result<(), ConfigError> {
    profile.validate()?;
    settings.upsert_profile(profile, activate);
    Ok(())
}

pub(crate) fn replaced_credential(
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

pub(crate) fn write_json_owner_only<T: Serialize>(
    path: &Path,
    value: &T,
) -> Result<(), ConfigError> {
    let mut content =
        serde_json::to_vec_pretty(value).map_err(|error| ConfigError::Json(error.to_string()))?;
    content.push(b'\n');
    write_owner_only(path, &content)
}

pub(crate) fn write_owner_only(path: &Path, content: &[u8]) -> Result<(), ConfigError> {
    let parent = normalized_parent(path);
    fs::create_dir_all(parent).map_err(|error| ConfigError::Io(error.to_string()))?;
    set_owner_only_dir(parent)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)
        .map_err(|error| ConfigError::Io(error.to_string()))?;
    temporary
        .write_all(content)
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

pub(crate) fn normalized_parent(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

#[cfg(unix)]
pub(crate) fn sync_directory(path: &Path) -> Result<(), ConfigError> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| ConfigError::Io(error.to_string()))
}

#[cfg(not(unix))]
pub(crate) fn sync_directory(_path: &Path) -> Result<(), ConfigError> {
    Ok(())
}

#[cfg(unix)]
pub(crate) fn set_owner_only_file(path: &Path) -> Result<(), ConfigError> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .map_err(|error| ConfigError::Io(error.to_string()))
}

#[cfg(not(unix))]
pub(crate) fn set_owner_only_file(_path: &Path) -> Result<(), ConfigError> {
    Ok(())
}

#[cfg(unix)]
pub(crate) fn set_owner_only_dir(path: &Path) -> Result<(), ConfigError> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(|error| ConfigError::Io(error.to_string()))
}

#[cfg(not(unix))]
pub(crate) fn set_owner_only_dir(_path: &Path) -> Result<(), ConfigError> {
    Ok(())
}
