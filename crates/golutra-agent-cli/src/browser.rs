use clap::{Subcommand, ValueEnum};
use golutra_agent_tools::browser::{
    EgoBrowserConfig,
    connection::{
        self, AutoConfig, BrowserConfig, DisabledConfig, DisabledKind, discover_ego, probe_ego,
    },
    launch::{self, BrowserKind},
    opencli::{self, OpenCliConfig},
};
use serde_json::{Value, json};
use std::{path::PathBuf, time::Duration};
use tokio_util::sync::CancellationToken;

#[path = "browser_install.rs"]
mod install;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Backend {
    Auto,
    Ego,
    Chrome,
}

#[derive(Debug, Subcommand)]
pub enum BrowserCommand {
    /// 检查已配置的浏览器；不创建网页或取得控制权。
    Status,
    /// 启动用户原有浏览器，保留 Profile/登录状态；不自动授权或切换配置。
    Launch {
        #[arg(value_enum)]
        backend: Option<Backend>,
        /// 非标准安装位置，指定 .app（macOS）或程序绝对路径。
        #[arg(long)]
        application: Option<PathBuf>,
    },
    /// 自动优先 Ego，其次 Chrome；都未安装时询问安装 Ego。通过后保存配置。
    Connect {
        #[arg(value_enum)]
        backend: Option<Backend>,
        #[arg(long)]
        executable: Option<PathBuf>,
        /// Chrome 的 OpenCLI Profile 名称或 contextId，不修改全局账号。
        #[arg(long)]
        profile: Option<String>,
        #[arg(long, default_value_t=0, value_parser=clap::value_parser!(u64).range(0..=300))]
        wait: u64,
        /// 连接前正常启动浏览器；首次安装官方扩展仍由用户完成。
        #[arg(long)]
        launch: bool,
        /// 已同意在两种浏览器都未安装时自动下载并安装 Ego。
        #[arg(long)]
        yes: bool,
    },
    /// 确认后从官方来源下载安装 Ego；首次引导在浏览器中完成。
    InstallEgo {
        /// 已确认安装，不再重复询问。
        #[arg(long)]
        yes: bool,
    },
    /// 查看 OpenCLI Chrome 安装入口，不再部署旧 Golutra 扩展。
    Chrome {
        #[command(subcommand)]
        command: ChromeCommand,
    },
    /// 禁用后续任务的浏览器工具，保留页面和会话证据。
    Disconnect,
}

#[derive(Debug, Subcommand)]
pub enum ChromeCommand {
    Install,
}

fn fail(error: impl std::fmt::Display) -> miette::Report {
    miette::miette!("{error}")
}

impl Backend {
    fn name(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Ego => "ego",
            Self::Chrome => "chrome",
        }
    }

    fn kind(self) -> BrowserKind {
        match self {
            Self::Auto => unreachable!("automatic selection must resolve before launch"),
            Self::Chrome => BrowserKind::Chrome,
            Self::Ego => BrowserKind::Ego,
        }
    }
}

async fn launch_browser(
    backend: Backend,
    application: Option<&std::path::Path>,
) -> miette::Result<Value> {
    let result = launch::launch(backend.kind(), application, CancellationToken::new())
        .await
        .map_err(fail)?;
    if result.is_error {
        return Err(fail(result.summary));
    }
    Ok(result.structured_facts)
}

/// Private CLI/TUI contract. stdout contains exactly one envelope in --json mode;
/// progress and low-level diagnostics never become the user-facing result.
#[derive(Debug, serde::Serialize)]
pub(super) struct BrowserResult {
    version: u8,
    ok: bool,
    state: String,
    backend: Option<String>,
    selection: Option<String>,
    saved: bool,
    timed_out: bool,
    diagnostics: Value,
}

impl BrowserResult {
    pub(super) fn new(state: &str, ok: bool, diagnostics: Value) -> Self {
        Self {
            version: 1,
            ok,
            state: state.into(),
            backend: diagnostics["backend"]
                .as_str()
                .filter(|backend| matches!(*backend, "ego" | "chrome"))
                .map(str::to_owned),
            selection: diagnostics["browser_backend"].as_str().map(str::to_owned),
            saved: false,
            timed_out: false,
            diagnostics,
        }
    }
}

fn connection_config(
    backend: Backend,
    executable: Option<&std::path::Path>,
    profile: Option<&str>,
    home: &std::path::Path,
) -> miette::Result<Option<BrowserConfig>> {
    match backend {
        Backend::Auto => unreachable!("automatic selection must resolve before configuration"),
        Backend::Chrome => {
            let mut config = match BrowserConfig::load(home).map_err(fail)? {
                Some(BrowserConfig::Chrome(config)) => config,
                _ => OpenCliConfig::default(),
            };
            if let Some(path) = executable {
                config.executable = Some(path.to_path_buf());
            }
            if let Some(profile) = profile {
                config.profile = Some(profile.to_owned());
            }
            config.validate().map_err(fail)?;
            Ok(Some(BrowserConfig::Chrome(config)))
        }
        Backend::Ego => {
            if executable.is_none()
                && let Some(BrowserConfig::Ego(config)) = BrowserConfig::load(home).map_err(fail)?
            {
                return Ok(Some(BrowserConfig::Ego(config)));
            }
            Ok(executable
                .map(std::path::Path::to_path_buf)
                .or_else(discover_ego)
                .map(|executable| {
                    BrowserConfig::Ego(EgoBrowserConfig {
                        executable,
                        sdk_path: None,
                    })
                }))
        }
    }
}

fn selected_backend(
    requested: Option<Backend>,
    home: &std::path::Path,
) -> miette::Result<Option<Backend>> {
    let config = match requested {
        Some(Backend::Ego | Backend::Chrome) => return Ok(requested),
        Some(Backend::Auto) => Some(BrowserConfig::automatic()),
        None => BrowserConfig::effective(home).map_err(fail)?,
    };
    match config {
        Some(BrowserConfig::Ego(_)) => Ok(Some(Backend::Ego)),
        Some(BrowserConfig::Chrome(_)) => Ok(Some(Backend::Chrome)),
        Some(BrowserConfig::Auto(_)) => Ok(launch::discover(BrowserKind::Ego, None)
            .ok()
            .map(|_| Backend::Ego)),
        _ => Err(fail(
            "浏览器已禁用；请明确选择 browser connect auto、chrome 或 ego。",
        )),
    }
}

pub async fn run(command: &BrowserCommand, json_output: bool) -> miette::Result<()> {
    let result = execute(command, json_output).await.unwrap_or_else(|error| {
        BrowserResult::new("error", false, json!({"message":error.to_string()}))
    });
    if json_output {
        println!("{}", serde_json::to_string(&result).map_err(fail)?);
    } else {
        // Keep the existing scriptable status and installation-discovery output.
        match command {
            BrowserCommand::Status | BrowserCommand::Chrome { .. } if result.ok => {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&result.diagnostics).map_err(fail)?
                );
            }
            _ if result.ok => {
                let message = match result.state.as_str() {
                    "ready" => "浏览器已连接，默认选择已保存。",
                    "launch_requested" => "已请求打开浏览器；可用 browser status 检查连接。",
                    "installed" => "Ego 已安装并启动。完成首次引导后运行 browser connect auto。",
                    "already_installed" => {
                        "Ego 已安装，无需重复下载。可用 browser connect ego 连接。"
                    }
                    "disabled" => "已禁用后续任务的浏览器工具。",
                    _ => "浏览器操作已完成。",
                };
                println!("{message}");
            }
            _ => {
                eprintln!(
                    "{}",
                    serde_json::to_string_pretty(&result.diagnostics).map_err(fail)?
                );
            }
        }
    }
    if result.ok {
        Ok(())
    } else {
        // The process exit status is authoritative, including consent and timeout.
        Err(fail(format!("浏览器操作未完成：{}", result.state)))
    }
}

async fn execute(command: &BrowserCommand, json_output: bool) -> miette::Result<BrowserResult> {
    let home = golutra_agent_config::golutra_agent_home().map_err(fail)?;
    match command {
        BrowserCommand::Launch {
            backend,
            application,
        } => {
            let backend = selected_backend(*backend, &home)?
                .ok_or_else(|| fail(connection::INSTALL_EGO_GUIDANCE))?;
            Ok(BrowserResult::new(
                "launch_requested",
                true,
                launch_browser(backend, application.as_deref()).await?,
            ))
        }
        BrowserCommand::Status => {
            let configured = BrowserConfig::load(&home).map_err(fail)?;
            let mut value = match BrowserConfig::effective(&home).map_err(fail)? {
                Some(BrowserConfig::Auto(_)) => connection::setup_status(),
                Some(BrowserConfig::Ego(config)) => probe_ego(config, &home).await.map_err(fail)?,
                Some(BrowserConfig::Chrome(config)) => chrome_probe(&config).await,
                Some(BrowserConfig::Disabled(_)) | None => {
                    json!({"backend":"disabled","status":"disabled","next":"运行 /connect auto、chrome 或 ego 重新连接。"})
                }
            };
            value["browser_backend"] = json!(
                configured
                    .as_ref()
                    .map_or("auto", BrowserConfig::backend_name)
            );
            value["resolved_backend"] = match value["backend"].as_str() {
                Some("ego") => json!("ego"),
                Some("chrome") => json!("chrome"),
                _ => Value::Null,
            };
            value["selection_scope"] = json!("host_default");
            value["guidance"] = json!(connection::EXTENSION_GUIDANCE);
            Ok(BrowserResult::new(
                value["status"].as_str().unwrap_or("unavailable"),
                true,
                value.clone(),
            ))
        }
        BrowserCommand::Connect {
            backend,
            executable,
            profile,
            wait,
            launch,
            yes,
        } => {
            if executable.is_some() && !matches!(backend, Some(Backend::Chrome | Backend::Ego)) {
                return Err(fail(
                    "--executable requires an explicit ego or chrome backend",
                ));
            }
            if *backend == Some(Backend::Auto) && profile.is_some() {
                return Err(fail(
                    "--profile requires an explicit chrome backend; auto must not discard a selected Profile",
                ));
            }
            let automatic = *backend == Some(Backend::Auto)
                || (backend.is_none()
                    && matches!(
                        BrowserConfig::load(&home).map_err(fail)?,
                        None | Some(BrowserConfig::Auto(_))
                    ));
            let Some(backend) = selected_backend(*backend, &home)? else {
                if profile.is_some() {
                    return Err(fail("--profile only applies to Chrome/OpenCLI"));
                }
                return install::run(*yes, !json_output).await;
            };
            if matches!(backend, Backend::Ego) && profile.is_some() {
                return Err(fail("--profile only applies to Chrome/OpenCLI"));
            }
            if executable.as_ref().is_some_and(|path| !path.is_absolute()) {
                return Err(fail("--executable must be absolute"));
            }
            connection_config(backend, executable.as_deref(), profile.as_deref(), &home)?;
            let launch_result = if *launch {
                Some(launch_browser(backend, None).await?)
            } else {
                None
            };
            let deadline = tokio::time::Instant::now() + Duration::from_secs(*wait);
            loop {
                // 首次引导完成后才会注册 Ego 启动器，因此每次等待都重新发现，而非提前失败。
                let config =
                    connection_config(backend, executable.as_deref(), profile.as_deref(), &home)?;
                let mut status = match &config {
                    Some(BrowserConfig::Disabled(_) | BrowserConfig::Auto(_)) => {
                        unreachable!("connect selects a browser")
                    }
                    Some(BrowserConfig::Ego(config)) => {
                        probe_ego(config.clone(), &home).await.map_err(fail)?
                    }
                    Some(BrowserConfig::Chrome(config)) => chrome_probe(config).await,
                    None => {
                        json!({"backend":"ego","status":"setup_required","message":"未找到 ego-browser。请安装 Ego 并完成首次引导，启动器就绪后自动重试连接；下载 https://lite.ego.app/。已有启动器可用 --executable 指定。"})
                    }
                };
                status["browser_backend"] = json!(if automatic { "auto" } else { backend.name() });
                status["resolved_backend"] = json!(backend.name());
                if let Some(launch_result) = &launch_result {
                    status["launch"] = launch_result.clone();
                }
                if status["status"] == "ready" {
                    let saved = if automatic {
                        BrowserConfig::Auto(AutoConfig::default())
                    } else {
                        config.expect("ready requires browser configuration")
                    };
                    saved.save(&home).map_err(fail)?;
                    status["browser_backend"] = json!(saved.backend_name());
                    status["resolved_backend"] = status["backend"].clone();
                    let mut result = BrowserResult::new("ready", true, status);
                    result.saved = true;
                    return Ok(result);
                }
                if tokio::time::Instant::now() >= deadline {
                    let mut result = BrowserResult::new(
                        status["status"].as_str().unwrap_or("unavailable"),
                        false,
                        status.clone(),
                    );
                    result.timed_out = *wait > 0;
                    return Ok(result);
                }
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => return Ok(BrowserResult::new("cancelled", false, status)),
                    _ = tokio::time::sleep_until(deadline.min(tokio::time::Instant::now() + Duration::from_secs(2))) => ()
                }
            }
        }
        BrowserCommand::InstallEgo { yes } => install::run(*yes, !json_output).await,
        BrowserCommand::Chrome {
            command: ChromeCommand::Install,
        } => {
            let result = json!({"backend":"chrome","transport":"opencli","executable":OpenCliConfig::default().executable(),"extension_url":"https://chromewebstore.google.com/detail/opencli/ildkmabpimmkaediidaifkhjpohdnifk","next":opencli::SETUP});
            Ok(BrowserResult::new("install_help", true, result))
        }
        BrowserCommand::Disconnect => {
            BrowserConfig::Disabled(DisabledConfig {
                backend: DisabledKind::Disabled,
            })
            .save(&home)
            .map_err(fail)?;
            let mut result =
                BrowserResult::new("disabled", true, json!({"browser_backend":"disabled"}));
            result.saved = true;
            Ok(result)
        }
    }
}

async fn chrome_probe(config: &OpenCliConfig) -> Value {
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::env::temp_dir());
    opencli::probe(config, &cwd, CancellationToken::new()).await.unwrap_or_else(|error| json!({"backend":"chrome","transport":"opencli","status":"unavailable","message":error.to_string(),"next":opencli::SETUP}))
}
