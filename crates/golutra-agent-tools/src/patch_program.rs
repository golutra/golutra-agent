//! macOS 的系统 Git 是开发工具启动器；在沙箱外只做路径发现，补丁执行仍受原沙箱约束。

#[cfg(target_os = "macos")]
pub(super) async fn git_program() -> &'static str {
    static PROGRAM: tokio::sync::OnceCell<String> = tokio::sync::OnceCell::const_new();
    PROGRAM
        .get_or_init(|| async {
            let first = std::env::var_os("PATH").and_then(|paths| {
                std::env::split_paths(&paths)
                    .map(|path| path.join("git"))
                    .find(|path| path.is_file())
            });
            // 尊重用户 PATH 中的非系统 Git；只有 Apple 启动器需要提前解析。
            if first.as_deref() != Some(std::path::Path::new("/usr/bin/git")) {
                return "git".into();
            }
            let output = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                tokio::process::Command::new("/usr/bin/xcrun")
                    .args(["--find", "git"])
                    .kill_on_drop(true)
                    .output(),
            )
            .await;
            if let Ok(Ok(output)) = output
                && output.status.success()
                && let Ok(path) = String::from_utf8(output.stdout)
            {
                let path = path.trim();
                if std::path::Path::new(path).is_absolute() && std::path::Path::new(path).is_file()
                {
                    return path.to_owned();
                }
            }
            // 工具链未安装时保留原命令的错误，不改变权限或悄悄跳过补丁校验。
            "git".into()
        })
        .await
        .as_str()
}

#[cfg(not(target_os = "macos"))]
pub(super) async fn git_program() -> &'static str {
    "git"
}
