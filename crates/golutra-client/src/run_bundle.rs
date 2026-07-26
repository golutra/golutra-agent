//! Owner-only run bundle export for unattended `golutra exec` invocations.
//!
//! The raw runtime store remains in `state/`. This module writes a stable,
//! machine-readable observation projection next to it, plus an optional
//! redacted handoff export. It never copies provider configuration or
//! credentials into the bundle.

use std::{
    fs::{self, File},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
};

use chrono::{DateTime, Utc};
use golutra_core::{SessionId, TaskId};
use golutra_protocol::{AgentTurnResult, RuntimeEvent, SessionWindowRequest, TaskTracePage};
use golutra_store::RuntimeStore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{
    ClientError, DebugExportCoordinator, DebugExportReceipt, ObservedSession,
    RuntimeObservationCollector, RuntimeObservationSnapshot, RuntimeTransport, set_owner_only_file,
};

const RUN_BUNDLE_FORMAT_VERSION: u32 = 2;
const OBSERVATION_FORMAT_VERSION: u32 = 1;
const MAX_PRIOR_TRACE_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq)]
pub struct RunBundleExportRequest {
    /// The run directory. It already exists and contains `state/` because the
    /// persistent ephemeral runtime created it before executing the turn.
    pub destination: PathBuf,
    pub selection: SessionWindowRequest,
    pub terminal_outcome: RunBundleTerminalOutcome,
}

#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RunBundleTerminalOutcome {
    /// A durable checkpoint written before the turn reaches a terminal state.
    InProgress {
        reason: String,
    },
    Result {
        result: AgentTurnResult,
    },
    Error {
        error: String,
    },
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
        let prior_manifest = load_existing_manifest(&request.destination)?;
        self.export_with_mode(request, prior_manifest, true).await
    }

    /// Persist a recoverable snapshot while an exec turn is still running.
    ///
    /// The runtime database is durable before this method is called, but an
    /// external harness can terminate the CLI before the normal terminal
    /// export runs.  Keeping this checkpoint append-only gives a later
    /// evaluator process enough identity and event boundaries to reopen the
    /// run and finish the export.
    pub async fn checkpoint(
        &self,
        request: RunBundleExportRequest,
    ) -> Result<RunBundleReceipt, ClientError> {
        let prior_manifest = load_existing_manifest(&request.destination)?;
        self.export_with_mode(request, prior_manifest, false).await
    }

    /// Rebuild derived observations after an evaluator overlay was appended
    /// to a completed run. Raw state is retained and directory replacement is
    /// staged so an interrupted refresh remains recoverable.
    pub async fn refresh(
        &self,
        destination: impl AsRef<Path>,
    ) -> Result<RunBundleReceipt, ClientError> {
        let destination = destination.as_ref().to_path_buf();
        validate_run_root(&destination)?;
        let manifest_path = destination.join("manifest.json");
        let manifest: RunBundleManifest = serde_json::from_slice(
            &fs::read(&manifest_path).map_err(|error| bundle_io(&manifest_path, error))?,
        )?;
        self.export_with_mode(
            RunBundleExportRequest {
                destination,
                selection: manifest.selection.clone(),
                terminal_outcome: manifest.terminal_outcome.clone(),
            },
            Some(manifest),
            true,
        )
        .await
    }

    async fn export_with_mode(
        &self,
        request: RunBundleExportRequest,
        prior_manifest: Option<RunBundleManifest>,
        wait_for_evaluation: bool,
    ) -> Result<RunBundleReceipt, ClientError> {
        let replace_existing = prior_manifest.is_some();
        validate_run_root(&request.destination)?;
        if replace_existing {
            recover_stale_directory_swap(&request.destination.join("observations"))?;
            recover_stale_directory_swap(&request.destination.join("debug-export"))?;
        }
        let collector = RuntimeObservationCollector::new(self.transport);
        let snapshot = if wait_for_evaluation {
            collector.collect_settled(request.selection.clone()).await?
        } else {
            collector.collect(request.selection.clone()).await?
        };
        if let Some(prior_manifest) = &prior_manifest {
            validate_append_only_refresh(&request.destination, prior_manifest, &snapshot)?;
        }
        let observations = write_observations(&request.destination, &snapshot, replace_existing)?;

        let debug_export_path = request.destination.join("debug-export");
        let debug_staging_path = replace_existing.then(|| {
            request
                .destination
                .join(format!(".debug-export-refresh-{}", uuid::Uuid::now_v7()))
        });
        let debug_destination = debug_staging_path
            .as_ref()
            .unwrap_or(&debug_export_path)
            .clone();
        let debug_export = match DebugExportCoordinator::new(self.transport)
            .export_snapshot(snapshot.clone(), debug_destination.clone())
            .await
        {
            Ok(receipt) => {
                if replace_existing
                    && let Err(error) = swap_directory(&debug_destination, &debug_export_path)
                {
                    let _ = remove_path_if_present(&debug_destination);
                    return Err(error);
                }
                DebugExportOutcome::Exported {
                    path: "debug-export".to_owned(),
                    receipt: debug_export_receipt(receipt),
                }
            }
            Err(error) => {
                if replace_existing {
                    let _ = remove_path_if_present(&debug_destination);
                }
                DebugExportOutcome::Failed {
                    path: "debug-export".to_owned(),
                    error: error.to_string(),
                }
            }
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
        if replace_existing {
            write_atomic_file(&manifest_path, &manifest_bytes)?;
        } else {
            write_new_file(&manifest_path, &manifest_bytes)?;
        }
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

fn load_existing_manifest(destination: &Path) -> Result<Option<RunBundleManifest>, ClientError> {
    let path = destination.join("manifest.json");
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(bundle_io(&path, error)),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(ClientError::Io(format!(
            "run bundle manifest is not a regular file: {}",
            path.display()
        )));
    }
    if metadata.len() > 4 * 1024 * 1024 {
        return Err(ClientError::Io(format!(
            "run bundle manifest is too large: {}",
            path.display()
        )));
    }
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) => return Err(bundle_io(&path, error)),
    };
    let manifest = serde_json::from_slice(&bytes)
        .map_err(|error| ClientError::Io(format!("{}: {error}", path.display())))?;
    Ok(Some(manifest))
}

fn write_observations(
    run_root: &Path,
    snapshot: &RuntimeObservationSnapshot,
    replace_existing: bool,
) -> Result<ObservationBundleManifest, ClientError> {
    let destination = run_root.join("observations");
    if !replace_existing {
        ensure_new_directory_destination(&destination, "observations")?;
    }
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
    if replace_existing {
        if let Err(error) = swap_directory(&staging_path, &destination) {
            let _ = remove_path_if_present(&staging_path);
            return Err(error);
        }
    } else if let Err(error) = fs::rename(&staging_path, &destination) {
        let _ = fs::remove_dir_all(&staging_path);
        return Err(bundle_io(&destination, error));
    }
    sync_directory(run_root)?;
    Ok(manifest)
}

fn swap_directory(staging: &Path, destination: &Path) -> Result<(), ClientError> {
    let backup = destination.with_file_name(format!(
        ".{}-backup-{}",
        destination
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("run-bundle"),
        uuid::Uuid::now_v7()
    ));
    let had_destination = match fs::symlink_metadata(destination) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(ClientError::Io(format!(
                "run bundle replacement target is not a real directory: {}",
                destination.display()
            )));
        }
        Ok(_) => {
            fs::rename(destination, &backup).map_err(|error| bundle_io(destination, error))?;
            true
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(bundle_io(destination, error)),
    };
    if let Err(error) = fs::rename(staging, destination) {
        if had_destination && let Err(restore_error) = fs::rename(&backup, destination) {
            return Err(ClientError::Io(format!(
                "{}: {error}; failed to restore {}: {restore_error}",
                destination.display(),
                backup.display()
            )));
        }
        return Err(bundle_io(destination, error));
    }
    if had_destination {
        // Cleanup is best effort. A later refresh repairs a leftover backup;
        // the new destination is already durable and must remain usable.
        let _ = fs::remove_dir_all(&backup);
    }
    Ok(())
}

fn write_atomic_file(path: &Path, bytes: &[u8]) -> Result<(), ClientError> {
    let temporary = path.with_file_name(format!(
        ".{}-{}-tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("manifest"),
        uuid::Uuid::now_v7()
    ));
    write_new_file(&temporary, bytes)?;
    let backup = path.with_file_name(format!(
        ".{}-backup-{}",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("manifest"),
        uuid::Uuid::now_v7()
    ));
    let had_destination = match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            let _ = fs::remove_file(&temporary);
            return Err(ClientError::Io(format!(
                "atomic file target is not a regular file: {}",
                path.display()
            )));
        }
        Ok(_) => {
            fs::rename(path, &backup).map_err(|error| bundle_io(path, error))?;
            true
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(bundle_io(path, error)),
    };
    if let Err(error) = fs::rename(&temporary, path) {
        if had_destination {
            let _ = fs::rename(&backup, path);
        }
        let _ = fs::remove_file(&temporary);
        return Err(bundle_io(path, error));
    }
    if had_destination {
        let _ = fs::remove_file(&backup);
    }
    Ok(())
}

fn validate_append_only_refresh(
    run_root: &Path,
    prior_manifest: &RunBundleManifest,
    snapshot: &RuntimeObservationSnapshot,
) -> Result<(), ClientError> {
    validate_raw_state_layout(run_root, &prior_manifest.raw_state)?;
    let mut verified_tasks = 0_usize;
    for prior_session in &prior_manifest.observations.sessions {
        let current_session = snapshot
            .sessions
            .iter()
            .find(|session| session.summary.session_id.to_string() == prior_session.session_id)
            .ok_or_else(|| {
                ClientError::Io(format!(
                    "run bundle refresh lost prior session {}",
                    prior_session.session_id
                ))
            })?;
        for prior_task in &prior_session.tasks {
            let prior_trace = read_prior_trace(
                run_root,
                &prior_manifest.observations,
                &prior_task.trace_path,
            )?;
            let current_task = current_session
                .tasks
                .iter()
                .find(|task| task.task_id.to_string() == prior_task.task_id)
                .ok_or_else(|| {
                    ClientError::Io(format!(
                        "run bundle refresh lost prior task {}",
                        prior_task.task_id
                    ))
                })?;
            if current_task.trace.runtime_identity != prior_trace.runtime_identity {
                return Err(ClientError::Io(format!(
                    "run bundle task {} runtime identity changed during refresh",
                    prior_task.task_id
                )));
            }
            let Some(last_sequence) = prior_trace.integrity.last_sequence else {
                return Err(ClientError::Io(format!(
                    "run bundle task {} has no prior event boundary",
                    prior_task.task_id
                )));
            };
            let prefix = current_session
                .events
                .iter()
                .filter(|event| {
                    event.task_id == Some(current_task.task_id)
                        && event.sequence_no <= last_sequence
                })
                .collect::<Vec<_>>();
            let event_count = u64::try_from(prefix.len()).unwrap_or(u64::MAX);
            let first_sequence = prefix.first().map(|event| event.sequence_no);
            let observed_last_sequence = prefix.last().map(|event| event.sequence_no);
            let observed_digest = event_prefix_digest(&prefix)?;
            if event_count != prior_trace.integrity.event_count
                || first_sequence != prior_trace.integrity.first_sequence
                || observed_last_sequence != prior_trace.integrity.last_sequence
                || observed_digest != prior_trace.integrity.event_chain_digest
            {
                return Err(ClientError::Io(format!(
                    "run bundle task {} changed before its prior event boundary",
                    prior_task.task_id
                )));
            }
            verified_tasks = verified_tasks.saturating_add(1);
        }
    }
    if verified_tasks == 0 {
        return Err(ClientError::Io(
            "run bundle refresh has no prior task trace to validate".to_owned(),
        ));
    }
    Ok(())
}

/// Validate the immutable task prefix and its referenced artifact blobs before
/// a persisted run is exposed to status, diagnosis, or evaluator commands.
/// Evaluator events may be appended after the exported boundary; source facts
/// at or before that boundary must continue to match the checksummed trace.
pub(crate) async fn validate_persisted_run_store(
    run_root: &Path,
    manifest: &RunBundleManifest,
    store: &RuntimeStore,
) -> Result<(), ClientError> {
    validate_raw_state_layout(run_root, &manifest.raw_state)?;
    if manifest.observations.format != "golutra-runtime-observation"
        || manifest.observations.format_version != OBSERVATION_FORMAT_VERSION
        || manifest.observations.path != "observations"
    {
        return Err(ClientError::Io(
            "persisted run observation manifest format is unsupported".to_owned(),
        ));
    }

    let mut seen_sessions = std::collections::HashSet::new();
    let mut seen_tasks = std::collections::HashSet::new();
    for session in &manifest.observations.sessions {
        let session_id = parse_bundle_id::<SessionId>("session", &session.session_id)?;
        if !seen_sessions.insert(session_id) {
            return Err(ClientError::Io(format!(
                "persisted run contains duplicate session {session_id}"
            )));
        }
        for task in &session.tasks {
            let task_id = parse_bundle_id::<TaskId>("task", &task.task_id)?;
            if !seen_tasks.insert((session_id, task_id)) {
                return Err(ClientError::Io(format!(
                    "persisted run contains duplicate task {task_id} in session {session_id}"
                )));
            }
            let prior_trace = read_prior_trace(run_root, &manifest.observations, &task.trace_path)?;
            if prior_trace.session_id != session_id || prior_trace.task_id != task_id {
                return Err(ClientError::Io(format!(
                    "persisted run trace identity does not match task {task_id}"
                )));
            }
            validate_persisted_task_prefix(store, session_id, task_id, &prior_trace).await?;
            validate_persisted_artifacts(store, session_id, task_id, &prior_trace).await?;
        }
    }
    Ok(())
}

fn parse_bundle_id<T>(label: &str, value: &str) -> Result<T, ClientError>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    value.parse::<T>().map_err(|error| {
        ClientError::Io(format!(
            "persisted run {label} id `{value}` is invalid: {error}"
        ))
    })
}

async fn validate_persisted_task_prefix(
    store: &RuntimeStore,
    session_id: SessionId,
    task_id: TaskId,
    prior_trace: &TaskTracePage,
) -> Result<(), ClientError> {
    let Some(last_sequence) = prior_trace.integrity.last_sequence else {
        return Err(ClientError::Io(format!(
            "persisted run task {task_id} has no event boundary"
        )));
    };
    let events = store.load_events(session_id, Some(task_id), None).await?;
    let prefix = events
        .iter()
        .filter(|event| event.sequence_no <= last_sequence)
        .collect::<Vec<_>>();
    let event_count = u64::try_from(prefix.len()).unwrap_or(u64::MAX);
    let first_sequence = prefix.first().map(|event| event.sequence_no);
    let observed_last_sequence = prefix.last().map(|event| event.sequence_no);
    if event_count != prior_trace.integrity.event_count
        || first_sequence != prior_trace.integrity.first_sequence
        || observed_last_sequence != prior_trace.integrity.last_sequence
        || event_prefix_digest(&prefix)? != prior_trace.integrity.event_chain_digest
    {
        return Err(ClientError::Io(format!(
            "persisted run task {task_id} source event prefix failed integrity validation"
        )));
    }
    Ok(())
}

async fn validate_persisted_artifacts(
    store: &RuntimeStore,
    session_id: SessionId,
    task_id: TaskId,
    prior_trace: &TaskTracePage,
) -> Result<(), ClientError> {
    for expected in &prior_trace.artifacts {
        if expected.session_id != session_id {
            return Err(ClientError::Io(format!(
                "persisted run task {task_id} references a foreign-session artifact {}",
                expected.artifact_id
            )));
        }
        let artifact_id = expected.artifact_id;
        let actual = store.load_artifact(artifact_id).await?.ok_or_else(|| {
            ClientError::Io(format!(
                "persisted run task {task_id} is missing artifact {artifact_id}"
            ))
        })?;
        if actual != *expected {
            return Err(ClientError::Io(format!(
                "persisted run artifact {artifact_id} metadata failed integrity validation"
            )));
        }
        match store.load_artifact_bytes(artifact_id).await {
            Ok(Some(_)) => {}
            Ok(None)
                if prior_trace
                    .integrity
                    .retention_losses
                    .iter()
                    .any(|loss| loss == &format!("artifact_blob:{artifact_id}")) => {}
            Ok(None) => {
                return Err(ClientError::Io(format!(
                    "persisted run artifact {artifact_id} blob is missing"
                )));
            }
            Err(error) => {
                return Err(ClientError::Io(format!(
                    "persisted run artifact {artifact_id} failed integrity validation: {error}"
                )));
            }
        }
    }
    Ok(())
}

fn validate_raw_state_layout(
    run_root: &Path,
    raw_state: &RawStateManifest,
) -> Result<(), ClientError> {
    if !raw_state.runtime_database.present
        || raw_state.runtime_database.path != "state/runtime.sqlite"
    {
        return Err(ClientError::Io(
            "run bundle has no canonical state/runtime.sqlite database".to_owned(),
        ));
    }
    let path = run_root.join(&raw_state.runtime_database.path);
    let metadata = fs::symlink_metadata(&path).map_err(|error| bundle_io(&path, error))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(ClientError::Io(format!(
            "run bundle runtime database is not a regular file: {}",
            path.display()
        )));
    }
    Ok(())
}

fn read_prior_trace(
    run_root: &Path,
    observations: &ObservationBundleManifest,
    relative: &str,
) -> Result<TaskTracePage, ClientError> {
    let relative_path = Path::new(relative);
    if relative_path.is_absolute()
        || relative_path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
        || !relative_path.starts_with("observations")
    {
        return Err(ClientError::Io(format!(
            "run bundle trace path is invalid: {}",
            relative_path.display()
        )));
    }
    let path = run_root.join(relative_path);
    validate_real_path_components(run_root, relative_path)?;
    let metadata = fs::symlink_metadata(&path).map_err(|error| bundle_io(&path, error))?;
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.len() > MAX_PRIOR_TRACE_BYTES
    {
        return Err(ClientError::Io(format!(
            "run bundle prior trace is not a bounded regular file: {}",
            path.display()
        )));
    }
    let mut matching_files = observations
        .files
        .iter()
        .filter(|file| file.path == relative);
    let expected = matching_files.next().ok_or_else(|| {
        ClientError::Io(format!(
            "run bundle trace is not listed in the observation manifest: {relative}"
        ))
    })?;
    if matching_files.next().is_some() {
        return Err(ClientError::Io(format!(
            "run bundle trace has duplicate observation manifest entries: {relative}"
        )));
    }
    let bytes = fs::read(&path).map_err(|error| bundle_io(&path, error))?;
    if metadata.len() != expected.bytes || checksum(&bytes) != expected.checksum {
        return Err(ClientError::Io(format!(
            "run bundle trace failed observation manifest integrity validation: {relative}"
        )));
    }
    serde_json::from_slice(&bytes).map_err(ClientError::from)
}

fn validate_real_path_components(root: &Path, relative: &Path) -> Result<(), ClientError> {
    let mut current = root.to_path_buf();
    let component_count = relative.components().count();
    for (index, component) in relative.components().enumerate() {
        let Component::Normal(component) = component else {
            return Err(ClientError::Io(format!(
                "run bundle path is not normalized: {}",
                relative.display()
            )));
        };
        current.push(component);
        let metadata =
            fs::symlink_metadata(&current).map_err(|error| bundle_io(&current, error))?;
        if metadata.file_type().is_symlink() {
            return Err(ClientError::Io(format!(
                "run bundle path cannot traverse a symbolic link: {}",
                current.display()
            )));
        }
        if index + 1 < component_count && !metadata.is_dir() {
            return Err(ClientError::Io(format!(
                "run bundle path component is not a directory: {}",
                current.display()
            )));
        }
    }
    Ok(())
}

fn event_prefix_digest(events: &[&RuntimeEvent]) -> Result<String, ClientError> {
    let mut digest = Sha256::new();
    for event in events {
        digest.update(event.sequence_no.to_be_bytes());
        digest.update(serde_json::to_vec(event)?);
    }
    Ok(format!("sha256:{:x}", digest.finalize()))
}

fn recover_stale_directory_swap(destination: &Path) -> Result<(), ClientError> {
    let Some(parent) = destination.parent() else {
        return Ok(());
    };
    let Some(base) = destination.file_name().and_then(|value| value.to_str()) else {
        return Ok(());
    };
    let backup_prefix = format!(".{base}-backup-");
    let staging_prefix = format!(".{base}-refresh-");
    let temporary_prefix = format!(".golutra-{base}-");
    let mut backups = Vec::new();
    let mut staging = Vec::new();
    for entry in fs::read_dir(parent).map_err(|error| bundle_io(parent, error))? {
        let entry = entry.map_err(|error| bundle_io(parent, error))?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with(&backup_prefix) {
            backups.push(entry.path());
        } else if name.starts_with(&staging_prefix) || name.starts_with(&temporary_prefix) {
            staging.push(entry.path());
        }
    }
    backups.sort();
    staging.sort();
    let destination_present = fs::symlink_metadata(destination).is_ok();
    if !destination_present && let Some(recovery) = backups.pop() {
        fs::rename(&recovery, destination).map_err(|error| bundle_io(destination, error))?;
    }
    for path in backups.into_iter().chain(staging) {
        let _ = remove_path_if_present(&path);
    }
    Ok(())
}

fn remove_path_if_present(path: &Path) -> Result<(), ClientError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(bundle_io(path, error)),
    };
    if metadata.file_type().is_symlink() {
        return Err(ClientError::Io(format!(
            "refusing to remove symbolic link in run bundle: {}",
            path.display()
        )));
    }
    if metadata.is_dir() {
        fs::remove_dir_all(path).map_err(|error| bundle_io(path, error))
    } else {
        fs::remove_file(path).map_err(|error| bundle_io(path, error))
    }
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

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn refresh_requires_the_canonical_raw_runtime_database_layout() {
        let root = tempdir().expect("run root");
        let state = root.path().join("state");
        fs::create_dir_all(&state).expect("state");
        let database = state.join("runtime.sqlite");
        fs::write(&database, b"runtime-state").expect("database");
        let raw = RawStateManifest {
            path: "state".to_owned(),
            runtime_database: RunBundlePath {
                path: "state/runtime.sqlite".to_owned(),
                present: true,
                bytes: Some(13),
                checksum: Some(file_checksum(&database).expect("checksum")),
            },
            artifacts: RunBundlePath {
                path: "state/artifacts".to_owned(),
                present: false,
                bytes: None,
                checksum: None,
            },
            workspaces: RunBundlePath {
                path: "state/workspaces".to_owned(),
                present: false,
                bytes: None,
                checksum: None,
            },
        };
        validate_raw_state_layout(root.path(), &raw).expect("valid database");
        fs::remove_file(&database).expect("remove database");
        fs::create_dir(&database).expect("replace with directory");
        assert!(validate_raw_state_layout(root.path(), &raw).is_err());
    }

    #[test]
    fn stale_directory_swap_entries_are_recovered_and_cleaned() {
        let root = tempdir().expect("run root");
        let destination = root.path().join("observations");
        let backup = root.path().join(".observations-backup-old");
        let staging = root.path().join(".observations-refresh-old");
        fs::create_dir_all(&backup).expect("backup");
        fs::create_dir_all(&staging).expect("staging");
        recover_stale_directory_swap(&destination).expect("recover");
        assert!(destination.is_dir());
        assert!(!backup.exists());
        assert!(!staging.exists());

        let second_backup = root.path().join(".observations-backup-second");
        let second_staging = root.path().join(".observations-refresh-second");
        fs::create_dir_all(&second_backup).expect("second backup");
        fs::create_dir_all(&second_staging).expect("second staging");
        recover_stale_directory_swap(&destination).expect("clean");
        assert!(destination.is_dir());
        assert!(!second_backup.exists());
        assert!(!second_staging.exists());
    }
}
