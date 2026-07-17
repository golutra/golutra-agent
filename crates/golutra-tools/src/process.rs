//! 子进程执行、取消与有界输出收集。

use std::{path::Path, process::Stdio, time::Duration};

use golutra_sandbox::{SandboxRequest, SystemSandbox, WorkspaceAccess};
#[cfg(unix)]
use nix::{
    sys::signal::{Signal, killpg},
    unistd::Pid,
};
use tokio::{io::AsyncReadExt, process::Command, task::JoinHandle};
use tokio_util::sync::CancellationToken;

use super::ToolError;

pub(crate) const MAX_PIPE_OUTPUT_BYTES: usize = 2 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ShellOutput {
    pub(crate) exit_code: Option<i32>,
    pub(crate) timed_out: bool,
    pub(crate) cancelled: bool,
    pub(crate) raw_output: String,
    pub(crate) sandbox_backend: golutra_sandbox::SandboxBackendKind,
    pub(crate) sandbox_os_enforced: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CommandLine {
    pub(crate) program: String,
    pub(crate) args: Vec<String>,
}

impl CommandLine {
    pub(crate) fn parse(command: &str) -> Result<Self, ToolError> {
        let mut parts = shlex::split(command).ok_or_else(|| {
            ToolError::InvalidArguments("shell command contains invalid quoting".to_owned())
        })?;
        if parts.is_empty() {
            return Err(ToolError::InvalidArguments(
                "shell command cannot be empty".to_owned(),
            ));
        }
        let program = parts.remove(0);
        Ok(Self {
            program,
            args: parts,
        })
    }
}

pub(crate) async fn run_process(
    program: &str,
    args: &[String],
    cwd: &Path,
    timeout_ms: u64,
    cancellation: CancellationToken,
    sandbox: &SystemSandbox,
    workspace_access: WorkspaceAccess,
) -> Result<ShellOutput, ToolError> {
    let scratch = tempfile::Builder::new()
        .prefix("golutra-sandbox-")
        .tempdir()
        .map_err(|error| ToolError::Execution(format!("sandbox scratch setup failed: {error}")))?;
    let launch = sandbox
        .plan(&SandboxRequest {
            program: program.into(),
            args: args.iter().map(Into::into).collect(),
            cwd: cwd.to_path_buf(),
            workspace_root: cwd.to_path_buf(),
            scratch_dir: scratch.path().to_path_buf(),
            read_only_roots: Vec::new(),
            workspace_access,
            allow_network: false,
        })
        .map_err(|error| ToolError::Execution(error.to_string()))?;
    let mut command = Command::new(&launch.program);
    command
        .args(&launch.args)
        .current_dir(cwd)
        .env_clear()
        .envs(&launch.environment)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command
        .spawn()
        .map_err(|error| ToolError::Execution(error.to_string()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| ToolError::Execution("process stdout pipe is unavailable".to_owned()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| ToolError::Execution("process stderr pipe is unavailable".to_owned()))?;
    let stdout_reader = spawn_pipe_reader(stdout);
    let stderr_reader = spawn_pipe_reader(stderr);
    let timeout = tokio::time::sleep(Duration::from_millis(timeout_ms));
    tokio::pin!(timeout);

    let (status, timed_out, cancelled) = tokio::select! {
        biased;
        _ = cancellation.cancelled() => {
            terminate_process_tree(&mut child);
            let status = child.wait().await
                .map_err(|error| ToolError::Execution(error.to_string()))?;
            (status, false, true)
        }
        _ = &mut timeout => {
            terminate_process_tree(&mut child);
            let status = child.wait().await
                .map_err(|error| ToolError::Execution(error.to_string()))?;
            (status, true, false)
        }
        status = child.wait() => {
            (
                status.map_err(|error| ToolError::Execution(error.to_string()))?,
                false,
                false,
            )
        }
    };
    let stdout = join_pipe_reader(stdout_reader).await?;
    let stderr = join_pipe_reader(stderr_reader).await?;
    let raw_output = if stderr.is_empty() {
        stdout
    } else {
        format!("{stdout}\n{stderr}")
    };
    Ok(ShellOutput {
        exit_code: status.code(),
        timed_out,
        cancelled,
        raw_output,
        sandbox_backend: launch.backend,
        sandbox_os_enforced: launch.os_enforced,
    })
}

fn terminate_process_tree(child: &mut tokio::process::Child) {
    #[cfg(unix)]
    if let Some(process_id) = child.id().and_then(|id| i32::try_from(id).ok()) {
        let _ = killpg(Pid::from_raw(process_id), Signal::SIGKILL);
    }
    let _ = child.start_kill();
}

pub(crate) fn spawn_pipe_reader<R>(mut reader: R) -> JoinHandle<std::io::Result<String>>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut bytes = Vec::with_capacity(MAX_PIPE_OUTPUT_BYTES.min(64 * 1024));
        let mut buffer = [0_u8; 8192];
        let mut truncated = false;
        loop {
            let read = reader.read(&mut buffer).await?;
            if read == 0 {
                break;
            }
            let remaining = MAX_PIPE_OUTPUT_BYTES.saturating_sub(bytes.len());
            let retained = remaining.min(read);
            bytes.extend_from_slice(&buffer[..retained]);
            truncated |= retained < read;
        }
        if truncated {
            bytes.extend_from_slice(b"\n[process output truncated]\n");
        }
        Ok(String::from_utf8_lossy(&bytes).to_string())
    })
}

pub(crate) async fn join_pipe_reader(
    reader: JoinHandle<std::io::Result<String>>,
) -> Result<String, ToolError> {
    reader
        .await
        .map_err(|error| ToolError::Execution(error.to_string()))?
        .map_err(|error| ToolError::Execution(error.to_string()))
}
