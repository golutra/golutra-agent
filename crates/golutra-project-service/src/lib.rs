//! Project-owned persistent service lifecycle.
//!
//! Runtime-managed process sessions intentionally terminate with their runtime.
//! This module controls services whose ownership belongs to an external project or
//! system backend, and keeps only a bounded recovery descriptor in Golutra state.

use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{self, ErrorKind, Write},
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use fs2::FileExt;
#[cfg(unix)]
use nix::{
    sys::signal::{Signal, killpg},
    unistd::Pid,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
};

const DESCRIPTOR_VERSION: u32 = 1;
const MAX_SERVICES_PER_WORKSPACE: usize = 256;
const MAX_DESCRIPTOR_BYTES: u64 = 64 * 1024;
const MAX_COMMAND_ARGS: usize = 128;
const MAX_COMMAND_BYTES: usize = 32 * 1024;
const MAX_SERVICE_NAME_BYTES: usize = 64;
const MAX_CAPTURE_BYTES: usize = 1024 * 1024;
const MAX_LOG_TAIL_LINES: u32 = 5_000;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(120);
const READER_DRAIN_TIMEOUT: Duration = Duration::from_secs(2);
const SUCCESSFUL_READER_DRAIN_TIMEOUT: Duration = Duration::from_millis(250);
const LOCK_WAIT_TIMEOUT: Duration = if cfg!(test) {
    Duration::from_millis(100)
} else {
    Duration::from_secs(5 * 60)
};
const LOCK_RETRY_INTERVAL: Duration = Duration::from_millis(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectServiceBackendKind {
    Tmux,
    DockerCompose,
    SystemdUser,
}

impl std::fmt::Display for ProjectServiceBackendKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Tmux => "tmux",
            Self::DockerCompose => "docker_compose",
            Self::SystemdUser => "systemd_user",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectServiceSpec {
    Tmux {
        command: Vec<String>,
    },
    DockerCompose {
        compose_file: PathBuf,
        services: Vec<String>,
    },
    SystemdUser {
        command: Vec<String>,
    },
}

impl ProjectServiceSpec {
    #[must_use]
    pub const fn backend(&self) -> ProjectServiceBackendKind {
        match self {
            Self::Tmux { .. } => ProjectServiceBackendKind::Tmux,
            Self::DockerCompose { .. } => ProjectServiceBackendKind::DockerCompose,
            Self::SystemdUser { .. } => ProjectServiceBackendKind::SystemdUser,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectServiceStartRequest {
    pub name: String,
    pub spec: ProjectServiceSpec,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectServiceState {
    Running,
    Stopped,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProjectServiceSummary {
    pub name: String,
    pub backend: ProjectServiceBackendKind,
    pub state: ProjectServiceState,
    pub detail: String,
    pub registered_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProjectServiceLog {
    pub service: ProjectServiceSummary,
    pub output: String,
    pub truncated: bool,
}

#[derive(Debug, Error)]
pub enum ProjectServiceError {
    #[error("invalid project service request: {0}")]
    InvalidRequest(String),
    #[error("project service `{0}` is already running")]
    AlreadyRunning(String),
    #[error(
        "project service `{name}` status is unknown for backend `{backend}`; refusing to start again: {detail}"
    )]
    StatusUnknown {
        name: String,
        backend: ProjectServiceBackendKind,
        detail: String,
    },
    #[error("project service `{0}` is not registered")]
    NotRegistered(String),
    #[error(
        "project service backend `{backend}` target `{target}` already exists without a Golutra ownership descriptor"
    )]
    OwnershipConflict {
        backend: ProjectServiceBackendKind,
        target: String,
    },
    #[error("project service backend `{backend}` is unavailable: `{program}` was not found")]
    BackendUnavailable {
        backend: ProjectServiceBackendKind,
        program: String,
    },
    #[error("project service backend `{backend}` failed: {message}")]
    Backend {
        backend: ProjectServiceBackendKind,
        message: String,
    },
    #[error("project service registry failed: {0}")]
    Registry(String),
    #[error("project service start failed: {primary}; rollback also failed: {rollback}")]
    StartRollback {
        primary: Box<ProjectServiceError>,
        rollback: Box<ProjectServiceError>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "backend", rename_all = "snake_case")]
enum BackendTarget {
    Tmux {
        session_name: String,
    },
    DockerCompose {
        project_name: String,
        compose_file: PathBuf,
        services: Vec<String>,
    },
    SystemdUser {
        unit_name: String,
    },
}

impl BackendTarget {
    const fn kind(&self) -> ProjectServiceBackendKind {
        match self {
            Self::Tmux { .. } => ProjectServiceBackendKind::Tmux,
            Self::DockerCompose { .. } => ProjectServiceBackendKind::DockerCompose,
            Self::SystemdUser { .. } => ProjectServiceBackendKind::SystemdUser,
        }
    }
}

fn descriptor_owns_target(existing: Option<&ServiceDescriptor>, requested: &BackendTarget) -> bool {
    existing.is_some_and(|descriptor| {
        descriptor.target.kind() == requested.kind() && descriptor.target == *requested
    })
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ServiceDescriptorPhase {
    Launching,
    #[default]
    Registered,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ServiceDescriptor {
    version: u32,
    workspace_fingerprint: String,
    name: String,
    registered_at: DateTime<Utc>,
    #[serde(default)]
    phase: ServiceDescriptorPhase,
    target: BackendTarget,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BackendInspection {
    state: ProjectServiceState,
    detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BackendLog {
    output: String,
    truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum StartPreparation {
    Launch,
    AlreadyRunning(BackendInspection),
}

#[derive(Debug, Clone, Copy)]
struct BackendContext<'a> {
    workspace_root: &'a Path,
    workspace_fingerprint: &'a str,
    service_name: &'a str,
}

#[async_trait]
trait ProjectServiceAdapter: std::fmt::Debug + Send + Sync {
    fn kind(&self) -> ProjectServiceBackendKind;

    fn validate_target(
        &self,
        context: &BackendContext<'_>,
        target: &BackendTarget,
    ) -> Result<(), ProjectServiceError>;

    fn resolve_target(
        &self,
        context: &BackendContext<'_>,
        spec: &ProjectServiceSpec,
    ) -> Result<BackendTarget, ProjectServiceError>;

    async fn prepare_start(
        &self,
        context: &BackendContext<'_>,
        target: &BackendTarget,
        owned: bool,
    ) -> Result<StartPreparation, ProjectServiceError>;

    async fn launch(
        &self,
        context: &BackendContext<'_>,
        spec: &ProjectServiceSpec,
        target: &BackendTarget,
    ) -> Result<(), ProjectServiceError>;

    async fn inspect(
        &self,
        context: &BackendContext<'_>,
        target: &BackendTarget,
    ) -> Result<BackendInspection, ProjectServiceError>;

    async fn logs(
        &self,
        context: &BackendContext<'_>,
        target: &BackendTarget,
        tail_lines: u32,
    ) -> Result<BackendLog, ProjectServiceError>;

    async fn stop(
        &self,
        context: &BackendContext<'_>,
        target: &BackendTarget,
    ) -> Result<(), ProjectServiceError>;
}

#[derive(Debug, Clone)]
pub struct ProjectServiceManager {
    workspace_root: PathBuf,
    workspace_fingerprint: String,
    registry_dir: PathBuf,
    adapters: BTreeMap<ProjectServiceBackendKind, Arc<dyn ProjectServiceAdapter>>,
}

impl ProjectServiceManager {
    pub fn new(
        workspace_root: impl AsRef<Path>,
        state_root: impl AsRef<Path>,
    ) -> Result<Self, ProjectServiceError> {
        let adapters = [
            Arc::new(TmuxAdapter) as Arc<dyn ProjectServiceAdapter>,
            Arc::new(DockerComposeAdapter),
            Arc::new(SystemdUserAdapter),
        ]
        .into_iter()
        .map(|adapter| (adapter.kind(), adapter))
        .collect();
        Self::with_adapters(workspace_root, state_root, adapters)
    }

    fn with_adapters(
        workspace_root: impl AsRef<Path>,
        state_root: impl AsRef<Path>,
        adapters: BTreeMap<ProjectServiceBackendKind, Arc<dyn ProjectServiceAdapter>>,
    ) -> Result<Self, ProjectServiceError> {
        let workspace_root = fs::canonicalize(workspace_root.as_ref()).map_err(|error| {
            ProjectServiceError::InvalidRequest(format!(
                "workspace root {} cannot be resolved: {error}",
                workspace_root.as_ref().display()
            ))
        })?;
        if !workspace_root.is_dir() {
            return Err(ProjectServiceError::InvalidRequest(format!(
                "workspace root is not a directory: {}",
                workspace_root.display()
            )));
        }
        let workspace_fingerprint = workspace_fingerprint(&workspace_root);
        let registry_dir = state_root
            .as_ref()
            .join("project-services")
            .join(&workspace_fingerprint);
        Ok(Self {
            workspace_root,
            workspace_fingerprint,
            registry_dir,
            adapters,
        })
    }

    pub async fn start(
        &self,
        request: ProjectServiceStartRequest,
    ) -> Result<ProjectServiceSummary, ProjectServiceError> {
        validate_service_name(&request.name)?;
        validate_spec(&self.workspace_root, &request.spec)?;
        let _service_lock = self.acquire_service_lock(&request.name).await?;
        let adapter = self.adapter(request.spec.backend())?;
        let context = self.context(&request.name);
        let target = adapter.resolve_target(&context, &request.spec)?;
        let existing = self.read_descriptor_optional(&request.name).await?;
        if let Some(existing) = &existing {
            let summary = self.inspect_descriptor(existing).await?;
            match summary.state {
                ProjectServiceState::Running => {
                    if existing.phase == ServiceDescriptorPhase::Launching {
                        let mut registered = existing.clone();
                        registered.phase = ServiceDescriptorPhase::Registered;
                        self.write_descriptor(&registered)?;
                    }
                    return Err(ProjectServiceError::AlreadyRunning(request.name));
                }
                ProjectServiceState::Unknown => {
                    // A failed status probe is not evidence that the external
                    // service stopped. Preserve the descriptor and fail closed
                    // to avoid launching a duplicate project-owned service.
                    return Err(ProjectServiceError::StatusUnknown {
                        name: request.name,
                        backend: summary.backend,
                        detail: bounded_detail(&summary.detail),
                    });
                }
                ProjectServiceState::Stopped => {}
            }
        }
        let owned = descriptor_owns_target(existing.as_ref(), &target);
        match adapter.prepare_start(&context, &target, owned).await? {
            StartPreparation::AlreadyRunning(inspection) => {
                let existing = existing
                    .as_ref()
                    .ok_or_else(|| ProjectServiceError::Backend {
                        backend: request.spec.backend(),
                        message: "backend reported an owned running target without a descriptor"
                            .to_owned(),
                    })?;
                let mut registered = existing.clone();
                registered.phase = ServiceDescriptorPhase::Registered;
                self.write_descriptor(&registered)?;
                return Ok(self.summary_from_inspection(&registered, inspection));
            }
            StartPreparation::Launch => {}
        }
        let launching = ServiceDescriptor {
            version: DESCRIPTOR_VERSION,
            workspace_fingerprint: self.workspace_fingerprint.clone(),
            name: request.name.clone(),
            registered_at: Utc::now(),
            phase: ServiceDescriptorPhase::Launching,
            target,
        };
        // Write the recovery intent before the adapter invokes its external start command. A
        // cancellation or an uncertain launcher failure can therefore be reconciled later.
        // Reserve the registry slot only for this short descriptor transaction; backend probes
        // and process startup must not block unrelated services.
        let _registry_lock = self.acquire_registry_lock().await?;
        self.ensure_registry_capacity(&request.name).await?;
        self.write_descriptor(&launching)?;
        drop(_registry_lock);
        if let Err(primary) = adapter
            .launch(&context, &request.spec, &launching.target)
            .await
        {
            return self.finish_failed_start(&launching, primary).await;
        }
        let summary = self.inspect_descriptor(&launching).await?;
        if summary.state != ProjectServiceState::Running {
            let primary = ProjectServiceError::Backend {
                backend: summary.backend,
                message: format!(
                    "backend did not reach running state after launch: {}",
                    bounded_detail(&summary.detail)
                ),
            };
            return self
                .finish_failed_start_with_inspection(&launching, summary, primary)
                .await;
        }
        let mut registered = launching;
        registered.phase = ServiceDescriptorPhase::Registered;
        // If this write fails, the on-disk Launching descriptor remains available for recovery.
        self.write_descriptor(&registered)?;
        Ok(summary)
    }

    pub async fn status(&self, name: &str) -> Result<ProjectServiceSummary, ProjectServiceError> {
        validate_service_name(name)?;
        let _service_lock = self.acquire_service_lock(name).await?;
        let descriptor = self.read_descriptor(name).await?;
        let summary = self.inspect_descriptor(&descriptor).await?;
        self.reconcile_descriptor(&descriptor, &summary).await?;
        Ok(summary)
    }

    pub async fn list(&self) -> Result<Vec<ProjectServiceSummary>, ProjectServiceError> {
        // Listing is also a recovery boundary, but backend probes can take up to the command
        // timeout. Keep the global lock only long enough to take a bounded snapshot, then use
        // each service lock to serialize its probe and reconciliation with start/status/stop.
        let descriptors = {
            let _registry_lock = self.acquire_registry_lock().await?;
            self.read_descriptors().await?
        };
        let mut summaries = Vec::with_capacity(descriptors.len());
        for snapshot in descriptors {
            let _service_lock = self.acquire_service_lock(&snapshot.name).await?;
            let Some(descriptor) = self.read_descriptor_optional(&snapshot.name).await? else {
                continue;
            };
            let summary = self.inspect_descriptor(&descriptor).await?;
            self.reconcile_descriptor(&descriptor, &summary).await?;
            summaries.push(summary);
        }
        summaries.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(summaries)
    }

    pub async fn logs(
        &self,
        name: &str,
        tail_lines: u32,
    ) -> Result<ProjectServiceLog, ProjectServiceError> {
        validate_service_name(name)?;
        if tail_lines == 0 || tail_lines > MAX_LOG_TAIL_LINES {
            return Err(ProjectServiceError::InvalidRequest(format!(
                "tail_lines must be between 1 and {MAX_LOG_TAIL_LINES}"
            )));
        }
        let _service_lock = self.acquire_service_lock(name).await?;
        let descriptor = self.read_descriptor(name).await?;
        let adapter = self.adapter(descriptor.target.kind())?;
        let context = self.context(name);
        let service = self.inspect_descriptor(&descriptor).await?;
        let log = adapter
            .logs(&context, &descriptor.target, tail_lines)
            .await?;
        Ok(ProjectServiceLog {
            service,
            output: log.output,
            truncated: log.truncated,
        })
    }

    pub async fn stop(&self, name: &str) -> Result<ProjectServiceSummary, ProjectServiceError> {
        validate_service_name(name)?;
        let _service_lock = self.acquire_service_lock(name).await?;
        let descriptor = self.read_descriptor(name).await?;
        let adapter = self.adapter(descriptor.target.kind())?;
        let context = self.context(name);
        adapter.stop(&context, &descriptor.target).await?;
        let summary = self.inspect_descriptor(&descriptor).await?;
        if summary.state != ProjectServiceState::Stopped {
            return Err(ProjectServiceError::Backend {
                backend: summary.backend,
                message: format!(
                    "stop command completed but the service is {}; recovery descriptor retained: {}",
                    service_state_name(summary.state),
                    bounded_detail(&summary.detail)
                ),
            });
        }
        self.remove_descriptor_with_registry_lock(name).await?;
        Ok(summary)
    }

    fn adapter(
        &self,
        kind: ProjectServiceBackendKind,
    ) -> Result<&Arc<dyn ProjectServiceAdapter>, ProjectServiceError> {
        self.adapters
            .get(&kind)
            .ok_or_else(|| ProjectServiceError::Backend {
                backend: kind,
                message: "backend adapter is not registered".to_owned(),
            })
    }

    fn context<'a>(&'a self, service_name: &'a str) -> BackendContext<'a> {
        BackendContext {
            workspace_root: &self.workspace_root,
            workspace_fingerprint: &self.workspace_fingerprint,
            service_name,
        }
    }

    async fn inspect_descriptor(
        &self,
        descriptor: &ServiceDescriptor,
    ) -> Result<ProjectServiceSummary, ProjectServiceError> {
        self.validate_descriptor(descriptor)?;
        let adapter = self.adapter(descriptor.target.kind())?;
        let context = self.context(&descriptor.name);
        let inspection = adapter.inspect(&context, &descriptor.target).await?;
        Ok(self.summary_from_inspection(descriptor, inspection))
    }

    fn summary_from_inspection(
        &self,
        descriptor: &ServiceDescriptor,
        inspection: BackendInspection,
    ) -> ProjectServiceSummary {
        ProjectServiceSummary {
            name: descriptor.name.clone(),
            backend: descriptor.target.kind(),
            state: inspection.state,
            detail: inspection.detail,
            registered_at: descriptor.registered_at,
        }
    }

    async fn reconcile_descriptor(
        &self,
        descriptor: &ServiceDescriptor,
        summary: &ProjectServiceSummary,
    ) -> Result<(), ProjectServiceError> {
        if descriptor.phase != ServiceDescriptorPhase::Launching {
            return Ok(());
        }
        match summary.state {
            ProjectServiceState::Running => {
                let mut registered = descriptor.clone();
                registered.phase = ServiceDescriptorPhase::Registered;
                self.write_descriptor(&registered)
            }
            ProjectServiceState::Stopped => {
                self.remove_descriptor_with_registry_lock(&descriptor.name)
                    .await
            }
            ProjectServiceState::Unknown => Ok(()),
        }
    }

    async fn finish_failed_start(
        &self,
        descriptor: &ServiceDescriptor,
        primary: ProjectServiceError,
    ) -> Result<ProjectServiceSummary, ProjectServiceError> {
        let Ok(summary) = self.inspect_descriptor(descriptor).await else {
            // The descriptor is intentionally retained when the post-failure probe itself is
            // inconclusive; a later status/stop operation can still reconcile it.
            return Err(primary);
        };
        self.finish_failed_start_with_inspection(descriptor, summary, primary)
            .await
    }

    async fn finish_failed_start_with_inspection(
        &self,
        descriptor: &ServiceDescriptor,
        summary: ProjectServiceSummary,
        primary: ProjectServiceError,
    ) -> Result<ProjectServiceSummary, ProjectServiceError> {
        if summary.state == ProjectServiceState::Stopped
            && let Err(rollback) = self
                .remove_descriptor_with_registry_lock(&descriptor.name)
                .await
        {
            return Err(ProjectServiceError::StartRollback {
                primary: Box::new(primary),
                rollback: Box::new(rollback),
            });
        }
        Err(primary)
    }

    async fn ensure_registry_capacity(&self, requested: &str) -> Result<(), ProjectServiceError> {
        if self.descriptor_path(requested).is_file() {
            return Ok(());
        }
        let count = self.read_descriptors().await?.len();
        if count >= MAX_SERVICES_PER_WORKSPACE {
            return Err(ProjectServiceError::Registry(format!(
                "workspace service limit reached ({MAX_SERVICES_PER_WORKSPACE})"
            )));
        }
        Ok(())
    }

    async fn read_descriptors(&self) -> Result<Vec<ServiceDescriptor>, ProjectServiceError> {
        let mut entries = match tokio::fs::read_dir(&self.registry_dir).await {
            Ok(entries) => entries,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(ProjectServiceError::Registry(error.to_string())),
        };
        let mut descriptors = Vec::new();
        while let Some(entry) = entries
            .next_entry()
            .await
            .map_err(|error| ProjectServiceError::Registry(error.to_string()))?
        {
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            let expected_name = path
                .file_stem()
                .and_then(|value| value.to_str())
                .ok_or_else(|| {
                    ProjectServiceError::Registry(format!(
                        "service descriptor has an invalid file name: {}",
                        path.display()
                    ))
                })?;
            let descriptor = self.read_descriptor_path(path.clone()).await?;
            self.validate_descriptor_name(expected_name, &descriptor, &path)?;
            descriptors.push(descriptor);
            if descriptors.len() > MAX_SERVICES_PER_WORKSPACE {
                return Err(ProjectServiceError::Registry(
                    "workspace service registry exceeds its bounded capacity".to_owned(),
                ));
            }
        }
        Ok(descriptors)
    }

    async fn read_descriptor_optional(
        &self,
        name: &str,
    ) -> Result<Option<ServiceDescriptor>, ProjectServiceError> {
        let path = self.descriptor_path(name);
        match tokio::fs::symlink_metadata(&path).await {
            Ok(_) => {
                let descriptor = self.read_descriptor_path(path.clone()).await?;
                self.validate_descriptor_name(name, &descriptor, &path)?;
                Ok(Some(descriptor))
            }
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
            Err(error) => Err(ProjectServiceError::Registry(error.to_string())),
        }
    }

    async fn read_descriptor(&self, name: &str) -> Result<ServiceDescriptor, ProjectServiceError> {
        self.read_descriptor_optional(name)
            .await?
            .ok_or_else(|| ProjectServiceError::NotRegistered(name.to_owned()))
    }

    async fn read_descriptor_path(
        &self,
        path: PathBuf,
    ) -> Result<ServiceDescriptor, ProjectServiceError> {
        let metadata = tokio::fs::symlink_metadata(&path)
            .await
            .map_err(|error| ProjectServiceError::Registry(error.to_string()))?;
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return Err(ProjectServiceError::Registry(format!(
                "service descriptor must be a regular file: {}",
                path.display()
            )));
        }
        if metadata.len() > MAX_DESCRIPTOR_BYTES {
            return Err(ProjectServiceError::Registry(format!(
                "service descriptor exceeds {MAX_DESCRIPTOR_BYTES} bytes: {}",
                path.display()
            )));
        }
        let bytes = tokio::fs::read(&path)
            .await
            .map_err(|error| ProjectServiceError::Registry(error.to_string()))?;
        let descriptor: ServiceDescriptor = serde_json::from_slice(&bytes)
            .map_err(|error| ProjectServiceError::Registry(error.to_string()))?;
        self.validate_descriptor(&descriptor)?;
        Ok(descriptor)
    }

    fn validate_descriptor(
        &self,
        descriptor: &ServiceDescriptor,
    ) -> Result<(), ProjectServiceError> {
        if descriptor.version != DESCRIPTOR_VERSION {
            return Err(ProjectServiceError::Registry(format!(
                "unsupported project service descriptor version {}",
                descriptor.version
            )));
        }
        if descriptor.workspace_fingerprint != self.workspace_fingerprint {
            return Err(ProjectServiceError::Registry(
                "project service descriptor belongs to another workspace".to_owned(),
            ));
        }
        validate_service_name(&descriptor.name)?;
        let context = self.context(&descriptor.name);
        self.adapter(descriptor.target.kind())?
            .validate_target(&context, &descriptor.target)
    }

    fn validate_descriptor_name(
        &self,
        expected_name: &str,
        descriptor: &ServiceDescriptor,
        path: &Path,
    ) -> Result<(), ProjectServiceError> {
        if descriptor.name == expected_name {
            Ok(())
        } else {
            Err(ProjectServiceError::Registry(format!(
                "service descriptor name `{}` does not match {}",
                descriptor.name,
                path.display()
            )))
        }
    }

    fn write_descriptor(&self, descriptor: &ServiceDescriptor) -> Result<(), ProjectServiceError> {
        let destination = self.descriptor_path(&descriptor.name);
        let bytes = serde_json::to_vec_pretty(descriptor)
            .map_err(|error| ProjectServiceError::Registry(error.to_string()))?;
        // Keep this bounded descriptor transaction synchronous while the caller's service or
        // registry lock is held. A cancellable `spawn_blocking` future can be dropped after its
        // closure starts; that would release the lock while the old rename is still able to
        // overwrite a newer descriptor.
        write_owner_only_atomic(&self.registry_dir, &destination, &bytes)
    }

    fn remove_descriptor(&self, name: &str) -> Result<(), ProjectServiceError> {
        fs::remove_file(self.descriptor_path(name))
            .map_err(|error| ProjectServiceError::Registry(error.to_string()))?;
        sync_directory(&self.registry_dir)
    }

    async fn remove_descriptor_with_registry_lock(
        &self,
        name: &str,
    ) -> Result<(), ProjectServiceError> {
        let _registry_lock = self.acquire_registry_lock().await?;
        self.remove_descriptor(name)
    }

    async fn acquire_service_lock(&self, name: &str) -> Result<ServiceLock, ProjectServiceError> {
        let lock_dir = self.registry_dir.join("locks");
        let lock_path = lock_dir.join(format!("service-{name}.lock"));
        self.acquire_lock(lock_dir, lock_path).await
    }

    async fn acquire_registry_lock(&self) -> Result<ServiceLock, ProjectServiceError> {
        let lock_dir = self.registry_dir.join("locks");
        let lock_path = lock_dir.join(".registry.lock");
        self.acquire_lock(lock_dir, lock_path).await
    }

    async fn acquire_lock(
        &self,
        lock_dir: PathBuf,
        lock_path: PathBuf,
    ) -> Result<ServiceLock, ProjectServiceError> {
        tokio::task::spawn_blocking(move || {
            let registry_dir = lock_dir.parent().ok_or_else(|| {
                ProjectServiceError::Registry(format!(
                    "project service lock directory has no parent: {}",
                    lock_dir.display()
                ))
            })?;
            ensure_owner_only_dir(registry_dir)?;
            ensure_owner_only_dir(&lock_dir)?;
            let mut options = OpenOptions::new();
            options.create(true).truncate(false).read(true).write(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.custom_flags(nix::libc::O_NOFOLLOW);
            }
            #[cfg(windows)]
            {
                use std::os::windows::fs::OpenOptionsExt;
                options.custom_flags(0x0020_0000); // FILE_FLAG_OPEN_REPARSE_POINT
            }
            let file = options
                .open(&lock_path)
                .map_err(|error| ProjectServiceError::Registry(error.to_string()))?;
            let started_at = std::time::Instant::now();
            loop {
                match FileExt::try_lock_exclusive(&file) {
                    Ok(()) => return Ok(ServiceLock(file)),
                    Err(error)
                        if error.kind() == ErrorKind::WouldBlock
                            && started_at.elapsed() < LOCK_WAIT_TIMEOUT =>
                    {
                        std::thread::sleep(LOCK_RETRY_INTERVAL);
                    }
                    Err(error) if error.kind() == ErrorKind::WouldBlock => {
                        return Err(ProjectServiceError::Registry(format!(
                            "timed out after {} acquiring project service lock {}",
                            format_duration(LOCK_WAIT_TIMEOUT),
                            lock_path.display()
                        )));
                    }
                    Err(error) => {
                        return Err(ProjectServiceError::Registry(error.to_string()));
                    }
                }
            }
        })
        .await
        .map_err(|error| ProjectServiceError::Registry(error.to_string()))?
    }

    fn descriptor_path(&self, name: &str) -> PathBuf {
        self.registry_dir.join(format!("{name}.json"))
    }
}

struct ServiceLock(File);

impl Drop for ServiceLock {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.0);
    }
}

#[derive(Debug)]
struct TmuxAdapter;

#[async_trait]
impl ProjectServiceAdapter for TmuxAdapter {
    fn kind(&self) -> ProjectServiceBackendKind {
        ProjectServiceBackendKind::Tmux
    }

    fn validate_target(
        &self,
        context: &BackendContext<'_>,
        target: &BackendTarget,
    ) -> Result<(), ProjectServiceError> {
        let BackendTarget::Tmux { session_name } = target else {
            return Err(wrong_target(self.kind()));
        };
        let expected = backend_identifier("golutra", context, false);
        let legacy = legacy_backend_identifier("golutra", context, false);
        if session_name == &expected || session_name == &legacy {
            Ok(())
        } else {
            Err(invalid_descriptor_target(format!(
                "tmux session must be `{expected}`"
            )))
        }
    }

    fn resolve_target(
        &self,
        context: &BackendContext<'_>,
        spec: &ProjectServiceSpec,
    ) -> Result<BackendTarget, ProjectServiceError> {
        if !matches!(spec, ProjectServiceSpec::Tmux { .. }) {
            return Err(wrong_spec(self.kind()));
        }
        Ok(BackendTarget::Tmux {
            session_name: backend_identifier("golutra", context, false),
        })
    }

    async fn prepare_start(
        &self,
        context: &BackendContext<'_>,
        target: &BackendTarget,
        owned: bool,
    ) -> Result<StartPreparation, ProjectServiceError> {
        let BackendTarget::Tmux { session_name } = target else {
            return Err(wrong_target(self.kind()));
        };
        let inspection = self.inspect(context, target).await?;
        match inspection.state {
            ProjectServiceState::Running if owned => {
                Ok(StartPreparation::AlreadyRunning(inspection))
            }
            ProjectServiceState::Running => Err(ProjectServiceError::OwnershipConflict {
                backend: self.kind(),
                target: session_name.clone(),
            }),
            ProjectServiceState::Unknown => Err(ProjectServiceError::Backend {
                backend: self.kind(),
                message: format!(
                    "cannot prove tmux target is absent before launch: {}",
                    bounded_detail(&inspection.detail)
                ),
            }),
            ProjectServiceState::Stopped => Ok(StartPreparation::Launch),
        }
    }

    async fn launch(
        &self,
        context: &BackendContext<'_>,
        spec: &ProjectServiceSpec,
        target: &BackendTarget,
    ) -> Result<(), ProjectServiceError> {
        let ProjectServiceSpec::Tmux { command } = spec else {
            return Err(wrong_spec(self.kind()));
        };
        let BackendTarget::Tmux { session_name } = target else {
            return Err(wrong_target(self.kind()));
        };
        let shell_command =
            shlex::try_join(command.iter().map(String::as_str)).map_err(|error| {
                ProjectServiceError::InvalidRequest(format!(
                    "tmux command cannot be quoted: {error}"
                ))
            })?;
        let output = run_backend_command(
            self.kind(),
            "tmux",
            &[
                "new-session".to_owned(),
                "-d".to_owned(),
                "-s".to_owned(),
                session_name.clone(),
                "-c".to_owned(),
                context.workspace_root.to_string_lossy().into_owned(),
                shell_command,
            ],
            context.workspace_root,
        )
        .await?;
        require_success(self.kind(), output)?;
        Ok(())
    }

    async fn inspect(
        &self,
        context: &BackendContext<'_>,
        target: &BackendTarget,
    ) -> Result<BackendInspection, ProjectServiceError> {
        let BackendTarget::Tmux { session_name } = target else {
            return Err(wrong_target(self.kind()));
        };
        let output = run_backend_command(
            self.kind(),
            "tmux",
            &[
                "has-session".to_owned(),
                "-t".to_owned(),
                format!("={session_name}"),
            ],
            context.workspace_root,
        )
        .await?;
        Ok(inspect_tmux_session(session_name, &output))
    }

    async fn logs(
        &self,
        context: &BackendContext<'_>,
        target: &BackendTarget,
        tail_lines: u32,
    ) -> Result<BackendLog, ProjectServiceError> {
        let BackendTarget::Tmux { session_name } = target else {
            return Err(wrong_target(self.kind()));
        };
        let output = run_backend_command(
            self.kind(),
            "tmux",
            &[
                "capture-pane".to_owned(),
                "-p".to_owned(),
                "-J".to_owned(),
                "-S".to_owned(),
                format!("-{tail_lines}"),
                "-t".to_owned(),
                format!("={session_name}"),
            ],
            context.workspace_root,
        )
        .await?;
        let output = require_success(self.kind(), output)?;
        Ok(BackendLog {
            output: output.text(),
            truncated: output.truncated,
        })
    }

    async fn stop(
        &self,
        context: &BackendContext<'_>,
        target: &BackendTarget,
    ) -> Result<(), ProjectServiceError> {
        let BackendTarget::Tmux { session_name } = target else {
            return Err(wrong_target(self.kind()));
        };
        let inspection = self.inspect(context, target).await?;
        if inspection.state == ProjectServiceState::Stopped {
            return Ok(());
        }
        let output = run_backend_command(
            self.kind(),
            "tmux",
            &[
                "kill-session".to_owned(),
                "-t".to_owned(),
                format!("={session_name}"),
            ],
            context.workspace_root,
        )
        .await?;
        require_success(self.kind(), output).map(|_| ())
    }
}

#[derive(Debug)]
struct DockerComposeAdapter;

#[derive(Debug, Deserialize)]
struct DockerComposePsEntry {
    #[serde(rename = "Service", alias = "service", default)]
    service: String,
    #[serde(rename = "State", alias = "state", default)]
    state: String,
    #[serde(rename = "Status", alias = "status", default)]
    status: String,
}

#[async_trait]
impl ProjectServiceAdapter for DockerComposeAdapter {
    fn kind(&self) -> ProjectServiceBackendKind {
        ProjectServiceBackendKind::DockerCompose
    }

    fn validate_target(
        &self,
        context: &BackendContext<'_>,
        target: &BackendTarget,
    ) -> Result<(), ProjectServiceError> {
        let BackendTarget::DockerCompose {
            project_name,
            compose_file,
            services,
        } = target
        else {
            return Err(wrong_target(self.kind()));
        };
        let expected = backend_identifier("golutra", context, true);
        let legacy = legacy_backend_identifier("golutra", context, true);
        if project_name != &expected && project_name != &legacy {
            return Err(invalid_descriptor_target(format!(
                "Docker Compose project must be `{expected}`"
            )));
        }
        let resolved = resolve_workspace_file(context.workspace_root, compose_file)
            .map_err(|error| invalid_descriptor_target(error.to_string()))?;
        if &resolved != compose_file {
            return Err(invalid_descriptor_target(
                "Docker Compose file is not the canonical workspace path".to_owned(),
            ));
        }
        if services.len() > MAX_COMMAND_ARGS {
            return Err(invalid_descriptor_target(format!(
                "Docker Compose service selection exceeds {MAX_COMMAND_ARGS} entries"
            )));
        }
        for service in services {
            validate_backend_label("Docker Compose service", service)
                .map_err(|error| invalid_descriptor_target(error.to_string()))?;
        }
        Ok(())
    }

    fn resolve_target(
        &self,
        context: &BackendContext<'_>,
        spec: &ProjectServiceSpec,
    ) -> Result<BackendTarget, ProjectServiceError> {
        let ProjectServiceSpec::DockerCompose {
            compose_file,
            services,
        } = spec
        else {
            return Err(wrong_spec(self.kind()));
        };
        let compose_file = resolve_workspace_file(context.workspace_root, compose_file)?;
        Ok(BackendTarget::DockerCompose {
            project_name: backend_identifier("golutra", context, true),
            compose_file,
            services: services.clone(),
        })
    }

    async fn prepare_start(
        &self,
        context: &BackendContext<'_>,
        target: &BackendTarget,
        owned: bool,
    ) -> Result<StartPreparation, ProjectServiceError> {
        let BackendTarget::DockerCompose { project_name, .. } = target else {
            return Err(wrong_target(self.kind()));
        };
        if owned {
            return Ok(StartPreparation::Launch);
        }
        let mut probe_args = compose_args(context.workspace_root, target)?;
        probe_args.extend([
            "ps".to_owned(),
            "--all".to_owned(),
            "--format".to_owned(),
            "json".to_owned(),
        ]);
        let probe =
            run_backend_command(self.kind(), "docker", &probe_args, context.workspace_root).await?;
        let probe = require_success(self.kind(), probe)?;
        let entries = parse_docker_compose_ps(&probe.stdout).map_err(|error| {
            ProjectServiceError::Backend {
                backend: self.kind(),
                message: format!("cannot verify Docker Compose ownership: {error}"),
            }
        })?;
        if !entries.is_empty() {
            return Err(ProjectServiceError::OwnershipConflict {
                backend: self.kind(),
                target: project_name.clone(),
            });
        }
        Ok(StartPreparation::Launch)
    }

    async fn launch(
        &self,
        context: &BackendContext<'_>,
        spec: &ProjectServiceSpec,
        target: &BackendTarget,
    ) -> Result<(), ProjectServiceError> {
        let ProjectServiceSpec::DockerCompose { services, .. } = spec else {
            return Err(wrong_spec(self.kind()));
        };
        let mut args = compose_args(context.workspace_root, target)?;
        args.extend(["up".to_owned(), "-d".to_owned()]);
        args.extend(services.iter().cloned());
        let output =
            run_backend_command(self.kind(), "docker", &args, context.workspace_root).await?;
        require_success(self.kind(), output)?;
        Ok(())
    }

    async fn inspect(
        &self,
        context: &BackendContext<'_>,
        target: &BackendTarget,
    ) -> Result<BackendInspection, ProjectServiceError> {
        let BackendTarget::DockerCompose { services, .. } = target else {
            return Err(wrong_target(self.kind()));
        };
        let mut args = compose_args(context.workspace_root, target)?;
        args.extend([
            "ps".to_owned(),
            "--all".to_owned(),
            "--format".to_owned(),
            "json".to_owned(),
        ]);
        args.extend(services.iter().cloned());
        let output =
            run_backend_command(self.kind(), "docker", &args, context.workspace_root).await?;
        if !output.success {
            return Ok(BackendInspection {
                state: ProjectServiceState::Unknown,
                detail: bounded_detail(&output.text()),
            });
        }
        Ok(inspect_docker_compose_ps_for_services(
            &output.stdout,
            services,
        ))
    }

    async fn logs(
        &self,
        context: &BackendContext<'_>,
        target: &BackendTarget,
        tail_lines: u32,
    ) -> Result<BackendLog, ProjectServiceError> {
        let BackendTarget::DockerCompose { services, .. } = target else {
            return Err(wrong_target(self.kind()));
        };
        let mut args = compose_args(context.workspace_root, target)?;
        args.extend([
            "logs".to_owned(),
            "--no-color".to_owned(),
            "--tail".to_owned(),
            tail_lines.to_string(),
        ]);
        args.extend(services.iter().cloned());
        let output =
            run_backend_command(self.kind(), "docker", &args, context.workspace_root).await?;
        let output = require_success(self.kind(), output)?;
        Ok(BackendLog {
            output: output.text(),
            truncated: output.truncated,
        })
    }

    async fn stop(
        &self,
        context: &BackendContext<'_>,
        target: &BackendTarget,
    ) -> Result<(), ProjectServiceError> {
        let BackendTarget::DockerCompose { services, .. } = target else {
            return Err(wrong_target(self.kind()));
        };
        let mut args = compose_args(context.workspace_root, target)?;
        if services.is_empty() {
            args.extend(["down".to_owned(), "--remove-orphans".to_owned()]);
        } else {
            args.push("stop".to_owned());
            args.extend(services.iter().cloned());
        }
        let output =
            run_backend_command(self.kind(), "docker", &args, context.workspace_root).await?;
        require_success(self.kind(), output).map(|_| ())
    }
}

#[cfg(test)]
fn inspect_docker_compose_ps(output: &str) -> BackendInspection {
    inspect_docker_compose_ps_for_services(output, &[])
}

fn inspect_docker_compose_ps_for_services(
    output: &str,
    requested_services: &[String],
) -> BackendInspection {
    let entries = match parse_docker_compose_ps(output) {
        Ok(entries) => entries,
        Err(error) => {
            return BackendInspection {
                state: ProjectServiceState::Unknown,
                detail: bounded_detail(&format!(
                    "Docker Compose returned an unreadable status payload: {error}"
                )),
            };
        }
    };
    if !requested_services.is_empty() {
        if entries.iter().any(|entry| entry.service.is_empty()) {
            return BackendInspection {
                state: ProjectServiceState::Unknown,
                detail: "Docker Compose status omitted service names for a service-scoped query"
                    .to_owned(),
            };
        }
        let mut active = 0_usize;
        let mut known_inactive = 0_usize;
        let mut unknown = 0_usize;
        let mut unique_services: Vec<&String> = Vec::new();
        for service in requested_services {
            if unique_services
                .iter()
                .any(|seen| seen.as_str() == service.as_str())
            {
                continue;
            }
            unique_services.push(service);
            let matching = entries
                .iter()
                .filter(|entry| entry.service == *service)
                .collect::<Vec<_>>();
            if matching.is_empty() {
                // `ps --all <service>` returning no row means there is no container for that
                // service, which is a known stopped state rather than a failed status probe.
                known_inactive += 1;
            } else if matching
                .iter()
                .all(|entry| docker_container_is_active(entry))
            {
                active += 1;
            } else if matching
                .iter()
                .all(|entry| docker_container_is_known_inactive(entry))
            {
                known_inactive += 1;
            } else {
                unknown += 1;
            }
        }
        let total = active + known_inactive + unknown;
        return if active == total {
            BackendInspection {
                state: ProjectServiceState::Running,
                detail: format!("all {total} requested Docker Compose services are active"),
            }
        } else if known_inactive == total {
            BackendInspection {
                state: ProjectServiceState::Stopped,
                detail: format!("all {total} requested Docker Compose services are stopped"),
            }
        } else {
            BackendInspection {
                state: ProjectServiceState::Unknown,
                detail: format!(
                    "requested Docker Compose service state is incomplete: {active} active, {known_inactive} inactive, {unknown} unrecognized"
                ),
            }
        };
    }
    if entries.is_empty() {
        return BackendInspection {
            state: ProjectServiceState::Stopped,
            detail: "Docker Compose project has no service containers".to_owned(),
        };
    }

    let active = entries
        .iter()
        .filter(|entry| docker_container_is_active(entry))
        .count();
    let known_inactive = entries
        .iter()
        .filter(|entry| {
            !docker_container_is_active(entry) && docker_container_is_known_inactive(entry)
        })
        .count();
    if active == entries.len() {
        BackendInspection {
            state: ProjectServiceState::Running,
            detail: format!(
                "all {} Docker Compose service containers are active",
                entries.len()
            ),
        }
    } else if known_inactive == entries.len() {
        BackendInspection {
            state: ProjectServiceState::Stopped,
            detail: format!(
                "all {} Docker Compose service containers are stopped",
                entries.len()
            ),
        }
    } else {
        let unknown = entries
            .len()
            .saturating_sub(active.saturating_add(known_inactive));
        BackendInspection {
            state: ProjectServiceState::Unknown,
            detail: format!(
                "Docker Compose service state is incomplete: {active} active, {known_inactive} inactive, {unknown} unrecognized",
            ),
        }
    }
}

fn inspect_tmux_session(session_name: &str, output: &BackendCommandOutput) -> BackendInspection {
    if output.success {
        return BackendInspection {
            state: ProjectServiceState::Running,
            detail: format!("tmux session {session_name} is active"),
        };
    }
    let detail = output.text();
    let normalized = detail.to_ascii_lowercase();
    let state = if ["can't find session", "no server running", "no sessions"]
        .into_iter()
        .any(|message| normalized.contains(message))
    {
        ProjectServiceState::Stopped
    } else {
        ProjectServiceState::Unknown
    };
    BackendInspection {
        state,
        detail: format!("tmux session {session_name}: {}", bounded_detail(&detail)),
    }
}

fn parse_docker_compose_ps(output: &str) -> Result<Vec<DockerComposePsEntry>, String> {
    let output = output.trim();
    if output.is_empty() {
        return Ok(Vec::new());
    }
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(output) {
        return docker_compose_entries_from_value(value);
    }

    let mut entries = Vec::new();
    for line in output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        let value =
            serde_json::from_str::<serde_json::Value>(line).map_err(|error| error.to_string())?;
        entries.extend(docker_compose_entries_from_value(value)?);
    }
    Ok(entries)
}

fn docker_compose_entries_from_value(
    value: serde_json::Value,
) -> Result<Vec<DockerComposePsEntry>, String> {
    match value {
        serde_json::Value::Array(entries) => entries
            .into_iter()
            .map(|entry| serde_json::from_value(entry).map_err(|error| error.to_string()))
            .collect(),
        serde_json::Value::Object(_) => serde_json::from_value(value)
            .map(|entry| vec![entry])
            .map_err(|error| error.to_string()),
        serde_json::Value::Null => Ok(Vec::new()),
        _ => Err("expected a JSON object, array, or newline-delimited objects".to_owned()),
    }
}

fn docker_container_is_active(entry: &DockerComposePsEntry) -> bool {
    matches!(
        entry.state.trim().to_ascii_lowercase().as_str(),
        "running" | "restarting"
    ) || entry.status.trim().eq_ignore_ascii_case("up")
        || entry.status.trim().to_ascii_lowercase().starts_with("up ")
}

fn docker_container_is_known_inactive(entry: &DockerComposePsEntry) -> bool {
    if matches!(
        entry.state.trim().to_ascii_lowercase().as_str(),
        "created" | "exited" | "dead" | "paused" | "removing"
    ) {
        return true;
    }
    let status = entry.status.trim().to_ascii_lowercase();
    ["created", "exited", "dead", "paused", "removing"]
        .into_iter()
        .any(|prefix| {
            status
                .strip_prefix(prefix)
                .is_some_and(|suffix| suffix.is_empty() || suffix.starts_with(' '))
        })
}

#[derive(Debug)]
struct SystemdUserAdapter;

#[async_trait]
impl ProjectServiceAdapter for SystemdUserAdapter {
    fn kind(&self) -> ProjectServiceBackendKind {
        ProjectServiceBackendKind::SystemdUser
    }

    fn validate_target(
        &self,
        context: &BackendContext<'_>,
        target: &BackendTarget,
    ) -> Result<(), ProjectServiceError> {
        let BackendTarget::SystemdUser { unit_name } = target else {
            return Err(wrong_target(self.kind()));
        };
        let expected = format!("{}.service", backend_identifier("golutra", context, true));
        let legacy = format!(
            "{}.service",
            legacy_backend_identifier("golutra", context, true)
        );
        if unit_name == &expected || unit_name == &legacy {
            Ok(())
        } else {
            Err(invalid_descriptor_target(format!(
                "systemd user unit must be `{expected}`"
            )))
        }
    }

    fn resolve_target(
        &self,
        context: &BackendContext<'_>,
        spec: &ProjectServiceSpec,
    ) -> Result<BackendTarget, ProjectServiceError> {
        let ProjectServiceSpec::SystemdUser { .. } = spec else {
            return Err(wrong_spec(self.kind()));
        };
        Ok(BackendTarget::SystemdUser {
            unit_name: format!("{}.service", backend_identifier("golutra", context, true)),
        })
    }

    async fn prepare_start(
        &self,
        context: &BackendContext<'_>,
        target: &BackendTarget,
        owned: bool,
    ) -> Result<StartPreparation, ProjectServiceError> {
        let BackendTarget::SystemdUser { unit_name } = target else {
            return Err(wrong_target(self.kind()));
        };
        let inspection = self.inspect(context, target).await?;
        match inspection.state {
            ProjectServiceState::Running if owned => {
                Ok(StartPreparation::AlreadyRunning(inspection))
            }
            ProjectServiceState::Running => Err(ProjectServiceError::OwnershipConflict {
                backend: self.kind(),
                target: unit_name.clone(),
            }),
            ProjectServiceState::Unknown => Err(ProjectServiceError::Backend {
                backend: self.kind(),
                message: format!(
                    "cannot prove systemd target is inactive before launch: {}",
                    bounded_detail(&inspection.detail)
                ),
            }),
            ProjectServiceState::Stopped => Ok(StartPreparation::Launch),
        }
    }

    async fn launch(
        &self,
        context: &BackendContext<'_>,
        spec: &ProjectServiceSpec,
        target: &BackendTarget,
    ) -> Result<(), ProjectServiceError> {
        let ProjectServiceSpec::SystemdUser { command } = spec else {
            return Err(wrong_spec(self.kind()));
        };
        let BackendTarget::SystemdUser { unit_name } = target else {
            return Err(wrong_target(self.kind()));
        };
        let mut args = vec![
            "--user".to_owned(),
            "--unit".to_owned(),
            unit_name.clone(),
            "--collect".to_owned(),
            format!(
                "--property=WorkingDirectory={}",
                context.workspace_root.display()
            ),
            "--".to_owned(),
        ];
        args.extend(command.iter().cloned());
        let output =
            run_backend_command(self.kind(), "systemd-run", &args, context.workspace_root).await?;
        require_success(self.kind(), output)?;
        Ok(())
    }

    async fn inspect(
        &self,
        context: &BackendContext<'_>,
        target: &BackendTarget,
    ) -> Result<BackendInspection, ProjectServiceError> {
        let BackendTarget::SystemdUser { unit_name } = target else {
            return Err(wrong_target(self.kind()));
        };
        let output = run_backend_command(
            self.kind(),
            "systemctl",
            &[
                "--user".to_owned(),
                "is-active".to_owned(),
                unit_name.clone(),
            ],
            context.workspace_root,
        )
        .await?;
        Ok(inspect_systemd_user_unit(unit_name, &output))
    }

    async fn logs(
        &self,
        context: &BackendContext<'_>,
        target: &BackendTarget,
        tail_lines: u32,
    ) -> Result<BackendLog, ProjectServiceError> {
        let BackendTarget::SystemdUser { unit_name } = target else {
            return Err(wrong_target(self.kind()));
        };
        let output = run_backend_command(
            self.kind(),
            "journalctl",
            &[
                "--user-unit".to_owned(),
                unit_name.clone(),
                "--lines".to_owned(),
                tail_lines.to_string(),
                "--no-pager".to_owned(),
                "--output".to_owned(),
                "cat".to_owned(),
            ],
            context.workspace_root,
        )
        .await?;
        let output = require_success(self.kind(), output)?;
        Ok(BackendLog {
            output: output.text(),
            truncated: output.truncated,
        })
    }

    async fn stop(
        &self,
        context: &BackendContext<'_>,
        target: &BackendTarget,
    ) -> Result<(), ProjectServiceError> {
        let BackendTarget::SystemdUser { unit_name } = target else {
            return Err(wrong_target(self.kind()));
        };
        if self.inspect(context, target).await?.state == ProjectServiceState::Stopped {
            return Ok(());
        }
        let output = run_backend_command(
            self.kind(),
            "systemctl",
            &["--user".to_owned(), "stop".to_owned(), unit_name.clone()],
            context.workspace_root,
        )
        .await?;
        require_success(self.kind(), output).map(|_| ())
    }
}

#[derive(Debug)]
struct BackendCommandOutput {
    success: bool,
    stdout: String,
    stderr: String,
    truncated: bool,
}

impl BackendCommandOutput {
    fn text(&self) -> String {
        match (self.stdout.trim(), self.stderr.trim()) {
            ("", "") => String::new(),
            (stdout, "") => stdout.to_owned(),
            ("", stderr) => stderr.to_owned(),
            (stdout, stderr) => format!("{stdout}\n{stderr}"),
        }
    }
}

fn inspect_systemd_user_unit(unit_name: &str, output: &BackendCommandOutput) -> BackendInspection {
    let state = match output.stdout.trim() {
        "active" | "activating" | "reloading" => ProjectServiceState::Running,
        "inactive" | "failed" | "deactivating" => ProjectServiceState::Stopped,
        _ => ProjectServiceState::Unknown,
    };
    BackendInspection {
        state,
        detail: format!(
            "systemd user unit {unit_name}: {}",
            bounded_detail(&output.text())
        ),
    }
}

async fn run_backend_command(
    backend: ProjectServiceBackendKind,
    program: &str,
    args: &[String],
    cwd: &Path,
) -> Result<BackendCommandOutput, ProjectServiceError> {
    run_backend_command_with_timeout(backend, program, args, cwd, COMMAND_TIMEOUT).await
}

async fn run_backend_command_with_timeout(
    backend: ProjectServiceBackendKind,
    program: &str,
    args: &[String],
    cwd: &Path,
    timeout_duration: Duration,
) -> Result<BackendCommandOutput, ProjectServiceError> {
    let mut command = Command::new(program);
    command
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command.spawn().map_err(|error| {
        if error.kind() == ErrorKind::NotFound {
            ProjectServiceError::BackendUnavailable {
                backend,
                program: program.to_owned(),
            }
        } else {
            ProjectServiceError::Backend {
                backend,
                message: error.to_string(),
            }
        }
    })?;
    let process_id = child.id();
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| ProjectServiceError::Backend {
            backend,
            message: "backend stdout pipe is unavailable".to_owned(),
        })?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| ProjectServiceError::Backend {
            backend,
            message: "backend stderr pipe is unavailable".to_owned(),
        })?;
    let stdout_state = Arc::new(Mutex::new(BoundedReadState::default()));
    let stderr_state = Arc::new(Mutex::new(BoundedReadState::default()));
    let mut stdout_reader = tokio::spawn(read_bounded_into(stdout, stdout_state.clone()));
    let mut stderr_reader = tokio::spawn(read_bounded_into(stderr, stderr_state.clone()));
    let status = match tokio::time::timeout(timeout_duration, child.wait()).await {
        Ok(Ok(status)) => status,
        Ok(Err(error)) => {
            terminate_backend_child(&mut child, process_id).await;
            abort_reader_tasks([stdout_reader, stderr_reader]).await;
            return Err(ProjectServiceError::Backend {
                backend,
                message: format!("backend command wait failed: {error}"),
            });
        }
        Err(_) => {
            terminate_backend_child(&mut child, process_id).await;
            abort_reader_tasks([stdout_reader, stderr_reader]).await;
            return Err(ProjectServiceError::Backend {
                backend,
                message: format!(
                    "backend command exceeded {}",
                    format_duration(timeout_duration)
                ),
            });
        }
    };
    if !status.success() {
        terminate_backend_process_group(process_id).await;
    }
    let drain_timeout = if status.success() {
        SUCCESSFUL_READER_DRAIN_TIMEOUT
    } else {
        READER_DRAIN_TIMEOUT
    };
    let captured = tokio::time::timeout(
        drain_timeout,
        wait_for_bounded_readers(backend, &mut stdout_reader, &mut stderr_reader),
    )
    .await;
    let (stdout, stdout_truncated, stderr, stderr_truncated) = match captured {
        Ok(Ok(())) => {
            let stdout = snapshot_bounded_read(&stdout_state);
            let stderr = snapshot_bounded_read(&stderr_state);
            (stdout.0, stdout.1, stderr.0, stderr.1)
        }
        Ok(Err(error)) => {
            abort_reader_tasks([stdout_reader, stderr_reader]).await;
            return Err(error);
        }
        Err(_) => {
            let (stdout, stderr) = (
                snapshot_bounded_read(&stdout_state),
                snapshot_bounded_read(&stderr_state),
            );
            abort_reader_tasks([stdout_reader, stderr_reader]).await;
            if status.success() {
                return Ok(BackendCommandOutput {
                    success: true,
                    stdout: stdout.0,
                    stderr: stderr.0,
                    truncated: true,
                });
            }
            return Err(ProjectServiceError::Backend {
                backend,
                message: format!(
                    "backend output readers exceeded {}",
                    format_duration(drain_timeout)
                ),
            });
        }
    };
    Ok(BackendCommandOutput {
        success: status.success(),
        stdout,
        stderr,
        truncated: stdout_truncated || stderr_truncated,
    })
}

async fn terminate_backend_child(child: &mut tokio::process::Child, process_id: Option<u32>) {
    terminate_backend_process_group(process_id).await;
    let _ = child.start_kill();
    let _ = child.wait().await;
}

async fn terminate_backend_process_group(process_id: Option<u32>) {
    #[cfg(unix)]
    if let Some(process_id) = process_id.and_then(|id| i32::try_from(id).ok()) {
        let _ = killpg(Pid::from_raw(process_id), Signal::SIGKILL);
    }
    #[cfg(windows)]
    if let Some(process_id) = process_id {
        let args = windows_taskkill_args(process_id);
        let _ = Command::new("taskkill").args(args).status().await;
    }
    #[cfg(not(any(unix, windows)))]
    let _ = process_id;
}

#[cfg(any(windows, test))]
fn windows_taskkill_args(process_id: u32) -> [String; 4] {
    [
        "/PID".to_owned(),
        process_id.to_string(),
        "/T".to_owned(),
        "/F".to_owned(),
    ]
}

async fn abort_reader_tasks<T>(readers: impl IntoIterator<Item = tokio::task::JoinHandle<T>>)
where
    T: Send + 'static,
{
    let readers = readers.into_iter().collect::<Vec<_>>();
    for reader in &readers {
        reader.abort();
    }
    for reader in readers {
        let _ = reader.await;
    }
}

async fn wait_for_bounded_readers(
    backend: ProjectServiceBackendKind,
    stdout_reader: &mut tokio::task::JoinHandle<io::Result<()>>,
    stderr_reader: &mut tokio::task::JoinHandle<io::Result<()>>,
) -> Result<(), ProjectServiceError> {
    let (stdout, stderr) = tokio::join!(stdout_reader, stderr_reader);
    decode_bounded_reader(backend, "stdout", stdout)?;
    decode_bounded_reader(backend, "stderr", stderr)?;
    Ok(())
}

fn decode_bounded_reader(
    backend: ProjectServiceBackendKind,
    stream_name: &str,
    output: Result<io::Result<()>, tokio::task::JoinError>,
) -> Result<(), ProjectServiceError> {
    let output = output.map_err(|error| ProjectServiceError::Backend {
        backend,
        message: format!("{stream_name} reader task failed: {error}"),
    })?;
    output.map_err(|error| ProjectServiceError::Backend {
        backend,
        message: format!("{stream_name} reader failed: {error}"),
    })
}

#[derive(Debug, Default)]
struct BoundedReadState {
    bytes: Vec<u8>,
    truncated: bool,
}

fn snapshot_bounded_read(state: &Mutex<BoundedReadState>) -> (String, bool) {
    let state = state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    (
        String::from_utf8_lossy(&state.bytes).into_owned(),
        state.truncated,
    )
}

fn format_duration(duration: Duration) -> String {
    if duration.as_secs() > 0 {
        format!("{} seconds", duration.as_secs())
    } else {
        format!("{} milliseconds", duration.as_millis())
    }
}

#[cfg(test)]
async fn read_bounded<R>(mut reader: R) -> io::Result<(String, bool)>
where
    R: AsyncRead + Unpin,
{
    let state = Arc::new(Mutex::new(BoundedReadState::default()));
    read_bounded_into(&mut reader, state.clone()).await?;
    Ok(snapshot_bounded_read(&state))
}

async fn read_bounded_into<R>(mut reader: R, state: Arc<Mutex<BoundedReadState>>) -> io::Result<()>
where
    R: AsyncRead + Unpin,
{
    let mut buffer = vec![0_u8; 16 * 1024];
    loop {
        let read = loop {
            match reader.read(&mut buffer).await {
                Ok(read) => break read,
                Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        };
        if read == 0 {
            break;
        }
        let mut state = state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let remaining = MAX_CAPTURE_BYTES.saturating_sub(state.bytes.len());
        let keep = remaining.min(read);
        state.bytes.extend_from_slice(&buffer[..keep]);
        state.truncated |= keep < read;
    }
    Ok(())
}

fn require_success(
    backend: ProjectServiceBackendKind,
    output: BackendCommandOutput,
) -> Result<BackendCommandOutput, ProjectServiceError> {
    if output.success {
        Ok(output)
    } else {
        Err(ProjectServiceError::Backend {
            backend,
            message: bounded_detail(&output.text()),
        })
    }
}

fn compose_args(
    workspace_root: &Path,
    target: &BackendTarget,
) -> Result<Vec<String>, ProjectServiceError> {
    let BackendTarget::DockerCompose {
        project_name,
        compose_file,
        ..
    } = target
    else {
        return Err(wrong_target(ProjectServiceBackendKind::DockerCompose));
    };
    Ok(vec![
        "compose".to_owned(),
        "--project-directory".to_owned(),
        workspace_root.to_string_lossy().into_owned(),
        "--file".to_owned(),
        compose_file.to_string_lossy().into_owned(),
        "--project-name".to_owned(),
        project_name.clone(),
    ])
}

fn validate_spec(
    workspace_root: &Path,
    spec: &ProjectServiceSpec,
) -> Result<(), ProjectServiceError> {
    match spec {
        ProjectServiceSpec::Tmux { command } | ProjectServiceSpec::SystemdUser { command } => {
            validate_command(command)
        }
        ProjectServiceSpec::DockerCompose {
            compose_file,
            services,
        } => {
            resolve_workspace_file(workspace_root, compose_file)?;
            if services.len() > MAX_COMMAND_ARGS {
                return Err(ProjectServiceError::InvalidRequest(format!(
                    "Docker Compose service selection exceeds {MAX_COMMAND_ARGS} entries"
                )));
            }
            for service in services {
                validate_backend_label("Docker Compose service", service)?;
            }
            Ok(())
        }
    }
}

fn validate_command(command: &[String]) -> Result<(), ProjectServiceError> {
    if command.is_empty() {
        return Err(ProjectServiceError::InvalidRequest(
            "project service command cannot be empty".to_owned(),
        ));
    }
    if command.len() > MAX_COMMAND_ARGS {
        return Err(ProjectServiceError::InvalidRequest(format!(
            "project service command exceeds {MAX_COMMAND_ARGS} arguments"
        )));
    }
    let total = command.iter().map(String::len).sum::<usize>();
    if total > MAX_COMMAND_BYTES || command.iter().any(|argument| argument.contains('\0')) {
        return Err(ProjectServiceError::InvalidRequest(format!(
            "project service command exceeds {MAX_COMMAND_BYTES} bytes or contains NUL"
        )));
    }
    Ok(())
}

fn validate_service_name(name: &str) -> Result<(), ProjectServiceError> {
    if name.is_empty()
        || name.len() > MAX_SERVICE_NAME_BYTES
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        || !name.as_bytes()[0].is_ascii_alphanumeric()
    {
        return Err(ProjectServiceError::InvalidRequest(format!(
            "service name must start with an alphanumeric character and contain at most {MAX_SERVICE_NAME_BYTES} ASCII alphanumeric, '-', '_', or '.' characters"
        )));
    }
    Ok(())
}

fn validate_backend_label(label: &str, value: &str) -> Result<(), ProjectServiceError> {
    if value.is_empty()
        || value.len() > MAX_SERVICE_NAME_BYTES
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'/'))
    {
        return Err(ProjectServiceError::InvalidRequest(format!(
            "{label} contains unsupported characters"
        )));
    }
    Ok(())
}

fn resolve_workspace_file(
    workspace_root: &Path,
    requested: &Path,
) -> Result<PathBuf, ProjectServiceError> {
    let canonical_root = fs::canonicalize(workspace_root).map_err(|error| {
        ProjectServiceError::InvalidRequest(format!(
            "workspace root {} cannot be resolved: {error}",
            workspace_root.display()
        ))
    })?;
    let candidate = if requested.is_absolute() {
        requested.to_path_buf()
    } else {
        canonical_root.join(requested)
    };
    let metadata = fs::symlink_metadata(&candidate).map_err(|error| {
        ProjectServiceError::InvalidRequest(format!(
            "project service file {} cannot be read: {error}",
            candidate.display()
        ))
    })?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(ProjectServiceError::InvalidRequest(format!(
            "project service file must be a regular non-symlink file: {}",
            candidate.display()
        )));
    }
    let canonical = fs::canonicalize(&candidate).map_err(|error| {
        ProjectServiceError::InvalidRequest(format!("{}: {error}", candidate.display()))
    })?;
    if !canonical.starts_with(&canonical_root) {
        return Err(ProjectServiceError::InvalidRequest(format!(
            "project service file escapes the workspace: {}",
            candidate.display()
        )));
    }
    Ok(canonical)
}

fn backend_identifier(prefix: &str, context: &BackendContext<'_>, lowercase: bool) -> String {
    let mut service = context
        .service_name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '-'
            }
        })
        .collect::<String>();
    let needs_disambiguation = context.service_name.len() > 24
        || context.service_name.contains('.')
        || (lowercase
            && context
                .service_name
                .bytes()
                .any(|byte| byte.is_ascii_uppercase()));
    service.truncate(24);
    if lowercase {
        service.make_ascii_lowercase();
    }
    if needs_disambiguation {
        let service_fingerprint = format!("{:x}", Sha256::digest(context.service_name.as_bytes()));
        format!(
            "{prefix}-{}-{service}-{}",
            &context.workspace_fingerprint[..12],
            &service_fingerprint[..16]
        )
    } else {
        format!(
            "{prefix}-{}-{service}",
            &context.workspace_fingerprint[..12]
        )
    }
}

fn legacy_backend_identifier(
    prefix: &str,
    context: &BackendContext<'_>,
    lowercase: bool,
) -> String {
    let mut service = context
        .service_name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '-'
            }
        })
        .collect::<String>();
    if lowercase {
        service.make_ascii_lowercase();
    }
    format!(
        "{prefix}-{}-{service}",
        &context.workspace_fingerprint[..12]
    )
}

fn workspace_fingerprint(workspace_root: &Path) -> String {
    format!(
        "{:x}",
        Sha256::digest(workspace_root.as_os_str().as_encoded_bytes())
    )
}

fn bounded_detail(value: &str) -> String {
    const MAX_DETAIL_CHARS: usize = 2_000;
    let mut chars = value.chars();
    let detail = chars.by_ref().take(MAX_DETAIL_CHARS).collect::<String>();
    if chars.next().is_some() {
        format!("{detail}...")
    } else if detail.is_empty() {
        "backend returned no detail".to_owned()
    } else {
        detail
    }
}

const fn service_state_name(state: ProjectServiceState) -> &'static str {
    match state {
        ProjectServiceState::Running => "running",
        ProjectServiceState::Stopped => "stopped",
        ProjectServiceState::Unknown => "unknown",
    }
}

fn invalid_descriptor_target(message: String) -> ProjectServiceError {
    ProjectServiceError::Registry(format!(
        "project service descriptor target is invalid: {message}"
    ))
}

fn wrong_spec(backend: ProjectServiceBackendKind) -> ProjectServiceError {
    ProjectServiceError::InvalidRequest(format!(
        "project service spec does not match backend `{backend}`"
    ))
}

fn wrong_target(backend: ProjectServiceBackendKind) -> ProjectServiceError {
    ProjectServiceError::Registry(format!(
        "project service target does not match backend `{backend}`"
    ))
}

fn write_owner_only_atomic(
    directory: &Path,
    destination: &Path,
    bytes: &[u8],
) -> Result<(), ProjectServiceError> {
    ensure_owner_only_dir(directory)?;
    let temporary = directory.join(format!(".descriptor-{}.tmp", uuid::Uuid::now_v7()));
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&temporary)
        .map_err(|error| ProjectServiceError::Registry(error.to_string()))?;
    let result = (|| {
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&temporary, destination)?;
        sync_directory(directory).map_err(|error| io::Error::other(error.to_string()))?;
        Ok::<(), std::io::Error>(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result.map_err(|error| ProjectServiceError::Registry(error.to_string()))
}

fn sync_directory(directory: &Path) -> Result<(), ProjectServiceError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;

        let directory = OpenOptions::new()
            .read(true)
            .custom_flags(nix::libc::O_DIRECTORY | nix::libc::O_NOFOLLOW)
            .open(directory)
            .map_err(|error| ProjectServiceError::Registry(error.to_string()))?;
        directory
            .sync_all()
            .map_err(|error| ProjectServiceError::Registry(error.to_string()))?;
    }
    #[cfg(not(unix))]
    {
        let _ = directory;
    }
    Ok(())
}

fn ensure_owner_only_dir(path: &Path) -> Result<(), ProjectServiceError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => validate_real_directory(path, &metadata)?,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            fs::create_dir_all(path)
                .map_err(|error| ProjectServiceError::Registry(error.to_string()))?;
        }
        Err(error) => return Err(ProjectServiceError::Registry(error.to_string())),
    }
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| ProjectServiceError::Registry(error.to_string()))?;
    validate_real_directory(path, &metadata)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

        let directory = OpenOptions::new()
            .read(true)
            .custom_flags(nix::libc::O_DIRECTORY | nix::libc::O_NOFOLLOW)
            .open(path)
            .map_err(|error| ProjectServiceError::Registry(error.to_string()))?;
        directory
            .set_permissions(fs::Permissions::from_mode(0o700))
            .map_err(|error| ProjectServiceError::Registry(error.to_string()))?;
    }
    Ok(())
}

fn validate_real_directory(
    path: &Path,
    metadata: &fs::Metadata,
) -> Result<(), ProjectServiceError> {
    if metadata.file_type().is_symlink() {
        return Err(ProjectServiceError::Registry(format!(
            "project service state directory cannot be a symbolic link: {}",
            path.display()
        )));
    }
    if !metadata.is_dir() {
        return Err(ProjectServiceError::Registry(format!(
            "project service state path is not a directory: {}",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{
        pin::Pin,
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, Ordering},
        },
        task::{Context, Poll},
        time::Instant,
    };

    use async_trait::async_trait;
    use tempfile::tempdir;
    use tokio::{
        io::ReadBuf,
        sync::{Mutex as AsyncMutex, Notify},
        time::timeout,
    };

    use super::*;

    #[derive(Debug, Default)]
    struct FakeBackendState {
        running: bool,
        starts: usize,
        stops: usize,
    }

    struct FailingReader;

    impl tokio::io::AsyncRead for FailingReader {
        fn poll_read(
            self: Pin<&mut Self>,
            _context: &mut Context<'_>,
            _buffer: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            Poll::Ready(Err(io::Error::other("fixture reader failed")))
        }
    }

    struct ReaderDropMarker(Arc<AtomicBool>);

    impl Drop for ReaderDropMarker {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    #[derive(Debug, Clone)]
    struct FakeAdapter {
        state: Arc<AsyncMutex<FakeBackendState>>,
        output: Arc<Mutex<String>>,
        stop_keeps_running: Arc<AtomicBool>,
        inspect_unknown: Arc<AtomicBool>,
        fail_after_start: Arc<AtomicBool>,
        blocked_inspect_service: Arc<Mutex<Option<String>>>,
        inspect_started: Arc<Notify>,
        inspect_release: Arc<Notify>,
    }

    impl FakeAdapter {
        fn new() -> Self {
            Self {
                state: Arc::new(AsyncMutex::new(FakeBackendState::default())),
                output: Arc::new(Mutex::new("fake service log\n".to_owned())),
                stop_keeps_running: Arc::new(AtomicBool::new(false)),
                inspect_unknown: Arc::new(AtomicBool::new(false)),
                fail_after_start: Arc::new(AtomicBool::new(false)),
                blocked_inspect_service: Arc::new(Mutex::new(None)),
                inspect_started: Arc::new(Notify::new()),
                inspect_release: Arc::new(Notify::new()),
            }
        }

        fn adapters(&self) -> BTreeMap<ProjectServiceBackendKind, Arc<dyn ProjectServiceAdapter>> {
            let adapter: Arc<dyn ProjectServiceAdapter> = Arc::new(self.clone());
            [(ProjectServiceBackendKind::Tmux, adapter)]
                .into_iter()
                .collect()
        }
    }

    #[async_trait]
    impl ProjectServiceAdapter for FakeAdapter {
        fn kind(&self) -> ProjectServiceBackendKind {
            ProjectServiceBackendKind::Tmux
        }

        fn validate_target(
            &self,
            context: &BackendContext<'_>,
            target: &BackendTarget,
        ) -> Result<(), ProjectServiceError> {
            let BackendTarget::Tmux { session_name } = target else {
                return Err(wrong_target(self.kind()));
            };
            let expected = backend_identifier("fake", context, true);
            if session_name == &expected {
                Ok(())
            } else {
                Err(invalid_descriptor_target(format!(
                    "fake session must be `{expected}`"
                )))
            }
        }

        fn resolve_target(
            &self,
            context: &BackendContext<'_>,
            spec: &ProjectServiceSpec,
        ) -> Result<BackendTarget, ProjectServiceError> {
            if !matches!(spec, ProjectServiceSpec::Tmux { .. }) {
                return Err(wrong_spec(self.kind()));
            }
            Ok(BackendTarget::Tmux {
                session_name: backend_identifier("fake", context, true),
            })
        }

        async fn prepare_start(
            &self,
            context: &BackendContext<'_>,
            target: &BackendTarget,
            owned: bool,
        ) -> Result<StartPreparation, ProjectServiceError> {
            let BackendTarget::Tmux { session_name } = target else {
                return Err(wrong_target(self.kind()));
            };
            let inspection = self.inspect(context, target).await?;
            match inspection.state {
                ProjectServiceState::Running if owned => {
                    Ok(StartPreparation::AlreadyRunning(inspection))
                }
                ProjectServiceState::Running => Err(ProjectServiceError::OwnershipConflict {
                    backend: self.kind(),
                    target: session_name.clone(),
                }),
                ProjectServiceState::Unknown => Err(ProjectServiceError::Backend {
                    backend: self.kind(),
                    message: bounded_detail(&inspection.detail),
                }),
                ProjectServiceState::Stopped => Ok(StartPreparation::Launch),
            }
        }

        async fn launch(
            &self,
            _context: &BackendContext<'_>,
            spec: &ProjectServiceSpec,
            target: &BackendTarget,
        ) -> Result<(), ProjectServiceError> {
            if !matches!(spec, ProjectServiceSpec::Tmux { .. }) {
                return Err(wrong_spec(self.kind()));
            }
            if !matches!(target, BackendTarget::Tmux { .. }) {
                return Err(wrong_target(self.kind()));
            }
            let mut state = self.state.lock().await;
            state.running = true;
            state.starts += 1;
            if self.fail_after_start.load(Ordering::SeqCst) {
                return Err(ProjectServiceError::Backend {
                    backend: self.kind(),
                    message: "fake launcher failed after starting the service".to_owned(),
                });
            }
            Ok(())
        }

        async fn inspect(
            &self,
            context: &BackendContext<'_>,
            target: &BackendTarget,
        ) -> Result<BackendInspection, ProjectServiceError> {
            if !matches!(target, BackendTarget::Tmux { .. }) {
                return Err(wrong_target(self.kind()));
            }
            let blocked = self
                .blocked_inspect_service
                .lock()
                .expect("blocked inspect service lock")
                .as_deref()
                == Some(context.service_name);
            if blocked {
                self.inspect_started.notify_one();
                self.inspect_release.notified().await;
            }
            if self.inspect_unknown.load(Ordering::SeqCst) {
                return Ok(BackendInspection {
                    state: ProjectServiceState::Unknown,
                    detail: "fake backend status probe failed".to_owned(),
                });
            }
            let running = self.state.lock().await.running;
            Ok(BackendInspection {
                state: if running {
                    ProjectServiceState::Running
                } else {
                    ProjectServiceState::Stopped
                },
                detail: if running {
                    "fake backend is running".to_owned()
                } else {
                    "fake backend is stopped".to_owned()
                },
            })
        }

        async fn logs(
            &self,
            _context: &BackendContext<'_>,
            target: &BackendTarget,
            _tail_lines: u32,
        ) -> Result<BackendLog, ProjectServiceError> {
            if !matches!(target, BackendTarget::Tmux { .. }) {
                return Err(wrong_target(self.kind()));
            }
            Ok(BackendLog {
                output: self.output.lock().expect("fake log lock").clone(),
                truncated: false,
            })
        }

        async fn stop(
            &self,
            _context: &BackendContext<'_>,
            target: &BackendTarget,
        ) -> Result<(), ProjectServiceError> {
            if !matches!(target, BackendTarget::Tmux { .. }) {
                return Err(wrong_target(self.kind()));
            }
            let mut state = self.state.lock().await;
            if !self.stop_keeps_running.load(Ordering::SeqCst) {
                state.running = false;
            }
            state.stops += 1;
            Ok(())
        }
    }

    #[test]
    fn service_names_and_commands_are_bounded() {
        assert!(validate_service_name("web.dev_1").is_ok());
        assert!(validate_service_name("../escape").is_err());
        assert!(validate_service_name("-leading").is_err());
        assert!(validate_command(&[]).is_err());
        assert!(validate_command(&["npm".to_owned(), "run".to_owned(), "dev".to_owned()]).is_ok());
    }

    #[test]
    fn windows_process_tree_termination_uses_recursive_force_flags() {
        assert_eq!(
            windows_taskkill_args(42),
            [
                "/PID".to_owned(),
                "42".to_owned(),
                "/T".to_owned(),
                "/F".to_owned(),
            ]
        );
    }

    #[test]
    fn compose_files_must_be_regular_workspace_files() {
        let workspace = tempdir().expect("workspace");
        let outside = tempdir().expect("outside");
        fs::write(workspace.path().join("compose.yaml"), "services: {}").expect("compose");
        fs::write(outside.path().join("compose.yaml"), "services: {}").expect("outside compose");

        assert!(resolve_workspace_file(workspace.path(), Path::new("compose.yaml")).is_ok());
        assert!(
            resolve_workspace_file(workspace.path(), &outside.path().join("compose.yaml")).is_err()
        );
    }

    #[test]
    fn descriptor_ownership_requires_backend_and_exact_target_match() {
        let workspace = tempdir().expect("workspace");
        let root = fs::canonicalize(workspace.path()).expect("canonical workspace");
        let fingerprint = workspace_fingerprint(&root);
        let context = BackendContext {
            workspace_root: &root,
            workspace_fingerprint: &fingerprint,
            service_name: "web",
        };
        let tmux = BackendTarget::Tmux {
            session_name: backend_identifier("golutra", &context, false),
        };
        let descriptor = ServiceDescriptor {
            version: DESCRIPTOR_VERSION,
            workspace_fingerprint: fingerprint,
            name: "web".to_owned(),
            registered_at: Utc::now(),
            phase: ServiceDescriptorPhase::Registered,
            target: tmux.clone(),
        };

        assert!(descriptor_owns_target(Some(&descriptor), &tmux));
        assert!(!descriptor_owns_target(
            Some(&descriptor),
            &BackendTarget::SystemdUser {
                unit_name: "golutra-web.service".to_owned(),
            },
        ));

        let docker = BackendTarget::DockerCompose {
            project_name: "golutra-project".to_owned(),
            compose_file: root.join("compose.yaml"),
            services: vec!["web".to_owned()],
        };
        let docker_descriptor = ServiceDescriptor {
            target: docker.clone(),
            ..descriptor
        };
        let changed_target = BackendTarget::DockerCompose {
            project_name: "golutra-project".to_owned(),
            compose_file: root.join("compose.yaml"),
            services: vec!["worker".to_owned()],
        };
        assert!(!descriptor_owns_target(
            Some(&docker_descriptor),
            &changed_target
        ));
    }

    #[test]
    fn legacy_descriptor_without_phase_is_registered() {
        let workspace = tempdir().expect("workspace");
        let root = fs::canonicalize(workspace.path()).expect("canonical workspace");
        let fingerprint = workspace_fingerprint(&root);
        let descriptor = serde_json::json!({
            "version": DESCRIPTOR_VERSION,
            "workspace_fingerprint": fingerprint,
            "name": "web",
            "registered_at": Utc::now(),
            "target": {
                "backend": "tmux",
                "session_name": "golutra-web"
            }
        });

        let parsed: ServiceDescriptor =
            serde_json::from_value(descriptor).expect("legacy descriptor");
        assert_eq!(parsed.phase, ServiceDescriptorPhase::Registered);
    }

    #[tokio::test]
    async fn registry_descriptors_are_owner_only_and_workspace_scoped() {
        let workspace = tempdir().expect("workspace");
        let state = tempdir().expect("state");
        let manager = ProjectServiceManager::new(workspace.path(), state.path()).expect("manager");
        let context = manager.context("web");
        let descriptor = ServiceDescriptor {
            version: DESCRIPTOR_VERSION,
            workspace_fingerprint: manager.workspace_fingerprint.clone(),
            name: "web".to_owned(),
            registered_at: Utc::now(),
            phase: ServiceDescriptorPhase::Registered,
            target: BackendTarget::Tmux {
                session_name: backend_identifier("golutra", &context, false),
            },
        };
        let bytes = serde_json::to_vec(&descriptor).expect("descriptor json");
        write_owner_only_atomic(
            &manager.registry_dir,
            &manager.descriptor_path("web"),
            &bytes,
        )
        .expect("descriptor write");
        let persisted: ServiceDescriptor = serde_json::from_slice(
            &fs::read(manager.descriptor_path("web")).expect("descriptor read"),
        )
        .expect("descriptor parse");

        assert_eq!(persisted, descriptor);
        assert!(manager.validate_descriptor(&persisted).is_ok());
        let mut foreign = persisted;
        foreign.workspace_fingerprint = "foreign".to_owned();
        assert!(manager.validate_descriptor(&foreign).is_err());

        let mut foreign_target = descriptor.clone();
        foreign_target.target = BackendTarget::Tmux {
            session_name: "unrelated-user-session".to_owned(),
        };
        assert!(manager.validate_descriptor(&foreign_target).is_err());

        let mut mismatched_name = descriptor;
        mismatched_name.name = "other".to_owned();
        write_owner_only_atomic(
            &manager.registry_dir,
            &manager.descriptor_path("web"),
            &serde_json::to_vec(&mismatched_name).expect("mismatched descriptor json"),
        )
        .expect("mismatched descriptor write");
        assert!(manager.read_descriptor("web").await.is_err());
    }

    #[test]
    fn backend_identifiers_are_stable_and_backend_safe() {
        let workspace = tempdir().expect("workspace");
        let root = fs::canonicalize(workspace.path()).expect("canonical workspace");
        let fingerprint = workspace_fingerprint(&root);
        let context = BackendContext {
            workspace_root: &root,
            workspace_fingerprint: &fingerprint,
            service_name: "web_dev_1",
        };

        let identifier = backend_identifier("golutra", &context, true);
        assert_eq!(
            identifier,
            format!("golutra-{}-web_dev_1", &fingerprint[..12])
        );
        assert!(identifier.len() <= 63);

        let dotted = BackendContext {
            service_name: "api.v1",
            ..context
        };
        let dashed = BackendContext {
            service_name: "api-v1",
            ..dotted
        };
        assert_ne!(
            backend_identifier("golutra", &dotted, true),
            backend_identifier("golutra", &dashed, true)
        );

        let uppercase = BackendContext {
            service_name: "Web",
            ..dotted
        };
        let lowercase = BackendContext {
            service_name: "web",
            ..dotted
        };
        assert_ne!(
            backend_identifier("golutra", &uppercase, true),
            backend_identifier("golutra", &lowercase, true)
        );
    }

    #[test]
    fn docker_compose_status_uses_container_state_instead_of_record_presence() {
        let running = inspect_docker_compose_ps(
            r#"[{"Service":"web","State":"running","Status":"Up 20 seconds"}]"#,
        );
        let exited = inspect_docker_compose_ps(
            r#"[{"Service":"web","State":"exited","Status":"Exited (1)"}]"#,
        );
        let mixed_ndjson = inspect_docker_compose_ps(
            "{\"Service\":\"worker\",\"State\":\"exited\"}\n{\"Service\":\"web\",\"State\":\"running\"}",
        );
        let malformed = inspect_docker_compose_ps("not-json");

        assert_eq!(running.state, ProjectServiceState::Running);
        assert_eq!(exited.state, ProjectServiceState::Stopped);
        assert_eq!(mixed_ndjson.state, ProjectServiceState::Unknown);
        assert!(mixed_ndjson.detail.contains("1 active, 1 inactive"));
        assert_eq!(malformed.state, ProjectServiceState::Unknown);
        assert_eq!(
            inspect_docker_compose_ps("[]").state,
            ProjectServiceState::Stopped
        );
    }

    #[test]
    fn docker_compose_service_scoped_status_ignores_sibling_services() {
        let output = concat!(
            "{\"Service\":\"web\",\"State\":\"running\",\"Status\":\"Up 20 seconds\"}\n",
            "{\"Service\":\"worker\",\"State\":\"exited\",\"Status\":\"Exited (1)\"}"
        );
        let web = vec!["web".to_owned()];
        let worker = vec!["worker".to_owned()];
        let both = vec!["web".to_owned(), "worker".to_owned()];

        assert_eq!(
            inspect_docker_compose_ps_for_services(output, &web).state,
            ProjectServiceState::Running
        );
        assert_eq!(
            inspect_docker_compose_ps_for_services(output, &worker).state,
            ProjectServiceState::Stopped
        );
        assert_eq!(
            inspect_docker_compose_ps_for_services(output, &both).state,
            ProjectServiceState::Unknown
        );
        assert_eq!(
            inspect_docker_compose_ps_for_services(
                r#"[{"Service":"worker","State":"running","Status":"Up 1 second"}]"#,
                &web,
            )
            .state,
            ProjectServiceState::Stopped
        );
    }

    #[test]
    fn backend_query_failures_do_not_claim_services_are_stopped() {
        let failed_query = BackendCommandOutput {
            success: false,
            stdout: String::new(),
            stderr: "backend connection failed".to_owned(),
            truncated: false,
        };
        let missing_tmux_session = BackendCommandOutput {
            success: false,
            stdout: String::new(),
            stderr: "can't find session: web".to_owned(),
            truncated: false,
        };
        let inactive_systemd_unit = BackendCommandOutput {
            success: false,
            stdout: "inactive\n".to_owned(),
            stderr: String::new(),
            truncated: false,
        };
        let unrecognized_success = BackendCommandOutput {
            success: true,
            stdout: "unexpected\n".to_owned(),
            stderr: String::new(),
            truncated: false,
        };

        assert_eq!(
            inspect_tmux_session("web", &failed_query).state,
            ProjectServiceState::Unknown
        );
        assert_eq!(
            inspect_tmux_session("web", &missing_tmux_session).state,
            ProjectServiceState::Stopped
        );
        assert_eq!(
            inspect_systemd_user_unit("web.service", &failed_query).state,
            ProjectServiceState::Unknown
        );
        assert_eq!(
            inspect_systemd_user_unit("web.service", &inactive_systemd_unit).state,
            ProjectServiceState::Stopped
        );
        assert_eq!(
            inspect_systemd_user_unit("web.service", &unrecognized_success).state,
            ProjectServiceState::Unknown
        );
    }

    #[tokio::test]
    async fn missing_backend_is_reported_without_runtime_ownership() {
        let workspace = tempdir().expect("workspace");
        let error = run_backend_command(
            ProjectServiceBackendKind::Tmux,
            "definitely-not-a-golutra-backend",
            &[],
            workspace.path(),
        )
        .await
        .expect_err("missing backend");

        assert!(matches!(
            error,
            ProjectServiceError::BackendUnavailable {
                backend: ProjectServiceBackendKind::Tmux,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn bounded_reader_preserves_io_errors() {
        let error = read_bounded(FailingReader)
            .await
            .expect_err("reader error must not be converted into EOF");

        assert_eq!(error.kind(), ErrorKind::Other);
        assert_eq!(error.to_string(), "fixture reader failed");
    }

    #[tokio::test]
    async fn timed_out_backend_reaps_child_and_joins_blocked_readers() {
        let workspace = tempdir().expect("workspace");
        let stdout_dropped = Arc::new(AtomicBool::new(false));
        let stderr_dropped = Arc::new(AtomicBool::new(false));
        let stdout_marker = ReaderDropMarker(stdout_dropped.clone());
        let stderr_marker = ReaderDropMarker(stderr_dropped.clone());
        let stdout_reader = tokio::spawn(async move {
            let _marker = stdout_marker;
            std::future::pending::<io::Result<(String, bool)>>().await
        });
        let stderr_reader = tokio::spawn(async move {
            let _marker = stderr_marker;
            std::future::pending::<io::Result<(String, bool)>>().await
        });

        abort_reader_tasks([stdout_reader, stderr_reader]).await;

        assert!(stdout_dropped.load(Ordering::SeqCst));
        assert!(stderr_dropped.load(Ordering::SeqCst));

        let started_at = Instant::now();
        let error = run_backend_command_with_timeout(
            ProjectServiceBackendKind::Tmux,
            "sh",
            &["-c".to_owned(), "sleep 0.2 & wait".to_owned()],
            workspace.path(),
            Duration::from_millis(20),
        )
        .await
        .expect_err("backend timeout");

        assert!(
            error
                .to_string()
                .contains("backend command exceeded 20 milliseconds")
        );
        assert!(started_at.elapsed() < Duration::from_secs(1));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn timed_out_backend_terminates_descendants_with_the_process_group() {
        let workspace = tempdir().expect("workspace");
        let pid_path = workspace.path().join("descendant.pid");
        let script = format!(
            "sleep 30 & child=$!; printf '%s' $child > '{}'; wait",
            pid_path.display()
        );

        let error = run_backend_command_with_timeout(
            ProjectServiceBackendKind::Tmux,
            "sh",
            &["-c".to_owned(), script],
            workspace.path(),
            Duration::from_millis(50),
        )
        .await
        .expect_err("backend timeout");
        assert!(error.to_string().contains("backend command exceeded"));

        let pid = fs::read_to_string(pid_path)
            .expect("descendant pid")
            .parse::<i32>()
            .expect("numeric descendant pid");
        for _ in 0..20 {
            if nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_err() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("timed-out backend descendant is still alive: {pid}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn failed_backend_terminates_descendants_with_the_process_group() {
        let workspace = tempdir().expect("workspace");
        let pid_path = workspace.path().join("failed-descendant.pid");
        let script = format!(
            "sleep 30 </dev/null >/dev/null 2>&1 & child=$!; printf '%s' $child > '{}'; exit 1",
            pid_path.display()
        );

        let output = run_backend_command_with_timeout(
            ProjectServiceBackendKind::Tmux,
            "sh",
            &["-c".to_owned(), script],
            workspace.path(),
            Duration::from_secs(1),
        )
        .await
        .expect("failed backend command should return its exit status");
        assert!(!output.success);

        let pid = fs::read_to_string(pid_path)
            .expect("descendant pid")
            .parse::<i32>()
            .expect("numeric descendant pid");
        for _ in 0..20 {
            if nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_err() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("failed backend descendant is still alive: {pid}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn successful_backend_preserves_started_descendants() {
        let workspace = tempdir().expect("workspace");
        let marker_path = workspace.path().join("descendant-finished");
        let script = format!(
            "(sleep 0.05; printf alive > '{}') </dev/null >/dev/null 2>&1 &",
            marker_path.display()
        );

        let output = run_backend_command_with_timeout(
            ProjectServiceBackendKind::Tmux,
            "sh",
            &["-c".to_owned(), script],
            workspace.path(),
            Duration::from_secs(1),
        )
        .await
        .expect("backend command");
        assert!(output.success);

        for _ in 0..50 {
            if marker_path.exists() {
                assert_eq!(fs::read_to_string(marker_path).expect("marker"), "alive");
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("successful backend descendant was terminated with its launcher");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn successful_backend_does_not_wait_for_inherited_output_pipes() {
        let workspace = tempdir().expect("workspace");
        let pid_path = workspace.path().join("inherited-pipe-descendant.pid");
        let script = format!(
            "sleep 1 & child=$!; printf '%s' $child > '{}'; exit 0",
            pid_path.display()
        );
        let started_at = Instant::now();

        let output = run_backend_command_with_timeout(
            ProjectServiceBackendKind::Tmux,
            "sh",
            &["-c".to_owned(), script],
            workspace.path(),
            Duration::from_secs(1),
        )
        .await
        .expect("successful backend command");

        assert!(output.success);
        assert!(
            started_at.elapsed() < Duration::from_secs(1),
            "launcher should not inherit the service's lifetime"
        );
        let pid = fs::read_to_string(&pid_path)
            .expect("descendant pid")
            .parse::<i32>()
            .expect("numeric descendant pid");
        assert!(
            nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_ok(),
            "successful launch should leave the external descendant running"
        );
        let _ = nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(pid),
            nix::sys::signal::Signal::SIGKILL,
        );
    }

    #[tokio::test]
    async fn manager_lifecycle_persists_and_recovers_project_service_descriptors() {
        let workspace = tempdir().expect("workspace");
        let state = tempdir().expect("state");
        let fake = FakeAdapter::new();
        let manager =
            ProjectServiceManager::with_adapters(workspace.path(), state.path(), fake.adapters())
                .expect("manager");
        let request = ProjectServiceStartRequest {
            name: "web".to_owned(),
            spec: ProjectServiceSpec::Tmux {
                command: vec!["fake-server".to_owned()],
            },
        };

        let started = manager.start(request.clone()).await.expect("start");
        assert_eq!(started.state, ProjectServiceState::Running);
        assert_eq!(manager.list().await.expect("list").len(), 1);
        assert_eq!(
            manager.status("web").await.expect("status").state,
            ProjectServiceState::Running
        );
        let logs = manager.logs("web", 20).await.expect("logs");
        assert_eq!(logs.output, "fake service log\n");
        assert!(!logs.truncated);
        assert!(matches!(
            manager.start(request.clone()).await,
            Err(ProjectServiceError::AlreadyRunning(name)) if name == "web"
        ));

        let recovered =
            ProjectServiceManager::with_adapters(workspace.path(), state.path(), fake.adapters())
                .expect("recovered manager");
        assert_eq!(
            recovered
                .status("web")
                .await
                .expect("recovered status")
                .state,
            ProjectServiceState::Running
        );

        let stopped = recovered.stop("web").await.expect("stop");
        assert_eq!(stopped.state, ProjectServiceState::Stopped);
        assert!(recovered.list().await.expect("empty list").is_empty());
        assert!(matches!(
            recovered.status("web").await,
            Err(ProjectServiceError::NotRegistered(name)) if name == "web"
        ));
        assert!(!recovered.descriptor_path("web").exists());

        let state = fake.state.lock().await;
        assert_eq!(state.starts, 1);
        assert_eq!(state.stops, 1);
    }

    #[tokio::test]
    async fn slow_list_probe_does_not_block_an_unrelated_service_operation() {
        let workspace = tempdir().expect("workspace");
        let state = tempdir().expect("state");
        let fake = FakeAdapter::new();
        let manager =
            ProjectServiceManager::with_adapters(workspace.path(), state.path(), fake.adapters())
                .expect("manager");
        for name in ["slow", "other"] {
            let context = manager.context(name);
            manager
                .write_descriptor(&ServiceDescriptor {
                    version: DESCRIPTOR_VERSION,
                    workspace_fingerprint: manager.workspace_fingerprint.clone(),
                    name: name.to_owned(),
                    registered_at: Utc::now(),
                    phase: ServiceDescriptorPhase::Registered,
                    target: BackendTarget::Tmux {
                        session_name: backend_identifier("fake", &context, true),
                    },
                })
                .expect("descriptor");
        }
        fake.state.lock().await.running = true;
        *fake
            .blocked_inspect_service
            .lock()
            .expect("blocked inspect service lock") = Some("slow".to_owned());

        let listing_manager = manager.clone();
        let listing = tokio::spawn(async move { listing_manager.list().await });
        timeout(Duration::from_secs(1), fake.inspect_started.notified())
            .await
            .expect("slow inspection started");

        let unrelated = timeout(Duration::from_millis(250), manager.status("other")).await;
        fake.inspect_release.notify_one();
        let summaries = listing.await.expect("list task").expect("list services");

        assert_eq!(
            unrelated
                .expect("unrelated status must not wait for the slow probe")
                .expect("unrelated status")
                .state,
            ProjectServiceState::Running
        );
        assert_eq!(summaries.len(), 2);
    }

    #[tokio::test]
    async fn slow_status_probe_does_not_block_an_unrelated_service_operation() {
        let workspace = tempdir().expect("workspace");
        let state = tempdir().expect("state");
        let fake = FakeAdapter::new();
        let manager =
            ProjectServiceManager::with_adapters(workspace.path(), state.path(), fake.adapters())
                .expect("manager");
        for name in ["slow", "other"] {
            let context = manager.context(name);
            manager
                .write_descriptor(&ServiceDescriptor {
                    version: DESCRIPTOR_VERSION,
                    workspace_fingerprint: manager.workspace_fingerprint.clone(),
                    name: name.to_owned(),
                    registered_at: Utc::now(),
                    phase: ServiceDescriptorPhase::Registered,
                    target: BackendTarget::Tmux {
                        session_name: backend_identifier("fake", &context, true),
                    },
                })
                .expect("descriptor");
        }
        fake.state.lock().await.running = true;
        *fake
            .blocked_inspect_service
            .lock()
            .expect("blocked inspect service lock") = Some("slow".to_owned());

        let status_manager = manager.clone();
        let slow_status = tokio::spawn(async move { status_manager.status("slow").await });
        timeout(Duration::from_secs(1), fake.inspect_started.notified())
            .await
            .expect("slow inspection started");

        let unrelated = timeout(Duration::from_millis(250), manager.status("other")).await;
        fake.inspect_release.notify_one();
        slow_status
            .await
            .expect("slow status task")
            .expect("slow status");

        assert_eq!(
            unrelated
                .expect("unrelated status must not wait for the slow probe")
                .expect("unrelated status")
                .state,
            ProjectServiceState::Running
        );
    }

    #[tokio::test]
    async fn start_fails_closed_when_existing_service_status_is_unknown() {
        let workspace = tempdir().expect("workspace");
        let state = tempdir().expect("state");
        let fake = FakeAdapter::new();
        let manager =
            ProjectServiceManager::with_adapters(workspace.path(), state.path(), fake.adapters())
                .expect("manager");
        let request = ProjectServiceStartRequest {
            name: "web".to_owned(),
            spec: ProjectServiceSpec::Tmux {
                command: vec!["fake-server".to_owned()],
            },
        };
        manager.start(request.clone()).await.expect("initial start");
        fake.inspect_unknown.store(true, Ordering::SeqCst);

        let error = manager
            .start(request)
            .await
            .expect_err("unknown status must block duplicate start");
        assert!(matches!(
            error,
            ProjectServiceError::StatusUnknown {
                name,
                backend: ProjectServiceBackendKind::Tmux,
                detail,
            } if name == "web" && detail.contains("status probe failed")
        ));
        assert!(manager.descriptor_path("web").is_file());
        assert_eq!(fake.state.lock().await.starts, 1);
    }

    #[tokio::test]
    async fn registry_lock_wait_is_bounded() {
        let workspace = tempdir().expect("workspace");
        let state = tempdir().expect("state");
        let manager = ProjectServiceManager::new(workspace.path(), state.path()).expect("manager");
        let _held = manager
            .acquire_registry_lock()
            .await
            .expect("hold registry lock");

        let blocked =
            tokio::time::timeout(Duration::from_millis(250), manager.acquire_registry_lock()).await;

        assert!(
            matches!(blocked, Ok(Err(ProjectServiceError::Registry(message))) if message.contains("timed out"))
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn registry_and_lock_directories_reject_symbolic_links() {
        use std::os::unix::fs::symlink;

        let workspace = tempdir().expect("workspace");
        let state = tempdir().expect("state");
        let target = tempdir().expect("symlink target");

        let registry_manager =
            ProjectServiceManager::new(workspace.path(), state.path().join("registry-case"))
                .expect("registry manager");
        fs::create_dir_all(
            registry_manager
                .registry_dir
                .parent()
                .expect("registry parent"),
        )
        .expect("registry parent directory");
        symlink(target.path(), &registry_manager.registry_dir).expect("registry symlink");
        assert!(matches!(
            registry_manager.acquire_registry_lock().await,
            Err(ProjectServiceError::Registry(message)) if message.contains("symbolic link")
        ));

        let lock_manager =
            ProjectServiceManager::new(workspace.path(), state.path().join("lock-case"))
                .expect("lock manager");
        fs::create_dir_all(&lock_manager.registry_dir).expect("registry directory");
        symlink(target.path(), lock_manager.registry_dir.join("locks")).expect("lock symlink");
        assert!(matches!(
            lock_manager.acquire_registry_lock().await,
            Err(ProjectServiceError::Registry(message)) if message.contains("symbolic link")
        ));
    }

    #[test]
    fn owner_only_directory_rejects_a_regular_file() {
        let state = tempdir().expect("state");
        let path = state.path().join("not-a-directory");
        fs::write(&path, "state").expect("state file");

        assert!(matches!(
            ensure_owner_only_dir(&path),
            Err(ProjectServiceError::Registry(message)) if message.contains("not a directory")
        ));
    }

    #[tokio::test]
    async fn stop_retains_descriptor_until_backend_is_observed_stopped() {
        let workspace = tempdir().expect("workspace");
        let state = tempdir().expect("state");
        let fake = FakeAdapter::new();
        let manager =
            ProjectServiceManager::with_adapters(workspace.path(), state.path(), fake.adapters())
                .expect("manager");
        manager
            .start(ProjectServiceStartRequest {
                name: "web".to_owned(),
                spec: ProjectServiceSpec::Tmux {
                    command: vec!["fake-server".to_owned()],
                },
            })
            .await
            .expect("start");
        fake.stop_keeps_running.store(true, Ordering::SeqCst);

        let error = manager
            .stop("web")
            .await
            .expect_err("running backend must retain descriptor");
        assert!(matches!(error, ProjectServiceError::Backend { .. }));
        assert!(manager.descriptor_path("web").is_file());
        assert_eq!(
            manager.status("web").await.expect("retryable status").state,
            ProjectServiceState::Running
        );

        fake.stop_keeps_running.store(false, Ordering::SeqCst);
        assert_eq!(
            manager.stop("web").await.expect("retry stop").state,
            ProjectServiceState::Stopped
        );
        assert!(!manager.descriptor_path("web").exists());
    }

    #[tokio::test]
    async fn start_failure_retains_launching_descriptor_until_reconciled() {
        let workspace = tempdir().expect("workspace");
        let state = tempdir().expect("state");
        let fake = FakeAdapter::new();
        let manager =
            ProjectServiceManager::with_adapters(workspace.path(), state.path(), fake.adapters())
                .expect("manager");
        let request = ProjectServiceStartRequest {
            name: "web".to_owned(),
            spec: ProjectServiceSpec::Tmux {
                command: vec!["fake-server".to_owned()],
            },
        };
        fake.fail_after_start.store(true, Ordering::SeqCst);

        let error = manager
            .start(request.clone())
            .await
            .expect_err("launcher failure");

        assert!(matches!(error, ProjectServiceError::Backend { .. }));
        let persisted: ServiceDescriptor = serde_json::from_slice(
            &fs::read(manager.descriptor_path("web")).expect("launching descriptor"),
        )
        .expect("descriptor json");
        assert_eq!(persisted.phase, ServiceDescriptorPhase::Launching);
        assert_eq!(
            manager
                .list()
                .await
                .expect("reconciled list")
                .first()
                .expect("listed service")
                .state,
            ProjectServiceState::Running,
        );
        let reconciled: ServiceDescriptor = serde_json::from_slice(
            &fs::read(manager.descriptor_path("web")).expect("registered descriptor"),
        )
        .expect("descriptor json");
        assert_eq!(reconciled.phase, ServiceDescriptorPhase::Registered);

        // A second list must also clean up a Launching descriptor when the
        // backend is observed stopped, rather than leaving stale recovery state.
        fake.state.lock().await.running = false;
        let mut launching = reconciled;
        launching.phase = ServiceDescriptorPhase::Launching;
        manager
            .write_descriptor(&launching)
            .expect("relaunch marker");
        let listed = manager.list().await.expect("stopped recovery list");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].state, ProjectServiceState::Stopped);
        assert!(!manager.descriptor_path("web").exists());

        fake.fail_after_start.store(false, Ordering::SeqCst);
        manager
            .start(request)
            .await
            .expect("restart after recovery");
        manager.stop("web").await.expect("stop recovered service");

        let state = fake.state.lock().await;
        assert_eq!(state.starts, 2);
        assert_eq!(state.stops, 1);
        assert!(!manager.descriptor_path("web").exists());
    }
}
