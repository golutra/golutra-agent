//! Long-lived, bounded process sessions used by the interactive shell tools.
//!
//! A process session deliberately lives above one agent turn.  The supervisor
//! owns the child, drains both pipes, keeps a bounded cursor-addressable output
//! journal, and performs the workspace comparison only after the child exits.
//! Callers can therefore reconnect from a later turn without replaying output
//! that they have already consumed.

use std::{
    collections::{HashMap, VecDeque},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use golutra_core::SessionId;
use golutra_sandbox::{SandboxRequest, SystemSandbox, WorkspaceAccess};
use tempfile::TempDir;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    process::{Child, ChildStdin, Command},
    sync::{Mutex, Notify},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

use super::{ToolError, process, workspace_scan};

const MAX_PROCESSES: usize = 64;
const MAX_OUTPUT_BYTES: usize = 2 * 1024 * 1024;
const MAX_POLL_WAIT_MS: u64 = 30_000;
const DEFAULT_POLL_WAIT_MS: u64 = 5_000;
const DEFAULT_START_WAIT_MS: u64 = 1_000;
const MAX_RETENTION: Duration = Duration::from_secs(15 * 60);
const READ_BUFFER_BYTES: usize = 16 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProcessState {
    Running,
    Exited,
    TimedOut,
    Cancelled,
    Terminated,
    Failed,
}

impl ProcessState {
    pub(crate) const fn is_terminal(self) -> bool {
        !matches!(self, Self::Running)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProcessSnapshot {
    pub(crate) process_id: String,
    pub(crate) state: ProcessState,
    pub(crate) exit_code: Option<i32>,
    pub(crate) output: String,
    pub(crate) output_cursor: u64,
    pub(crate) output_bytes: u64,
    pub(crate) output_lines: u64,
    pub(crate) output_truncated: bool,
    pub(crate) output_lost: bool,
    pub(crate) sandbox_backend: golutra_sandbox::SandboxBackendKind,
    pub(crate) sandbox_os_enforced: bool,
    pub(crate) changed_files: Vec<PathBuf>,
    pub(crate) before_images: Vec<super::FileBeforeImage>,
    pub(crate) after_images: Vec<super::FileBeforeImage>,
    pub(crate) workspace_changes_known: bool,
}

pub(crate) struct ProcessStartRequest<'a> {
    pub(crate) process_id: String,
    pub(crate) session_id: SessionId,
    pub(crate) program: &'a str,
    pub(crate) args: &'a [String],
    pub(crate) command_display: String,
    pub(crate) cwd: &'a Path,
    pub(crate) timeout_ms: u64,
    pub(crate) wait_ms: u64,
    pub(crate) cancellation: CancellationToken,
    pub(crate) sandbox: &'a SystemSandbox,
    pub(crate) workspace_access: WorkspaceAccess,
    pub(crate) workspace_before: workspace_scan::WorkspaceSnapshot,
}

#[derive(Debug)]
struct ProcessStateRecord {
    state: ProcessState,
    exit_code: Option<i32>,
    workspace_scan: Option<workspace_scan::WorkspaceMutationScan>,
    completed_at: Option<Instant>,
}

#[derive(Debug, Clone)]
struct OutputChunk {
    start: u64,
    bytes: Vec<u8>,
}

#[derive(Debug, Default)]
struct OutputJournal {
    chunks: VecDeque<OutputChunk>,
    next_cursor: u64,
    retained_bytes: usize,
    total_lines: u64,
    total_bytes: u64,
    last_byte: Option<u8>,
    truncated: bool,
}

impl OutputJournal {
    fn append(&mut self, _stream: process::ProcessStream, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let start = self.next_cursor;
        self.next_cursor = self
            .next_cursor
            .saturating_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
        self.total_bytes = self
            .total_bytes
            .saturating_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
        self.total_lines = self.total_lines.saturating_add(
            u64::try_from(bytes.iter().filter(|byte| **byte == b'\n').count()).unwrap_or(u64::MAX),
        );
        self.last_byte = bytes.last().copied();
        self.chunks.push_back(OutputChunk {
            start,
            bytes: bytes.to_vec(),
        });
        self.retained_bytes = self.retained_bytes.saturating_add(bytes.len());
        self.trim_to_limit();
    }

    fn trim_to_limit(&mut self) {
        while self.retained_bytes > MAX_OUTPUT_BYTES {
            let excess = self.retained_bytes - MAX_OUTPUT_BYTES;
            let Some(front) = self.chunks.front_mut() else {
                self.retained_bytes = 0;
                break;
            };
            if front.bytes.len() <= excess {
                self.retained_bytes -= front.bytes.len();
                self.chunks.pop_front();
            } else {
                front.bytes.drain(..excess);
                front.start = front
                    .start
                    .saturating_add(u64::try_from(excess).unwrap_or(u64::MAX));
                self.retained_bytes -= excess;
            }
            self.truncated = true;
        }
    }

    fn snapshot(&self, cursor: u64) -> (String, bool) {
        let oldest_cursor = self
            .chunks
            .front()
            .map(|chunk| chunk.start)
            .unwrap_or(self.next_cursor);
        let output_lost = cursor < oldest_cursor && self.truncated;
        let mut bytes = Vec::new();
        if output_lost {
            bytes.extend_from_slice(b"[earlier process output omitted]\n");
        }
        for chunk in &self.chunks {
            let end = chunk
                .start
                .saturating_add(u64::try_from(chunk.bytes.len()).unwrap_or(u64::MAX));
            if end <= cursor {
                continue;
            }
            let offset = usize::try_from(cursor.saturating_sub(chunk.start)).unwrap_or(usize::MAX);
            if offset < chunk.bytes.len() {
                bytes.extend_from_slice(&chunk.bytes[offset..]);
            }
        }
        (String::from_utf8_lossy(&bytes).to_string(), output_lost)
    }

    fn lines(&self) -> u64 {
        self.total_lines
            .saturating_add(u64::from(self.last_byte.is_some_and(|byte| byte != b'\n')))
    }
}

struct ManagedProcess {
    id: String,
    session_id: SessionId,
    command_display: String,
    pid: Option<u32>,
    stdin: Mutex<Option<ChildStdin>>,
    output: Mutex<OutputJournal>,
    state: Mutex<ProcessStateRecord>,
    control: CancellationToken,
    notify: Notify,
    last_touched: Mutex<Instant>,
    sandbox_backend: golutra_sandbox::SandboxBackendKind,
    sandbox_os_enforced: bool,
}

impl std::fmt::Debug for ManagedProcess {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ManagedProcess")
            .field("id", &self.id)
            .field("session_id", &self.session_id)
            .field("command_display", &self.command_display)
            .field("pid", &self.pid)
            .finish_non_exhaustive()
    }
}

struct SupervisorInner {
    processes: Mutex<HashMap<String, Arc<ManagedProcess>>>,
    shutdown: CancellationToken,
}

impl Drop for SupervisorInner {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

#[derive(Clone)]
pub struct ProcessSupervisor {
    inner: Arc<SupervisorInner>,
}

impl std::fmt::Debug for ProcessSupervisor {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProcessSupervisor")
            .field("strong_count", &Arc::strong_count(&self.inner))
            .finish()
    }
}

impl Default for ProcessSupervisor {
    fn default() -> Self {
        Self::new()
    }
}

impl ProcessSupervisor {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(SupervisorInner {
                processes: Mutex::new(HashMap::new()),
                shutdown: CancellationToken::new(),
            }),
        }
    }

    /// Stop every child owned by this supervisor. The method is synchronous
    /// so a RuntimeHost can invoke it while being dropped.
    pub fn shutdown(&self) {
        self.inner.shutdown.cancel();
    }

    pub(crate) async fn start(
        &self,
        request: ProcessStartRequest<'_>,
    ) -> Result<ProcessSnapshot, ToolError> {
        if self.inner.shutdown.is_cancelled() {
            return Err(ToolError::Execution(
                "process supervisor is shutting down".to_owned(),
            ));
        }
        self.prune().await;
        if let Some(existing) = self
            .inner
            .processes
            .lock()
            .await
            .get(&request.process_id)
            .cloned()
        {
            if existing.session_id != request.session_id {
                return Err(ToolError::Execution(
                    "process id belongs to a different session".to_owned(),
                ));
            }
            return self.poll_for(&existing, 0, request.wait_ms).await;
        }
        let entries = self
            .inner
            .processes
            .lock()
            .await
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let mut running = 0_usize;
        for entry in entries {
            if !entry.state.lock().await.state.is_terminal() {
                running += 1;
            }
        }
        if running >= MAX_PROCESSES {
            return Err(ToolError::Execution(format!(
                "process supervisor limit reached ({MAX_PROCESSES})"
            )));
        }

        if request.cancellation.is_cancelled() {
            return Err(ToolError::Execution(
                "process start was cancelled".to_owned(),
            ));
        }
        let scratch = tempfile::Builder::new()
            .prefix("golutra-process-")
            .tempdir()
            .map_err(|error| {
                ToolError::Execution(format!("process scratch setup failed: {error}"))
            })?;
        let launch = request
            .sandbox
            .plan(&SandboxRequest {
                program: request.program.into(),
                args: request.args.iter().map(Into::into).collect(),
                cwd: request.cwd.to_path_buf(),
                workspace_root: request.cwd.to_path_buf(),
                scratch_dir: scratch.path().to_path_buf(),
                read_only_roots: Vec::new(),
                workspace_access: request.workspace_access,
                allow_network: false,
            })
            .map_err(|error| ToolError::Execution(error.to_string()))?;
        let mut command = Command::new(&launch.program);
        command
            .args(&launch.args)
            .current_dir(request.cwd)
            .env_clear()
            .envs(&launch.environment)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        command.process_group(0);
        let mut child = command
            .spawn()
            .map_err(|error| ToolError::Execution(error.to_string()))?;
        let pid = child.id();
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| ToolError::Execution("process stdin pipe is unavailable".to_owned()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| ToolError::Execution("process stdout pipe is unavailable".to_owned()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| ToolError::Execution("process stderr pipe is unavailable".to_owned()))?;
        let entry = Arc::new(ManagedProcess {
            id: request.process_id,
            session_id: request.session_id,
            command_display: request.command_display,
            pid,
            stdin: Mutex::new(Some(stdin)),
            output: Mutex::new(OutputJournal::default()),
            state: Mutex::new(ProcessStateRecord {
                state: ProcessState::Running,
                exit_code: None,
                workspace_scan: None,
                completed_at: None,
            }),
            control: CancellationToken::new(),
            notify: Notify::new(),
            last_touched: Mutex::new(Instant::now()),
            sandbox_backend: launch.backend,
            sandbox_os_enforced: launch.os_enforced,
        });
        let id = entry.id.clone();
        self.inner
            .processes
            .lock()
            .await
            .insert(id, Arc::clone(&entry));

        let weak_entry = Arc::downgrade(&entry);
        let stdout_reader =
            spawn_reader(stdout, process::ProcessStream::Stdout, weak_entry.clone());
        let stderr_reader = spawn_reader(stderr, process::ProcessStream::Stderr, weak_entry);
        let shutdown = self.inner.shutdown.clone();
        let process_control = entry.control.clone();
        let task_cancellation = request.cancellation;
        let workspace_root = request.cwd.to_path_buf();
        let workspace_before = request.workspace_before;
        let timeout = Duration::from_millis(request.timeout_ms.max(1));
        tokio::spawn(supervise_process(
            Arc::clone(&entry),
            child,
            stdout_reader,
            stderr_reader,
            scratch,
            shutdown,
            process_control,
            task_cancellation,
            workspace_root,
            workspace_before,
            timeout,
        ));

        self.poll_for(&entry, 0, request.wait_ms).await
    }

    pub(crate) async fn poll(
        &self,
        session_id: SessionId,
        process_id: &str,
        cursor: u64,
        wait_ms: u64,
    ) -> Result<ProcessSnapshot, ToolError> {
        let entry = self.entry(session_id, process_id).await?;
        self.poll_for(&entry, cursor, wait_ms).await
    }

    pub(crate) async fn reconnect(
        &self,
        session_id: SessionId,
        process_id: &str,
        cursor: u64,
    ) -> Result<ProcessSnapshot, ToolError> {
        self.poll(session_id, process_id, cursor, 0).await
    }

    pub(crate) async fn write(
        &self,
        session_id: SessionId,
        process_id: &str,
        input: &str,
        cursor: u64,
        wait_ms: u64,
    ) -> Result<ProcessSnapshot, ToolError> {
        let entry = self.entry(session_id, process_id).await?;
        self.touch(&entry).await;
        {
            let state = entry.state.lock().await;
            if state.state.is_terminal() {
                return Err(ToolError::Execution(format!(
                    "process `{process_id}` is no longer running"
                )));
            }
        }
        let mut stdin_guard = entry.stdin.lock().await;
        let Some(stdin) = stdin_guard.as_mut() else {
            return Err(ToolError::Execution("process stdin is closed".to_owned()));
        };
        stdin.write_all(input.as_bytes()).await.map_err(|error| {
            ToolError::Execution(format!("process stdin write failed: {error}"))
        })?;
        stdin.flush().await.map_err(|error| {
            ToolError::Execution(format!("process stdin flush failed: {error}"))
        })?;
        drop(stdin_guard);
        self.poll_for(&entry, cursor, wait_ms).await
    }

    pub(crate) async fn terminate(
        &self,
        session_id: SessionId,
        process_id: &str,
        cursor: u64,
    ) -> Result<ProcessSnapshot, ToolError> {
        let entry = self.entry(session_id, process_id).await?;
        entry.control.cancel();
        self.poll_for(&entry, cursor, 5_000).await
    }

    async fn entry(
        &self,
        session_id: SessionId,
        process_id: &str,
    ) -> Result<Arc<ManagedProcess>, ToolError> {
        let entry = self
            .inner
            .processes
            .lock()
            .await
            .get(process_id)
            .cloned()
            .ok_or_else(|| ToolError::Execution(format!("unknown process id `{process_id}`")))?;
        if entry.session_id != session_id {
            return Err(ToolError::Execution(
                "process id belongs to a different session".to_owned(),
            ));
        }
        Ok(entry)
    }

    async fn poll_for(
        &self,
        entry: &Arc<ManagedProcess>,
        cursor: u64,
        wait_ms: u64,
    ) -> Result<ProcessSnapshot, ToolError> {
        let wait_ms = wait_ms.min(MAX_POLL_WAIT_MS);
        let deadline = tokio::time::Instant::now() + Duration::from_millis(wait_ms);
        loop {
            self.touch(entry).await;
            let notification = entry.notify.notified();
            let snapshot = snapshot(entry, cursor).await;
            if snapshot.state.is_terminal()
                || snapshot.output_cursor > cursor
                || wait_ms == 0
                || tokio::time::Instant::now() >= deadline
            {
                return Ok(snapshot);
            }
            tokio::select! {
                _ = notification => {}
                _ = tokio::time::sleep_until(deadline) => return Ok(snapshot),
            }
        }
    }

    async fn touch(&self, entry: &ManagedProcess) {
        *entry.last_touched.lock().await = Instant::now();
    }

    async fn prune(&self) {
        let now = Instant::now();
        let entries = self
            .inner
            .processes
            .lock()
            .await
            .iter()
            .map(|(id, entry)| (id.clone(), Arc::clone(entry)))
            .collect::<Vec<_>>();
        let mut stale = Vec::new();
        for (id, entry) in entries {
            let state = entry.state.lock().await;
            let last_touched = *entry.last_touched.lock().await;
            let retention_anchor = state.completed_at.unwrap_or(last_touched).max(last_touched);
            if state.state.is_terminal() && now.duration_since(retention_anchor) > MAX_RETENTION {
                stale.push(id);
            }
        }
        let mut processes = self.inner.processes.lock().await;
        for id in stale {
            if processes.get(&id).is_some_and(|entry| {
                entry
                    .state
                    .try_lock()
                    .is_ok_and(|state| state.state.is_terminal())
            }) {
                processes.remove(&id);
            }
        }
    }
}

fn spawn_reader<R>(
    mut reader: R,
    stream: process::ProcessStream,
    entry: std::sync::Weak<ManagedProcess>,
) -> JoinHandle<()>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut buffer = vec![0_u8; READ_BUFFER_BYTES];
        loop {
            let read = match reader.read(&mut buffer).await {
                Ok(0) | Err(_) => break,
                Ok(read) => read,
            };
            let Some(entry) = entry.upgrade() else {
                break;
            };
            entry.output.lock().await.append(stream, &buffer[..read]);
            entry.notify.notify_waiters();
        }
        if let Some(entry) = entry.upgrade() {
            entry.notify.notify_waiters();
        }
    })
}

#[allow(clippy::too_many_arguments)]
async fn supervise_process(
    entry: Arc<ManagedProcess>,
    mut child: Child,
    stdout_reader: JoinHandle<()>,
    stderr_reader: JoinHandle<()>,
    _scratch: TempDir,
    shutdown: CancellationToken,
    process_control: CancellationToken,
    task_cancellation: CancellationToken,
    workspace_root: PathBuf,
    workspace_before: workspace_scan::WorkspaceSnapshot,
    timeout: Duration,
) {
    let child_id = child.id();
    let mut wait = Box::pin(child.wait());
    let mut timeout_sleep = Box::pin(tokio::time::sleep(timeout));
    let (exit_code, override_state) = tokio::select! {
        result = &mut wait => (result.ok().and_then(|status| status.code()), None),
        _ = process_control.cancelled() => {
            process::terminate_process_group(child_id);
            let exit_code = wait.await.ok().and_then(|status| status.code());
            (exit_code, Some(ProcessState::Terminated))
        }
        _ = task_cancellation.cancelled() => {
            process::terminate_process_group(child_id);
            let exit_code = wait.await.ok().and_then(|status| status.code());
            (exit_code, Some(ProcessState::Cancelled))
        }
        _ = shutdown.cancelled() => {
            process::terminate_process_group(child_id);
            let exit_code = wait.await.ok().and_then(|status| status.code());
            (exit_code, Some(ProcessState::Cancelled))
        }
        _ = &mut timeout_sleep => {
            process::terminate_process_group(child_id);
            let exit_code = wait.await.ok().and_then(|status| status.code());
            (exit_code, Some(ProcessState::TimedOut))
        }
    };
    let _ = stdout_reader.await;
    let _ = stderr_reader.await;
    let changes = workspace_scan::compare(&workspace_root, workspace_before).await;
    let state = override_state.unwrap_or_else(|| {
        if exit_code == Some(0) {
            ProcessState::Exited
        } else {
            ProcessState::Failed
        }
    });
    {
        let mut record = entry.state.lock().await;
        record.state = state;
        record.exit_code = exit_code;
        record.workspace_scan = Some(changes);
        record.completed_at = Some(Instant::now());
    }
    entry.notify.notify_waiters();
}

async fn snapshot(entry: &ManagedProcess, cursor: u64) -> ProcessSnapshot {
    let (output, output_lost, output_cursor, output_bytes, output_lines, output_truncated) = {
        let output = entry.output.lock().await;
        let (text, lost) = output.snapshot(cursor);
        (
            text,
            lost,
            output.next_cursor,
            output.total_bytes,
            output.lines(),
            output.truncated,
        )
    };
    let state = entry.state.lock().await;
    let (changed_files, before_images, after_images, workspace_changes_known) =
        state.workspace_scan.as_ref().map_or_else(
            || (Vec::new(), Vec::new(), Vec::new(), false),
            |scan| {
                (
                    scan.changed_files.clone(),
                    scan.before_images.clone(),
                    scan.after_images.clone(),
                    scan.complete,
                )
            },
        );
    ProcessSnapshot {
        process_id: entry.id.clone(),
        state: state.state,
        exit_code: state.exit_code,
        output,
        output_cursor,
        output_bytes,
        output_lines,
        output_truncated,
        output_lost,
        sandbox_backend: entry.sandbox_backend,
        sandbox_os_enforced: entry.sandbox_os_enforced,
        changed_files,
        before_images,
        after_images,
        workspace_changes_known,
    }
}

pub(crate) fn default_start_wait_ms() -> u64 {
    DEFAULT_START_WAIT_MS
}

pub(crate) fn default_poll_wait_ms() -> u64 {
    DEFAULT_POLL_WAIT_MS
}

pub(crate) fn max_poll_wait_ms() -> u64 {
    MAX_POLL_WAIT_MS
}
