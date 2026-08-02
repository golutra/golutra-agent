use std::{
    ffi::OsString, io, path::Path, path::PathBuf, process::ExitStatus, process::Stdio, sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use golutra_sandbox::{SandboxLaunch, SandboxRequest, SystemSandbox, WorkspaceAccess};
#[cfg(unix)]
use nix::{
    sys::signal::{Signal, killpg},
    unistd::Pid,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    process::Command,
    sync::mpsc,
    task::JoinHandle,
};

use crate::{CandidateProposal, CandidateRequest, ProducerKind, SupervisorError};

const MAX_PRODUCER_OUTPUT_BYTES: usize = 2 * 1024 * 1024;

#[async_trait]
pub trait CandidateProducer: Send + Sync {
    async fn produce(
        &self,
        request: CandidateRequest,
    ) -> Result<CandidateProposal, SupervisorError>;
}

#[derive(Debug, Clone)]
pub struct StaticCandidateProducer {
    proposal: Arc<CandidateProposal>,
}

impl StaticCandidateProducer {
    #[must_use]
    pub fn new(proposal: CandidateProposal) -> Self {
        Self {
            proposal: Arc::new(proposal),
        }
    }
}

#[async_trait]
impl CandidateProducer for StaticCandidateProducer {
    async fn produce(
        &self,
        request: CandidateRequest,
    ) -> Result<CandidateProposal, SupervisorError> {
        let mut proposal = (*self.proposal).clone();
        proposal.epoch_id = request.epoch_id;
        proposal.worktree = request.worktree;
        Ok(proposal)
    }
}

#[derive(Debug, Clone)]
pub struct ExternalCommandProducer {
    program: PathBuf,
    args: Vec<String>,
    timeout: Duration,
    kind: ProducerKind,
    sandbox: SystemSandbox,
}

impl ExternalCommandProducer {
    pub fn new(program: impl Into<PathBuf>) -> Result<Self, SupervisorError> {
        let program = program.into();
        if program.as_os_str().is_empty() {
            return Err(SupervisorError::Invalid(
                "external producer program is required".to_owned(),
            ));
        }
        Ok(Self {
            program,
            args: Vec::new(),
            timeout: Duration::from_secs(300),
            kind: ProducerKind::External,
            sandbox: SystemSandbox::detect(),
        })
    }

    #[must_use]
    pub fn with_args(mut self, args: Vec<String>) -> Self {
        self.args = args;
        self
    }

    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Overrides backend detection without weakening the OS-enforcement requirement.
    #[must_use]
    pub fn with_sandbox(mut self, sandbox: SystemSandbox) -> Self {
        self.sandbox = sandbox;
        self
    }
}

#[async_trait]
impl CandidateProducer for ExternalCommandProducer {
    async fn produce(
        &self,
        request: CandidateRequest,
    ) -> Result<CandidateProposal, SupervisorError> {
        let request_json = serde_json::to_vec(&request)?;
        let scratch = tempfile::Builder::new()
            .prefix("golutra-producer-")
            .tempdir()
            .map_err(|error| {
                SupervisorError::Producer(format!("producer scratch setup failed: {error}"))
            })?;
        let launch = self
            .sandbox
            .plan(&SandboxRequest {
                program: self.program.as_os_str().to_owned(),
                args: self.args.iter().map(OsString::from).collect(),
                cwd: request.worktree.clone(),
                workspace_root: request.worktree.clone(),
                scratch_dir: scratch.path().to_path_buf(),
                read_only_roots: Vec::new(),
                workspace_access: WorkspaceAccess::ReadWrite,
                allow_network: false,
            })
            .map_err(|error| SupervisorError::Producer(error.to_string()))?;
        if !launch.os_enforced {
            return Err(SupervisorError::Producer(
                "candidate producer requires macOS Seatbelt or Linux bubblewrap".to_owned(),
            ));
        }
        let output = run_producer_process(
            launch,
            &request.worktree,
            &request_json,
            self.timeout,
            MAX_PRODUCER_OUTPUT_BYTES,
        )
        .await?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let detail = stderr.trim().chars().take(512).collect::<String>();
            let detail = if detail.is_empty() {
                String::new()
            } else {
                format!(": {detail}")
            };
            return Err(SupervisorError::Producer(format!(
                "external producer exited with {}{}",
                output.status, detail
            )));
        }
        let mut proposal: CandidateProposal =
            serde_json::from_slice(&output.stdout).map_err(|error| {
                SupervisorError::Producer(format!("producer output is invalid: {error}"))
            })?;
        proposal.epoch_id = request.epoch_id;
        proposal.worktree = request.worktree;
        proposal.producer_kind = self.kind;
        Ok(proposal)
    }
}

#[derive(Debug, Clone)]
pub struct InternalCommandProducer {
    inner: ExternalCommandProducer,
}

impl InternalCommandProducer {
    pub fn new(program: impl Into<PathBuf>) -> Result<Self, SupervisorError> {
        let mut inner = ExternalCommandProducer::new(program)?;
        inner.kind = ProducerKind::Internal;
        Ok(Self { inner })
    }

    #[must_use]
    pub fn with_args(mut self, args: Vec<String>) -> Self {
        self.inner = self.inner.with_args(args);
        self
    }

    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.inner = self.inner.with_timeout(timeout);
        self
    }

    /// Overrides backend detection without weakening the OS-enforcement requirement.
    #[must_use]
    pub fn with_sandbox(mut self, sandbox: SystemSandbox) -> Self {
        self.inner = self.inner.with_sandbox(sandbox);
        self
    }
}

#[async_trait]
impl CandidateProducer for InternalCommandProducer {
    async fn produce(
        &self,
        request: CandidateRequest,
    ) -> Result<CandidateProposal, SupervisorError> {
        self.inner.produce(request).await
    }
}

#[derive(Debug)]
pub(crate) struct ProducerProcessOutput {
    pub(crate) status: ExitStatus,
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: Vec<u8>,
}

#[derive(Debug)]
struct BoundedPipeOutput {
    bytes: Vec<u8>,
    exceeded: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProcessStopReason {
    Completed,
    TimedOut,
    OutputLimit,
}

pub(crate) async fn run_producer_process(
    launch: SandboxLaunch,
    cwd: &Path,
    request_json: &[u8],
    timeout: Duration,
    output_limit: usize,
) -> Result<ProducerProcessOutput, SupervisorError> {
    let mut command = Command::new(&launch.program);
    command
        .args(&launch.args)
        .current_dir(cwd)
        .env_clear()
        .envs(&launch.environment)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);

    let mut child = command
        .spawn()
        .map_err(|error| SupervisorError::Producer(error.to_string()))?;
    let process_id = child.id();
    let stdout = child.stdout.take().ok_or_else(|| {
        SupervisorError::Producer("producer stdout pipe is unavailable".to_owned())
    })?;
    let stderr = child.stderr.take().ok_or_else(|| {
        SupervisorError::Producer("producer stderr pipe is unavailable".to_owned())
    })?;
    let (overflow_tx, mut overflow_rx) = mpsc::channel(1);
    let stdout_reader = spawn_bounded_pipe_reader(stdout, output_limit, overflow_tx.clone());
    let stderr_reader = spawn_bounded_pipe_reader(stderr, output_limit, overflow_tx);

    let mut stdin = child.stdin.take().ok_or_else(|| {
        SupervisorError::Producer("producer stdin pipe is unavailable".to_owned())
    })?;
    let request_json = request_json.to_vec();
    let stdin_writer = tokio::spawn(async move {
        stdin.write_all(&request_json).await?;
        stdin.shutdown().await
    });

    let deadline = tokio::time::sleep(timeout);
    tokio::pin!(deadline);
    let (status, stop_reason) = tokio::select! {
        biased;
        Some(()) = overflow_rx.recv() => {
            terminate_process_tree(&mut child, process_id);
            let status = child.wait().await
                .map_err(|error| SupervisorError::Producer(error.to_string()))?;
            (status, ProcessStopReason::OutputLimit)
        }
        _ = &mut deadline => {
            terminate_process_tree(&mut child, process_id);
            let status = child.wait().await
                .map_err(|error| SupervisorError::Producer(error.to_string()))?;
            (status, ProcessStopReason::TimedOut)
        }
        status = child.wait() => {
            let status = status.map_err(|error| SupervisorError::Producer(error.to_string()))?;
            terminate_process_group(process_id);
            (status, ProcessStopReason::Completed)
        }
    };
    let stdout = join_bounded_pipe_reader(stdout_reader).await?;
    let stderr = join_bounded_pipe_reader(stderr_reader).await?;
    let stdin_result = stdin_writer
        .await
        .map_err(|error| SupervisorError::Producer(error.to_string()))?;
    if stop_reason == ProcessStopReason::OutputLimit || stdout.exceeded || stderr.exceeded {
        return Err(SupervisorError::Producer(
            "external producer output exceeds its limit".to_owned(),
        ));
    }
    if stop_reason == ProcessStopReason::TimedOut {
        return Err(SupervisorError::Producer(
            "external producer timed out".to_owned(),
        ));
    }
    // A completed producer may close stdin without consuming the full request. Its exit
    // status and bounded output remain authoritative once the process group is settled.
    if let Err(error) = stdin_result
        && !(stop_reason == ProcessStopReason::Completed
            && error.kind() == io::ErrorKind::BrokenPipe)
    {
        return Err(SupervisorError::Producer(error.to_string()));
    }
    Ok(ProducerProcessOutput {
        status,
        stdout: stdout.bytes,
        stderr: stderr.bytes,
    })
}

fn spawn_bounded_pipe_reader<R>(
    mut reader: R,
    output_limit: usize,
    overflow_tx: mpsc::Sender<()>,
) -> JoinHandle<io::Result<BoundedPipeOutput>>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut bytes = Vec::with_capacity(output_limit.min(64 * 1024));
        let mut buffer = [0_u8; 8 * 1024];
        let mut exceeded = false;
        loop {
            let read = reader.read(&mut buffer).await?;
            if read == 0 {
                break;
            }
            let remaining = output_limit.saturating_sub(bytes.len());
            let retained = remaining.min(read);
            bytes.extend_from_slice(&buffer[..retained]);
            if retained < read && !exceeded {
                exceeded = true;
                let _ = overflow_tx.try_send(());
            }
        }
        Ok(BoundedPipeOutput { bytes, exceeded })
    })
}

async fn join_bounded_pipe_reader(
    reader: JoinHandle<io::Result<BoundedPipeOutput>>,
) -> Result<BoundedPipeOutput, SupervisorError> {
    reader
        .await
        .map_err(|error| SupervisorError::Producer(error.to_string()))?
        .map_err(|error| SupervisorError::Producer(error.to_string()))
}

fn terminate_process_tree(child: &mut tokio::process::Child, process_id: Option<u32>) {
    terminate_process_group(process_id);
    let _ = child.start_kill();
}

fn terminate_process_group(process_id: Option<u32>) {
    #[cfg(unix)]
    if let Some(process_id) = process_id.and_then(|id| i32::try_from(id).ok()) {
        let _ = killpg(Pid::from_raw(process_id), Signal::SIGKILL);
    }
    #[cfg(not(unix))]
    let _ = process_id;
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, fs, time::Instant};

    use chrono::Utc;
    use golutra_sandbox::SandboxBackendKind;

    use super::*;
    use crate::{CandidateRisk, EvolutionOpportunity, PrivacyClass};

    fn request(worktree: &Path) -> CandidateRequest {
        CandidateRequest {
            epoch_id: "epoch-test".to_owned(),
            opportunity: EvolutionOpportunity {
                opportunity_id: "opportunity-test".to_owned(),
                source_version: "runtime-v1".to_owned(),
                source_task_refs: vec!["task-1".to_owned(), "task-2".to_owned()],
                independent_groups: vec!["group-a".to_owned(), "group-b".to_owned()],
                observation_refs: vec!["event:1".to_owned()],
                failure_cluster: "provider-failure".to_owned(),
                suspected_layer: "provider".to_owned(),
                causal_hypothesis: "retry boundary is incomplete".to_owned(),
                expected_effect: "provider calls recover".to_owned(),
                confidence: 80,
                privacy_class: PrivacyClass::Redacted,
                proposed_eval_slices: vec!["provider-retry".to_owned()],
                created_at: Utc::now(),
            },
            worktree: worktree.to_path_buf(),
            source_version: "runtime-v1".to_owned(),
            observation_bundle_refs: vec!["observation://task-1".to_owned()],
        }
    }

    fn proposal(worktree: &Path) -> CandidateProposal {
        CandidateProposal {
            candidate_id: Some("candidate-test".to_owned()),
            epoch_id: "untrusted-epoch".to_owned(),
            producer_kind: ProducerKind::Internal,
            producer_version: "producer-v1".to_owned(),
            source_commit: "commit-v1".to_owned(),
            worktree: worktree.to_path_buf(),
            patch_digest: "sha256:proposal".to_owned(),
            target_paths: vec!["crates/golutra-runtime/src/lib.rs".to_owned()],
            change_class: "provider-retry".to_owned(),
            generation_model: "test-model".to_owned(),
            generation_config_digest: "sha256:config".to_owned(),
            risk_level: CandidateRisk::Low,
            state_migration_ref: None,
            rollback_plan: "restore the stable release pointer".to_owned(),
        }
    }

    fn plain_launch(program: &str, args: &[&str]) -> SandboxLaunch {
        SandboxLaunch {
            backend: SandboxBackendKind::ProcessOnly,
            os_enforced: true,
            program: program.into(),
            args: args.iter().map(OsString::from).collect(),
            environment: BTreeMap::new(),
        }
    }

    #[cfg(target_os = "linux")]
    fn process_is_running(pid: Pid) -> bool {
        let Ok(stat) = fs::read_to_string(format!("/proc/{}/stat", pid.as_raw())) else {
            return false;
        };
        let Some(command_end) = stat.rfind(')') else {
            return true;
        };
        !matches!(
            stat[command_end.saturating_add(1)..]
                .split_whitespace()
                .next(),
            Some("Z" | "X")
        )
    }

    #[cfg(all(unix, not(target_os = "linux")))]
    fn process_is_running(pid: Pid) -> bool {
        nix::sys::signal::kill(pid, None).is_ok()
    }

    #[tokio::test]
    async fn process_only_candidate_producer_is_rejected() {
        let workspace = tempfile::tempdir().expect("workspace");
        let producer = ExternalCommandProducer::new("/bin/cat")
            .expect("producer")
            .with_sandbox(SystemSandbox::process_only());

        let error = producer
            .produce(request(workspace.path()))
            .await
            .expect_err("process-only producer must fail");

        assert!(
            error
                .to_string()
                .contains("requires macOS Seatbelt or Linux bubblewrap")
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn bounded_process_returns_normal_output() {
        let workspace = tempfile::tempdir().expect("workspace");
        let output = run_producer_process(
            plain_launch("/bin/cat", &[]),
            workspace.path(),
            b"request",
            Duration::from_secs(2),
            1_024,
        )
        .await
        .expect("normal output");

        assert!(output.status.success());
        assert_eq!(output.stdout, b"request");
        assert!(output.stderr.is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn producer_timeout_terminates_the_process_group() {
        let workspace = tempfile::tempdir().expect("workspace");
        let pid_path = workspace.path().join("producer.pid");
        let script = format!(
            "printf '%s' $$ > '{}'; exec /bin/sleep 30",
            pid_path.display()
        );
        let started = Instant::now();
        let error = run_producer_process(
            plain_launch("/bin/sh", &["-c", &script]),
            workspace.path(),
            b"request",
            Duration::from_millis(50),
            1_024,
        )
        .await
        .expect_err("producer must time out");

        assert!(error.to_string().contains("timed out"));
        assert!(started.elapsed() < Duration::from_secs(2));
        let pid = fs::read_to_string(pid_path)
            .expect("pid file")
            .parse::<i32>()
            .expect("pid");
        assert!(nix::sys::signal::kill(Pid::from_raw(pid), None).is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn completed_producer_does_not_leave_pipe_holding_descendants() {
        let workspace = tempfile::tempdir().expect("workspace");
        let child_pid_path = workspace.path().join("producer-child.pid");
        let request = vec![b'x'; 256 * 1024];
        let script = format!(
            "/bin/sleep 30 & printf '%s' $! > '{}'",
            child_pid_path.display()
        );
        let started = Instant::now();
        let output = run_producer_process(
            plain_launch("/bin/sh", &["-c", &script]),
            workspace.path(),
            &request,
            Duration::from_secs(2),
            1_024,
        )
        .await
        .expect("producer output");

        assert!(output.status.success());
        assert!(started.elapsed() < Duration::from_secs(2));
        let child_pid = fs::read_to_string(child_pid_path)
            .expect("child pid")
            .parse::<i32>()
            .expect("numeric child pid");
        assert!(!process_is_running(Pid::from_raw(child_pid)));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn producer_output_limit_terminates_the_process() {
        let workspace = tempfile::tempdir().expect("workspace");
        let error = run_producer_process(
            plain_launch(
                "/bin/sh",
                &["-c", "while :; do printf '0123456789abcdef'; done"],
            ),
            workspace.path(),
            b"request",
            Duration::from_secs(2),
            1_024,
        )
        .await
        .expect_err("producer output must be bounded");

        assert!(error.to_string().contains("output exceeds its limit"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn os_sandboxed_producer_returns_a_normalized_proposal() {
        let sandbox = SystemSandbox::detect();
        if !sandbox.os_enforced() {
            return;
        }
        let workspace = tempfile::tempdir().expect("workspace");
        let proposal_path = workspace.path().join("proposal.json");
        fs::write(
            &proposal_path,
            serde_json::to_vec(&proposal(workspace.path())).expect("proposal json"),
        )
        .expect("proposal fixture");
        let script_path = workspace.path().join("producer.sh");
        fs::write(
            &script_path,
            format!(
                "#!/bin/sh\nset -eu\ncat >/dev/null\ncat '{}'\n",
                proposal_path.display()
            ),
        )
        .expect("producer script");
        let request = request(workspace.path());
        let producer = ExternalCommandProducer::new("/bin/sh")
            .expect("producer")
            .with_args(vec![script_path.display().to_string()])
            .with_sandbox(sandbox);

        let result = producer.produce(request.clone()).await.expect("proposal");

        assert_eq!(result.epoch_id, request.epoch_id);
        assert_eq!(result.worktree, request.worktree);
        assert_eq!(result.producer_kind, ProducerKind::External);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn producer_cannot_read_outside_its_worktree() {
        let sandbox = SystemSandbox::detect();
        if !sandbox.os_enforced() {
            return;
        }
        let workspace = tempfile::tempdir().expect("workspace");
        let outside = tempfile::tempdir().expect("outside");
        let secret_path = outside.path().join("secret.txt");
        fs::write(&secret_path, "supervisor-secret").expect("outside fixture");
        let script_path = workspace.path().join("producer.sh");
        fs::write(
            &script_path,
            "#!/bin/sh\nset -eu\ncat >/dev/null\ncat \"$1\" >/dev/null\n",
        )
        .expect("producer script");
        let producer = ExternalCommandProducer::new("/bin/sh")
            .expect("producer")
            .with_args(vec![
                script_path.display().to_string(),
                secret_path.display().to_string(),
            ])
            .with_sandbox(sandbox);

        let error = producer
            .produce(request(workspace.path()))
            .await
            .expect_err("outside read must fail");

        assert!(error.to_string().contains("external producer exited"));
        assert!(!error.to_string().contains("supervisor-secret"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn producer_cannot_open_network_connections() {
        use std::{
            io::Write,
            net::TcpListener,
            thread,
            time::{Duration, Instant},
        };

        let sandbox = SystemSandbox::detect();
        let curl = Path::new("/usr/bin/curl");
        if !sandbox.os_enforced() || !curl.is_file() {
            return;
        }
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
        listener
            .set_nonblocking(true)
            .expect("nonblocking listener");
        let address = listener.local_addr().expect("listener address");
        let server = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let _ = stream.write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                        );
                        return true;
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        if Instant::now() >= deadline {
                            return false;
                        }
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => return false,
                }
            }
        });
        let workspace = tempfile::tempdir().expect("workspace");
        let script_path = workspace.path().join("producer.sh");
        fs::write(
            &script_path,
            format!(
                "#!/bin/sh\nset -eu\ncat >/dev/null\n/usr/bin/curl --silent --show-error --max-time 1 'http://{address}' >/dev/null\n"
            ),
        )
        .expect("producer script");
        let producer = ExternalCommandProducer::new("/bin/sh")
            .expect("producer")
            .with_args(vec![script_path.display().to_string()])
            .with_sandbox(sandbox);

        let error = producer
            .produce(request(workspace.path()))
            .await
            .expect_err("network access must fail");

        assert!(error.to_string().contains("external producer exited"));
        assert!(!server.join().expect("server thread"));
    }
}
