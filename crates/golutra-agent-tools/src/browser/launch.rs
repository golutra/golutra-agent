//! 启动用户日常浏览器；不指定 user-data-dir、调试端口或隐身模式，不读取账号数据。
use crate::{ExternalToolOutput, ToolError};
use serde_json::json;
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowserKind {
    Chrome,
    Ego,
}

impl BrowserKind {
    pub fn name(self) -> &'static str {
        match self {
            Self::Chrome => "chrome",
            Self::Ego => "ego",
        }
    }
}

fn candidates(kind: BrowserKind) -> Vec<PathBuf> {
    // 可选的非标准应用安装目录；同时允许进程验收完全隔离宿主浏览器。
    if let Some(root) = std::env::var_os("GOLUTRA_AGENT_BROWSER_APPLICATIONS_DIR") {
        let name = match (kind, cfg!(target_os = "macos"), cfg!(windows)) {
            (BrowserKind::Ego, true, _) => "ego lite.app",
            (BrowserKind::Chrome, true, _) => "Google Chrome.app",
            (BrowserKind::Ego, _, true) => "ego lite.exe",
            (BrowserKind::Chrome, _, true) => "chrome.exe",
            (BrowserKind::Ego, _, _) => "ego-browser",
            (BrowserKind::Chrome, _, _) => "google-chrome",
        };
        return vec![PathBuf::from(root).join(name)];
    }
    let home = super::chrome::install::user_home().ok();
    if cfg!(target_os = "macos") {
        let name = match kind {
            BrowserKind::Chrome => "Google Chrome.app",
            BrowserKind::Ego => "ego lite.app",
        };
        let mut result = vec![PathBuf::from("/Applications").join(name)];
        if let Some(home) = home {
            result.push(home.join("Applications").join(name));
        }
        return result;
    }
    if cfg!(windows) {
        let relatives: &[&str] = match kind {
            BrowserKind::Chrome => &["Google/Chrome/Application/chrome.exe"],
            BrowserKind::Ego => &[
                "Programs/ego lite/ego lite.exe",
                "ego lite/Application/ego lite.exe",
            ],
        };
        return ["LOCALAPPDATA", "ProgramFiles", "ProgramFiles(x86)"]
            .iter()
            .filter_map(std::env::var_os)
            .flat_map(|base| {
                relatives
                    .iter()
                    .map(move |relative| PathBuf::from(&base).join(relative))
            })
            .collect();
    }
    if kind == BrowserKind::Ego {
        return Vec::new();
    }
    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .flat_map(|dir| [dir.join("google-chrome"), dir.join("google-chrome-stable")])
        .collect()
}

pub fn discover(kind: BrowserKind, explicit: Option<&Path>) -> Result<PathBuf, ToolError> {
    let valid = |path: &Path| {
        path.is_absolute()
            && if cfg!(target_os = "macos") {
                path.is_dir() && path.extension().is_some_and(|extension| extension == "app")
            } else {
                path.is_file()
            }
    };
    if let Some(path) = explicit {
        if !valid(path) {
            return Err(ToolError::InvalidArguments("Browser application must be an existing absolute application path (.app on macOS).".into()));
        }
        return Ok(path.to_path_buf());
    }
    candidates(kind).into_iter().find(|path| valid(path)).ok_or_else(|| ToolError::Execution(match kind {
        BrowserKind::Chrome => "未找到已安装的 Google Chrome。请安装 Chrome，或由用户通过 browser launch chrome --application 指定应用路径；也可选择下载 Ego：https://lite.ego.app/。",
        BrowserKind::Ego => "未找到已安装的 ego lite。请从 https://lite.ego.app/ 安装，或由用户通过 browser launch ego --application 指定应用路径。Ego 当前官方支持 macOS/Windows。",
    }.into()))
}

#[derive(Debug)]
struct LaunchPlan {
    program: PathBuf,
    args: Vec<String>,
    wait_for_launcher: bool,
}

fn plan(application: &Path) -> LaunchPlan {
    if cfg!(target_os = "macos") {
        LaunchPlan {
            program: "/usr/bin/open".into(),
            args: vec!["-a".into(), application.to_string_lossy().into_owned()],
            wait_for_launcher: true,
        }
    } else {
        LaunchPlan {
            program: application.into(),
            args: vec![],
            wait_for_launcher: false,
        }
    }
}

pub async fn launch(
    kind: BrowserKind,
    explicit: Option<&Path>,
    cancel: CancellationToken,
) -> Result<ExternalToolOutput, ToolError> {
    if cancel.is_cancelled() {
        return Ok(super::failure(
            "browser_cancelled",
            "Browser launch cancelled before dispatch.",
        ));
    }
    let application = match discover(kind, explicit) {
        Ok(application) => application,
        Err(error) => {
            return Ok(super::failure(
                "browser_application_missing",
                &error.to_string(),
            ));
        }
    };
    let launch = plan(&application);
    let mut command = tokio::process::Command::new(&launch.program);
    command
        .args(&launch.args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // GUI 生命周期属于用户，不能随 Agent 任务结束而杀掉。仅 macOS 的 open 辅助程序可取消。
    command.kill_on_drop(launch.wait_for_launcher);
    #[cfg(windows)]
    command.creation_flags(0x00000008 | 0x00000200);
    let mut child = command.spawn().map_err(super::state::io_error)?;
    if launch.wait_for_launcher {
        let status = tokio::select! {
            _ = cancel.cancelled() => return Ok(super::failure("browser_launch_unknown", "Launch cancelled; the browser may already be open. Check before retrying.")),
            result = tokio::time::timeout(Duration::from_secs(10), child.wait()) => result
                .map_err(|_| ToolError::Execution("Browser launch timed out; check the application before retrying.".into()))?
                .map_err(super::state::io_error)?,
        };
        if !status.success() {
            return Ok(super::failure(
                "browser_launch_failed",
                &format!(
                    "System launcher exited with {status}; open the application manually to inspect the error."
                ),
            ));
        }
    } else {
        // 回收直启 GUI 子进程；不等待整个浏览器关闭，也不干预其 Profile 锁。
        tokio::spawn(async move {
            let _ = child.wait().await;
        });
    }
    let next = match kind {
        BrowserKind::Chrome => {
            "Chrome 已收到启动请求，沿用用户原有 Profile。请在该 Profile 安装并启用 OpenCLI 官方扩展；已安装时会自动连接，无需逐页点击授权。随后用 browser_status 检查是否 ready。安装指引：/connect install-chrome。启动不等于连接成功。"
        }
        BrowserKind::Ego => {
            "Ego 已收到启动请求，沿用其原有 Profile/登录状态。首次使用请完成引导，再运行 /connect ego。已有连接用 browser_status 检查；任务 Space 只分隔页面，不创建新 Profile。"
        }
    };
    Ok(ExternalToolOutput {
        summary: "Browser launch requested".into(),
        content: format!("{next}\n{}", super::connection::EXTENSION_GUIDANCE),
        structured_facts: json!({"backend":kind.name(),"application":application,"status":"launch_requested","profile":"existing_user_profile","connection_verified":false,"automatic_retry":false}),
        is_error: false,
        images: vec![],
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn launch_plan_preserves_user_profile_and_separates_arguments() {
        let path = Path::new("/Applications/Browser With Spaces.app");
        let p = plan(path);
        if cfg!(target_os = "macos") {
            assert_eq!(p.args, vec!["-a", "/Applications/Browser With Spaces.app"]);
        } else {
            assert!(p.args.is_empty());
            assert_eq!(p.program, path);
        }
        assert!(!p.args.iter().any(|arg| arg.contains("user-data-dir")
            || arg.contains("remote-debugging")
            || arg.contains("incognito")));
    }
    #[tokio::test]
    async fn cancellation_and_invalid_application_do_not_launch() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert_eq!(
            launch(BrowserKind::Chrome, None, cancel)
                .await
                .unwrap()
                .structured_facts["code"],
            "browser_cancelled"
        );
        assert!(discover(BrowserKind::Ego, Some(Path::new("relative-app"))).is_err());
    }
}
