use std::{
    ffi::OsString,
    fs, io,
    path::{Path, PathBuf},
    process::{ExitStatus, Stdio},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use chrono::{DateTime, Utc};
use golutra_sandbox::{SandboxLaunch, SandboxRequest, SystemSandbox, WorkspaceAccess};
#[cfg(unix)]
use nix::{
    sys::signal::{Signal, killpg},
    unistd::Pid,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
    sync::mpsc,
    task::JoinHandle,
};
use uuid::Uuid;

use super::{
    MAX_RELEASE_BYTES, ReleaseError, canonical_directory, collect_release_files, digest_files,
    ensure_private_dir, set_owner_executable, sha256_file,
};

const MAX_BUILD_OUTPUT_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BuildStatus {
    Pass,
    Fail,
    Timeout,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct BuildCheck {
    pub name: String,
    pub command: Vec<String>,
    pub status: BuildStatus,
    pub exit_code: Option<i32>,
    pub duration_ms: u64,
    pub output_digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct BuildReport {
    pub builder_version: String,
    pub source_digest: String,
    pub sandbox_backend: String,
    pub sandbox_enforced: bool,
    pub checks: Vec<BuildCheck>,
    pub binary_artifacts: Vec<BuildArtifact>,
    pub passed: bool,
    pub completed_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct BuildArtifact {
    pub relative_path: String,
    pub checksum: String,
    pub size_bytes: u64,
}

#[derive(Debug, Clone)]
pub struct TrustedBuilder {
    sandbox: SystemSandbox,
    timeout: Duration,
    require_os_enforced: bool,
}

impl TrustedBuilder {
    #[must_use]
    pub fn new() -> Self {
        Self {
            sandbox: SystemSandbox::detect(),
            timeout: Duration::from_secs(10 * 60),
            require_os_enforced: true,
        }
    }

    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    #[must_use]
    pub fn with_sandbox(mut self, sandbox: SystemSandbox, require_os_enforced: bool) -> Self {
        self.sandbox = sandbox;
        self.require_os_enforced = require_os_enforced;
        self
    }

    pub async fn run(
        &self,
        source_root: impl AsRef<std::path::Path>,
        artifact_root: impl AsRef<std::path::Path>,
    ) -> Result<BuildReport, ReleaseError> {
        let source_root = canonical_directory(source_root.as_ref())?;
        let artifact_root = artifact_root.as_ref().to_path_buf();
        if self.require_os_enforced && !self.sandbox.os_enforced() {
            return Err(ReleaseError::Invalid(
                "trusted build requires macOS Seatbelt or Linux bubblewrap".to_owned(),
            ));
        }
        let scratch = tempfile::tempdir().map_err(|error| ReleaseError::Io(error.to_string()))?;
        let target_dir = scratch.path().join("target");
        fs::create_dir_all(&target_dir).map_err(|error| ReleaseError::Io(error.to_string()))?;
        let target_dir = canonical_directory(&target_dir)?;
        let target_arg = target_dir.as_os_str().to_owned();
        let commands = vec![
            (
                "fmt",
                vec!["fmt".into(), "--all".into(), "--".into(), "--check".into()],
            ),
            (
                "check",
                cargo_build_args("check", &target_arg, &["--workspace", "--all-targets"]),
            ),
            (
                "test",
                cargo_build_args("test", &target_arg, &["--workspace"]),
            ),
            (
                "build",
                cargo_build_args(
                    "build",
                    &target_arg,
                    &["--release", "--workspace", "--bins"],
                ),
            ),
        ];
        let mut checks = Vec::new();
        for (name, args) in commands {
            checks.push(
                self.run_check(name, &args, &source_root, scratch.path())
                    .await?,
            );
        }
        let files = collect_release_files(&source_root)?;
        let (source_digest, _, _) = digest_files(&source_root, &files)?;
        let sandbox_enforced = self.sandbox.os_enforced();
        let binary_artifacts = discover_binary_artifacts(&target_dir)?;
        if !binary_artifacts.is_empty() {
            stage_binary_artifacts(&target_dir, &artifact_root, &binary_artifacts)?;
        }
        Ok(BuildReport {
            builder_version: "golutra-trusted-builder-v1".to_owned(),
            source_digest,
            sandbox_backend: format!("{:?}", self.sandbox.backend()).to_ascii_lowercase(),
            sandbox_enforced,
            passed: sandbox_enforced
                && !binary_artifacts.is_empty()
                && checks.iter().all(|check| check.status == BuildStatus::Pass),
            checks,
            binary_artifacts,
            completed_at: Utc::now(),
        })
    }

    async fn run_check(
        &self,
        name: &str,
        args: &[OsString],
        source_root: &std::path::Path,
        scratch: &std::path::Path,
    ) -> Result<BuildCheck, ReleaseError> {
        let cargo = std::env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo"));
        let launch = self
            .sandbox
            .plan(&SandboxRequest {
                program: cargo,
                args: args.iter().map(OsString::from).collect(),
                cwd: source_root.to_owned(),
                workspace_root: source_root.to_owned(),
                scratch_dir: scratch.to_owned(),
                read_only_roots: Vec::new(),
                workspace_access: WorkspaceAccess::ReadOnly,
                allow_network: false,
            })
            .map_err(|error| ReleaseError::Invalid(error.to_string()))?;
        if self.require_os_enforced && !launch.os_enforced {
            return Err(ReleaseError::Invalid(
                "trusted build plan is not OS-enforced".to_owned(),
            ));
        }
        let started = Instant::now();
        let output =
            run_bounded_build_process(launch, source_root, self.timeout, MAX_BUILD_OUTPUT_BYTES)
                .await?;
        let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        match output.stop_reason {
            BuildStopReason::TimedOut => Ok(BuildCheck {
                name: name.to_owned(),
                command: rendered_args(args),
                status: BuildStatus::Timeout,
                exit_code: None,
                duration_ms,
                output_digest: "sha256:timeout".to_owned(),
            }),
            BuildStopReason::OutputLimit => Err(ReleaseError::Invalid(
                "trusted build output exceeds its size limit".to_owned(),
            )),
            BuildStopReason::Completed => {
                let mut digest = Sha256::new();
                digest.update(&output.stdout);
                digest.update(&output.stderr);
                Ok(BuildCheck {
                    name: name.to_owned(),
                    command: rendered_args(args),
                    status: if output.status.success() {
                        BuildStatus::Pass
                    } else {
                        BuildStatus::Fail
                    },
                    exit_code: output.status.code(),
                    duration_ms,
                    output_digest: format!("sha256:{:x}", digest.finalize()),
                })
            }
        }
    }
}

#[derive(Debug)]
struct BuildProcessOutput {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    stop_reason: BuildStopReason,
}

#[derive(Debug)]
struct BoundedPipeOutput {
    bytes: Vec<u8>,
    exceeded: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BuildStopReason {
    Completed,
    TimedOut,
    OutputLimit,
}

async fn run_bounded_build_process(
    launch: SandboxLaunch,
    cwd: &Path,
    timeout: Duration,
    output_limit: usize,
) -> Result<BuildProcessOutput, ReleaseError> {
    let mut command = Command::new(&launch.program);
    command
        .args(&launch.args)
        .current_dir(cwd)
        .env_clear()
        .envs(&launch.environment)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);

    let mut child = command
        .spawn()
        .map_err(|error| ReleaseError::Io(error.to_string()))?;
    let process_id = child.id();
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| ReleaseError::Io("trusted build stdout pipe is unavailable".to_owned()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| ReleaseError::Io("trusted build stderr pipe is unavailable".to_owned()))?;
    let total = Arc::new(AtomicUsize::new(0));
    let (overflow_tx, mut overflow_rx) = mpsc::channel(1);
    let stdout_reader = spawn_bounded_pipe_reader(
        stdout,
        output_limit,
        Arc::clone(&total),
        overflow_tx.clone(),
    );
    let stderr_reader = spawn_bounded_pipe_reader(stderr, output_limit, total, overflow_tx);

    let deadline = tokio::time::sleep(timeout);
    tokio::pin!(deadline);
    let (status, mut stop_reason) = tokio::select! {
        biased;
        Some(()) = overflow_rx.recv() => {
            terminate_process_tree(&mut child, process_id);
            let status = child.wait().await
                .map_err(|error| ReleaseError::Io(error.to_string()))?;
            (status, BuildStopReason::OutputLimit)
        }
        _ = &mut deadline => {
            terminate_process_tree(&mut child, process_id);
            let status = child.wait().await
                .map_err(|error| ReleaseError::Io(error.to_string()))?;
            (status, BuildStopReason::TimedOut)
        }
        status = child.wait() => {
            let status = status.map_err(|error| ReleaseError::Io(error.to_string()))?;
            // A command that exits while leaving descendants behind must not keep the output
            // pipes or build sandbox alive.
            terminate_process_group(process_id);
            (status, BuildStopReason::Completed)
        }
    };
    let stdout = join_bounded_pipe_reader(stdout_reader).await?;
    let stderr = join_bounded_pipe_reader(stderr_reader).await?;
    if stdout.exceeded || stderr.exceeded {
        stop_reason = BuildStopReason::OutputLimit;
    }
    Ok(BuildProcessOutput {
        status,
        stdout: stdout.bytes,
        stderr: stderr.bytes,
        stop_reason,
    })
}

fn spawn_bounded_pipe_reader<R>(
    mut reader: R,
    output_limit: usize,
    total: Arc<AtomicUsize>,
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
            let previous = total.fetch_add(read, Ordering::AcqRel);
            let retained = output_limit.saturating_sub(previous).min(read);
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
) -> Result<BoundedPipeOutput, ReleaseError> {
    reader
        .await
        .map_err(|error| ReleaseError::Io(error.to_string()))?
        .map_err(|error| ReleaseError::Io(error.to_string()))
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

fn cargo_build_args(command: &str, target_dir: &OsString, trailing: &[&str]) -> Vec<OsString> {
    let mut args = vec![
        command.into(),
        "--locked".into(),
        "--offline".into(),
        "--target-dir".into(),
        target_dir.clone(),
    ];
    args.extend(trailing.iter().map(OsString::from));
    args
}

fn rendered_args(args: &[OsString]) -> Vec<String> {
    args.iter()
        .map(|value| value.to_string_lossy().into_owned())
        .collect()
}

fn discover_binary_artifacts(target_dir: &Path) -> Result<Vec<BuildArtifact>, ReleaseError> {
    let mut artifacts = Vec::new();
    for name in [
        "golutra-cli",
        "golutra-tui",
        "golutra-app-server",
        "golutra-eval-worker",
        "golutra-vis",
        "golutra-supervisor",
        "golutra-launcher",
    ] {
        let file_name = if cfg!(windows) {
            format!("{name}.exe")
        } else {
            name.to_owned()
        };
        let relative_path = PathBuf::from("target").join("release").join(&file_name);
        let path = target_dir.join("release").join(&file_name);
        if !path.is_file() {
            continue;
        }
        let metadata = std::fs::symlink_metadata(&path)
            .map_err(|error| ReleaseError::Io(error.to_string()))?;
        if metadata.file_type().is_symlink() {
            return Err(ReleaseError::Invalid(format!(
                "trusted build artifact is a symlink: {}",
                path.display()
            )));
        }
        if metadata.len() > MAX_RELEASE_BYTES {
            return Err(ReleaseError::Invalid(format!(
                "trusted build artifact exceeds its size limit: {}",
                path.display()
            )));
        }
        artifacts.push(BuildArtifact {
            relative_path: relative_path.to_string_lossy().replace('\\', "/"),
            checksum: sha256_file(&path, MAX_RELEASE_BYTES)?,
            size_bytes: metadata.len(),
        });
    }
    Ok(artifacts)
}

fn stage_binary_artifacts(
    target_dir: &Path,
    artifact_root: &Path,
    artifacts: &[BuildArtifact],
) -> Result<(), ReleaseError> {
    let parent = artifact_root.parent().ok_or_else(|| {
        ReleaseError::Invalid("trusted build artifact root requires a parent directory".to_owned())
    })?;
    ensure_private_dir(parent)?;
    let staging = parent.join(format!(".artifacts-tmp-{}", Uuid::now_v7()));
    ensure_private_dir(&staging)?;
    let result = (|| {
        for artifact in artifacts {
            let relative = validated_artifact_path(&artifact.relative_path)?;
            let source = target_dir.join(
                relative
                    .strip_prefix("target")
                    .map_err(|error| ReleaseError::Invalid(error.to_string()))?,
            );
            let destination = staging.join(&relative);
            if let Some(parent) = destination.parent() {
                ensure_private_dir(parent)?;
            }
            fs::copy(&source, &destination)
                .map_err(|error| ReleaseError::Io(format!("{}: {error}", source.display())))?;
            set_owner_executable(&destination)?;
            let metadata = fs::symlink_metadata(&destination)
                .map_err(|error| ReleaseError::Io(format!("{}: {error}", destination.display())))?;
            if metadata.file_type().is_symlink()
                || !metadata.is_file()
                || metadata.len() != artifact.size_bytes
                || sha256_file(&destination, MAX_RELEASE_BYTES)? != artifact.checksum
            {
                return Err(ReleaseError::Integrity(format!(
                    "trusted build artifact changed while staging: {}",
                    artifact.relative_path
                )));
            }
        }
        if artifact_root.exists() {
            let metadata = fs::symlink_metadata(artifact_root)
                .map_err(|error| ReleaseError::Io(error.to_string()))?;
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(ReleaseError::Invalid(
                    "trusted build artifact root is not a regular directory".to_owned(),
                ));
            }
            fs::remove_dir_all(artifact_root)
                .map_err(|error| ReleaseError::Io(error.to_string()))?;
        }
        fs::rename(&staging, artifact_root).map_err(|error| ReleaseError::Io(error.to_string()))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(&staging);
    }
    result
}

fn validated_artifact_path(value: &str) -> Result<PathBuf, ReleaseError> {
    let path = Path::new(value);
    let components = path.components().collect::<Vec<_>>();
    if components.len() != 3
        || components[0].as_os_str() != "target"
        || components[1].as_os_str() != "release"
        || path.file_name().is_none()
    {
        return Err(ReleaseError::Invalid(format!(
            "trusted build artifact path is invalid: {value}"
        )));
    }
    Ok(path.to_path_buf())
}

impl Default for TrustedBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, time::Duration};

    use golutra_sandbox::{SandboxBackendKind, SandboxLaunch, SystemSandbox};
    use tempfile::tempdir;

    use super::*;

    fn minimal_workspace() -> tempfile::TempDir {
        let source = tempdir().expect("source workspace");
        fs::create_dir_all(source.path().join("src")).expect("source directory");
        fs::write(
            source.path().join("Cargo.toml"),
            r#"[package]
name = "trusted-builder-fixture"
version = "0.1.0"
edition = "2021"

[[bin]]
name = "golutra-cli"
path = "src/main.rs"

[workspace]
"#,
        )
        .expect("manifest");
        fs::write(
            source.path().join("Cargo.lock"),
            r#"# This file is automatically @generated by Cargo.
# It is not intended for manual editing.
version = 4

[[package]]
name = "trusted-builder-fixture"
version = "0.1.0"
"#,
        )
        .expect("lock file");
        fs::write(
            source.path().join("src/main.rs"),
            "fn main() {\n    println!(\"ok\");\n}\n",
        )
        .expect("binary source");
        source
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

    #[cfg(unix)]
    #[tokio::test]
    async fn bounded_build_process_collects_normal_output() {
        let cwd = tempdir().expect("cwd");
        let output = run_bounded_build_process(
            plain_launch("/bin/sh", &["-c", "printf out; printf err >&2"]),
            cwd.path(),
            Duration::from_secs(2),
            1_024,
        )
        .await
        .expect("bounded process");

        assert_eq!(output.stop_reason, BuildStopReason::Completed);
        assert!(output.status.success());
        assert_eq!(output.stdout, b"out");
        assert_eq!(output.stderr, b"err");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn bounded_build_timeout_terminates_the_process_group() {
        let cwd = tempdir().expect("cwd");
        let pid_path = cwd.path().join("build.pid");
        let script = format!(
            "printf '%s' $$ > '{}'; exec /bin/sleep 30",
            pid_path.display()
        );
        let output = run_bounded_build_process(
            plain_launch("/bin/sh", &["-c", &script]),
            cwd.path(),
            Duration::from_millis(50),
            1_024,
        )
        .await
        .expect("timed out process result");

        assert_eq!(output.stop_reason, BuildStopReason::TimedOut);
        let pid = fs::read_to_string(pid_path)
            .expect("pid")
            .parse::<i32>()
            .expect("numeric pid");
        assert!(nix::sys::signal::kill(Pid::from_raw(pid), None).is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn bounded_build_output_limit_terminates_the_process() {
        let cwd = tempdir().expect("cwd");
        let output = run_bounded_build_process(
            plain_launch(
                "/bin/sh",
                &["-c", "while :; do printf '0123456789abcdef'; done"],
            ),
            cwd.path(),
            Duration::from_secs(2),
            1_024,
        )
        .await
        .expect("bounded process result");

        assert_eq!(output.stop_reason, BuildStopReason::OutputLimit);
        assert_eq!(output.stdout.len() + output.stderr.len(), 1_024);
    }

    #[tokio::test]
    async fn trusted_builder_rejects_process_only_execution() {
        let source = minimal_workspace();
        let artifacts = tempdir().expect("artifact parent");
        let error = TrustedBuilder::new()
            .with_sandbox(SystemSandbox::process_only(), true)
            .run(source.path(), artifacts.path().join("candidate"))
            .await
            .expect_err("process-only build must be rejected");

        assert!(
            error
                .to_string()
                .contains("requires macOS Seatbelt or Linux bubblewrap")
        );
        assert!(!source.path().join("target").exists());
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn trusted_builder_uses_read_only_source_and_stages_verified_binary() {
        let source = minimal_workspace();
        let artifacts = tempdir().expect("artifact parent");
        let artifact_root = artifacts.path().join("candidate");
        let files = collect_release_files(source.path()).expect("source files");
        let (digest_before, _, _) = digest_files(source.path(), &files).expect("source digest");

        let report = TrustedBuilder::new()
            .with_timeout(Duration::from_secs(120))
            .run(source.path(), &artifact_root)
            .await
            .expect("trusted build");

        assert!(report.sandbox_enforced);
        assert!(report.passed, "{report:?}");
        assert_eq!(report.source_digest, digest_before);
        assert!(!source.path().join("target").exists());
        let staged = artifact_root.join("target/release/golutra-cli");
        assert!(staged.is_file());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_ne!(
                fs::metadata(staged)
                    .expect("staged metadata")
                    .permissions()
                    .mode()
                    & 0o111,
                0
            );
        }
    }
}
