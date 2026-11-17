//! 宿主连接配置与就绪检查；Agent 通过会话路由修改浏览器偏好，不改程序路径或账号。
use super::opencli::OpenCliConfig;
use super::{
    EgoBrowserBackend, EgoBrowserConfig,
    state::{io_error, private_directory},
};
use crate::{ExternalToolBackend, ToolError, ToolRequest};
use golutra_agent_core::{SessionId, ToolCallId};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChromeConfig {
    pub backend: ChromeKind,
    pub connection_dir: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ChromeKind {
    #[default]
    Chrome,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DisabledConfig {
    pub backend: DisabledKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DisabledKind {
    Disabled,
}

pub const CHROME_UNAVAILABLE_GUIDANCE: &str = super::opencli::SETUP;
pub const EXTENSION_GUIDANCE: &str = "Chrome 使用 OpenCLI 扩展；Ego 使用原生 SDK，不需要 OpenCLI 扩展。若两边同时启用后反复重连，请停用 Ego 中的 OpenCLI 扩展，再检查 Chrome 连接。";
pub const INSTALL_EGO_GUIDANCE: &str = "未发现 Ego 或 Chrome。是否同意从 Ego 官网下载并安装 Ego？同意后运行 golutra-agent browser install-ego --yes；TUI 使用 /connect install-ego 确认安装。未获得同意不要下载或安装。";

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct AutoConfig {
    pub backend: AutoKind,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum AutoKind {
    #[default]
    Auto,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum BrowserConfig {
    Disabled(DisabledConfig),
    Auto(AutoConfig),
    Chrome(OpenCliConfig),
    Ego(EgoBrowserConfig),
}

impl BrowserConfig {
    pub fn backend_name(&self) -> &'static str {
        match self {
            Self::Disabled(_) => "disabled",
            Self::Auto(_) => "auto",
            Self::Chrome(_) => "chrome",
            Self::Ego(_) => "ego",
        }
    }

    /// 自动模式优先已安装的 Ego，其次 Chrome；显式选择和禁用不被覆盖。
    pub fn effective(home: &Path) -> Result<Option<Self>, ToolError> {
        Self::effective_with(home, Self::automatic)
    }

    fn effective_with(
        home: &Path,
        discover: impl FnOnce() -> Self,
    ) -> Result<Option<Self>, ToolError> {
        match Self::load(home)? {
            Some(Self::Disabled(_)) => Ok(None),
            Some(Self::Auto(_)) | None => Ok(Some(discover())),
            Some(config) => Ok(Some(config)),
        }
    }

    pub fn automatic() -> Self {
        let ego = discover_ego();
        let ego_installed =
            ego.is_some() || super::launch::discover(super::launch::BrowserKind::Ego, None).is_ok();
        let chrome_installed =
            super::launch::discover(super::launch::BrowserKind::Chrome, None).is_ok();
        automatic_selection(ego, ego_installed, chrome_installed)
    }

    pub fn load(home: &Path) -> Result<Option<Self>, ToolError> {
        let path = home.join("browser.json");
        if !path.exists() {
            return Ok(None);
        }
        if std::fs::metadata(&path).map_err(io_error)?.len() > 16 * 1024 {
            return Err(ToolError::InvalidArguments(
                "browser.json exceeds 16 KiB".into(),
            ));
        }
        let value: Self = super::state::read_json(&path)?;
        match &value {
            Self::Ego(config) => config.validate()?,
            Self::Chrome(config) => config.validate()?,
            _ => (),
        }
        Ok(Some(value))
    }

    pub fn save(&self, home: &Path) -> Result<(), ToolError> {
        use std::io::Write;
        private_directory(home)?;
        let mut file = tempfile::NamedTempFile::new_in(home).map_err(io_error)?;
        serde_json::to_writer_pretty(&mut file, self)
            .map_err(|e| ToolError::Execution(e.to_string()))?;
        file.flush().map_err(io_error)?;
        file.as_file().sync_all().map_err(io_error)?;
        file.persist(home.join("browser.json"))
            .map_err(|e| io_error(e.error))?;
        Ok(())
    }
}

pub fn discover_ego() -> Option<PathBuf> {
    let mut paths: Vec<_> = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|dir| {
            dir.join(if cfg!(windows) {
                "ego-browser.exe"
            } else {
                "ego-browser"
            })
        })
        .collect();
    if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
        paths.push(PathBuf::from(home).join(".local/bin/ego-browser"));
    }
    if cfg!(target_os = "macos")
        && let Ok(app) = super::launch::discover(super::launch::BrowserKind::Ego, None)
    {
        paths.push(app.join(
            "Contents/Frameworks/ego Framework.framework/Versions/Current/Helpers/ego-browser",
        ));
    }
    paths.into_iter().find(|path| {
        if !path.is_absolute() || !path.is_file() {
            return false;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            path.metadata()
                .is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
        }
        #[cfg(not(unix))]
        {
            true
        }
    })
}

fn automatic_selection(
    ego: Option<PathBuf>,
    ego_installed: bool,
    chrome_installed: bool,
) -> BrowserConfig {
    if let Some(executable) = ego {
        BrowserConfig::Ego(EgoBrowserConfig {
            executable,
            sdk_path: None,
        })
    } else if !ego_installed && chrome_installed {
        BrowserConfig::Chrome(OpenCliConfig::default())
    } else {
        // 缺少浏览器或 Ego 尚未完成安装/引导时保留设置入口，不能静默改用其他浏览器。
        BrowserConfig::Auto(AutoConfig::default())
    }
}

pub fn setup_status() -> Value {
    let installed = super::launch::discover(super::launch::BrowserKind::Ego, None).is_ok();
    json!({"backend":"auto", "preferred_backend":"ego", "status":if installed {"setup_required"} else {"install_required"},
        "installation_requires_confirmation":!installed,
        "message":if installed {"已安装 Ego，但原生启动器尚未就绪。请打开 Ego 完成首次引导，然后运行 /connect auto；不会改用 Chrome。"} else {INSTALL_EGO_GUIDANCE},
        "next":if installed {"golutra-agent browser connect ego --launch --wait 120"} else {"golutra-agent browser install-ego"},
        "guidance":EXTENSION_GUIDANCE})
}

pub async fn probe_ego(config: EgoBrowserConfig, home: &Path) -> Result<Value, ToolError> {
    // 探测不绑定工作区/任务，也不创建浏览器页面；临时状态用完即可回收。
    private_directory(home)?;
    let scratch = tempfile::tempdir_in(home).map_err(io_error)?;
    let backend =
        EgoBrowserBackend::new(config, scratch.path(), &scratch.path().join("state"), true)?;
    let output = backend
        .call(
            &ToolRequest {
                tool_call_id: ToolCallId::new(),
                provider_tool_call_id: None,
                session_id: SessionId::new(),
                turn_id: None,
                tool_name: "browser_status".into(),
                arguments: json!({"timeout_ms":10_000}),
            },
            CancellationToken::new(),
        )
        .await?;
    let status = if !output.is_error {
        "ready"
    } else if output.content.contains("complete the onboarding") {
        "setup_required"
    } else {
        "unavailable"
    };
    Ok(
        json!({"backend":"ego", "status":status, "version":output.structured_facts["browser"]["browser_version"],
        "message": if status == "setup_required" { "请在 Ego 窗口完成首次引导；数据导入和设为默认浏览器可以跳过。完成后重新连接。" } else { &output.content }, "guidance":EXTENSION_GUIDANCE}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn automatic_selection_prioritizes_ego_and_keeps_setup_distinct_from_absence() {
        assert!(matches!(
            automatic_selection(Some("/ego".into()), true, true),
            BrowserConfig::Ego(_)
        ));
        assert!(matches!(
            automatic_selection(None, false, true),
            BrowserConfig::Chrome(_)
        ));
        assert!(matches!(
            automatic_selection(None, false, false),
            BrowserConfig::Auto(_)
        ));
        assert!(matches!(
            automatic_selection(None, true, true),
            BrowserConfig::Auto(_)
        ));
    }
    #[test]
    fn automatic_respects_explicit_selection_and_disable() {
        let root = tempfile::tempdir().unwrap();
        assert!(matches!(
            BrowserConfig::effective_with(root.path(), || automatic_selection(None, false, true))
                .unwrap(),
            Some(BrowserConfig::Chrome(_))
        ));
        assert!(!root.path().join("browser.json").exists());
        BrowserConfig::Ego(EgoBrowserConfig {
            executable: root.path().join("explicit-ego"),
            sdk_path: None,
        })
        .save(root.path())
        .unwrap();
        assert!(matches!(
            BrowserConfig::effective_with(root.path(), || panic!(
                "explicit selection must not discover"
            ))
            .unwrap(),
            Some(BrowserConfig::Ego(_))
        ));
        BrowserConfig::Chrome(OpenCliConfig::default())
            .save(root.path())
            .unwrap();
        assert!(matches!(
            BrowserConfig::effective_with(root.path(), || panic!(
                "explicit Chrome must not discover"
            ))
            .unwrap(),
            Some(BrowserConfig::Chrome(_))
        ));
        BrowserConfig::Auto(AutoConfig::default())
            .save(root.path())
            .unwrap();
        assert!(matches!(
            BrowserConfig::effective_with(root.path(), || automatic_selection(
                Some("/ego".into()),
                true,
                true
            ))
            .unwrap(),
            Some(BrowserConfig::Ego(_))
        ));
        BrowserConfig::Disabled(DisabledConfig {
            backend: DisabledKind::Disabled,
        })
        .save(root.path())
        .unwrap();
        assert!(
            BrowserConfig::effective_with(root.path(), || panic!("disabled must not discover"))
                .unwrap()
                .is_none()
        );
    }
    #[test]
    fn selection_preserves_legacy_ego_and_rejects_unknown_backend_or_relative_path() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("browser.json");
        std::fs::write(
            &path,
            json!({"executable":root.path().join("ego")}).to_string(),
        )
        .unwrap();
        assert!(matches!(
            BrowserConfig::load(root.path()).unwrap(),
            Some(BrowserConfig::Ego(_))
        ));
        BrowserConfig::Chrome(OpenCliConfig {
            connection_dir: Some(root.path().join("bridge")),
            ..Default::default()
        })
        .save(root.path())
        .unwrap();
        assert!(matches!(
            BrowserConfig::load(root.path()).unwrap(),
            Some(BrowserConfig::Chrome(_))
        ));
        for value in [
            r#"{"backend":"unknown","connection_dir":"/bridge"}"#,
            r#"{"backend":"chrome","connection_dir":"relative"}"#,
            r#"{"backend":"chrome","executable":"relative"}"#,
        ] {
            std::fs::write(&path, value).unwrap();
            assert!(BrowserConfig::load(root.path()).is_err());
        }
    }
}
