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
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicU8, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use golutra_core::SessionId;
use golutra_sandbox::{SandboxBackendKind, SandboxRequest, SystemSandbox, WorkspaceAccess};
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
const MAX_TERMINAL_PROCESSES: usize = 32;
const MAX_TERMINAL_OUTPUT_BYTES: usize = 32 * 1024 * 1024;
const MAX_OUTPUT_BYTES: usize = 2 * 1024 * 1024;
const MAX_POLL_WAIT_MS: u64 = 30_000;
const DEFAULT_POLL_WAIT_MS: u64 = 5_000;
const DEFAULT_START_WAIT_MS: u64 = 1_000;
const MAX_RETENTION: Duration = Duration::from_secs(15 * 60);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
const READ_BUFFER_BYTES: usize = 16 * 1024;
const READER_DRAIN_TIMEOUT: Duration = if cfg!(test) {
    Duration::from_millis(100)
} else {
    Duration::from_secs(2)
};

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

/// 终止原因按优先级锁存，避免 child.wait 与取消信号的调度顺序改写用户可见终态。
/// 优先级为显式 terminate > 取消 > 超时；较高优先级可以覆盖较低优先级。
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum TerminationReason {
    TimedOut = 1,
    Cancelled = 2,
    Terminated = 3,
}

impl TerminationReason {
    fn for_state(state: ProcessState) -> Option<Self> {
        match state {
            ProcessState::TimedOut => Some(Self::TimedOut),
            ProcessState::Cancelled => Some(Self::Cancelled),
            ProcessState::Terminated => Some(Self::Terminated),
            ProcessState::Running | ProcessState::Exited | ProcessState::Failed => None,
        }
    }

    const fn state(self) -> ProcessState {
        match self {
            Self::TimedOut => ProcessState::TimedOut,
            Self::Cancelled => ProcessState::Cancelled,
            Self::Terminated => ProcessState::Terminated,
        }
    }

    const fn code(self) -> u8 {
        self as u8
    }

    const fn from_code(code: u8) -> Option<Self> {
        match code {
            1 => Some(Self::TimedOut),
            2 => Some(Self::Cancelled),
            3 => Some(Self::Terminated),
            _ => None,
        }
    }
}

#[derive(Debug, Default)]
struct TerminationIntent {
    reason: AtomicU8,
}

impl TerminationIntent {
    fn request(&self, state: ProcessState) {
        let Some(reason) = TerminationReason::for_state(state) else {
            return;
        };
        let requested = reason.code();
        let mut current = self.reason.load(Ordering::Acquire);
        while current < requested {
            match self.reason.compare_exchange_weak(
                current,
                requested,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return,
                Err(observed) => current = observed,
            }
        }
    }

    fn state(&self) -> Option<ProcessState> {
        TerminationReason::from_code(self.reason.load(Ordering::Acquire))
            .map(TerminationReason::state)
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
    pub(crate) network_access: bool,
    pub(crate) changed_files: Vec<PathBuf>,
    pub(crate) before_images: Vec<super::FileBeforeImage>,
    pub(crate) after_images: Vec<super::FileBeforeImage>,
    pub(crate) workspace_changes_known: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProcessSummary {
    pub(crate) process_id: String,
    pub(crate) command_display: String,
    pub(crate) state: ProcessState,
    pub(crate) exit_code: Option<i32>,
    pub(crate) output_cursor: u64,
    pub(crate) output_bytes: u64,
    pub(crate) output_lines: u64,
    pub(crate) output_truncated: bool,
}

pub(crate) struct ProcessStartRequest<'a> {
    pub(crate) process_id: String,
    pub(crate) session_id: SessionId,
    pub(crate) program: &'a str,
    pub(crate) args: &'a [String],
    pub(crate) command_display: String,
    pub(crate) cwd: &'a Path,
    pub(crate) workspace_root: &'a Path,
    pub(crate) timeout_ms: u64,
    pub(crate) wait_ms: u64,
    pub(crate) cancellation: CancellationToken,
    pub(crate) sandbox: &'a SystemSandbox,
    pub(crate) workspace_access: WorkspaceAccess,
    pub(crate) allow_network: bool,
    pub(crate) workspace_before: workspace_scan::WorkspaceSnapshot,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProcessRequestIdentity {
    program: String,
    args: Vec<String>,
    cwd: PathBuf,
    workspace_root: PathBuf,
    timeout_ms: u64,
    sandbox_backend: SandboxBackendKind,
    workspace_access: WorkspaceAccess,
    allow_network: bool,
}

impl ProcessRequestIdentity {
    async fn from_request(request: &ProcessStartRequest<'_>) -> Result<Self, ToolError> {
        let cwd = canonical_process_path("working directory", request.cwd).await?;
        let workspace_root =
            canonical_process_path("workspace root", request.workspace_root).await?;
        Ok(Self {
            program: request.program.to_owned(),
            args: request.args.to_vec(),
            cwd,
            workspace_root,
            timeout_ms: request.timeout_ms.max(1),
            sandbox_backend: request.sandbox.backend(),
            workspace_access: request.workspace_access,
            allow_network: request.allow_network,
        })
    }
}

async fn canonical_process_path(label: &str, path: &Path) -> Result<PathBuf, ToolError> {
    tokio::fs::canonicalize(path).await.map_err(|error| {
        ToolError::Execution(format!(
            "process {label} could not be canonicalized (`{}`): {error}",
            path.display()
        ))
    })
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
    request_identity: ProcessRequestIdentity,
    command_display: String,
    /// Cleared once the process reaches a terminal state so a recycled OS PID
    /// cannot be signaled by a late terminate/shutdown path.
    pid: StdMutex<Option<u32>>,
    pid_registration: StdMutex<Option<PidRegistration>>,
    termination_intent: Arc<TerminationIntent>,
    stdin: Mutex<Option<ChildStdin>>,
    operation: Mutex<()>,
    output: Mutex<OutputJournal>,
    state: Mutex<ProcessStateRecord>,
    control: CancellationToken,
    notify: Notify,
    terminal_notify: Notify,
    last_touched: Mutex<Instant>,
    sandbox_backend: golutra_sandbox::SandboxBackendKind,
    sandbox_os_enforced: bool,
    network_access: bool,
}

impl std::fmt::Debug for ManagedProcess {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ManagedProcess")
            .field("id", &self.id)
            .field("session_id", &self.session_id)
            .field("command_display", &self.command_display)
            .field(
                "pid",
                &self.pid.try_lock().map(|guard| *guard).unwrap_or(None),
            )
            .finish_non_exhaustive()
    }
}

struct SupervisorInner {
    processes: Mutex<HashMap<String, Arc<ManagedProcess>>>,
    start_gate: Mutex<()>,
    shutdown: CancellationToken,
    // Drop 不能 await Tokio mutex；同步 PID 表保证 runtime 消失时仍能立即清理进程组。
    active_pids: StdMutex<HashMap<u64, PidRegistration>>,
    next_pid_token: AtomicU64,
    terminating_sessions: StdMutex<HashMap<SessionId, usize>>,
}

#[derive(Debug, Clone)]
struct PidRegistration {
    token: u64,
    pid: u32,
    termination_intent: Arc<TerminationIntent>,
}

struct TerminatingSessionGuard {
    inner: Arc<SupervisorInner>,
    session_id: SessionId,
}

impl Drop for TerminatingSessionGuard {
    fn drop(&mut self) {
        self.inner.clear_session_terminating(self.session_id);
    }
}

impl Drop for SupervisorInner {
    fn drop(&mut self) {
        self.terminate_active_processes(ProcessState::Cancelled);
        self.shutdown.cancel();
    }
}

impl SupervisorInner {
    fn register_pid(
        &self,
        pid: Option<u32>,
        termination_intent: Arc<TerminationIntent>,
    ) -> Option<PidRegistration> {
        let pid = pid?;
        let token = self.next_pid_token.fetch_add(1, Ordering::Relaxed);
        let registration = PidRegistration {
            token,
            pid,
            termination_intent,
        };
        if let Ok(mut active) = self.active_pids.lock() {
            active.insert(token, registration.clone());
            Some(registration)
        } else {
            None
        }
    }

    fn unregister_pid(&self, registration: Option<PidRegistration>) {
        if let Some(registration) = registration
            && let Ok(mut active) = self.active_pids.lock()
            && active.get(&registration.token).is_some_and(|current| {
                current.token == registration.token
                    && current.pid == registration.pid
                    && Arc::ptr_eq(
                        &current.termination_intent,
                        &registration.termination_intent,
                    )
            })
        {
            active.remove(&registration.token);
        }
    }

    fn terminate_active_processes(&self, state: ProcessState) {
        // 先锁存原因，再发信号；child.wait 即使抢先完成也只能发布该原因。
        let active = self
            .active_pids
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for registration in active.values() {
            registration.termination_intent.request(state);
            // 持有注册表锁直到发信号，避免 PID 释放与信号发送之间出现复用窗口。
            process::terminate_process_group(Some(registration.pid));
        }
    }

    fn mark_session_terminating(&self, session_id: SessionId) {
        if let Ok(mut sessions) = self.terminating_sessions.lock() {
            *sessions.entry(session_id).or_default() += 1;
        }
    }

    fn clear_session_terminating(&self, session_id: SessionId) {
        if let Ok(mut sessions) = self.terminating_sessions.lock()
            && let Some(count) = sessions.get_mut(&session_id)
        {
            *count = count.saturating_sub(1);
            if *count == 0 {
                sessions.remove(&session_id);
            }
        }
    }

    fn is_session_terminating(&self, session_id: SessionId) -> bool {
        self.terminating_sessions
            .lock()
            .map(|sessions| sessions.get(&session_id).is_some_and(|count| *count > 0))
            .unwrap_or(true)
    }

    async fn prune(&self) {
        let now = Instant::now();
        let entries = self
            .processes
            .lock()
            .await
            .iter()
            .map(|(id, entry)| (id.clone(), Arc::clone(entry)))
            .collect::<Vec<_>>();
        let mut terminal = Vec::new();
        for (id, entry) in entries {
            let (state, completed_at) = {
                let state = entry.state.lock().await;
                (state.state, state.completed_at)
            };
            if !state.is_terminal() {
                continue;
            }
            let last_touched = *entry.last_touched.lock().await;
            let retained_bytes = entry.output.lock().await.retained_bytes;
            let retention_anchor = completed_at.unwrap_or(last_touched).max(last_touched);
            terminal.push((
                id,
                retention_anchor,
                now.duration_since(retention_anchor),
                retained_bytes,
            ));
        }
        // 保留最近完成的 journal；同时限制总字节，防止大量短任务在 retention 窗口内堆积。
        terminal.sort_by_key(|(_, retention_anchor, _, _)| *retention_anchor);
        let mut remove = Vec::new();
        let mut retained_bytes = terminal
            .iter()
            .map(|(_, _, _, bytes)| *bytes)
            .sum::<usize>();
        let count_overflow = terminal.len().saturating_sub(MAX_TERMINAL_PROCESSES);
        for (index, (id, _, age, bytes)) in terminal.iter().enumerate() {
            if index < count_overflow
                || *age > MAX_RETENTION
                || retained_bytes > MAX_TERMINAL_OUTPUT_BYTES
            {
                remove.push(id.clone());
                retained_bytes = retained_bytes.saturating_sub(*bytes);
            }
        }
        remove.sort();
        remove.dedup();
        let mut processes = self.processes.lock().await;
        for id in remove {
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
                start_gate: Mutex::new(()),
                shutdown: CancellationToken::new(),
                active_pids: StdMutex::new(HashMap::new()),
                next_pid_token: AtomicU64::new(1),
                terminating_sessions: StdMutex::new(HashMap::new()),
            }),
        }
    }

    /// Stop every child owned by this supervisor. The method is synchronous
    /// so a RuntimeHost can invoke it while being dropped. Call
    /// [`Self::shutdown_and_wait`] when the caller still owns an async runtime
    /// and needs terminal bookkeeping to complete before teardown.
    pub fn shutdown(&self) {
        self.inner
            .terminate_active_processes(ProcessState::Cancelled);
        self.inner.shutdown.cancel();
    }

    /// Cancel all running processes and wait for their supervisors to record a
    /// terminal state before returning.
    ///
    /// The start gate is acquired after cancellation so a start already in
    /// flight is either registered and included in this shutdown, or observes
    /// the cancelled supervisor and never launches. Process groups are killed
    /// eagerly as well as through the supervisor cancellation branch; this
    /// closes the window in which a descendant could outlive the direct child
    /// while the Tokio runtime is being torn down.
    pub async fn shutdown_and_wait(&self) -> Result<(), ToolError> {
        self.inner
            .terminate_active_processes(ProcessState::Cancelled);
        self.inner.shutdown.cancel();
        let start_guard = self.inner.start_gate.lock().await;
        let entries = self
            .inner
            .processes
            .lock()
            .await
            .values()
            .cloned()
            .collect::<Vec<_>>();
        drop(start_guard);

        let mut running = Vec::new();
        for entry in entries {
            if !entry.state.lock().await.state.is_terminal() {
                entry.termination_intent.request(ProcessState::Cancelled);
                // Do this synchronously before awaiting terminal bookkeeping so
                // descendants are terminated even if the shutdown token is
                // delayed on a nearly-tearing-down runtime.
                let pid = take_pid(&self.inner, &entry);
                process::terminate_process_group(pid);
                running.push(entry);
            }
        }
        if running.is_empty() {
            return Ok(());
        }

        let deadline = tokio::time::Instant::now() + SHUTDOWN_TIMEOUT;
        let waits = running.iter().map(|entry| async {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            let wait_ms = u64::try_from(remaining.as_millis()).unwrap_or(u64::MAX);
            (
                entry.id.clone(),
                self.wait_for_terminal(entry, 0, wait_ms).await,
            )
        });
        let snapshots = futures_util::future::join_all(waits).await;
        let unfinished = snapshots
            .into_iter()
            .filter_map(|(id, snapshot)| (!snapshot.state.is_terminal()).then_some(id))
            .collect::<Vec<_>>();
        if unfinished.is_empty() {
            Ok(())
        } else {
            Err(ToolError::Execution(format!(
                "processes did not finish shutdown bookkeeping within {} ms: {}",
                SHUTDOWN_TIMEOUT.as_millis(),
                unfinished.join(", ")
            )))
        }
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
        let request_identity = ProcessRequestIdentity::from_request(&request).await?;
        self.prune().await;
        // Registration and launch are one operation: duplicate provider retries can reach this
        // method concurrently with the same tool-call-derived process id.
        let start_guard = self.inner.start_gate.lock().await;
        if self.inner.shutdown.is_cancelled() {
            return Err(ToolError::Execution(
                "process supervisor is shutting down".to_owned(),
            ));
        }
        if self.inner.is_session_terminating(request.session_id) {
            return Err(ToolError::Execution(
                "process session is being terminated".to_owned(),
            ));
        }
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
            if existing.request_identity != request_identity {
                return Err(ToolError::Execution(format!(
                    "process `{}` idempotency conflict: start request does not match the existing process",
                    request.process_id
                )));
            }
            drop(start_guard);
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
        let stdin = match child.stdin.take() {
            Some(stdin) => stdin,
            None => {
                return Err(
                    abort_spawned_child(child, pid, "process stdin pipe is unavailable").await,
                );
            }
        };
        let stdout = match child.stdout.take() {
            Some(stdout) => stdout,
            None => {
                return Err(
                    abort_spawned_child(child, pid, "process stdout pipe is unavailable").await,
                );
            }
        };
        let stderr = match child.stderr.take() {
            Some(stderr) => stderr,
            None => {
                return Err(
                    abort_spawned_child(child, pid, "process stderr pipe is unavailable").await,
                );
            }
        };
        let termination_intent = Arc::new(TerminationIntent::default());
        let Some(pid_registration) = self
            .inner
            .register_pid(pid, Arc::clone(&termination_intent))
        else {
            return Err(
                abort_spawned_child(child, pid, "process PID registry is unavailable").await,
            );
        };
        // shutdown() may race with the final pre-spawn check. Register first,
        // then abort immediately so that race cannot leave an unowned child.
        if self.inner.shutdown.is_cancelled() {
            self.inner.unregister_pid(Some(pid_registration));
            return Err(
                abort_spawned_child(child, pid, "process supervisor is shutting down").await,
            );
        }
        let entry = Arc::new(ManagedProcess {
            id: request.process_id,
            session_id: request.session_id,
            request_identity,
            command_display: request.command_display,
            pid: StdMutex::new(pid),
            pid_registration: StdMutex::new(Some(pid_registration)),
            termination_intent,
            stdin: Mutex::new(Some(stdin)),
            operation: Mutex::new(()),
            output: Mutex::new(OutputJournal::default()),
            state: Mutex::new(ProcessStateRecord {
                state: ProcessState::Running,
                exit_code: None,
                workspace_scan: None,
                completed_at: None,
            }),
            control: CancellationToken::new(),
            notify: Notify::new(),
            terminal_notify: Notify::new(),
            last_touched: Mutex::new(Instant::now()),
            sandbox_backend: launch.backend,
            sandbox_os_enforced: launch.os_enforced,
            network_access: request.allow_network,
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
        let workspace_root = request.workspace_root.to_path_buf();
        let workspace_before = request.workspace_before;
        let timeout = Duration::from_millis(request.timeout_ms.max(1));
        let supervisor_inner = Arc::downgrade(&self.inner);
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
            supervisor_inner,
        ));
        drop(start_guard);

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

    pub(crate) async fn list(&self, session_id: SessionId) -> Vec<ProcessSummary> {
        self.prune().await;
        let mut entries = self
            .inner
            .processes
            .lock()
            .await
            .values()
            .filter(|entry| entry.session_id == session_id)
            .cloned()
            .collect::<Vec<_>>();
        entries.sort_by(|left, right| left.id.cmp(&right.id));

        let mut summaries = Vec::with_capacity(entries.len());
        for entry in entries {
            let output = entry.output.lock().await;
            let output_cursor = output.next_cursor;
            let output_bytes = output.total_bytes;
            let output_lines = output.lines();
            let output_truncated = output.truncated;
            drop(output);
            let state = entry.state.lock().await;
            summaries.push(ProcessSummary {
                process_id: entry.id.clone(),
                command_display: entry.command_display.clone(),
                state: state.state,
                exit_code: state.exit_code,
                output_cursor,
                output_bytes,
                output_lines,
                output_truncated,
            });
        }
        summaries
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
        // 只串行化 stdin 和终止控制，等待事件时不持有该锁。
        let _operation_guard = entry.operation.lock().await;
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
        drop(_operation_guard);
        self.poll_for(&entry, cursor, wait_ms).await
    }

    pub(crate) async fn terminate(
        &self,
        session_id: SessionId,
        process_id: &str,
        cursor: u64,
    ) -> Result<ProcessSnapshot, ToolError> {
        let entry = self.entry(session_id, process_id).await?;
        {
            let _operation_guard = entry.operation.lock().await;
            if entry.state.lock().await.state.is_terminal() {
                // Already terminal: never signal a retained/stale PID.
                return Ok(snapshot(&entry, cursor).await);
            }
            entry.termination_intent.request(ProcessState::Terminated);
            entry.control.cancel();
            // Mirror shutdown_and_wait: kill the process group eagerly so a
            // descendant cannot outlive the wait window while cancellation is
            // still propagating through the supervisor task. take() so a racing
            // terminal publication cannot leave us signaling after release.
            let pid = take_pid(&self.inner, &entry);
            process::terminate_process_group(pid);
        }
        let snapshot = self.wait_for_terminal(&entry, cursor, 5_000).await;
        if snapshot.state.is_terminal() {
            Ok(snapshot)
        } else {
            Err(ToolError::Execution(format!(
                "process `{process_id}` did not terminate within 5000 ms"
            )))
        }
    }

    /// Terminate all running processes owned by one session and wait for their
    /// supervisors to record a terminal state.
    ///
    /// A delegated child is archived as soon as it returns, so leaving one of
    /// its managed processes alive would make that process unreachable through
    /// the normal session-scoped process tools. The shared deadline keeps one
    /// misbehaving process from serially extending cleanup for every sibling.
    pub async fn terminate_session(&self, session_id: SessionId) -> Result<usize, ToolError> {
        self.prune().await;
        // 只在收集并标记目标 session 时持有全局 gate；等待结束不能阻塞其他
        // session 启动。start() 会在标记期间拒绝该 session 的新进程。
        let start_guard = self.inner.start_gate.lock().await;
        self.inner.mark_session_terminating(session_id);
        let _terminating_guard = TerminatingSessionGuard {
            inner: Arc::clone(&self.inner),
            session_id,
        };
        let entries = self
            .inner
            .processes
            .lock()
            .await
            .values()
            .filter(|entry| entry.session_id == session_id)
            .cloned()
            .collect::<Vec<_>>();
        drop(start_guard);
        let mut running = Vec::new();
        for entry in entries {
            if !entry.state.lock().await.state.is_terminal() {
                entry.termination_intent.request(ProcessState::Terminated);
                entry.control.cancel();
                // Same eager kill as shutdown_and_wait: do not wait for the
                // supervisor cancellation branch to schedule before descendants
                // are terminated.
                let pid = take_pid(&self.inner, &entry);
                process::terminate_process_group(pid);
                running.push(entry);
            }
        }
        if running.is_empty() {
            return Ok(0);
        }

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let waits = running.iter().map(|entry| async {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            let wait_ms = u64::try_from(remaining.as_millis()).unwrap_or(u64::MAX);
            (
                entry.id.clone(),
                self.wait_for_terminal(entry, 0, wait_ms).await,
            )
        });
        let snapshots = futures_util::future::join_all(waits).await;
        let unfinished = snapshots
            .into_iter()
            .filter_map(|(id, snapshot)| (!snapshot.state.is_terminal()).then_some(id))
            .collect::<Vec<_>>();
        if unfinished.is_empty() {
            Ok(running.len())
        } else {
            Err(ToolError::Execution(format!(
                "processes did not terminate within 5000 ms: {}",
                unfinished.join(", ")
            )))
        }
    }

    /// Returns whether this runtime still owns a running child process.
    ///
    /// Terminal process journals may be retained for reconnects, but they do not
    /// keep the runtime host alive once every attachment is gone.
    pub async fn has_running_processes(&self) -> bool {
        self.prune().await;
        let entries = self
            .inner
            .processes
            .lock()
            .await
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for entry in entries {
            if !entry.state.lock().await.state.is_terminal() {
                return true;
            }
        }
        false
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
            tokio::pin!(notification);
            // 在读取快照前注册通知，避免输出恰好在检查窗口到达时丢失唤醒。
            notification.as_mut().enable();
            let snapshot = snapshot(entry, cursor).await;
            if snapshot.state.is_terminal()
                || snapshot.output_cursor > cursor
                || wait_ms == 0
                || tokio::time::Instant::now() >= deadline
            {
                return Ok(snapshot);
            }
            tokio::select! {
                _ = &mut notification => {}
                _ = tokio::time::sleep_until(deadline) => return Ok(snapshot),
            }
        }
    }

    async fn wait_for_terminal(
        &self,
        entry: &Arc<ManagedProcess>,
        cursor: u64,
        wait_ms: u64,
    ) -> ProcessSnapshot {
        let deadline = tokio::time::Instant::now() + Duration::from_millis(wait_ms);
        loop {
            self.touch(entry).await;
            let notification = entry.terminal_notify.notified();
            tokio::pin!(notification);
            notification.as_mut().enable();
            if entry.state.lock().await.state.is_terminal()
                || tokio::time::Instant::now() >= deadline
            {
                return snapshot(entry, cursor).await;
            }
            tokio::select! {
                _ = &mut notification => {}
                _ = tokio::time::sleep_until(deadline) => return snapshot(entry, cursor).await,
            }
        }
    }

    async fn touch(&self, entry: &ManagedProcess) {
        *entry.last_touched.lock().await = Instant::now();
    }

    async fn prune(&self) {
        self.inner.prune().await;
    }
}

async fn abort_spawned_child(mut child: Child, pid: Option<u32>, message: &str) -> ToolError {
    // 管道初始化失败时 `kill_on_drop` 只保证直接子进程，显式终止进程组才能
    // 收回已经继承管道的后代；随后 wait 负责回收 child，避免僵尸进程。
    process::terminate_process_group(pid);
    let _ = child.start_kill();
    let _ = child.wait().await;
    ToolError::Execution(message.to_owned())
}

fn take_pid(inner: &SupervisorInner, entry: &ManagedProcess) -> Option<u32> {
    let pid = entry.pid.lock().ok().and_then(|mut guard| guard.take());
    let registration = entry
        .pid_registration
        .lock()
        .ok()
        .and_then(|mut registration| registration.take());
    inner.unregister_pid(registration);
    pid
}

fn clear_pid(entry: &ManagedProcess) {
    if let Ok(mut guard) = entry.pid.lock() {
        *guard = None;
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
            // A continuously readable pipe can otherwise monopolize a current-thread
            // runtime and delay process cancellation, timeout, and terminal bookkeeping.
            tokio::task::yield_now().await;
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
    supervisor_inner: std::sync::Weak<SupervisorInner>,
) {
    let child_id = child.id();
    let mut wait = Box::pin(child.wait());
    let mut timeout_sleep = Box::pin(tokio::time::sleep(timeout));
    let exit_code = tokio::select! {
        biased;
        _ = process_control.cancelled() => {
            entry.termination_intent.request(ProcessState::Terminated);
            process::terminate_process_group(child_id);
            wait.await.ok().and_then(|status| status.code())
        }
        _ = task_cancellation.cancelled() => {
            entry.termination_intent.request(ProcessState::Cancelled);
            process::terminate_process_group(child_id);
            wait.await.ok().and_then(|status| status.code())
        }
        _ = shutdown.cancelled() => {
            entry.termination_intent.request(ProcessState::Cancelled);
            process::terminate_process_group(child_id);
            wait.await.ok().and_then(|status| status.code())
        }
        _ = &mut timeout_sleep => {
            entry.termination_intent.request(ProcessState::TimedOut);
            process::terminate_process_group(child_id);
            wait.await.ok().and_then(|status| status.code())
        }
        result = &mut wait => {
            // 先在同一无 await 路径终止继承管道的后代，随后才允许旧 PID
            // 被释放和复用；reader drain 不再需要对旧 PID 发信号。
            let exit_code = result.ok().and_then(|status| status.code());
            process::terminate_process_group_only(child_id);
            // 取消可能和 child.wait 同时完成；在最终发布前再次锁存已发出的原因。
            if process_control.is_cancelled() {
                entry.termination_intent.request(ProcessState::Terminated);
            }
            if task_cancellation.is_cancelled() || shutdown.is_cancelled() {
                entry.termination_intent.request(ProcessState::Cancelled);
            }
            exit_code
        }
    };
    release_process_pid(&entry, &supervisor_inner);
    drain_process_readers(stdout_reader, stderr_reader).await;
    let changes = workspace_scan::compare(&workspace_root, workspace_before).await;
    // Serialize terminal publication with terminate()/write() via the operation
    // lock. PID ownership was released immediately after child wait above.
    let _operation_guard = entry.operation.lock().await;
    *entry.stdin.lock().await = None;
    // 在发布锁内再次锁存取消原因，覆盖 child.wait 与终态发布之间的竞态窗口。
    if process_control.is_cancelled() {
        entry.termination_intent.request(ProcessState::Terminated);
    }
    if task_cancellation.is_cancelled() || shutdown.is_cancelled() {
        entry.termination_intent.request(ProcessState::Cancelled);
    }
    // 必须在发布锁内读取原因，避免等待锁期间新到达的 terminate 请求被自然退出覆盖。
    let state = entry
        .termination_intent
        .state()
        .unwrap_or(if exit_code == Some(0) {
            ProcessState::Exited
        } else {
            ProcessState::Failed
        });
    {
        let mut record = entry.state.lock().await;
        record.state = state;
        record.exit_code = exit_code;
        record.workspace_scan = Some(changes);
        record.completed_at = Some(Instant::now());
    }
    drop(_operation_guard);
    if let Some(inner) = supervisor_inner.upgrade() {
        inner.prune().await;
    }
    entry.notify.notify_waiters();
    entry.terminal_notify.notify_waiters();
}

fn release_process_pid(
    entry: &ManagedProcess,
    supervisor_inner: &std::sync::Weak<SupervisorInner>,
) {
    clear_pid(entry);
    let registration = entry
        .pid_registration
        .lock()
        .ok()
        .and_then(|mut registration| registration.take());
    if let Some(inner) = supervisor_inner.upgrade() {
        inner.unregister_pid(registration);
    }
}

async fn drain_process_readers(stdout_reader: JoinHandle<()>, stderr_reader: JoinHandle<()>) {
    let mut stdout_reader = stdout_reader;
    let mut stderr_reader = stderr_reader;
    // The process group has already been terminated before this point. Join
    // both pipe readers directly and use one deadline as the only fallback;
    // polling their completion status adds latency without improving safety.
    let timed_out = tokio::time::timeout(READER_DRAIN_TIMEOUT, async {
        let _ = tokio::join!(&mut stdout_reader, &mut stderr_reader);
    })
    .await
    .is_err();
    if timed_out {
        stdout_reader.abort();
        stderr_reader.abort();
    }
    // A successful join has already polled both handles to completion. Only
    // await handles that still have work after the timeout/abort branch.
    if !stdout_reader.is_finished() {
        let _ = stdout_reader.await;
    }
    if !stderr_reader.is_finished() {
        let _ = stderr_reader.await;
    }
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
        network_access: entry.network_access,
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

#[cfg(test)]
pub(crate) fn max_terminal_processes() -> usize {
    MAX_TERMINAL_PROCESSES
}

#[cfg(test)]
impl ProcessSupervisor {
    pub(crate) async fn retained_pid_for_test(
        &self,
        session_id: SessionId,
        process_id: &str,
    ) -> Option<u32> {
        let entry = self.entry(session_id, process_id).await.ok()?;
        entry.pid.lock().ok().and_then(|guard| *guard)
    }

    pub(crate) async fn retained_process_count_for_test(&self) -> usize {
        self.inner.processes.lock().await.len()
    }

    pub(crate) async fn inject_pid_for_test(
        &self,
        session_id: SessionId,
        process_id: &str,
        pid: u32,
    ) -> bool {
        let Ok(entry) = self.entry(session_id, process_id).await else {
            return false;
        };
        if !entry.state.lock().await.state.is_terminal() {
            return false;
        }
        if let Ok(mut guard) = entry.pid.lock() {
            *guard = Some(pid);
        } else {
            return false;
        }
        true
    }
}
