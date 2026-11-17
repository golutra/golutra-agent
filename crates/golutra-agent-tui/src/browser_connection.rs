//! 连接/安装使用同版本 CLI；确认、取消、输出排空均不阻塞 TUI。
use super::*;
use tokio::io::{AsyncRead, AsyncReadExt};

#[path = "browser_presentation.rs"]
mod presentation;
use presentation::{BrowserReport, State};

const OUTPUT_TRUNCATION_NOTICE: &str = "\n[输出已截断：超出 128 KiB 上限]";

#[derive(Debug)]
pub(crate) struct ConnectionOutput {
    report: Result<BrowserReport, &'static str>,
    details: String,
}

impl ConnectionOutput {
    fn decode(output: &str, errors: &str, success: bool) -> Self {
        let mut details = format!("stdout:\n{output}\nstderr:\n{errors}");
        let report = BrowserReport::parse(output, success).map_err(|error| {
            details.push_str(&format!("\n解析失败：{error}"));
            "无法确认浏览器状态。请确认 CLI 与 TUI 来自同一版本。"
        });
        Self { report, details }
    }
}

#[derive(Debug)]
pub(crate) struct BrowserConnection {
    pub task: JoinHandle<Result<ConnectionOutput, String>>,
    pub cancellation: CancellationToken,
}

fn connection_args(target: Option<&str>, confirmed: bool) -> Option<Vec<&'static str>> {
    Some(match target {
        None => vec!["connect", "--launch", "--wait", "120"],
        Some("auto") => vec!["connect", "auto", "--launch", "--wait", "120"],
        Some("ego") => vec!["connect", "ego", "--launch", "--wait", "120"],
        Some("chrome") => vec!["connect", "chrome", "--launch", "--wait", "120"],
        Some("launch") => vec!["launch"],
        Some("launch-chrome") => vec!["launch", "chrome"],
        Some("launch-ego") => vec!["launch", "ego"],
        Some("install-chrome") => vec!["chrome", "install"],
        Some("install-ego") => vec!["install-ego"],
        Some("confirm-install-ego") if confirmed => vec!["install-ego", "--yes"],
        Some("disconnect") => vec!["disconnect"],
        Some("status") => vec!["status"],
        _ => return None,
    })
}

impl TuiApp {
    pub(crate) fn start_browser_connection(&mut self, target: Option<&str>) {
        if target == Some("cancel") {
            let pending_confirmation = self.browser_install_confirmation;
            self.browser_install_confirmation = false;
            let message = if let Some(operation) = &self.browser_connection {
                operation.cancellation.cancel();
                "正在取消连接或安装，并清理临时资源。"
            } else if pending_confirmation {
                "已取消安装，未开始下载。"
            } else {
                "没有正在进行的连接或安装。"
            };
            self.push_system_message("浏览器", vec![message.into()]);
            return;
        }
        if target == Some("details") {
            let details = self
                .browser_connection_details
                .as_deref()
                .unwrap_or("暂无连接诊断。先运行 /connect 或 /connect status。");
            let mut lines = vec!["最近一次已完成操作的诊断（不会重新连接或修改设置）：".into()];
            lines.extend(details.lines().map(str::to_owned));
            self.push_system_message("浏览器诊断", lines);
            return;
        }
        if target == Some("help") {
            self.push_system_message("连接浏览器", vec![
                "/connect — 按已有选择连接；无配置时优先 Ego，其次 Chrome".into(),
                "/connect auto — 恢复自动选择；/connect ego 或 chrome — 明确选择".into(),
                "/connect launch、launch-ego、launch-chrome — 打开原有浏览器，复用登录状态".into(),
                "/connect install-ego — 先确认，再从官网下载安装；首次引导由用户完成".into(),
                "/connect install-chrome — 查看 OpenCLI 与官方 Chrome 扩展安装入口".into(),
                "Ego 使用原生 SDK，无需 OpenCLI 扩展。Chrome 反复重连时，请停用 Ego 中的 OpenCLI 扩展。".into(),
                "/connect status — 查看连接；details — 查看最近诊断；disconnect — 禁用；cancel — 取消".into(),
            ]);
            return;
        }
        if self.browser_connection.is_some() {
            self.status_message = "连接或安装正在进行；/connect cancel 可取消".into();
            return;
        }
        let Some(args) = connection_args(target, self.browser_install_confirmation) else {
            self.push_system_message(
                "浏览器",
                vec!["尚无待确认的安装。请先运行 /connect 或 /connect install-ego。".into()],
            );
            return;
        };
        self.browser_install_confirmation = false;
        let cancellation = CancellationToken::new();
        self.browser_connection = Some(BrowserConnection {
            task: tokio::spawn(run_connection(args, cancellation.clone())),
            cancellation,
        });
        let message = match target {
            Some("confirm-install-ego") => {
                "已确认：正在从 Ego 官网下载并安装。可继续使用界面，/connect cancel 可取消。"
            }
            Some("launch" | "launch-chrome" | "launch-ego") => {
                "正在打开浏览器；启动后还需检查连接。"
            }
            None => "正在按已有选择连接浏览器… /connect cancel 可取消。",
            Some("auto") => "正在自动选择浏览器：优先 Ego，其次 Chrome… /connect cancel 可取消。",
            Some("chrome") => "正在连接 Chrome… /connect cancel 可取消。",
            Some("ego") => "正在连接 Ego… /connect cancel 可取消。",
            Some("install-chrome" | "install-ego") => "正在检查浏览器安装入口。",
            Some("disconnect") => "正在禁用浏览器工具。",
            _ => "正在检查浏览器状态。",
        };
        self.push_system_message("浏览器", vec![message.into()]);
    }

    pub(crate) async fn poll_browser_connection(&mut self) -> bool {
        if !self
            .browser_connection
            .as_ref()
            .is_some_and(|op| op.task.is_finished())
        {
            return false;
        }
        let operation = self.browser_connection.take().unwrap();
        let output = match operation.task.await {
            Ok(Ok(value)) => value,
            error => ConnectionOutput {
                report: Err("无法完成浏览器操作。"),
                details: format!("{error:?}"),
            },
        };
        self.browser_connection_details = Some(output.details);
        self.browser_install_confirmation = !operation.cancellation.is_cancelled()
            && output
                .report
                .as_ref()
                .is_ok_and(|report| report.state == State::InstallConfirmationRequired);
        let lines = match output.report {
            Ok(report)
                if report.state == State::InstallConfirmationRequired
                    && !self.browser_install_confirmation =>
            {
                vec!["已取消安装，未开始下载。".into()]
            }
            Ok(report) => report.lines(self.yolo),
            Err(message) => vec![
                message.into(),
                "/connect details 查看诊断；/connect status 重新检查状态。".into(),
            ],
        };
        self.push_system_message("浏览器", lines);
        true
    }
}

async fn run_connection(
    args: Vec<&'static str>,
    cancel: CancellationToken,
) -> Result<ConnectionOutput, String> {
    if cancel.is_cancelled() {
        return Ok(ConnectionOutput {
            report: Err("已取消操作，未启动连接或安装。"),
            details: "cancelled before starting browser CLI".into(),
        });
    }
    let executable = std::env::current_exe()
        .map_err(|e| e.to_string())?
        .with_file_name(if cfg!(windows) {
            "golutra-agent.exe"
        } else {
            "golutra-agent"
        });
    let timeout = if args.first() == Some(&"install-ego") {
        1500
    } else {
        150
    };
    let mut child = tokio::process::Command::new(executable)
        .arg("browser")
        .arg("--json")
        .args(args)
        .kill_on_drop(true)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("无法启动同版本 CLI：{e}"))?;
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let (output, errors, status) = tokio::join!(
        drain(stdout),
        drain(stderr),
        wait_child(&mut child, Duration::from_secs(timeout), cancel)
    );
    let (output, errors) = (output?, errors?);
    match status {
        Ok(status) => Ok(ConnectionOutput::decode(&output, &errors, status.success())),
        Err(error) => Ok(ConnectionOutput {
            report: Err(error.message()),
            details: format!("{error}\nstdout:\n{output}\nstderr:\n{errors}"),
        }),
    }
}

async fn drain(mut reader: impl AsyncRead + Unpin) -> Result<String, String> {
    let mut output = Vec::new();
    let mut buffer = [0; 8192];
    let mut truncated = false;
    loop {
        let size = reader.read(&mut buffer).await.map_err(|e| e.to_string())?;
        if size == 0 {
            break;
        }
        let keep = size.min((128usize * 1024).saturating_sub(output.len()));
        truncated |= keep < size;
        output.extend_from_slice(&buffer[..keep]);
        // 超出展示上限仍排空，不能堵住下载器或 CLI 的 stderr。
    }
    let mut text = String::from_utf8_lossy(&output).into_owned();
    if truncated {
        // Never accept a valid-looking JSON prefix after dropping the suffix.
        // Also make truncation visible when inspecting captured stderr.
        text.push_str(OUTPUT_TRUNCATION_NOTICE);
    }
    Ok(text)
}

#[derive(Debug)]
enum WaitFailure {
    Cancelled,
    TimedOut,
    Io(std::io::Error),
    ForcedStop(Box<WaitFailure>),
}

impl WaitFailure {
    fn message(&self) -> &'static str {
        match self {
            Self::Cancelled => "已停止等待。已完成的安装或设置会保留。",
            Self::TimedOut => "连接或安装超时，已停止等待。",
            Self::Io(_) => "无法读取浏览器进程状态。",
            Self::ForcedStop(_) => {
                "已强制停止连接或安装；请检查系统安装窗口或未卸载的 Ego 安装卷。"
            }
        }
    }
}

impl std::fmt::Display for WaitFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "{error}"),
            Self::ForcedStop(reason) => write!(formatter, "{reason}；{}", self.message()),
            _ => formatter.write_str(self.message()),
        }
    }
}

async fn wait_child(
    child: &mut tokio::process::Child,
    timeout: Duration,
    cancel: CancellationToken,
) -> Result<std::process::ExitStatus, WaitFailure> {
    let reason = tokio::select! {
        result = child.wait() => return result.map_err(WaitFailure::Io),
        _ = cancel.cancelled() => WaitFailure::Cancelled,
        _ = tokio::time::sleep(timeout) => WaitFailure::TimedOut,
    };
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        let _ = nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(pid as i32),
            nix::sys::signal::Signal::SIGINT,
        );
        // 安装器收到信号后先停止子进程，再卸载只读安装卷。
        if let Ok(result) = tokio::time::timeout(Duration::from_secs(40), child.wait()).await {
            let status = result.map_err(WaitFailure::Io)?;
            // A complete, successful response wins a late cancellation request.
            return if status.success() {
                Ok(status)
            } else {
                Err(reason)
            };
        }
    }
    let _ = child.kill().await;
    let _ = child.wait().await;
    Err(WaitFailure::ForcedStop(Box::new(reason)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(state: &str, ok: bool) -> serde_json::Value {
        serde_json::json!({
            "version":1,"ok":ok,"state":state,"backend":"chrome",
            "selection":"chrome","saved":state == "ready","timed_out":false,
            "diagnostics":{"executable":"/private/tool.js","message":"Agent 可用 browser_config；install_confirmation_required:","launch":{"status":"launch_requested"}}
        })
    }

    fn app() -> TuiApp {
        TuiApp::new(
            ThreadId::new(),
            SessionId::new(),
            None,
            false,
            "mock".into(),
            None,
        )
    }

    async fn deliver(app: &mut TuiApp, value: serde_json::Value, cancelled: bool) {
        let cancellation = CancellationToken::new();
        if cancelled {
            cancellation.cancel();
        }
        let success = value["ok"] == true;
        app.browser_connection = Some(BrowserConnection {
            task: tokio::spawn(async move {
                Ok(ConnectionOutput::decode(
                    &value.to_string(),
                    "old startup hints",
                    success,
                ))
            }),
            cancellation,
        });
        tokio::time::timeout(Duration::from_secs(5), async {
            while !app.poll_browser_connection().await {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn ready_shows_actual_selection_and_permissions_without_tool_instructions() {
        let mut app = app();
        deliver(&mut app, fixture("ready", true), false).await;
        let text = app.command_messages.last().unwrap().body.join("\n");
        assert!(text.contains("已连接 Chrome"));
        assert!(text.contains("固定使用 Chrome · 已保存"));
        assert!(text.contains("/permissions unrestricted"));
        for hidden in [
            "Agent",
            "browser_config",
            "launch_requested",
            "old startup",
            "tool.js",
            "自动",
            "--yolo",
            "{",
        ] {
            assert!(!text.contains(hidden), "unexpected {hidden}: {text}");
        }
        assert!(!app.browser_install_confirmation);
        assert!(!app.yolo);
        let before = app.browser_connection_details.clone();
        app.start_browser_connection(Some("details"));
        assert!(app.browser_connection.is_none());
        assert_eq!(app.browser_connection_details, before);
        assert!(
            app.command_messages
                .last()
                .unwrap()
                .body
                .join("\n")
                .contains("tool.js")
        );

        app.yolo = true;
        let mut automatic = fixture("ready", true);
        automatic["selection"] = serde_json::json!("auto");
        automatic["saved"] = serde_json::json!(false);
        deliver(&mut app, automatic, false).await;
        let text = app.command_messages.last().unwrap().body.join("\n");
        assert!(text.contains("自动（优先 Ego，其次 Chrome）"));
        assert!(!text.contains("permissions"));
        assert!(!text.contains("已保存"));
    }

    #[tokio::test]
    async fn only_explicit_install_state_requests_consent_and_cancel_discards_late_prompt() {
        let mut app = app();
        deliver(&mut app, fixture("error", false), false).await;
        assert!(!app.browser_install_confirmation);
        deliver(
            &mut app,
            fixture("install_confirmation_required", false),
            false,
        )
        .await;
        assert!(app.browser_install_confirmation);
        app.start_browser_connection(Some("cancel"));
        assert!(!app.browser_install_confirmation);
        assert!(app.browser_connection.is_none());
        deliver(
            &mut app,
            fixture("install_confirmation_required", false),
            true,
        )
        .await;
        assert!(!app.browser_install_confirmation);
        assert!(app.command_messages.last().unwrap().body[0].contains("已取消"));
        app.start_browser_connection(Some("confirm-install-ego"));
        assert!(app.browser_connection.is_none());
        // A result that committed before a late cancel still reports the true outcome.
        deliver(&mut app, fixture("ready", true), true).await;
        assert!(app.command_messages.last().unwrap().body[0].contains("已连接 Chrome"));
    }

    #[tokio::test]
    async fn active_task_still_allows_cancelling_connection_and_reading_diagnostics() {
        let transport = RuntimeTransport::in_memory().await.unwrap();
        let mut app = app();
        app.projection = Some(UserProjection {
            session_id: app.session_id,
            task_id: Some(TaskId::new()),
            status: golutra_agent_core::TaskStatus::Running,
            visible_steps: Vec::new(),
            pending_approval: None,
            final_message: None,
            residual_risks: Vec::new(),
        });
        let cancellation = CancellationToken::new();
        let waiting = cancellation.clone();
        app.browser_connection_details = Some("cached diagnostics".into());
        app.browser_connection = Some(BrowserConnection {
            task: tokio::spawn(async move {
                waiting.cancelled().await;
                Ok(ConnectionOutput::decode(
                    &fixture("cancelled", false).to_string(),
                    "",
                    false,
                ))
            }),
            cancellation: cancellation.clone(),
        });
        for target in ["help", "details", "cancel"] {
            app.execute_slash_command(
                &transport,
                SlashCommand::Connect {
                    target: Some(target.into()),
                },
            )
            .await
            .unwrap();
        }
        assert!(cancellation.is_cancelled());
        tokio::time::timeout(Duration::from_secs(5), async {
            while !app.poll_browser_connection().await {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        app.execute_slash_command(
            &transport,
            SlashCommand::Connect {
                target: Some("chrome".into()),
            },
        )
        .await
        .unwrap();
        assert!(app.browser_connection.is_none());
        assert!(app.command_messages.last().unwrap().body[0].contains("空闲会话"));
        transport.close().await.unwrap();
    }

    #[test]
    fn process_status_and_protocol_failures_cannot_claim_a_connection() {
        let value = fixture("ready", true);
        assert!(
            ConnectionOutput::decode(&value.to_string(), "", false)
                .report
                .is_err()
        );
        for raw in ["", "{", "启动成功\n{}", "{}\n{}"] {
            assert!(ConnectionOutput::decode(raw, "", true).report.is_err());
        }
        for (field, replacement) in [
            ("version", serde_json::json!(2)),
            ("backend", serde_json::Value::Null),
            ("state", serde_json::json!("future_state")),
            ("selection", serde_json::Value::Null),
        ] {
            let mut malformed = value.clone();
            malformed[field] = replacement;
            assert!(
                ConnectionOutput::decode(&malformed.to_string(), "", true)
                    .report
                    .is_err()
            );
        }
    }

    #[test]
    fn launch_install_setup_and_timeout_never_claim_readiness() {
        for (state, ok) in [
            ("launch_requested", true),
            ("installed", true),
            ("already_installed", true),
            ("setup_required", false),
            ("unavailable", false),
            ("profile_required", false),
            ("disabled", true),
        ] {
            let mut value = fixture(state, ok);
            value["timed_out"] = serde_json::json!(!ok);
            let report = BrowserReport::parse(&value.to_string(), ok).unwrap();
            let lines = report.lines(false).join("\n");
            assert!(!lines.contains("✓"));
            assert!(!lines.contains("网页操作尚未授权"));
            assert!(!lines.contains("browser_config"));
        }
        let mut setup = fixture("setup_required", false);
        setup["timed_out"] = serde_json::json!(true);
        setup["diagnostics"]["doctor"] = serde_json::json!({"data":{"extensionConnected":false}});
        let lines = BrowserReport::parse(&setup.to_string(), false)
            .unwrap()
            .lines(false)
            .join("\n");
        assert!(lines.contains("等待超时"));
        assert!(lines.contains("启用 OpenCLI 扩展"));
    }
    #[test]
    fn connect_accepts_only_supported_targets_and_install_requires_confirmation() {
        assert_eq!(
            connection_args(None, false).unwrap(),
            ["connect", "--launch", "--wait", "120"]
        );
        assert!(connection_args(Some("confirm-install-ego"), false).is_none());
        assert_eq!(
            connection_args(Some("install-ego"), false).unwrap(),
            ["install-ego"]
        );
        assert_eq!(
            connection_args(Some("confirm-install-ego"), true).unwrap(),
            ["install-ego", "--yes"]
        );
        assert!(matches!(
            parse_slash_input("/connect"),
            SlashInput::Command(SlashCommand::Connect { target: None })
        ));
        for name in [
            "auto",
            "help",
            "confirm-install-ego",
            "launch",
            "launch-chrome",
            "launch-ego",
            "ego",
            "chrome",
            "status",
            "details",
            "install-chrome",
            "install-ego",
            "disconnect",
            "cancel",
        ] {
            assert!(matches!(
                parse_slash_input(&format!("/connect {name}")),
                SlashInput::Command(SlashCommand::Connect { target: Some(_) })
            ));
        }
        assert!(matches!(
            parse_slash_input("/connect chrome;whoami"),
            SlashInput::Error(_)
        ));
        assert!(matches!(
            parse_slash_input("/connect ego unexpected"),
            SlashInput::Error(_)
        ));
    }

    #[tokio::test]
    async fn connection_output_is_bounded_but_fully_drained() {
        use tokio::io::AsyncWriteExt;
        let (mut writer, reader) = tokio::io::duplex(1024);
        let producer = tokio::spawn(async move {
            writer
                .write_all(fixture("ready", true).to_string().as_bytes())
                .await
                .unwrap();
            writer.write_all(&vec![b' '; 256 * 1024]).await.unwrap();
        });
        let output = tokio::time::timeout(Duration::from_secs(5), drain(reader))
            .await
            .unwrap()
            .unwrap();
        producer.await.unwrap();
        assert_eq!(output.len(), 128 * 1024 + OUTPUT_TRUNCATION_NOTICE.len());
        assert!(output.ends_with(OUTPUT_TRUNCATION_NOTICE));
        assert!(ConnectionOutput::decode(&output, "", true).report.is_err());
    }

    #[tokio::test]
    async fn cancellation_before_dispatch_does_not_start_a_mutating_cli_command() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let output = run_connection(vec!["disconnect"], cancel).await.unwrap();
        assert_eq!(output.details, "cancelled before starting browser CLI");
        assert!(output.report.unwrap_err().contains("未启动"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancellation_reaps_connection_process() {
        let mut child = tokio::process::Command::new("/bin/sleep")
            .arg("30")
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert!(
            wait_child(&mut child, Duration::from_secs(2), cancel)
                .await
                .is_err()
        );
        assert!(child.id().is_none());
    }
}
