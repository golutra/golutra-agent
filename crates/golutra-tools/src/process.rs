//! 子进程执行、取消与有界输出收集。

use std::{
    path::Path,
    process::Stdio,
    time::{Duration, Instant},
};

use golutra_policy::{
    contains_shell_metacharacter, explicit_shell_script, parse_shell_command_with_input,
};
use golutra_sandbox::{SandboxRequest, SystemSandbox, WorkspaceAccess};
#[cfg(unix)]
use nix::{
    sys::signal::{Signal, kill, killpg},
    unistd::Pid,
};
#[cfg(test)]
use tokio::task::JoinHandle;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::{ChildStdin, Command},
    sync::mpsc::{Sender, channel},
};
use tokio_util::sync::CancellationToken;

use super::{ToolError, redact_sensitive_text};

pub(crate) const MAX_PIPE_OUTPUT_BYTES: usize = 2 * 1024 * 1024;
const PIPE_MESSAGE_CAPACITY: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ShellOutput {
    pub(crate) exit_code: Option<i32>,
    pub(crate) timed_out: bool,
    pub(crate) cancelled: bool,
    pub(crate) stdin_error: Option<String>,
    pub(crate) raw_output: String,
    pub(crate) sandbox_backend: golutra_sandbox::SandboxBackendKind,
    pub(crate) sandbox_os_enforced: bool,
    pub(crate) output_bytes: u64,
    pub(crate) output_lines: u64,
    pub(crate) output_truncated: bool,
    pub(crate) network_access: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProcessStream {
    Stdout,
    Stderr,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProcessProgress {
    pub(crate) stream: ProcessStream,
    pub(crate) output_bytes: u64,
    pub(crate) output_lines: u64,
    pub(crate) retained_bytes: usize,
    pub(crate) truncated: bool,
    pub(crate) output_excerpt: Option<String>,
}

pub(crate) struct ProcessExecutionRequest<'a> {
    pub(crate) program: &'a str,
    pub(crate) args: &'a [String],
    pub(crate) cwd: &'a Path,
    pub(crate) workspace_root: &'a Path,
    pub(crate) timeout_ms: u64,
    pub(crate) cancellation: CancellationToken,
    pub(crate) sandbox: &'a SystemSandbox,
    pub(crate) workspace_access: WorkspaceAccess,
    pub(crate) allow_network: bool,
    pub(crate) stdin: Option<&'a [u8]>,
    pub(crate) isolated_home: bool,
}

#[derive(Debug)]
enum PipeMessage {
    Chunk(ProcessStream, Vec<u8>),
    Done(ProcessStream),
    Error(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CommandLine {
    pub(crate) program: String,
    pub(crate) args: Vec<String>,
    pub(crate) stdin: Option<Vec<u8>>,
}

impl CommandLine {
    pub(crate) fn parse(command: &str) -> Result<Self, ToolError> {
        let parsed = parse_shell_command_with_input(command).ok_or_else(|| {
            ToolError::InvalidArguments("shell command contains invalid quoting".to_owned())
        })?;
        if parsed.stdin.is_none()
            && explicit_shell_script(&parsed.parts).is_none()
            && contains_shell_metacharacter(command)
        {
            return Err(ToolError::InvalidArguments(
                "unquoted shell operators require an explicit bash -lc wrapper".to_owned(),
            ));
        }
        let mut parts = parsed.parts;
        if parts.is_empty() {
            return Err(ToolError::InvalidArguments(
                "shell command cannot be empty".to_owned(),
            ));
        }
        let program = parts.remove(0);
        Ok(Self {
            program,
            args: parts,
            stdin: parsed.stdin.map(String::into_bytes),
        })
    }
}

#[cfg(test)]
pub(crate) async fn run_process(
    program: &str,
    args: &[String],
    cwd: &Path,
    timeout_ms: u64,
    cancellation: CancellationToken,
    sandbox: &SystemSandbox,
    workspace_access: WorkspaceAccess,
) -> Result<ShellOutput, ToolError> {
    run_process_with_progress(
        ProcessExecutionRequest {
            program,
            args,
            cwd,
            workspace_root: cwd,
            timeout_ms,
            cancellation,
            sandbox,
            workspace_access,
            allow_network: false,
            stdin: None,
            isolated_home: false,
        },
        None,
    )
    .await
}

pub(crate) async fn run_process_with_progress(
    request: ProcessExecutionRequest<'_>,
    mut progress: Option<&mut (dyn FnMut(ProcessProgress) + Send)>,
) -> Result<ShellOutput, ToolError> {
    let scratch = tempfile::Builder::new()
        .prefix("golutra-sandbox-")
        .tempdir()
        .map_err(|error| ToolError::Execution(format!("sandbox scratch setup failed: {error}")))?;
    let launch = request
        .sandbox
        .plan(&SandboxRequest {
            program: request.program.into(),
            args: request.args.iter().map(Into::into).collect(),
            cwd: request.cwd.to_path_buf(),
            workspace_root: request.workspace_root.to_path_buf(),
            scratch_dir: scratch.path().to_path_buf(),
            read_only_roots: Vec::new(),
            workspace_access: request.workspace_access,
            allow_network: request.allow_network,
        })
        .map_err(|error| ToolError::Execution(error.to_string()))?;
    let mut command = Command::new(&launch.program);
    command
        .args(&launch.args)
        .current_dir(request.cwd)
        .env_clear()
        .envs(&launch.environment)
        .stdin(if request.stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if request.isolated_home {
        command
            .env("HOME", scratch.path())
            .env("CFFIXED_USER_HOME", scratch.path())
            .env("XDG_CONFIG_HOME", scratch.path())
            .env("XDG_CACHE_HOME", scratch.path())
            .env("DARWIN_USER_CACHE_DIR", scratch.path())
            .env("GIT_CONFIG_GLOBAL", scratch.path().join(".gitconfig"))
            .env("GIT_CONFIG_NOSYSTEM", "1");
    }
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command
        .spawn()
        .map_err(|error| ToolError::Execution(error.to_string()))?;
    let input = request.stdin;
    let stdin =
        if input.is_some() {
            Some(child.stdin.take().ok_or_else(|| {
                ToolError::Execution("process stdin pipe is unavailable".to_owned())
            })?)
        } else {
            None
        };
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| ToolError::Execution("process stdout pipe is unavailable".to_owned()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| ToolError::Execution("process stderr pipe is unavailable".to_owned()))?;
    let (progress_tx, mut progress_rx) = channel(PIPE_MESSAGE_CAPACITY);
    spawn_stream_reader(stdout, ProcessStream::Stdout, progress_tx.clone());
    spawn_stream_reader(stderr, ProcessStream::Stderr, progress_tx);
    let timeout = tokio::time::sleep(Duration::from_millis(request.timeout_ms));
    tokio::pin!(timeout);
    let stdin_write = write_process_stdin(stdin, input);
    tokio::pin!(stdin_write);

    let child_id = child.id();
    let mut child_wait = Box::pin(child.wait());
    let mut status = None;
    let mut timed_out = false;
    let mut cancelled = false;
    let mut termination_requested = false;
    let mut stdin_done = input.is_none();
    let mut stdin_error = None;
    let mut readers_done = 0_u8;
    let mut stdout = OutputBuffer::default();
    let mut stderr = OutputBuffer::default();
    let mut live_output = LiveOutputBuffer::default();
    let mut last_progress = Instant::now();

    while status.is_none() || readers_done < 2 || !stdin_done {
        tokio::select! {
            biased;
            _ = request.cancellation.cancelled(), if status.is_none() && !termination_requested => {
                terminate_process_group(child_id);
                termination_requested = true;
                cancelled = true;
            }
            _ = &mut timeout, if status.is_none() && !termination_requested => {
                terminate_process_group(child_id);
                termination_requested = true;
                timed_out = true;
            }
            result = &mut child_wait, if status.is_none() => {
                status = Some(result.map_err(|error| ToolError::Execution(error.to_string()))?);
            }
            result = &mut stdin_write, if !stdin_done => {
                stdin_done = true;
                if let Err(error) = result
                    && !termination_requested
                {
                    stdin_error = Some(error.to_string());
                }
            }
            message = progress_rx.recv(), if readers_done < 2 => {
                match message {
                    Some(PipeMessage::Chunk(stream, bytes)) => {
                        live_output.push(&bytes);
                        match stream {
                            ProcessStream::Stdout => stdout.push(&bytes),
                            ProcessStream::Stderr => stderr.push(&bytes),
                        }
                        let now = Instant::now();
                        if now.duration_since(last_progress) >= Duration::from_millis(50)
                            || bytes.len() >= 16 * 1024
                        {
                            emit_progress(
                                &mut progress,
                                ProcessProgress {
                                    stream,
                                    output_bytes: stdout.total_bytes.saturating_add(stderr.total_bytes),
                                    output_lines: stdout.total_lines().saturating_add(stderr.total_lines()),
                                    retained_bytes: stdout.bytes.len().saturating_add(stderr.bytes.len()),
                                    truncated: stdout.truncated || stderr.truncated,
                                    output_excerpt: live_output.excerpt(),
                                },
                            );
                            last_progress = now;
                        }
                    }
                    Some(PipeMessage::Done(_stream)) => readers_done = readers_done.saturating_add(1),
                    Some(PipeMessage::Error(error)) => return Err(ToolError::Execution(error)),
                    None => readers_done = 2,
                }
            }
        }
    }
    let status =
        status.ok_or_else(|| ToolError::Execution("process exited without a status".to_owned()))?;
    emit_progress(
        &mut progress,
        ProcessProgress {
            stream: ProcessStream::Stdout,
            output_bytes: stdout.total_bytes.saturating_add(stderr.total_bytes),
            output_lines: stdout.total_lines().saturating_add(stderr.total_lines()),
            retained_bytes: stdout.bytes.len().saturating_add(stderr.bytes.len()),
            truncated: stdout.truncated || stderr.truncated,
            output_excerpt: live_output.excerpt(),
        },
    );
    let output_bytes = stdout.total_bytes.saturating_add(stderr.total_bytes);
    let output_lines = stdout.total_lines().saturating_add(stderr.total_lines());
    let output_truncated = stdout.truncated || stderr.truncated;
    let stdout_text = stdout.finish();
    let stderr_text = stderr.finish();
    let raw_output = if stderr_text.is_empty() {
        stdout_text
    } else if stdout_text.is_empty() {
        stderr_text
    } else {
        format!("{stdout_text}\n{stderr_text}")
    };
    Ok(ShellOutput {
        exit_code: status.code(),
        timed_out,
        cancelled,
        stdin_error,
        raw_output,
        sandbox_backend: launch.backend,
        sandbox_os_enforced: launch.os_enforced,
        output_bytes,
        output_lines,
        output_truncated,
        network_access: request.allow_network,
    })
}

async fn write_process_stdin(
    stdin: Option<ChildStdin>,
    input: Option<&[u8]>,
) -> std::io::Result<()> {
    let (Some(mut stdin), Some(input)) = (stdin, input) else {
        return Ok(());
    };
    stdin.write_all(input).await?;
    stdin.shutdown().await
}

pub(crate) fn terminate_process_group(process_id: Option<u32>) {
    #[cfg(unix)]
    if let Some(process_id) = process_id.and_then(|id| i32::try_from(id).ok()) {
        let pid = Pid::from_raw(process_id);
        if killpg(pid, Signal::SIGKILL).is_err() {
            // Keep the single-process fallback when a child did not create the
            // expected process group or the group disappeared concurrently.
            let _ = kill(pid, Signal::SIGKILL);
        }
    }

    #[cfg(windows)]
    if let Some(process_id) = process_id {
        let _ = std::process::Command::new("taskkill")
            .args(["/PID", &process_id.to_string(), "/T", "/F"])
            .status();
    }
}

fn emit_progress(
    progress: &mut Option<&mut (dyn FnMut(ProcessProgress) + Send)>,
    value: ProcessProgress,
) {
    if let Some(sink) = progress.as_mut() {
        sink(value);
    }
}

#[derive(Debug, Default)]
struct OutputBuffer {
    bytes: Vec<u8>,
    total_bytes: u64,
    newline_count: u64,
    partial_line: bool,
    truncated: bool,
}

const MAX_LIVE_OUTPUT_BYTES: usize = 8 * 1024;
const MAX_LIVE_OUTPUT_LINES: usize = 8;
const MAX_LIVE_OUTPUT_CHARS: usize = 2_000;

#[derive(Debug, Default)]
struct LiveOutputBuffer {
    bytes: Vec<u8>,
}

impl LiveOutputBuffer {
    fn push(&mut self, input: &[u8]) {
        self.bytes.extend_from_slice(input);
        if self.bytes.len() > MAX_LIVE_OUTPUT_BYTES {
            let remove = self.bytes.len().saturating_sub(MAX_LIVE_OUTPUT_BYTES);
            self.bytes.drain(..remove);
        }
    }

    fn excerpt(&self) -> Option<String> {
        let visible = strip_terminal_controls(&String::from_utf8_lossy(&self.bytes));
        let (redacted, _) = redact_sensitive_text(&visible);
        let mut lines = redacted
            .lines()
            .filter(|line| !line.trim().is_empty())
            .rev()
            .take(MAX_LIVE_OUTPUT_LINES)
            .map(str::to_owned)
            .collect::<Vec<_>>();
        lines.reverse();
        let excerpt = lines.join("\n");
        let excerpt = excerpt
            .chars()
            .take(MAX_LIVE_OUTPUT_CHARS)
            .collect::<String>();
        (!excerpt.is_empty()).then_some(excerpt)
    }
}

fn strip_terminal_controls(value: &str) -> String {
    #[derive(Clone, Copy)]
    enum TerminalControlState {
        None,
        Start,
        Csi,
        Osc,
        OscEscape,
    }

    let mut state = TerminalControlState::None;
    let mut output = String::with_capacity(value.len());
    for character in value.chars() {
        state = match state {
            TerminalControlState::None if character == '\u{1b}' => TerminalControlState::Start,
            TerminalControlState::None if character == '\r' => {
                output.push('\n');
                TerminalControlState::None
            }
            TerminalControlState::None
                if character == '\n' || character == '\t' || !character.is_control() =>
            {
                output.push(character);
                TerminalControlState::None
            }
            TerminalControlState::None => TerminalControlState::None,
            TerminalControlState::Start if character == '[' => TerminalControlState::Csi,
            TerminalControlState::Start if character == ']' => TerminalControlState::Osc,
            TerminalControlState::Start => TerminalControlState::None,
            TerminalControlState::Csi if ('@'..='~').contains(&character) => {
                TerminalControlState::None
            }
            TerminalControlState::Csi => TerminalControlState::Csi,
            TerminalControlState::Osc if character == '\u{7}' => TerminalControlState::None,
            TerminalControlState::Osc if character == '\u{1b}' => TerminalControlState::OscEscape,
            TerminalControlState::Osc => TerminalControlState::Osc,
            TerminalControlState::OscEscape if character == '\\' => TerminalControlState::None,
            TerminalControlState::OscEscape => TerminalControlState::Osc,
        };
    }
    output
}

impl OutputBuffer {
    fn push(&mut self, input: &[u8]) {
        self.total_bytes = self
            .total_bytes
            .saturating_add(u64::try_from(input.len()).unwrap_or(u64::MAX));
        self.newline_count = self.newline_count.saturating_add(
            u64::try_from(input.iter().filter(|byte| **byte == b'\n').count()).unwrap_or(u64::MAX),
        );
        self.partial_line = input.last().is_some_and(|byte| *byte != b'\n');
        let remaining = MAX_PIPE_OUTPUT_BYTES.saturating_sub(self.bytes.len());
        let retained = remaining.min(input.len());
        self.bytes.extend_from_slice(&input[..retained]);
        self.truncated |= retained < input.len();
    }

    fn total_lines(&self) -> u64 {
        self.newline_count
            .saturating_add(u64::from(self.partial_line))
    }

    fn finish(mut self) -> String {
        if self.truncated {
            self.bytes
                .extend_from_slice(b"\n[process output truncated]\n");
        }
        String::from_utf8_lossy(&self.bytes).to_string()
    }
}

fn spawn_stream_reader<R>(mut reader: R, stream: ProcessStream, sender: Sender<PipeMessage>)
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut buffer = [0_u8; 8192];
        loop {
            match reader.read(&mut buffer).await {
                Ok(0) => {
                    let _ = sender.send(PipeMessage::Done(stream)).await;
                    break;
                }
                Ok(read) => {
                    if sender
                        .send(PipeMessage::Chunk(stream, buffer[..read].to_vec()))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Err(error) => {
                    let _ = sender.send(PipeMessage::Error(error.to_string())).await;
                    break;
                }
            }
        }
    });
}

#[cfg(test)]
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

#[cfg(test)]
pub(crate) async fn join_pipe_reader(
    reader: JoinHandle<std::io::Result<String>>,
) -> Result<String, ToolError> {
    reader
        .await
        .map_err(|error| ToolError::Execution(error.to_string()))?
        .map_err(|error| ToolError::Execution(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_excerpt_removes_terminal_controls_and_redacts_keys() {
        let mut output = LiveOutputBuffer::default();
        output.push(b"\x1b[31mstarting\x1b[0m\napi_key='secret-value'\ndone\n");

        let excerpt = output.excerpt().expect("excerpt");
        assert!(excerpt.contains("starting"));
        assert!(excerpt.contains("done"));
        assert!(!excerpt.contains("\x1b"));
        assert!(!excerpt.contains("secret-value"));
        assert!(excerpt.contains("<redacted-secret>"));
    }
}
