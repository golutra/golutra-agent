//! 用户确认后安装官方 Ego；不覆盖已有安装、不修改系统安全设置或接受首次引导。
use super::{BrowserResult, fail};
use golutra_agent_tools::browser::{
    connection,
    launch::{self, BrowserKind},
};
use serde_json::json;
use std::{
    io::{IsTerminal, Write},
    path::Path,
    time::Duration,
};
#[cfg(any(target_os = "macos", target_os = "windows"))]
use std::{path::PathBuf, process::Stdio};
use tokio::io::AsyncWriteExt;
use tokio_util::sync::CancellationToken;

const MAX_DOWNLOAD: u64 = 1024 * 1024 * 1024;
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(600);

fn download_url(os: &str, arch: &str) -> miette::Result<&'static str> {
    match (os, arch) {
        ("macos", "aarch64") => Ok("https://cdn.ego.app/setup/macos/arm64/egolite.dmg"),
        ("macos", "x86_64") => Ok("https://cdn.ego.app/setup/macos/x64/egolite.dmg"),
        ("windows", "x86_64") => Ok("https://cdn.ego.app/setup/windows/x64/egoliteSetup.exe"),
        _ => Err(fail(
            "当前平台没有已确认支持的 Ego 自动安装包，请查看 https://lite.ego.app/zh-cn/download；可安装 Chrome 后使用 /connect chrome。",
        )),
    }
}

enum Consent {
    Granted,
    Required,
    Declined,
}

fn confirm(yes: bool, interactive: bool) -> miette::Result<Consent> {
    if yes {
        return Ok(Consent::Granted);
    }
    if !interactive || !std::io::stdin().is_terminal() {
        return Ok(Consent::Required);
    }
    eprint!("是否从 Ego 官网下载并安装 Ego？安装后需要完成首次引导。[y/N] ");
    std::io::stderr().flush().map_err(fail)?;
    let mut input = String::new();
    std::io::stdin().read_line(&mut input).map_err(fail)?;
    if matches!(
        input.trim().to_lowercase().as_str(),
        "y" | "yes" | "是" | "同意"
    ) {
        Ok(Consent::Granted)
    } else {
        Ok(Consent::Declined)
    }
}

pub(super) async fn run(yes: bool, interactive: bool) -> miette::Result<BrowserResult> {
    if launch::discover(BrowserKind::Ego, None).is_ok() || connection::discover_ego().is_some() {
        return Ok(BrowserResult::new(
            "already_installed",
            true,
            json!({"backend":"ego"}),
        ));
    }
    let url = download_url(std::env::consts::OS, std::env::consts::ARCH)?;
    match confirm(yes, interactive)? {
        Consent::Granted => (),
        Consent::Required => {
            return Ok(BrowserResult::new(
                "install_confirmation_required",
                false,
                json!({"backend":"ego","message":"是否同意从 Ego 官网下载并安装？安装后需要完成首次引导。","installation_requires_confirmation":true}),
            ));
        }
        Consent::Declined => {
            return Ok(BrowserResult::new(
                "cancelled",
                false,
                json!({"backend":"ego"}),
            ));
        }
    }
    let cancel = CancellationToken::new();
    let signal = tokio::spawn({
        let cancel = cancel.clone();
        async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                cancel.cancel();
            }
        }
    });
    let result = install(url, cancel.clone()).await;
    signal.abort();
    let _ = signal.await;
    match result {
        Err(error) if cancel.is_cancelled() => Ok(BrowserResult::new(
            "cancelled",
            false,
            json!({"backend":"ego","message":error.to_string()}),
        )),
        // Preserve a completed installation when cancellation arrives after commit.
        result => result,
    }
}

async fn install(url: &str, cancel: CancellationToken) -> miette::Result<BrowserResult> {
    let temporary = tempfile::Builder::new()
        .prefix("golutra-ego-install-")
        .tempdir()
        .map_err(fail)?;
    let package = temporary.path().join(if cfg!(windows) {
        "egoliteSetup.exe"
    } else {
        "egolite.dmg"
    });
    eprintln!("正在从 Ego 官方来源下载安装包：{url}");
    tokio::select! {
        result = download(url, &package) => result?,
        _ = cancel.cancelled() => return Err(fail("下载已取消，未安装浏览器。")),
    }
    #[cfg(target_os = "macos")]
    let application = install_macos(&package, &cancel).await?;
    #[cfg(target_os = "windows")]
    let application = install_windows(&package, &cancel).await?;
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    {
        if cancel.is_cancelled() {
            return Err(fail("已取消启动；已完成的 Ego 安装保留。"));
        }
        let launch = super::launch_browser(super::Backend::Ego, Some(&application)).await?;
        Ok(BrowserResult::new(
            "installed",
            true,
            json!({"backend":"ego","launch":launch}),
        ))
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    Err(fail("Ego 尚不支持此平台。"))
}

fn official_download(url: &reqwest::Url) -> bool {
    url.scheme() == "https"
        && url.host_str() == Some("cdn.ego.app")
        && url.port_or_known_default() == Some(443)
        && url.username().is_empty()
        && url.password().is_none()
}

async fn download(url: &str, target: &Path) -> miette::Result<()> {
    let url = reqwest::Url::parse(url).map_err(fail)?;
    if !official_download(&url) {
        return Err(fail("拒绝非官方 Ego 下载地址"));
    }
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(20))
        .timeout(DOWNLOAD_TIMEOUT)
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() < 5 && official_download(attempt.url()) {
                attempt.follow()
            } else {
                attempt.error("Ego installer redirect must stay on the official HTTPS CDN")
            }
        }))
        .build()
        .map_err(fail)?;
    let mut response = client
        .get(url)
        .send()
        .await
        .map_err(fail)?
        .error_for_status()
        .map_err(fail)?;
    let expected = response.content_length();
    if expected.is_some_and(|n| n > MAX_DOWNLOAD) {
        return Err(fail("Ego 安装包超出 1 GiB 限制"));
    }
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(target)
        .await
        .map_err(fail)?;
    let mut received = 0u64;
    let mut reported = 0u64;
    while let Some(chunk) = response.chunk().await.map_err(fail)? {
        received += chunk.len() as u64;
        if received > MAX_DOWNLOAD {
            return Err(fail("Ego 安装包超出 1 GiB 限制"));
        }
        file.write_all(&chunk).await.map_err(fail)?;
        if received - reported >= 32 * 1024 * 1024 {
            eprintln!("已下载 {} MiB", received / 1024 / 1024);
            reported = received;
        }
    }
    if received == 0 || expected.is_some_and(|n| n != received) {
        return Err(fail("Ego 安装包下载不完整"));
    }
    file.flush().await.map_err(fail)?;
    file.sync_all().await.map_err(fail)?;
    Ok(())
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
async fn command(
    program: &Path,
    args: &[&std::ffi::OsStr],
    timeout: Duration,
    cancel: &CancellationToken,
) -> miette::Result<()> {
    if cancel.is_cancelled() {
        return Err(fail("安装已取消。"));
    }
    let child = tokio::process::Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(fail)?;
    wait_command(child, program, timeout, cancel).await
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
async fn wait_command(
    mut child: tokio::process::Child,
    program: &Path,
    timeout: Duration,
    cancel: &CancellationToken,
) -> miette::Result<()> {
    let status = tokio::select! {
        result = tokio::time::timeout(timeout, child.wait()) => result.ok(),
        _ = cancel.cancelled() => None,
    };
    match status {
        Some(Ok(status)) if status.success() => Ok(()),
        Some(Ok(status)) => Err(fail(format!(
            "{} 退出 {status}；安装未完成，请检查系统提示。",
            program.display()
        ))),
        Some(Err(error)) => Err(fail(error)),
        None => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            Err(fail(format!(
                "{} 已取消或超时；请检查安装状态再重试。",
                program.display()
            )))
        }
    }
}

#[cfg(target_os = "macos")]
async fn install_macos(package: &Path, cancel: &CancellationToken) -> miette::Result<PathBuf> {
    let mount_root = tempfile::Builder::new()
        .prefix("golutra-ego-mount-")
        .tempdir()
        .map_err(fail)?;
    let mount = mount_root.path().join("mount");
    std::fs::create_dir(&mount).map_err(fail)?;
    let attached = command(
        Path::new("/usr/bin/hdiutil"),
        &[
            "attach".as_ref(),
            package.as_os_str(),
            "-readonly".as_ref(),
            "-nobrowse".as_ref(),
            "-mountpoint".as_ref(),
            mount.as_os_str(),
        ],
        Duration::from_secs(90),
        cancel,
    )
    .await;
    let result = match attached {
        Ok(()) => copy_macos_app(&mount, cancel).await,
        Err(error) => Err(error),
    };
    // 仅在真正挂载的情况下卸载；清理不受取消令牌影响。
    use std::os::unix::fs::MetadataExt;
    let mounted =
        mount.metadata().map_err(fail)?.dev() != mount_root.path().metadata().map_err(fail)?.dev();
    if mounted
        && let Err(error) = command(
            Path::new("/usr/bin/hdiutil"),
            &["detach".as_ref(), mount.as_os_str()],
            Duration::from_secs(30),
            &CancellationToken::new(),
        )
        .await
    {
        let retained = mount_root.keep();
        return Err(fail(format!(
            "{error} 原操作：{}。保留安装挂载目录 {}；请在磁盘工具中推出 Ego 安装卷后清理。",
            result
                .as_ref()
                .err()
                .map(ToString::to_string)
                .unwrap_or_else(|| "应用已安装".into()),
            retained.display()
        )));
    }
    result
}

#[cfg(target_os = "macos")]
async fn copy_macos_app(mount: &Path, cancel: &CancellationToken) -> miette::Result<PathBuf> {
    let source = mount.join("ego lite.app");
    if !source.is_dir() {
        return Err(fail("官方安装包未包含 ego lite.app"));
    }
    // 同一文件系统暂存，签名通过后才移动；不提权、不覆盖用户的应用。
    let stage =
        if let Some(applications) = std::env::var_os("GOLUTRA_AGENT_BROWSER_APPLICATIONS_DIR") {
            let applications = PathBuf::from(applications);
            if !applications.is_absolute() {
                return Err(fail("浏览器应用目录必须是绝对路径"));
            }
            std::fs::create_dir_all(&applications).map_err(fail)?;
            tempfile::Builder::new()
                .prefix(".golutra-ego-")
                .tempdir_in(applications)
                .map_err(fail)?
        } else {
            match tempfile::Builder::new()
                .prefix(".golutra-ego-")
                .tempdir_in("/Applications")
            {
                Ok(stage) => stage,
                Err(_) => {
                    let home =
                        golutra_agent_tools::browser::chrome::install::user_home().map_err(fail)?;
                    let applications = home.join("Applications");
                    std::fs::create_dir_all(&applications).map_err(fail)?;
                    tempfile::Builder::new()
                        .prefix(".golutra-ego-")
                        .tempdir_in(applications)
                        .map_err(fail)?
                }
            }
        };
    let staged = stage.path().join("ego lite.app");
    eprintln!("正在复制并校验 Ego 应用签名…");
    command(
        Path::new("/usr/bin/ditto"),
        &[
            "--norsrc".as_ref(),
            "--noextattr".as_ref(),
            source.as_os_str(),
            staged.as_os_str(),
        ],
        Duration::from_secs(120),
        cancel,
    )
    .await?;
    command(
        Path::new("/usr/bin/codesign"),
        &[
            "--verify".as_ref(),
            "--deep".as_ref(),
            "--strict".as_ref(),
            staged.as_os_str(),
        ],
        Duration::from_secs(60),
        cancel,
    )
    .await?;
    command(
        Path::new("/usr/sbin/spctl"),
        &[
            "--assess".as_ref(),
            "--type".as_ref(),
            "execute".as_ref(),
            staged.as_os_str(),
        ],
        Duration::from_secs(60),
        cancel,
    )
    .await?;
    let destination = stage.path().parent().unwrap().join("ego lite.app");
    if std::fs::symlink_metadata(&destination).is_ok() {
        return Err(fail("Ego 已存在，保留原安装；请重新检查连接。"));
    }
    if cancel.is_cancelled() {
        return Err(fail("安装已取消，未替换应用。"));
    }
    std::fs::rename(staged, &destination).map_err(fail)?;
    Ok(destination)
}

#[cfg(target_os = "windows")]
async fn install_windows(package: &Path, cancel: &CancellationToken) -> miette::Result<PathBuf> {
    // 路径经环境传递，不拼进 PowerShell；不猜测静默安装参数或自动接受许可。
    let child = tokio::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", "if ((Get-AuthenticodeSignature -LiteralPath $env:GOLUTRA_AGENT_EGO_INSTALLER).Status -ne 'Valid') { exit 1 }"])
        .env("GOLUTRA_AGENT_EGO_INSTALLER", package).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).kill_on_drop(true)
        .spawn().map_err(fail)?;
    wait_command(
        child,
        Path::new("powershell.exe"),
        Duration::from_secs(60),
        cancel,
    )
    .await?;
    eprintln!("正在运行 Ego 官方安装器；如出现系统确认或许可条款，请在安装窗口处理。");
    command(package, &[], Duration::from_secs(600), cancel).await?;
    launch::discover(BrowserKind::Ego, None).map_err(fail)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn installer_uses_supported_official_packages_only() {
        for (os, arch) in [
            ("macos", "aarch64"),
            ("macos", "x86_64"),
            ("windows", "x86_64"),
        ] {
            assert!(official_download(
                &reqwest::Url::parse(download_url(os, arch).unwrap()).unwrap()
            ));
        }
        assert!(download_url("linux", "x86_64").is_err());
        assert!(download_url("windows", "aarch64").is_err());
        for url in [
            "http://cdn.ego.app/package",
            "https://cdn.ego.app.evil.invalid/package",
            "https://user@cdn.ego.app/package",
            "https://cdn.ego.app:444/package",
        ] {
            assert!(!official_download(&reqwest::Url::parse(url).unwrap()));
        }
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn installer_cancel_reaps_child_and_keeps_original_application() {
        let cancel = CancellationToken::new();
        let child = tokio::process::Command::new("/bin/sleep")
            .arg("30")
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        cancel.cancel();
        assert!(
            wait_command(
                child,
                Path::new("/bin/sleep"),
                Duration::from_secs(1),
                &cancel
            )
            .await
            .is_err()
        );
        // 不接触真实安装目录；取消在启动任何拷贝/签名程序之前生效。
        let root = tempfile::tempdir().unwrap();
        assert!(
            install_macos(&root.path().join("missing.dmg"), &cancel)
                .await
                .is_err()
        );
        assert!(root.path().read_dir().unwrap().next().is_none());
    }
}
