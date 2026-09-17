//! 非敏感设置的锁内版本比较与字段补丁；不允许桌面直接改凭据文件。

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::{
    ConfigError, NonSecretRuntimeSettings, ProviderConfigPaths, acquire_provider_settings_lock,
    load_non_secret_runtime_layer, write_json_owner_only,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeSettingsSnapshot {
    pub revision: String,
    pub settings: NonSecretRuntimeSettings,
}

pub(crate) fn content_revision(value: &impl Serialize) -> Result<String, ConfigError> {
    let bytes = serde_json::to_vec(value).map_err(|error| ConfigError::Json(error.to_string()))?;
    Ok(format!("sha256:{:x}", Sha256::digest(bytes)))
}

pub(crate) fn require_revision(expected: &str, actual: &str) -> Result<(), ConfigError> {
    if expected != actual {
        return Err(ConfigError::VersionConflict {
            expected: expected.to_owned(),
            actual: actual.to_owned(),
        });
    }
    Ok(())
}

fn snapshot(settings: NonSecretRuntimeSettings) -> Result<RuntimeSettingsSnapshot, ConfigError> {
    Ok(RuntimeSettingsSnapshot {
        revision: content_revision(&settings)?,
        settings,
    })
}

/// 返回全局层及内容 revision；项目/session 覆盖不混入待编辑快照。
pub fn read_runtime_settings(
    paths: &ProviderConfigPaths,
) -> Result<RuntimeSettingsSnapshot, ConfigError> {
    let path = paths.home.join("runtime.json");
    let _lock = acquire_provider_settings_lock(&path)?;
    snapshot(load_non_secret_runtime_layer(&path, Some(&paths.home))?)
}

/// 只修改给定字段；null 清除覆盖。比较、合并、校验和原子替换处于同一文件锁内。
pub fn patch_runtime_settings(
    paths: &ProviderConfigPaths,
    expected_revision: &str,
    patch: Map<String, Value>,
) -> Result<RuntimeSettingsSnapshot, ConfigError> {
    let path = paths.home.join("runtime.json");
    let _lock = acquire_provider_settings_lock(&path)?;
    let current = load_non_secret_runtime_layer(&path, Some(&paths.home))?;
    require_revision(expected_revision, &content_revision(&current)?)?;
    let mut value =
        serde_json::to_value(current).map_err(|error| ConfigError::Json(error.to_string()))?;
    let fields = value.as_object_mut().expect("settings serialize as object");
    for (key, value) in patch {
        if !fields.contains_key(&key) {
            return Err(ConfigError::Validation(format!(
                "unknown runtime setting: {key}"
            )));
        }
        fields.insert(key, value);
    }
    let settings: NonSecretRuntimeSettings =
        serde_json::from_value(value).map_err(|error| ConfigError::Json(error.to_string()))?;
    settings.validate()?;
    write_json_owner_only(&path, &settings)?;
    snapshot(settings)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_and_invalid_patches_leave_settings_unchanged() {
        let root = tempfile::tempdir().unwrap();
        let paths = ProviderConfigPaths::from_home(root.path()).unwrap();
        let first = read_runtime_settings(&paths).unwrap();
        let patch = serde_json::json!({"model":"new", "subagent_max_concurrent":10});
        let next =
            patch_runtime_settings(&paths, &first.revision, patch.as_object().unwrap().clone())
                .unwrap();
        assert!(matches!(
            patch_runtime_settings(&paths, &first.revision, Map::new()),
            Err(ConfigError::VersionConflict { .. })
        ));
        for patch in [
            serde_json::json!({"api_key":"secret"}),
            serde_json::json!({"subagent_max_concurrent":0}),
            serde_json::json!({"model":12}),
        ] {
            assert!(
                patch_runtime_settings(&paths, &next.revision, patch.as_object().unwrap().clone())
                    .is_err()
            );
            assert_eq!(
                read_runtime_settings(&paths).unwrap().revision,
                next.revision
            );
        }
        let cleared = patch_runtime_settings(
            &paths,
            &next.revision,
            serde_json::json!({"model":null})
                .as_object()
                .unwrap()
                .clone(),
        )
        .unwrap();
        assert_eq!(cleared.settings.model, None);
        assert_eq!(cleared.settings.subagent_max_concurrent, Some(10));
    }
}
