use std::{fs, path::Path};

use golutra_llm::ModelCatalog;
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("config io failed: {0}")]
    Io(String),
    #[error("config json failed: {0}")]
    Json(String),
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

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

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
}
