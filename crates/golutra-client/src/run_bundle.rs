//! Owner-only run bundle export for unattended `golutra exec` invocations.
//!
//! The raw runtime store remains in `state/`. This module writes a stable,
//! machine-readable observation projection next to it, plus an optional
//! redacted handoff export. It never copies provider configuration or
//! credentials into the bundle.

use std::{
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
};

use chrono::{DateTime, Utc};
use golutra_protocol::{AgentTurnResult, SessionWindowRequest};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{
    ClientError, DebugExportCoordinator, DebugExportReceipt, ObservedSession,
    RuntimeObservationCollector, RuntimeObservationSnapshot, RuntimeTransport, set_owner_only_file,
};

const RUN_BUNDLE_FORMAT_VERSION: u32 = 1;
const OBSERVATION_FORMAT_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq)]
pub struct RunBundleExportRequest {
    /// The run directory. It already exists and contains `state/` because the
    /// persistent ephemeral runtime created it before executing the turn.
    pub destination: PathBuf,
    pub selection: SessionWindowRequest,
    pub terminal_outcome: RunBundleTerminalOutcome,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RunBundleTerminalOutcome {
    Result { result: AgentTurnResult },
    Error { error: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunBundleReceipt {
    pub destination: PathBuf,
    pub observations_path: String,
    pub debug_export_path: Option<String>,
    pub debug_export_error: Option<String>,
    pub session_count: usize,
    pub task_count: usize,
    pub complete: bool,
    pub manifest_checksum: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunBundleManifest {
    pub format: String,
    pub format_version: u32,
    pub generated_at: DateTime<Utc>,
    pub mode: String,
    pub workspace_id: String,
    pub workspace_root: String,
    pub selection: SessionWindowRequest,
    pub terminal_outcome: RunBundleTerminalOutcome,
    pub raw_state: RawStateManifest,
    pub observations: ObservationBundleManifest,
    pub debug_export: DebugExportOutcome,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RawStateManifest {
    pub path: String,
    pub runtime_database: RunBundlePath,
    pub artifacts: RunBundlePath,
    pub workspaces: RunBundlePath,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunBundlePath {
    pub path: String,
    pub present: bool,
    pub bytes: Option<u64>,
    pub checksum: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservationBundleManifest {
    pub format: String,
    pub format_version: u32,
    pub generated_at: DateTime<Utc>,
    pub path: String,
    pub disclosure: String,
    pub complete: bool,
    pub missing_data: Vec<String>,
    pub retention_losses: Vec<String>,
    pub sessions: Vec<ObservationSessionManifest>,
    pub files: Vec<RunBundleFile>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservationSessionManifest {
    pub thread_id: String,
    pub session_id: String,
    pub path: String,
    pub event_count: usize,
    pub conversation_count: usize,
    pub events_complete: bool,
    pub tasks: Vec<ObservationTaskManifest>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservationTaskManifest {
    pub task_id: String,
    pub trace_path: String,
    pub complete: bool,
    pub unresolved_refs: Vec<String>,
    pub missing_sections: Vec<String>,
    pub retention_losses: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunBundleFile {
    pub path: String,
    pub bytes: u64,
    pub checksum: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum DebugExportOutcome {
    Exported {
        path: String,
        receipt: DebugExportManifestReceipt,
    },
    Failed {
        path: String,
        error: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DebugExportManifestReceipt {
    pub session_count: usize,
    pub task_count: usize,
    pub artifact_count: usize,
    pub complete: bool,
    pub manifest_checksum: String,
}

/// Local exporter used by `exec --run-dir`. It is intentionally a client-side
/// adapter: the RuntimeHost remains the only owner of facts and execution.
#[derive(Debug, Clone)]
pub struct RunBundleExporter<'a> {
    transport: &'a RuntimeTransport,
}

impl<'a> RunBundleExporter<'a> {
    #[must_use]
    pub fn new(transport: &'a RuntimeTransport) -> Self {
        Self { transport }
    }

    pub async fn export(
        &self,
        request: RunBundleExportRequest,
    ) -> Result<RunBundleReceipt, ClientError> {
        validate_run_root(&request.destination)?;
        let snapshot = RuntimeObservationCollector::new(self.transport)
            .collect(request.selection.clone())
            .await?;
        let observations = write_observations(&request.destination, &snapshot)?;

        let debug_export_path = request.destination.join("debug-export");
        let debug_export = match DebugExportCoordinator::new(self.transport)
            .export_snapshot(snapshot.clone(), debug_export_path.clone())
            .await
        {
            Ok(receipt) => DebugExportOutcome::Exported {
                path: "debug-export".to_owned(),
                receipt: debug_export_receipt(receipt),
            },
            Err(error) => DebugExportOutcome::Failed {
                path: "debug-export".to_owned(),
                error: error.to_string(),
            },
        };

        let manifest = RunBundleManifest {
            format: "golutra-run-bundle".to_owned(),
            format_version: RUN_BUNDLE_FORMAT_VERSION,
            generated_at: Utc::now(),
            mode: "full-owner-only".to_owned(),
            workspace_id: self.transport.workspace_id().to_string(),
            workspace_root: self
                .transport
                .cwd()
                .map(|path| path.display().to_string())
                .unwrap_or_default(),
            selection: request.selection,
            terminal_outcome: request.terminal_outcome,
            raw_state: raw_state_manifest(&request.destination)?,
            observations,
            debug_export,
        };
        let manifest_path = request.destination.join("manifest.json");
        let manifest_bytes = serde_json::to_vec_pretty(&manifest)?;
        write_new_file(&manifest_path, &manifest_bytes)?;
        sync_directory(&request.destination)?;

        let (debug_export_path, debug_export_error) = match &manifest.debug_export {
            DebugExportOutcome::Exported { path, .. } => (Some(path.clone()), None),
            DebugExportOutcome::Failed { error, .. } => (None, Some(error.clone())),
        };
        Ok(RunBundleReceipt {
            destination: request.destination,
            observations_path: "observations".to_owned(),
            debug_export_path,
            debug_export_error,
            session_count: manifest.observations.sessions.len(),
            task_count: manifest
                .observations
                .sessions
                .iter()
                .map(|session| session.tasks.len())
                .sum(),
            complete: manifest.observations.complete,
            manifest_checksum: checksum(&manifest_bytes),
        })
    }
}

fn write_observations(
    run_root: &Path,
    snapshot: &RuntimeObservationSnapshot,
) -> Result<ObservationBundleManifest, ClientError> {
    let destination = run_root.join("observations");
    ensure_new_directory_destination(&destination, "observations")?;
    let temporary = tempfile::Builder::new()
        .prefix(".golutra-observations-")
        .tempdir_in(run_root)
        .map_err(|error| bundle_io(run_root, error))?;
    let staging = temporary.path();
    set_owner_only_dir(staging)?;
    let sessions_root = staging.join("sessions");
    create_private_dir(&sessions_root)?;
    let mut files = Vec::new();
    let mut sessions = Vec::with_capacity(snapshot.sessions.len());
    for session in &snapshot.sessions {
        sessions.push(write_session_observations(staging, session, &mut files)?);
    }
    files.sort_by(|left, right| left.path.cmp(&right.path));
    let manifest = ObservationBundleManifest {
        format: "golutra-runtime-observation".to_owned(),
        format_version: OBSERVATION_FORMAT_VERSION,
        generated_at: Utc::now(),
        path: "observations".to_owned(),
        disclosure: "full-owner-only".to_owned(),
        complete: snapshot.complete,
        missing_data: snapshot.missing_data.clone(),
        retention_losses: snapshot.retention_losses.clone(),
        sessions,
        files,
    };
    let manifest_bytes = serde_json::to_vec_pretty(&manifest)?;
    write_new_file(&staging.join("manifest.json"), &manifest_bytes)?;
    sync_tree(staging)?;
    let staging_path = temporary.keep();
    if let Err(error) = fs::rename(&staging_path, &destination) {
        let _ = fs::remove_dir_all(&staging_path);
        return Err(bundle_io(&destination, error));
    }
    sync_directory(run_root)?;
    Ok(manifest)
}

fn write_session_observations(
    staging: &Path,
    session: &ObservedSession,
    files: &mut Vec<RunBundleFile>,
) -> Result<ObservationSessionManifest, ClientError> {
    let relative_root = format!("sessions/{}", session.summary.session_id);
    let session_root = staging.join(&relative_root);
    create_private_dir(&session_root)?;
    create_private_dir(&session_root.join("tasks"))?;
    files.push(write_json_file(
        &session_root.join("thread.json"),
        &session.thread,
        format!("observations/{relative_root}/thread.json"),
    )?);
    files.push(write_json_lines_file(
        &session_root.join("events.jsonl"),
        &session.events,
        format!("observations/{relative_root}/events.jsonl"),
    )?);
    files.push(write_json_lines_file(
        &session_root.join("conversation.jsonl"),
        &session.conversation,
        format!("observations/{relative_root}/conversation.jsonl"),
    )?);
    let mut tasks = Vec::with_capacity(session.tasks.len());
    for task in &session.tasks {
        let task_relative_root = format!("{relative_root}/tasks/{}", task.task_id);
        let task_root = staging.join(&task_relative_root);
        create_private_dir(&task_root)?;
        let trace_path = format!("observations/{task_relative_root}/trace.json");
        files.push(write_json_file(
            &task_root.join("trace.json"),
            &task.trace,
            trace_path.clone(),
        )?);
        tasks.push(ObservationTaskManifest {
            task_id: task.task_id.to_string(),
            trace_path,
            complete: task.trace.integrity.complete,
            unresolved_refs: task.trace.integrity.unresolved_refs.clone(),
            missing_sections: task.trace.integrity.missing_sections.clone(),
            retention_losses: task.trace.integrity.retention_losses.clone(),
        });
    }
    Ok(ObservationSessionManifest {
        thread_id: session.summary.thread_id.to_string(),
        session_id: session.summary.session_id.to_string(),
        path: format!("observations/{relative_root}"),
        event_count: session.events.len(),
        conversation_count: session.conversation.len(),
        events_complete: session.events_complete,
        tasks,
    })
}

fn raw_state_manifest(run_root: &Path) -> Result<RawStateManifest, ClientError> {
    Ok(RawStateManifest {
        path: "state".to_owned(),
        runtime_database: inspect_path(run_root, "state/runtime.sqlite", true)?,
        artifacts: inspect_path(run_root, "state/artifacts", false)?,
        workspaces: inspect_path(run_root, "state/workspaces", false)?,
    })
}

fn inspect_path(
    root: &Path,
    relative: &str,
    include_checksum: bool,
) -> Result<RunBundlePath, ClientError> {
    let path = root.join(relative);
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(ClientError::Io(format!(
                "run bundle state path cannot be a symbolic link: {}",
                path.display()
            )));
        }
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(RunBundlePath {
                path: relative.to_owned(),
                present: false,
                bytes: None,
                checksum: None,
            });
        }
        Err(error) => return Err(bundle_io(&path, error)),
    };
    let checksum = if include_checksum && metadata.is_file() {
        Some(file_checksum(&path)?)
    } else {
        None
    };
    Ok(RunBundlePath {
        path: relative.to_owned(),
        present: true,
        bytes: metadata.is_file().then_some(metadata.len()),
        checksum,
    })
}

fn debug_export_receipt(receipt: DebugExportReceipt) -> DebugExportManifestReceipt {
    DebugExportManifestReceipt {
        session_count: receipt.session_count,
        task_count: receipt.task_count,
        artifact_count: receipt.artifact_count,
        complete: receipt.complete,
        manifest_checksum: receipt.manifest_checksum,
    }
}

fn validate_run_root(root: &Path) -> Result<(), ClientError> {
    if !root.is_absolute() {
        return Err(ClientError::Io(format!(
            "run bundle destination must be absolute: {}",
            root.display()
        )));
    }
    match fs::symlink_metadata(root) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(ClientError::Io(format!(
            "run bundle destination cannot be a symbolic link: {}",
            root.display()
        ))),
        Ok(metadata) if metadata.is_dir() => Ok(()),
        Ok(_) => Err(ClientError::Io(format!(
            "run bundle destination is not a directory: {}",
            root.display()
        ))),
        Err(error) => Err(bundle_io(root, error)),
    }
}

fn ensure_new_directory_destination(path: &Path, label: &str) -> Result<(), ClientError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(ClientError::Io(format!(
            "run bundle {label} destination cannot be a symbolic link: {}",
            path.display()
        ))),
        Ok(_) => Err(ClientError::Io(format!(
            "run bundle {label} destination already exists: {}",
            path.display()
        ))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(bundle_io(path, error)),
    }
}

fn create_private_dir(path: &Path) -> Result<(), ClientError> {
    fs::create_dir_all(path).map_err(|error| bundle_io(path, error))?;
    set_owner_only_dir(path)
}

fn write_json_file<T: Serialize>(
    path: &Path,
    value: &T,
    bundle_path: String,
) -> Result<RunBundleFile, ClientError> {
    let bytes = serde_json::to_vec_pretty(value)?;
    let file = write_new_file(path, &bytes)?;
    Ok(RunBundleFile {
        path: bundle_path,
        ..file
    })
}

fn write_json_lines_file<T: Serialize>(
    path: &Path,
    values: &[T],
    bundle_path: String,
) -> Result<RunBundleFile, ClientError> {
    let mut bytes = Vec::new();
    for value in values {
        serde_json::to_writer(&mut bytes, value)?;
        bytes.push(b'\n');
    }
    let file = write_new_file(path, &bytes)?;
    Ok(RunBundleFile {
        path: bundle_path,
        ..file
    })
}

fn write_new_file(path: &Path, bytes: &[u8]) -> Result<RunBundleFile, ClientError> {
    if fs::symlink_metadata(path).is_ok() {
        return Err(ClientError::Io(format!(
            "run bundle file already exists: {}",
            path.display()
        )));
    }
    let mut file = File::options()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| bundle_io(path, error))?;
    set_owner_only_file(path)?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|error| bundle_io(path, error))?;
    Ok(RunBundleFile {
        path: String::new(),
        bytes: bytes.len() as u64,
        checksum: checksum(bytes),
    })
}

fn checksum(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

fn file_checksum(path: &Path) -> Result<String, ClientError> {
    let mut file = File::open(path).map_err(|error| bundle_io(path, error))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = file
            .read(&mut buffer)
            .map_err(|error| bundle_io(path, error))?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(format!("sha256:{:x}", hasher.finalize()))
}

#[cfg(unix)]
fn set_owner_only_dir(path: &Path) -> Result<(), ClientError> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(|error| bundle_io(path, error))
}

#[cfg(not(unix))]
fn set_owner_only_dir(_path: &Path) -> Result<(), ClientError> {
    Ok(())
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<(), ClientError> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| bundle_io(path, error))
}

#[cfg(unix)]
fn sync_tree(root: &Path) -> Result<(), ClientError> {
    let mut directories = walkdir::WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| ClientError::Io(format!("{}: {error}", root.display())))?
        .into_iter()
        .filter(|entry| entry.file_type().is_dir())
        .map(|entry| entry.into_path())
        .collect::<Vec<_>>();
    directories.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
    for directory in directories {
        sync_directory(&directory)?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> Result<(), ClientError> {
    Ok(())
}

#[cfg(not(unix))]
fn sync_tree(_root: &Path) -> Result<(), ClientError> {
    Ok(())
}

fn bundle_io(path: &Path, error: std::io::Error) -> ClientError {
    ClientError::Io(format!("{}: {error}", path.display()))
}
