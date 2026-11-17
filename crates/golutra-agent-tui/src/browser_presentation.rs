//! User-facing projection of the private, versioned browser CLI result.
//! Tool instructions and raw subprocess output belong only in explicit diagnostics.
use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(super) enum State {
    Ready,
    SetupRequired,
    ProfileRequired,
    InstallRequired,
    InstallConfirmationRequired,
    LaunchRequested,
    Installed,
    AlreadyInstalled,
    InstallHelp,
    Disabled,
    Cancelled,
    Unavailable,
    Error,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(super) enum Backend {
    Ego,
    Chrome,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Selection {
    Auto,
    Ego,
    Chrome,
    Disabled,
}

#[derive(Debug, Deserialize)]
pub(super) struct BrowserReport {
    version: u8,
    ok: bool,
    pub state: State,
    backend: Option<Backend>,
    selection: Option<Selection>,
    saved: bool,
    timed_out: bool,
    diagnostics: Value,
}

impl BrowserReport {
    pub fn parse(output: &str, success: bool) -> Result<Self, String> {
        let report: Self = serde_json::from_str(output).map_err(|error| error.to_string())?;
        if report.version != 1 || report.ok != success {
            return Err("浏览器结果版本或进程退出状态不一致".into());
        }
        if (report.state == State::Ready
            && (!report.ok
                || report.timed_out
                || !matches!(
                    (&report.selection, &report.backend),
                    (Some(Selection::Auto), Some(_))
                        | (Some(Selection::Chrome), Some(Backend::Chrome))
                        | (Some(Selection::Ego), Some(Backend::Ego))
                )))
            || (report.state == State::InstallConfirmationRequired && report.ok)
        {
            return Err("浏览器结果缺少必要的状态信息".into());
        }
        Ok(report)
    }

    pub fn lines(&self, unrestricted: bool) -> Vec<String> {
        let browser = match self.backend {
            Some(Backend::Ego) => "Ego",
            Some(Backend::Chrome) => "Chrome",
            None => "浏览器",
        };
        let retry = match (&self.selection, &self.backend) {
            (Some(Selection::Auto), _) => "/connect auto",
            (_, Some(Backend::Chrome)) => "/connect chrome",
            (_, Some(Backend::Ego)) => "/connect ego",
            _ => "/connect",
        };
        let mut lines = match self.state {
            State::Ready => {
                let selection = match self.selection {
                    Some(Selection::Auto) => "自动（优先 Ego，其次 Chrome）",
                    Some(Selection::Chrome) => "固定使用 Chrome",
                    Some(Selection::Ego) => "固定使用 Ego",
                    _ => "未指定",
                };
                let mut lines = vec![
                    format!("✓ 已连接 {browser}"),
                    "沿用现有浏览器，保留登录状态。".into(),
                    format!("浏览器选择：{selection}{}", if self.saved { " · 已保存" } else { "" }),
                ];
                if !unrestricted {
                    lines.push("网页操作尚未授权；/permissions unrestricted 可开放本会话的执行权限。".into());
                }
                lines
            }
            State::SetupRequired => {
                let next = if self.backend == Some(Backend::Chrome) {
                    if self.diagnostics.get("doctor").is_some() {
                        format!("请在 Chrome 中启用 OpenCLI 扩展，再运行 {retry}；安装入口：/connect install-chrome。")
                    } else {
                        "缺少 Chrome 连接组件；运行 /connect install-chrome 查看安装方法。".into()
                    }
                } else {
                    format!("请在 Ego 窗口完成首次引导，再运行 {retry}；无需安装 Chrome 扩展。")
                };
                vec![format!("尚未连接 {browser}{}", if self.timed_out { "（等待超时）" } else { "" }), next]
            }
            State::ProfileRequired => vec![
                "尚未连接 Chrome：需要选择浏览器 Profile。".into(),
                "用 /connect details 查看可用 Profile，再在终端运行 golutra-agent browser connect chrome --profile <名称>。".into(),
            ],
            State::InstallRequired => vec![
                "未发现 Ego 或 Chrome。".into(),
                "运行 /connect install-ego 确认下载安装 Ego。".into(),
            ],
            State::InstallConfirmationRequired => vec![
                "是否同意从 Ego 官网下载并安装浏览器？尚未开始下载；安装后需要完成首次引导。".into(),
                "同意请输入 /connect confirm-install-ego；取消请输入 /connect cancel。".into(),
            ],
            State::LaunchRequested => vec![
                format!("已请求打开 {browser}，沿用现有登录状态。"),
                format!("连接尚未验证；运行 {retry} 检查。"),
            ],
            State::Installed => vec![
                "Ego 已安装并启动。".into(),
                "完成首次引导后运行 /connect auto 检查连接。".into(),
            ],
            State::AlreadyInstalled => vec![
                "Ego 已安装，无需重复下载。".into(),
                "运行 /connect ego 打开并连接。".into(),
            ],
            State::InstallHelp => vec![
                "在 Chrome 中安装并启用 OpenCLI 官方扩展：".into(),
                "https://chromewebstore.google.com/detail/opencli/ildkmabpimmkaediidaifkhjpohdnifk".into(),
                "完成后运行 /connect chrome。Ego 无需此扩展。".into(),
                "独立二进制缺少连接组件时，在终端运行 npm install -g @jackwener/opencli@2.0.3。".into(),
            ],
            State::Disabled => vec![
                "浏览器工具已禁用。".into(),
                "运行 /connect auto、ego 或 chrome 可重新连接。".into(),
            ],
            State::Cancelled => vec!["操作已取消。已完成的安装或设置会保留；/connect status 可检查当前状态。".into()],
            State::Unavailable => vec![format!("{browser} 暂时无法连接{}。", if self.timed_out { "（等待超时）" } else { "" })],
            State::Error => vec!["浏览器操作未完成。".into()],
        };
        if matches!(
            self.state,
            State::SetupRequired | State::Unavailable | State::Error
        ) {
            lines.push(format!(
                "/connect details 查看原因；处理后运行 {retry} 重试。"
            ));
        }
        lines
    }
}
