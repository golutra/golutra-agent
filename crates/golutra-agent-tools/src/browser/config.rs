use std::{
    io::Read,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use super::super::ToolError;

/// 只从宿主配置读取启动器，网页和模型参数不能改变可执行程序。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EgoBrowserConfig {
    pub executable: PathBuf,
    #[serde(default)]
    pub sdk_path: Option<PathBuf>,
}

impl EgoBrowserConfig {
    pub fn load(home: &Path) -> Result<Option<Self>, ToolError> {
        let path = home.join("browser.json");
        match std::fs::File::open(&path) {
            Ok(file) => {
                let mut bytes = Vec::new();
                file.take(16 * 1024 + 1)
                    .read_to_end(&mut bytes)
                    .map_err(|error| ToolError::Execution(format!("browser.json: {error}")))?;
                if bytes.len() > 16 * 1024 {
                    return Err(ToolError::InvalidArguments(
                        "browser.json exceeds 16 KiB".to_owned(),
                    ));
                }
                serde_json::from_slice(&bytes)
                    .map(Some)
                    .map_err(|error| ToolError::InvalidArguments(format!("browser.json: {error}")))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(ToolError::Execution(format!("browser.json: {error}"))),
        }
    }

    pub(super) fn validate(&self) -> Result<(), ToolError> {
        for path in std::iter::once(&self.executable).chain(self.sdk_path.iter()) {
            if !path.is_absolute() {
                return Err(ToolError::InvalidArguments(
                    "Ego executable and SDK paths must be absolute".to_owned(),
                ));
            }
        }
        Ok(())
    }

    pub(super) fn args(&self, command: &str) -> Vec<String> {
        let mut args = vec![command.to_owned()];
        if let Some(path) = &self.sdk_path {
            args.extend(["--sdk-path".to_owned(), path.to_string_lossy().into_owned()]);
        }
        args
    }
}
