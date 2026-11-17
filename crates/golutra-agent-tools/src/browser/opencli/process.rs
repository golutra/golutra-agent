//! OpenCLI 进程边界：固定程序、独立 stdout JSON、双流有界读取；取消不宣称网页动作已撤销。
use crate::ToolError;
use serde_json::Value;
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
};
use tokio_util::sync::CancellationToken;

const OUTPUT_LIMIT: usize = 8 * 1024 * 1024;

#[derive(Debug)]
pub(super) struct Launcher {
    program: PathBuf,
    prefix: Vec<String>,
}

pub(super) fn find_on_path(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|dir| dir.join(name))
        .find(|path| path.is_absolute() && path.is_file())
}

impl Launcher {
    pub fn new(executable: &Path) -> Result<Self, ToolError> {
        if !executable.is_absolute() {
            return Err(ToolError::InvalidArguments(
                "OpenCLI executable must be absolute".into(),
            ));
        }
        let path = executable
            .canonicalize()
            .map_err(super::super::state::io_error)?;
        if path
            .extension()
            .is_some_and(|ext| ext == "js" || ext == "mjs")
        {
            let node = std::env::var_os("GOLUTRA_AGENT_OPENCLI_NODE")
                .map(PathBuf::from)
                .or_else(|| find_on_path(if cfg!(windows) { "node.exe" } else { "node" }))
                .filter(|p| p.is_absolute() && p.is_file())
                .ok_or_else(|| ToolError::Execution("OpenCLI requires Node.js >=20.18.1".into()))?;
            return Ok(Self {
                program: node,
                prefix: vec![path.to_string_lossy().into_owned()],
            });
        }
        if path
            .extension()
            .is_some_and(|ext| ext == "cmd" || ext == "bat" || ext == "ps1")
        {
            return Err(ToolError::InvalidArguments(
                "Configure OpenCLI's absolute dist/src/main.js path, not a shell wrapper".into(),
            ));
        }
        Ok(Self {
            program: path,
            prefix: vec![],
        })
    }

    async fn output(
        &self,
        args: &[String],
        cwd: &Path,
        cancel: CancellationToken,
        timeout: Duration,
    ) -> Result<(bool, String), ToolError> {
        if cancel.is_cancelled() {
            return Err(ToolError::Execution(
                "opencli_cancelled before launch".into(),
            ));
        }
        let mut command = Command::new(&self.program);
        command
            .args(&self.prefix)
            .args(args)
            .current_dir(cwd)
            .env("OPENCLI_NO_UPDATE_CHECK", "1")
            .env_remove("OPENCLI_PROFILE")
            .env_remove("OPENCLI_WINDOW")
            // 禁止继承远程 CDP、页面清理等外部设置改变已选择的 Chrome 后端语义。
            .env_remove("OPENCLI_CDP_ENDPOINT")
            .env_remove("OPENCLI_CDP_TARGET")
            .env_remove("OPENCLI_CDP_INIT_SCRIPT")
            .env_remove("OPENCLI_CLOSE_OWNED_TAB")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command.spawn().map_err(super::super::state::io_error)?;
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        let operation = async {
            let (status, out, _err) = tokio::try_join!(
                async { child.wait().await.map_err(super::super::state::io_error) },
                limited_read(stdout),
                limited_read(stderr)
            )?;
            Ok((
                status.success(),
                String::from_utf8(out)
                    .map_err(|_| ToolError::Execution("OpenCLI stdout is not UTF-8".into()))?,
            ))
        };
        let result = tokio::select! {
            _ = cancel.cancelled() => Err(ToolError::Execution("opencli_cancelled: browser outcome may be unknown; inspect before retrying".into())),
            result = tokio::time::timeout(timeout, operation) => result.unwrap_or_else(|_| Err(ToolError::Execution("opencli_timeout: browser outcome may be unknown; inspect before retrying".into()))),
        };
        if result.is_err() {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
        result
    }

    pub async fn verify(&self, cwd: &Path, cancel: CancellationToken) -> Result<(), ToolError> {
        let (ok, version) = self
            .output(&["--version".into()], cwd, cancel, Duration::from_secs(10))
            .await?;
        // 固定经过验收的 CLI 契约；不同版本不能把退出成功误判成兼容。
        if !ok || version.trim() != "2.0.3" {
            return Err(ToolError::Execution("OpenCLI 2.0.3 required; run npm install -g @jackwener/opencli@2.0.3 or configure the bundled executable".into()));
        }
        Ok(())
    }

    pub async fn json(
        &self,
        args: &[String],
        cwd: &Path,
        cancel: CancellationToken,
    ) -> Result<Value, ToolError> {
        let (success, text) = self
            .output(args, cwd, cancel, Duration::from_secs(75))
            .await?;
        let value: Value = serde_json::from_str(&text).map_err(|_| ToolError::Execution("OpenCLI did not return a valid JSON envelope; inspect browser state before retrying".into()))?;
        if !value["ok"].is_boolean() || (success != (value["ok"] == true)) {
            return Err(ToolError::Execution(
                "OpenCLI exit status disagrees with its JSON envelope".into(),
            ));
        }
        Ok(value)
    }
}

async fn limited_read(mut reader: impl AsyncRead + Unpin) -> Result<Vec<u8>, ToolError> {
    let mut bytes = Vec::new();
    let mut buffer = [0; 8192];
    loop {
        let size = reader
            .read(&mut buffer)
            .await
            .map_err(super::super::state::io_error)?;
        if size == 0 {
            return Ok(bytes);
        }
        if bytes.len() + size > OUTPUT_LIMIT {
            return Err(ToolError::Execution("OpenCLI output exceeds 8 MiB".into()));
        }
        bytes.extend_from_slice(&buffer[..size]);
    }
}
