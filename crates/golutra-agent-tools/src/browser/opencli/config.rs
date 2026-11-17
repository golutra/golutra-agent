//! Chrome 的 OpenCLI 配置；旧 connection_dir 仅用于迁移，不再连接自研扩展。
use crate::ToolError;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct OpenCliConfig {
    pub backend: super::super::connection::ChromeKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executable: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    #[serde(default, skip_serializing)]
    pub connection_dir: Option<PathBuf>,
}

impl OpenCliConfig {
    pub fn validate(&self) -> Result<(), ToolError> {
        if self
            .executable
            .iter()
            .chain(self.connection_dir.iter())
            .any(|p| !p.is_absolute())
        {
            return Err(ToolError::InvalidArguments(
                "OpenCLI paths must be absolute".into(),
            ));
        }
        if self.profile.as_ref().is_some_and(|v| {
            v.is_empty() || v.len() > 128 || v.starts_with('-') || v.chars().any(char::is_control)
        }) {
            return Err(ToolError::InvalidArguments(
                "Invalid OpenCLI profile".into(),
            ));
        }
        Ok(())
    }

    pub fn executable(&self) -> Option<PathBuf> {
        if let Some(path) = &self.executable {
            return Some(path.clone());
        }
        if let Some(path) = std::env::var_os("GOLUTRA_AGENT_OPENCLI_EXECUTABLE") {
            return Some(PathBuf::from(path));
        }
        // 独立原生安装与 npm 分发都优先自己的固定版本，不覆盖用户全局 OpenCLI。
        if let Ok(home) = super::super::chrome::install::user_home() {
            let path = home.join(
                ".golutra-agent/browser/opencli/node_modules/@jackwener/opencli/dist/src/main.js",
            );
            if path.is_file() {
                return Some(path);
            }
        }
        if cfg!(windows) {
            let shim = super::process::find_on_path("opencli.cmd")?;
            let js = shim
                .parent()?
                .join("node_modules/@jackwener/opencli/dist/src/main.js");
            js.is_file().then_some(js)
        } else {
            super::process::find_on_path("opencli")
        }
    }
}
