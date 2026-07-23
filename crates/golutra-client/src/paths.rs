//! Runtime home、工作区路径与权限初始化。

use std::{
    fs,
    path::{Path, PathBuf},
};

use golutra_config::golutra_home;
use golutra_core::{SessionId, ThreadId, WorkspaceId};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::ClientError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppServerPaths {
    pub home: PathBuf,
    pub app_server_dir: PathBuf,
    pub endpoint: PathBuf,
    pub lock: PathBuf,
    pub transport_token: PathBuf,
    pub ipc_socket: PathBuf,
}

impl AppServerPaths {
    pub fn global() -> Result<Self, ClientError> {
        let home = golutra_home().map_err(|error| ClientError::Io(error.to_string()))?;
        let home = prepare_private_home(&home)?;
        Self::from_canonical_home(home)
    }

    fn from_canonical_home(home: PathBuf) -> Result<Self, ClientError> {
        let app_server_dir = home.join("app-server");
        ensure_private_dir(&app_server_dir)?;
        Ok(Self {
            endpoint: app_server_dir.join("app-server.json"),
            lock: app_server_dir.join("daemon.lock"),
            transport_token: app_server_dir.join("transport.token"),
            ipc_socket: app_server_dir.join("app-server.sock"),
            home,
            app_server_dir,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimePaths {
    pub home: PathBuf,
    pub state_dir: PathBuf,
    pub runtime_db: PathBuf,
    pub artifacts_dir: PathBuf,
    pub workspace_state_dir: PathBuf,
    pub checkpoints_dir: PathBuf,
    pub rollouts_dir: PathBuf,
    pub memory_file: PathBuf,
    pub evaluation_file: PathBuf,
    pub evolution_file: PathBuf,
    pub evolution_skills_dir: PathBuf,
    pub evolution_runs_dir: PathBuf,
    pub mcp_scratch_dir: PathBuf,
    pub code_index_file: PathBuf,
    pub session_locks_dir: PathBuf,
    pub command_locks_dir: PathBuf,
    pub app_server_dir: PathBuf,
    pub app_server_endpoint: PathBuf,
    pub app_server_lock: PathBuf,
    pub app_server_transport_token: PathBuf,
    pub app_server_ipc_socket: PathBuf,
    pub cwd: PathBuf,
    pub workspace_hash: String,
}

impl RuntimePaths {
    pub fn for_cwd(cwd: impl AsRef<Path>) -> Result<Self, ClientError> {
        let home = golutra_home().map_err(|error| ClientError::Io(error.to_string()))?;
        Self::from_home_and_cwd(home, cwd)
    }

    pub fn from_home_and_cwd(
        home: impl AsRef<Path>,
        cwd: impl AsRef<Path>,
    ) -> Result<Self, ClientError> {
        let cwd = canonical_cwd(cwd.as_ref())?;
        let home = prepare_private_home(home.as_ref())?;
        let app_server_paths = AppServerPaths::from_canonical_home(home.clone())?;
        let state_dir = home.join("state");
        let artifacts_dir = state_dir.join("artifacts");
        let workspaces_dir = state_dir.join("workspaces");
        let workspace_hash = workspace_hash(&cwd);
        let workspace_state_dir = workspaces_dir.join(&workspace_hash);
        let checkpoints_dir = workspace_state_dir.join("checkpoints");
        let rollouts_dir = workspace_state_dir.join("rollouts");
        let evolution_skills_dir = workspace_state_dir.join("skills");
        let evolution_runs_dir = workspace_state_dir.join("evolution-runs");
        let mcp_scratch_dir = state_dir.join("mcp-scratch");
        let session_locks_dir = state_dir.join("session-locks");
        let command_locks_dir = state_dir.join("command-locks");
        for path in [
            &state_dir,
            &artifacts_dir,
            &workspaces_dir,
            &workspace_state_dir,
            &checkpoints_dir,
            &rollouts_dir,
            &evolution_skills_dir,
            &evolution_runs_dir,
            &mcp_scratch_dir,
            &session_locks_dir,
            &command_locks_dir,
        ] {
            ensure_private_dir(path)?;
        }

        Ok(Self {
            runtime_db: state_dir.join("runtime.sqlite"),
            memory_file: workspace_state_dir.join("memory.json"),
            evaluation_file: workspace_state_dir.join("evaluation.json"),
            evolution_file: workspace_state_dir.join("evolution.json"),
            evolution_skills_dir,
            evolution_runs_dir,
            mcp_scratch_dir,
            code_index_file: workspace_state_dir.join("code-index.json"),
            app_server_endpoint: app_server_paths.endpoint,
            app_server_lock: app_server_paths.lock,
            app_server_transport_token: app_server_paths.transport_token,
            app_server_ipc_socket: app_server_paths.ipc_socket,
            home,
            state_dir,
            artifacts_dir,
            workspace_state_dir,
            checkpoints_dir,
            rollouts_dir,
            session_locks_dir,
            command_locks_dir,
            app_server_dir: app_server_paths.app_server_dir,
            cwd,
            workspace_hash,
        })
    }

    /// Create a new isolated runtime home for one persisted ephemeral run.
    ///
    /// The caller supplies the run root rather than a shared Golutra home, so
    /// the resulting state can be retained or moved without joining the
    /// user's normal runtime history. Provider configuration is selected by
    /// the host separately.
    pub fn for_ephemeral_state_dir(
        state_home: impl AsRef<Path>,
        cwd: impl AsRef<Path>,
    ) -> Result<Self, ClientError> {
        let state_home = state_home.as_ref();
        if !state_home.is_absolute() {
            return Err(ClientError::Io(format!(
                "ephemeral state directory must be absolute: {}",
                state_home.display()
            )));
        }
        if state_home.file_name().is_none() {
            return Err(ClientError::Io(
                "ephemeral state directory must name a new directory".to_owned(),
            ));
        }

        let parent = state_home.parent().ok_or_else(|| {
            ClientError::Io(format!(
                "ephemeral state directory has no parent: {}",
                state_home.display()
            ))
        })?;
        match fs::symlink_metadata(parent) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(ClientError::Io(format!(
                    "ephemeral state directory parent cannot be a symbolic link: {}",
                    parent.display()
                )));
            }
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => {
                return Err(ClientError::Io(format!(
                    "ephemeral state directory parent is not a directory: {}",
                    parent.display()
                )));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(ClientError::Io(format!(
                    "ephemeral state directory parent does not exist: {}",
                    parent.display()
                )));
            }
            Err(error) => return Err(ClientError::Io(error.to_string())),
        }
        match fs::symlink_metadata(state_home) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(ClientError::Io(format!(
                    "ephemeral state directory cannot be a symbolic link: {}",
                    state_home.display()
                )));
            }
            Ok(_) => {
                return Err(ClientError::Io(format!(
                    "ephemeral state directory already exists: {}",
                    state_home.display()
                )));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(ClientError::Io(error.to_string())),
        }

        fs::create_dir(state_home).map_err(|error| {
            ClientError::Io(format!(
                "failed to create ephemeral state directory {}: {error}",
                state_home.display()
            ))
        })?;
        set_owner_only_dir(state_home)?;
        let state_home = state_home
            .canonicalize()
            .map_err(|error| ClientError::Io(error.to_string()))?;
        Self::from_home_and_cwd(state_home, cwd)
    }

    #[must_use]
    pub fn sqlite_url(&self) -> String {
        format!("sqlite://{}", self.runtime_db.display())
    }

    #[must_use]
    pub fn workspace_id(&self) -> WorkspaceId {
        deterministic_workspace_id(&self.cwd)
    }

    #[must_use]
    pub fn session_lock(&self, session_id: SessionId) -> PathBuf {
        self.session_locks_dir.join(format!("{session_id}.lock"))
    }

    #[must_use]
    pub fn command_lock(&self, idempotency_key: &str) -> PathBuf {
        let digest = Sha256::digest(idempotency_key.as_bytes());
        self.command_locks_dir.join(format!("{digest:x}.lock"))
    }

    #[must_use]
    pub fn rollout_path(&self, thread_id: ThreadId) -> PathBuf {
        self.rollouts_dir.join(format!("{thread_id}.jsonl"))
    }
}

fn absolute_path(path: &Path) -> Result<PathBuf, ClientError> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    std::env::current_dir()
        .map(|cwd| cwd.join(path))
        .map_err(|error| ClientError::Io(error.to_string()))
}

fn prepare_private_home(home: &Path) -> Result<PathBuf, ClientError> {
    let home = absolute_path(home)?;
    ensure_private_dir(&home)?;
    home.canonicalize()
        .map_err(|error| ClientError::Io(error.to_string()))
}

fn canonical_cwd(cwd: &Path) -> Result<PathBuf, ClientError> {
    let canonical = cwd
        .canonicalize()
        .map_err(|error| ClientError::Io(format!("{}: {error}", cwd.display())))?;
    if !canonical.is_dir() {
        return Err(ClientError::Io(format!(
            "runtime cwd is not a directory: {}",
            canonical.display()
        )));
    }
    Ok(canonical)
}

fn workspace_digest(cwd: &Path) -> [u8; 32] {
    Sha256::digest(cwd.to_string_lossy().as_bytes()).into()
}

pub(crate) fn workspace_hash(cwd: &Path) -> String {
    workspace_digest(cwd)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn deterministic_workspace_id(cwd: &Path) -> WorkspaceId {
    let digest = workspace_digest(cwd);
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x50;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    WorkspaceId(Uuid::from_bytes(bytes))
}

pub(crate) fn ensure_private_dir(path: &Path) -> Result<(), ClientError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(ClientError::Io(format!(
                "runtime directory cannot be a symbolic link: {}",
                path.display()
            )));
        }
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => {
            return Err(ClientError::Io(format!(
                "runtime path is not a directory: {}",
                path.display()
            )));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir_all(path).map_err(|error| ClientError::Io(error.to_string()))?;
        }
        Err(error) => return Err(ClientError::Io(error.to_string())),
    }
    set_owner_only_dir(path)
}

#[cfg(unix)]
fn set_owner_only_dir(path: &Path) -> Result<(), ClientError> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(|error| ClientError::Io(error.to_string()))
}

#[cfg(not(unix))]
fn set_owner_only_dir(_path: &Path) -> Result<(), ClientError> {
    Ok(())
}

#[cfg(unix)]
pub(crate) fn set_owner_only_file(path: &Path) -> Result<(), ClientError> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .map_err(|error| ClientError::Io(error.to_string()))
}

#[cfg(not(unix))]
pub(crate) fn set_owner_only_file(_path: &Path) -> Result<(), ClientError> {
    Ok(())
}
