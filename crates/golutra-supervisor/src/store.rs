use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
};

use chrono::Utc;
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use thiserror::Error;
use uuid::Uuid;
use walkdir::{DirEntry, WalkDir};

use crate::model::{ControlEvent, SupervisorState};

const MAX_STATE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_PENDING_TRANSACTION_BYTES: u64 = MAX_STATE_BYTES + 1024 * 1024;
const MAX_CONTROL_EVENT_BYTES: usize = 128 * 1024;
const MAX_CANDIDATE_FILES: usize = 100_000;
const MAX_CANDIDATE_BYTES: u64 = 512 * 1024 * 1024;
const MAX_SUPERVISOR_ARTIFACT_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, Error)]
pub enum SupervisorError {
    #[error("supervisor IO failed: {0}")]
    Io(String),
    #[error("supervisor JSON failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("supervisor input is invalid: {0}")]
    Invalid(String),
    #[error("supervisor object was not found: {0}")]
    NotFound(String),
    #[error("supervisor budget exhausted: {0}")]
    BudgetExhausted(String),
    #[error("supervisor state transition is invalid: {0}")]
    InvalidTransition(String),
    #[error("supervisor gate rejected candidate: {0}")]
    GateRejected(String),
    #[error("supervisor control log integrity failed: {0}")]
    Integrity(String),
    #[error("supervisor release failed: {0}")]
    Release(#[from] golutra_release::ReleaseError),
    #[error("supervisor producer failed: {0}")]
    Producer(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SupervisorPaths {
    pub root: PathBuf,
    pub state_path: PathBuf,
    pub control_log_path: PathBuf,
    pub pending_transaction_path: PathBuf,
    pub worktrees_root: PathBuf,
    pub artifacts_root: PathBuf,
    pub lock_path: PathBuf,
}

impl SupervisorPaths {
    pub fn from_root(root: impl Into<PathBuf>) -> Result<Self, SupervisorError> {
        let root = root.into();
        ensure_private_dir(&root)?;
        let worktrees_root = root.join("worktrees");
        let artifacts_root = root.join("artifacts");
        ensure_private_dir(&worktrees_root)?;
        ensure_private_dir(&artifacts_root)?;
        let paths = Self {
            state_path: root.join("state.json"),
            control_log_path: root.join("control.jsonl"),
            pending_transaction_path: root.join("pending.json"),
            lock_path: root.join("supervisor.lock"),
            root,
            worktrees_root,
            artifacts_root,
        };
        ensure_private_file(&paths.lock_path)?;
        Ok(paths)
    }
}

#[derive(Debug, Clone)]
pub struct SupervisorStore {
    paths: SupervisorPaths,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PendingTransaction {
    next_state: SupervisorState,
    state_digest: String,
    event: ControlEvent,
}

impl SupervisorStore {
    pub fn new(root: impl Into<PathBuf>) -> Result<Self, SupervisorError> {
        Ok(Self {
            paths: SupervisorPaths::from_root(root)?,
        })
    }

    #[must_use]
    pub fn paths(&self) -> &SupervisorPaths {
        &self.paths
    }

    pub(crate) fn prepare_candidate_worktree(
        &self,
        candidate_id: &str,
        parent_source: &Path,
    ) -> Result<PathBuf, SupervisorError> {
        let _lock = self.lock()?;
        self.recover_pending_locked()?;
        let destination = self.paths.worktrees_root.join(candidate_id);
        if fs::symlink_metadata(&destination).is_ok() {
            return Err(SupervisorError::InvalidTransition(format!(
                "candidate worktree already exists: {}",
                destination.display()
            )));
        }
        let staging = self
            .paths
            .worktrees_root
            .join(format!(".prepare-{}", Uuid::now_v7()));
        ensure_private_dir(&staging)?;
        let copied = copy_candidate_tree(parent_source, &staging);
        if let Err(error) = copied {
            let _ = fs::remove_dir_all(&staging);
            return Err(error);
        }
        if let Err(error) = fs::rename(&staging, &destination) {
            let _ = fs::remove_dir_all(&staging);
            return Err(SupervisorError::Io(format!(
                "failed to publish candidate worktree {}: {error}",
                destination.display()
            )));
        }
        sync_parent(&destination)?;
        validate_producer_worktree(&self.paths.root, &destination)
    }

    pub fn snapshot(&self) -> Result<SupervisorState, SupervisorError> {
        let _lock = self.lock()?;
        self.recover_pending_locked()?;
        self.verify_control_log_locked()?;
        read_state(&self.paths.state_path)
    }

    pub fn verify_control_log(&self) -> Result<Vec<ControlEvent>, SupervisorError> {
        let _lock = self.lock()?;
        self.recover_pending_locked()?;
        self.verify_control_log_locked()
    }

    pub(crate) fn store_artifact(
        &self,
        namespace: &str,
        bytes: &[u8],
    ) -> Result<String, SupervisorError> {
        if namespace.is_empty()
            || namespace.len() > 64
            || !namespace
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || character == '-')
        {
            return Err(SupervisorError::Invalid(
                "supervisor artifact namespace is invalid".to_owned(),
            ));
        }
        if bytes.len() > MAX_SUPERVISOR_ARTIFACT_BYTES {
            return Err(SupervisorError::Invalid(
                "supervisor artifact exceeds its size limit".to_owned(),
            ));
        }
        let _lock = self.lock()?;
        self.recover_pending_locked()?;
        let digest = format!("sha256:{:x}", Sha256::digest(bytes));
        let directory = self.paths.artifacts_root.join(namespace);
        ensure_private_dir(&directory)?;
        let path = directory.join(format!("{}.json", digest.trim_start_matches("sha256:")));
        if path.exists() {
            let metadata = fs::symlink_metadata(&path)
                .map_err(|error| SupervisorError::Io(format!("{}: {error}", path.display())))?;
            if metadata.file_type().is_symlink()
                || !metadata.is_file()
                || metadata.len() > u64::try_from(MAX_SUPERVISOR_ARTIFACT_BYTES).unwrap_or(u64::MAX)
            {
                return Err(SupervisorError::Integrity(
                    "stored supervisor artifact violates its file boundary".to_owned(),
                ));
            }
            let existing = fs::read(&path)
                .map_err(|error| SupervisorError::Io(format!("{}: {error}", path.display())))?;
            if existing != bytes {
                return Err(SupervisorError::Integrity(
                    "content-addressed supervisor artifact has different bytes".to_owned(),
                ));
            }
        } else {
            write_private_atomic(&path, bytes)?;
        }
        Ok(format!("artifact://supervisor-{namespace}/{digest}"))
    }

    pub(crate) fn verify_artifact(
        &self,
        namespace: &str,
        artifact_ref: &str,
        expected_bytes: &[u8],
    ) -> Result<(), SupervisorError> {
        if namespace.is_empty()
            || namespace.len() > 64
            || !namespace
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || character == '-')
        {
            return Err(SupervisorError::Invalid(
                "supervisor artifact namespace is invalid".to_owned(),
            ));
        }
        if expected_bytes.len() > MAX_SUPERVISOR_ARTIFACT_BYTES {
            return Err(SupervisorError::Invalid(
                "supervisor artifact exceeds its size limit".to_owned(),
            ));
        }
        let digest = format!("sha256:{:x}", Sha256::digest(expected_bytes));
        let expected_ref = format!("artifact://supervisor-{namespace}/{digest}");
        if artifact_ref != expected_ref {
            return Err(SupervisorError::Integrity(
                "supervisor artifact reference does not match its content".to_owned(),
            ));
        }
        let path = self
            .paths
            .artifacts_root
            .join(namespace)
            .join(format!("{}.json", digest.trim_start_matches("sha256:")));
        let metadata = fs::symlink_metadata(&path)
            .map_err(|error| SupervisorError::Io(format!("{}: {error}", path.display())))?;
        if metadata.file_type().is_symlink()
            || !metadata.is_file()
            || metadata.len() != u64::try_from(expected_bytes.len()).unwrap_or(u64::MAX)
        {
            return Err(SupervisorError::Integrity(
                "supervisor artifact violates its file boundary".to_owned(),
            ));
        }
        let actual = fs::read(&path)
            .map_err(|error| SupervisorError::Io(format!("{}: {error}", path.display())))?;
        if actual != expected_bytes {
            return Err(SupervisorError::Integrity(
                "supervisor artifact content does not match its reference".to_owned(),
            ));
        }
        Ok(())
    }

    pub(crate) fn transact<T>(
        &self,
        event_type: &str,
        epoch_id: Option<&str>,
        candidate_id: Option<&str>,
        payload: Value,
        operation: impl FnOnce(&mut SupervisorState) -> Result<T, SupervisorError>,
    ) -> Result<T, SupervisorError> {
        if event_type.trim().is_empty() {
            return Err(SupervisorError::Invalid(
                "control event type is required".to_owned(),
            ));
        }
        let _lock = self.lock()?;
        self.recover_pending_locked()?;
        let events = self.verify_control_log_locked()?;
        let mut state = read_state(&self.paths.state_path)?;
        let result = operation(&mut state)?;
        let event =
            self.prepare_control_event(&events, event_type, epoch_id, candidate_id, payload)?;
        let pending = PendingTransaction {
            state_digest: supervisor_state_digest(&state)?,
            next_state: state,
            event,
        };
        self.write_pending_locked(&pending)?;
        self.append_prepared_control_locked(&pending.event, &events)?;
        write_state(&self.paths.state_path, &pending.next_state)?;
        self.remove_pending_locked()?;
        Ok(result)
    }

    fn lock(&self) -> Result<File, SupervisorError> {
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&self.paths.lock_path)
            .map_err(|error| SupervisorError::Io(error.to_string()))?;
        file.lock_exclusive()
            .map_err(|error| SupervisorError::Io(error.to_string()))?;
        Ok(file)
    }

    fn verify_control_log_locked(&self) -> Result<Vec<ControlEvent>, SupervisorError> {
        if !self.paths.control_log_path.exists() {
            return Ok(Vec::new());
        }
        let metadata = fs::symlink_metadata(&self.paths.control_log_path)
            .map_err(|error| SupervisorError::Io(error.to_string()))?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(SupervisorError::Integrity(
                "control log is not a regular file".to_owned(),
            ));
        }
        if metadata.len() > MAX_STATE_BYTES {
            return Err(SupervisorError::Integrity(
                "control log exceeds its size limit".to_owned(),
            ));
        }
        let content = fs::read_to_string(&self.paths.control_log_path)
            .map_err(|error| SupervisorError::Io(error.to_string()))?;
        verify_control_log_content(&content)
    }

    fn recover_interrupted_append_locked(
        &self,
        pending: &PendingTransaction,
        original_error: SupervisorError,
    ) -> Result<Vec<ControlEvent>, SupervisorError> {
        let bytes = fs::read(&self.paths.control_log_path)
            .map_err(|error| SupervisorError::Io(error.to_string()))?;
        let prefix_len = bytes
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(0, |index| index.saturating_add(1));
        let trailing = &bytes[prefix_len..];
        let pending_line = serde_json::to_vec(&pending.event)?;
        if trailing.is_empty() || !pending_line.starts_with(trailing) {
            return Err(original_error);
        }
        let Ok(prefix) = std::str::from_utf8(&bytes[..prefix_len]) else {
            return Err(original_error);
        };
        let events = verify_control_log_content(prefix)?;
        let expected_sequence = u64::try_from(events.len())
            .unwrap_or(u64::MAX)
            .saturating_add(1);
        let expected_previous = events
            .last()
            .map_or_else(String::new, |event| event.digest.clone());
        if pending.event.sequence != expected_sequence
            || pending.event.previous_digest != expected_previous
        {
            return Err(original_error);
        }
        let file = OpenOptions::new()
            .write(true)
            .open(&self.paths.control_log_path)
            .map_err(|error| SupervisorError::Io(error.to_string()))?;
        file.set_len(u64::try_from(prefix_len).unwrap_or(u64::MAX))
            .and_then(|_| file.sync_all())
            .map_err(|error| SupervisorError::Io(error.to_string()))?;
        Ok(events)
    }

    fn prepare_control_event(
        &self,
        events: &[ControlEvent],
        event_type: &str,
        epoch_id: Option<&str>,
        candidate_id: Option<&str>,
        payload: Value,
    ) -> Result<ControlEvent, SupervisorError> {
        let previous_digest = events
            .last()
            .map_or_else(String::new, |event| event.digest.clone());
        let mut event = ControlEvent {
            sequence: u64::try_from(events.len())
                .unwrap_or(u64::MAX)
                .saturating_add(1),
            event_type: bounded_text(event_type, 128),
            epoch_id: epoch_id.map(|value| bounded_text(value, 256)),
            candidate_id: candidate_id.map(|value| bounded_text(value, 256)),
            payload: sanitize_payload(payload, 0),
            at: Utc::now(),
            previous_digest,
            digest: String::new(),
        };
        let encoded = serde_json::to_vec(&event)?;
        if encoded.len() > MAX_CONTROL_EVENT_BYTES {
            return Err(SupervisorError::Invalid(
                "control event exceeds its size limit".to_owned(),
            ));
        }
        event.digest = control_event_digest(&event)?;
        if serde_json::to_vec(&event)?.len() > MAX_CONTROL_EVENT_BYTES {
            return Err(SupervisorError::Invalid(
                "control event exceeds its size limit".to_owned(),
            ));
        }
        Ok(event)
    }

    fn append_prepared_control_locked(
        &self,
        event: &ControlEvent,
        events: &[ControlEvent],
    ) -> Result<(), SupervisorError> {
        let expected_sequence = u64::try_from(events.len())
            .unwrap_or(u64::MAX)
            .saturating_add(1);
        let expected_previous = events
            .last()
            .map_or_else(String::new, |event| event.digest.clone());
        if event.sequence != expected_sequence || event.previous_digest != expected_previous {
            return Err(SupervisorError::Integrity(
                "pending control event does not extend the control log".to_owned(),
            ));
        }
        if control_event_digest(event)? != event.digest {
            return Err(SupervisorError::Integrity(
                "pending control event digest is invalid".to_owned(),
            ));
        }
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.paths.control_log_path)
            .map_err(|error| SupervisorError::Io(error.to_string()))?;
        set_owner_only_file(&self.paths.control_log_path)?;
        let line = serde_json::to_vec(&event)?;
        let current_size = file
            .metadata()
            .map_err(|error| SupervisorError::Io(error.to_string()))?
            .len();
        let appended_size = u64::try_from(line.len().saturating_add(1)).unwrap_or(u64::MAX);
        if current_size.saturating_add(appended_size) > MAX_STATE_BYTES {
            return Err(SupervisorError::Invalid(
                "control log exceeds its size limit".to_owned(),
            ));
        }
        file.write_all(&line)
            .and_then(|_| file.write_all(b"\n"))
            .and_then(|_| file.sync_all())
            .map_err(|error| SupervisorError::Io(error.to_string()))?;
        Ok(())
    }

    fn write_pending_locked(&self, pending: &PendingTransaction) -> Result<(), SupervisorError> {
        let bytes = serde_json::to_vec(pending)?;
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_PENDING_TRANSACTION_BYTES {
            return Err(SupervisorError::Invalid(
                "supervisor pending transaction exceeds its size limit".to_owned(),
            ));
        }
        write_private_atomic(&self.paths.pending_transaction_path, &bytes)
    }

    fn recover_pending_locked(&self) -> Result<(), SupervisorError> {
        let Some(pending) = read_pending(&self.paths.pending_transaction_path)? else {
            return Ok(());
        };
        if supervisor_state_digest(&pending.next_state)? != pending.state_digest {
            return Err(SupervisorError::Integrity(
                "pending supervisor state digest is invalid".to_owned(),
            ));
        }
        if control_event_digest(&pending.event)? != pending.event.digest {
            return Err(SupervisorError::Integrity(
                "pending control event digest is invalid".to_owned(),
            ));
        }
        let events = match self.verify_control_log_locked() {
            Ok(events) => events,
            Err(error) => self.recover_interrupted_append_locked(&pending, error)?,
        };
        if events
            .last()
            .is_some_and(|event| event.sequence == pending.event.sequence)
        {
            if events.last() != Some(&pending.event) {
                return Err(SupervisorError::Integrity(
                    "control log conflicts with the pending transaction".to_owned(),
                ));
            }
        } else {
            self.append_prepared_control_locked(&pending.event, &events)?;
        }
        write_state(&self.paths.state_path, &pending.next_state)?;
        self.remove_pending_locked()
    }

    fn remove_pending_locked(&self) -> Result<(), SupervisorError> {
        match fs::remove_file(&self.paths.pending_transaction_path) {
            Ok(()) => sync_parent(&self.paths.pending_transaction_path),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(SupervisorError::Io(error.to_string())),
        }
    }
}

fn copy_candidate_tree(source: &Path, destination: &Path) -> Result<(), SupervisorError> {
    let source = source
        .canonicalize()
        .map_err(|error| SupervisorError::Io(format!("{}: {error}", source.display())))?;
    if !source.is_dir() {
        return Err(SupervisorError::Invalid(format!(
            "candidate parent source is not a directory: {}",
            source.display()
        )));
    }
    let entries = collect_candidate_entries(&source)?;
    let mut total = 0_u64;
    for entry in entries.into_iter().skip(1) {
        let relative = entry
            .path()
            .strip_prefix(&source)
            .map_err(|error| SupervisorError::Io(error.to_string()))?;
        let target = destination.join(relative);
        if entry.file_type().is_dir() {
            ensure_private_dir(&target)?;
            continue;
        }
        if !entry.file_type().is_file() {
            return Err(SupervisorError::GateRejected(format!(
                "candidate parent contains an unsupported entry: {}",
                entry.path().display()
            )));
        }
        let metadata = entry
            .metadata()
            .map_err(|error| SupervisorError::Io(format!("{}: {error}", entry.path().display())))?;
        total = total.saturating_add(metadata.len());
        if total > MAX_CANDIDATE_BYTES {
            return Err(SupervisorError::Invalid(
                "candidate parent source exceeds its size limit".to_owned(),
            ));
        }
        if let Some(parent) = target.parent() {
            ensure_private_dir(parent)?;
        }
        fs::copy(entry.path(), &target).map_err(|error| {
            SupervisorError::Io(format!(
                "failed to copy {} to {}: {error}",
                entry.path().display(),
                target.display()
            ))
        })?;
        set_private_candidate_file_permissions(entry.path(), &target)?;
    }
    Ok(())
}

#[cfg(unix)]
fn set_private_candidate_file_permissions(
    source: &Path,
    destination: &Path,
) -> Result<(), SupervisorError> {
    use std::os::unix::fs::PermissionsExt;

    let source_mode = fs::metadata(source)
        .map_err(|error| SupervisorError::Io(format!("{}: {error}", source.display())))?
        .permissions()
        .mode();
    let owner_mode = source_mode & 0o700;
    let mode = if owner_mode == 0 { 0o600 } else { owner_mode };
    fs::set_permissions(destination, fs::Permissions::from_mode(mode))
        .map_err(|error| SupervisorError::Io(format!("{}: {error}", destination.display())))
}

#[cfg(unix)]
fn candidate_file_is_executable(path: &Path) -> Result<bool, SupervisorError> {
    use std::os::unix::fs::PermissionsExt;

    let mode = fs::symlink_metadata(path)
        .map_err(|error| SupervisorError::Io(format!("{}: {error}", path.display())))?
        .permissions()
        .mode();
    Ok(mode & 0o111 != 0)
}

#[cfg(not(unix))]
fn set_private_candidate_file_permissions(
    _source: &Path,
    _destination: &Path,
) -> Result<(), SupervisorError> {
    Ok(())
}

#[cfg(not(unix))]
fn candidate_file_is_executable(_path: &Path) -> Result<bool, SupervisorError> {
    Ok(false)
}

fn verify_control_log_content(content: &str) -> Result<Vec<ControlEvent>, SupervisorError> {
    let mut previous_digest = String::new();
    let mut events = Vec::new();
    for (index, line) in content.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let event: ControlEvent = serde_json::from_str(line)?;
        let expected_sequence = u64::try_from(events.len())
            .unwrap_or(u64::MAX)
            .saturating_add(1);
        if event.sequence != expected_sequence || event.previous_digest != previous_digest {
            return Err(SupervisorError::Integrity(format!(
                "control log chain is broken at line {}",
                index.saturating_add(1)
            )));
        }
        let digest = control_event_digest(&event)?;
        if digest != event.digest {
            return Err(SupervisorError::Integrity(format!(
                "control log digest mismatch at sequence {}",
                event.sequence
            )));
        }
        previous_digest = event.digest.clone();
        events.push(event);
    }
    Ok(events)
}

pub(crate) fn control_event_digest(event: &ControlEvent) -> Result<String, SupervisorError> {
    let mut unsigned = event.clone();
    unsigned.digest.clear();
    Ok(format!(
        "sha256:{:x}",
        Sha256::digest(serde_json::to_vec(&unsigned)?)
    ))
}

pub(crate) fn read_state(path: &Path) -> Result<SupervisorState, SupervisorError> {
    if !path.exists() {
        return Ok(SupervisorState::default());
    }
    let metadata =
        fs::symlink_metadata(path).map_err(|error| SupervisorError::Io(error.to_string()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(SupervisorError::Integrity(format!(
            "supervisor state is not a regular file: {}",
            path.display()
        )));
    }
    if metadata.len() > MAX_STATE_BYTES {
        return Err(SupervisorError::Integrity(
            "supervisor state exceeds its size limit".to_owned(),
        ));
    }
    let bytes = fs::read(path).map_err(|error| SupervisorError::Io(error.to_string()))?;
    Ok(serde_json::from_slice(&bytes)?)
}

fn read_pending(path: &Path) -> Result<Option<PendingTransaction>, SupervisorError> {
    if !path.exists() {
        return Ok(None);
    }
    let metadata =
        fs::symlink_metadata(path).map_err(|error| SupervisorError::Io(error.to_string()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(SupervisorError::Integrity(
            "pending supervisor transaction is not a regular file".to_owned(),
        ));
    }
    if metadata.len() > MAX_PENDING_TRANSACTION_BYTES {
        return Err(SupervisorError::Integrity(
            "pending supervisor transaction exceeds its size limit".to_owned(),
        ));
    }
    let bytes = fs::read(path).map_err(|error| SupervisorError::Io(error.to_string()))?;
    Ok(Some(serde_json::from_slice(&bytes)?))
}

fn supervisor_state_digest(state: &SupervisorState) -> Result<String, SupervisorError> {
    Ok(format!(
        "sha256:{:x}",
        Sha256::digest(serde_json::to_vec(state)?)
    ))
}

pub(crate) fn write_state(path: &Path, state: &SupervisorState) -> Result<(), SupervisorError> {
    let bytes = serde_json::to_vec_pretty(state)?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_STATE_BYTES {
        return Err(SupervisorError::Invalid(
            "supervisor state exceeds its size limit".to_owned(),
        ));
    }
    write_private_atomic(path, &bytes)
}

pub fn candidate_tree_digest(path: &Path) -> Result<String, SupervisorError> {
    let root = path
        .canonicalize()
        .map_err(|error| SupervisorError::Io(format!("{}: {error}", path.display())))?;
    if !root.is_dir() {
        return Err(SupervisorError::Invalid(format!(
            "candidate worktree is not a directory: {}",
            root.display()
        )));
    }
    let entries = collect_candidate_entries(&root)?;
    let mut digest = Sha256::new();
    let mut total = 0_u64;
    for entry in entries.iter().filter(|entry| entry.file_type().is_file()) {
        let relative = entry
            .path()
            .strip_prefix(&root)
            .map_err(|error| SupervisorError::Io(error.to_string()))?;
        let relative = normalized_candidate_relative_path(relative)?;
        let metadata = entry
            .metadata()
            .map_err(|error| SupervisorError::Io(format!("{}: {error}", entry.path().display())))?;
        total = total.saturating_add(metadata.len());
        if total > MAX_CANDIDATE_BYTES {
            return Err(SupervisorError::Invalid(
                "candidate worktree exceeds its size limit".to_owned(),
            ));
        }
        let file_digest = sha256_file_digest(entry.path(), MAX_CANDIDATE_BYTES)?;
        digest.update(relative.as_bytes());
        digest.update([0]);
        digest.update(file_digest);
    }
    Ok(format!("sha256:{:x}", digest.finalize()))
}

fn sha256_file_digest(path: &Path, max_bytes: u64) -> Result<[u8; 32], SupervisorError> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| SupervisorError::Io(format!("{}: {error}", path.display())))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.len() > max_bytes {
        return Err(SupervisorError::Invalid(format!(
            "candidate file violates its boundary: {}",
            path.display()
        )));
    }
    let mut file = File::open(path)
        .map_err(|error| SupervisorError::Io(format!("{}: {error}", path.display())))?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    let mut total = 0_u64;
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| SupervisorError::Io(format!("{}: {error}", path.display())))?;
        if read == 0 {
            break;
        }
        total = total.saturating_add(u64::try_from(read).unwrap_or(u64::MAX));
        if total > max_bytes {
            return Err(SupervisorError::Invalid(format!(
                "candidate file exceeds its size limit: {}",
                path.display()
            )));
        }
        digest.update(&buffer[..read]);
    }
    if total != metadata.len() {
        return Err(SupervisorError::Integrity(format!(
            "candidate file changed while hashing: {}",
            path.display()
        )));
    }
    Ok(digest.finalize().into())
}

pub fn validate_candidate_worktree(
    supervisor_root: &Path,
    worktree: &Path,
    target_paths: &[String],
) -> Result<PathBuf, SupervisorError> {
    let worktree = validate_producer_worktree(supervisor_root, worktree)?;
    if target_paths.is_empty() {
        return Err(SupervisorError::Invalid(
            "candidate must declare at least one target path".to_owned(),
        ));
    }
    for target in target_paths {
        validate_target_path(target)?;
    }
    Ok(worktree)
}

pub fn validate_candidate_changes(
    parent_source: &Path,
    worktree: &Path,
    declared_target_paths: &[String],
) -> Result<Vec<String>, SupervisorError> {
    if declared_target_paths.is_empty() {
        return Err(SupervisorError::Invalid(
            "candidate must declare at least one target path".to_owned(),
        ));
    }
    for target in declared_target_paths {
        validate_target_path(target)?;
    }

    let parent_files = candidate_file_map(parent_source)?;
    let candidate_files = candidate_file_map(worktree)?;
    let mut changed_paths = parent_files
        .keys()
        .chain(candidate_files.keys())
        .filter(|path| parent_files.get(*path) != candidate_files.get(*path))
        .cloned()
        .collect::<Vec<_>>();
    changed_paths.sort();
    changed_paths.dedup();
    if changed_paths.is_empty() {
        return Err(SupervisorError::GateRejected(
            "candidate contains no source changes relative to its parent release".to_owned(),
        ));
    }

    for changed in &changed_paths {
        validate_target_path(changed)?;
        if !declared_target_paths
            .iter()
            .any(|declared| target_declaration_covers(declared, changed))
        {
            return Err(SupervisorError::GateRejected(format!(
                "candidate changed an undeclared path: {changed}"
            )));
        }
    }
    for declared in declared_target_paths {
        if !changed_paths
            .iter()
            .any(|changed| target_declaration_covers(declared, changed))
        {
            return Err(SupervisorError::GateRejected(format!(
                "candidate declared an unchanged target path: {declared}"
            )));
        }
    }
    Ok(changed_paths)
}

fn candidate_file_map(root: &Path) -> Result<BTreeMap<String, (bool, [u8; 32])>, SupervisorError> {
    let root = root
        .canonicalize()
        .map_err(|error| SupervisorError::Io(format!("{}: {error}", root.display())))?;
    if !root.is_dir() {
        return Err(SupervisorError::Invalid(format!(
            "candidate source is not a directory: {}",
            root.display()
        )));
    }
    collect_candidate_entries(&root)?
        .into_iter()
        .filter(|entry| entry.file_type().is_file())
        .map(|entry| {
            let relative = entry
                .path()
                .strip_prefix(&root)
                .map_err(|error| SupervisorError::Io(error.to_string()))?;
            let relative = normalized_candidate_relative_path(relative)?;
            let digest = sha256_file_digest(entry.path(), MAX_CANDIDATE_BYTES)?;
            let executable = candidate_file_is_executable(entry.path())?;
            Ok((relative, (executable, digest)))
        })
        .collect()
}

fn target_declaration_covers(declared: &str, changed: &str) -> bool {
    let declared = declared.replace('\\', "/");
    let declared = declared.trim_end_matches('/');
    changed == declared || changed.starts_with(&format!("{declared}/"))
}

fn normalized_candidate_relative_path(path: &Path) -> Result<String, SupervisorError> {
    let mut components = Vec::new();
    for component in path.components() {
        let Component::Normal(component) = component else {
            return Err(SupervisorError::GateRejected(format!(
                "candidate contains a non-normal path: {}",
                path.display()
            )));
        };
        let component = component.to_str().ok_or_else(|| {
            SupervisorError::GateRejected(format!(
                "candidate path is not valid UTF-8: {}",
                path.display()
            ))
        })?;
        if component.contains('\\') || component.chars().any(char::is_control) {
            return Err(SupervisorError::GateRejected(format!(
                "candidate path contains ambiguous characters: {}",
                path.display()
            )));
        }
        components.push(component);
    }
    if components.is_empty() {
        return Err(SupervisorError::GateRejected(
            "candidate relative path is empty".to_owned(),
        ));
    }
    Ok(components.join("/"))
}

pub(crate) fn validate_producer_worktree(
    supervisor_root: &Path,
    worktree: &Path,
) -> Result<PathBuf, SupervisorError> {
    let root = supervisor_root
        .canonicalize()
        .map_err(|error| SupervisorError::Io(error.to_string()))?;
    let worktree = worktree
        .canonicalize()
        .map_err(|error| SupervisorError::Io(format!("{}: {error}", worktree.display())))?;
    let worktrees_root = root.join("worktrees").canonicalize().map_err(|error| {
        SupervisorError::Io(format!("{}: {error}", root.join("worktrees").display()))
    })?;
    if worktree == worktrees_root || !worktree.starts_with(&worktrees_root) {
        return Err(SupervisorError::Invalid(
            "candidate worktree must be inside the supervisor worktrees root".to_owned(),
        ));
    }
    collect_candidate_entries(&worktree)?;
    Ok(worktree)
}

fn collect_candidate_entries(root: &Path) -> Result<Vec<DirEntry>, SupervisorError> {
    let mut entries = WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(included_candidate_entry)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| SupervisorError::Io(error.to_string()))?;
    if entries.len() > MAX_CANDIDATE_FILES {
        return Err(SupervisorError::Invalid(
            "candidate worktree exceeds its file-count limit".to_owned(),
        ));
    }
    for entry in &entries {
        if entry.depth() > 0 && entry.file_type().is_symlink() {
            return Err(SupervisorError::GateRejected(format!(
                "candidate contains symlink: {}",
                entry.path().display()
            )));
        }
    }
    entries.sort_by_key(|entry| entry.path().to_path_buf());
    Ok(entries)
}

fn included_candidate_entry(entry: &DirEntry) -> bool {
    if entry.depth() == 0 {
        return true;
    }
    if entry.file_type().is_symlink() {
        return true;
    }
    !matches!(
        entry.file_name().to_str(),
        Some(".git" | ".golutra" | "target" | "node_modules")
    )
}

pub fn validate_target_path(value: &str) -> Result<(), SupervisorError> {
    let path = Path::new(value);
    if value.trim().is_empty()
        || path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(SupervisorError::GateRejected(format!(
            "candidate target path is unsafe: {value}"
        )));
    }
    let normalized = value.replace('\\', "/");
    let sealed = [
        ".git",
        ".golutra",
        ".github",
        "target",
        "node_modules",
        "crates/golutra-eval",
        "crates/golutra-verify",
        "crates/golutra-policy",
        "crates/golutra-sandbox",
        "crates/golutra-supervisor",
        "crates/golutra-release",
        "Cargo.toml",
        "Cargo.lock",
        "justfile",
    ];
    if sealed.iter().any(|prefix| {
        normalized == *prefix
            || normalized.starts_with(&format!("{prefix}/"))
            || normalized
                .split('/')
                .any(|component| component.contains("hidden") || component.contains("signing"))
    }) {
        return Err(SupervisorError::GateRejected(format!(
            "candidate target path is sealed: {value}"
        )));
    }
    let allowed = [
        "crates/golutra-runtime/",
        "crates/golutra-context/",
        "crates/golutra-tools/",
        "crates/golutra-llm/",
        "crates/golutra-client/",
        "crates/golutra-tui/",
        "docs/",
    ];
    if !allowed.iter().any(|prefix| normalized.starts_with(prefix)) {
        return Err(SupervisorError::GateRejected(format!(
            "candidate target path is outside the allowlist: {value}"
        )));
    }
    Ok(())
}

fn sanitize_payload(value: Value, depth: usize) -> Value {
    if depth > 4 {
        return Value::String("[truncated]".to_owned());
    }
    match value {
        Value::Object(object) => Value::Object(
            object
                .into_iter()
                .map(|(key, value)| {
                    let lower = key.to_ascii_lowercase();
                    let value = if ["key", "token", "secret", "password", "credential"]
                        .iter()
                        .any(|marker| lower.contains(marker))
                    {
                        Value::String("[redacted]".to_owned())
                    } else {
                        sanitize_payload(value, depth.saturating_add(1))
                    };
                    (bounded_text(&key, 128), value)
                })
                .collect(),
        ),
        Value::Array(values) => Value::Array(
            values
                .into_iter()
                .take(128)
                .map(|value| sanitize_payload(value, depth.saturating_add(1)))
                .collect(),
        ),
        Value::String(value) => Value::String(bounded_text(&value, 2_048)),
        other => other,
    }
}

fn bounded_text(value: &str, max: usize) -> String {
    value.chars().take(max).collect()
}

fn ensure_private_dir(path: &Path) -> Result<(), SupervisorError> {
    fs::create_dir_all(path).map_err(|error| SupervisorError::Io(error.to_string()))?;
    set_owner_only_dir(path)
}

fn ensure_private_file(path: &Path) -> Result<(), SupervisorError> {
    match OpenOptions::new().create_new(true).write(true).open(path) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(SupervisorError::Io(error.to_string())),
    }
    let metadata =
        fs::symlink_metadata(path).map_err(|error| SupervisorError::Io(error.to_string()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(SupervisorError::Integrity(format!(
            "private lock path is not a regular file: {}",
            path.display()
        )));
    }
    set_owner_only_file(path)
}

fn write_private_atomic(path: &Path, bytes: &[u8]) -> Result<(), SupervisorError> {
    if let Some(parent) = path.parent() {
        ensure_private_dir(parent)?;
    }
    let temp = path.with_extension(format!("tmp-{}", Uuid::now_v7()));
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temp)
        .map_err(|error| SupervisorError::Io(error.to_string()))?;
    set_owner_only_file(&temp)?;
    let result = file
        .write_all(bytes)
        .and_then(|_| file.sync_all())
        .map_err(|error| SupervisorError::Io(error.to_string()));
    if result.is_err() {
        let _ = fs::remove_file(&temp);
        return result;
    }
    #[cfg(windows)]
    if path.exists() {
        fs::remove_file(path).map_err(|error| SupervisorError::Io(error.to_string()))?;
    }
    fs::rename(&temp, path).map_err(|error| {
        let _ = fs::remove_file(&temp);
        SupervisorError::Io(error.to_string())
    })?;
    set_owner_only_file(path)?;
    sync_parent(path)
}

#[cfg(unix)]
fn sync_parent(path: &Path) -> Result<(), SupervisorError> {
    let parent = path
        .parent()
        .ok_or_else(|| SupervisorError::Io(format!("path has no parent: {}", path.display())))?;
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| SupervisorError::Io(error.to_string()))
}

#[cfg(not(unix))]
fn sync_parent(_path: &Path) -> Result<(), SupervisorError> {
    Ok(())
}

#[cfg(unix)]
fn set_owner_only_dir(path: &Path) -> Result<(), SupervisorError> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(|error| SupervisorError::Io(error.to_string()))
}

#[cfg(not(unix))]
fn set_owner_only_dir(_path: &Path) -> Result<(), SupervisorError> {
    Ok(())
}

#[cfg(unix)]
fn set_owner_only_file(path: &Path) -> Result<(), SupervisorError> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .map_err(|error| SupervisorError::Io(error.to_string()))
}

#[cfg(not(unix))]
fn set_owner_only_file(_path: &Path) -> Result<(), SupervisorError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use crate::DisclosureBudget;

    use super::*;

    fn pending_transaction(store: &SupervisorStore, budget_id: &str) -> PendingTransaction {
        let mut next_state = SupervisorState::default();
        next_state.disclosure_budgets.push(DisclosureBudget {
            budget_id: budget_id.to_owned(),
            candidate_family_id: "family-test".to_owned(),
            maximum_queries: 3,
            query_count: 1,
            aggregate_feedback_count: 1,
            exact_feedback_count: 0,
            exhausted_at: None,
        });
        let events = store
            .verify_control_log_locked()
            .expect("control log before pending transaction");
        let event = store
            .prepare_control_event(
                &events,
                "TestTransaction",
                None,
                None,
                serde_json::json!({"budget_id": budget_id}),
            )
            .expect("prepared event");
        PendingTransaction {
            state_digest: supervisor_state_digest(&next_state).expect("state digest"),
            next_state,
            event,
        }
    }

    #[test]
    fn snapshot_recovers_pending_transaction_before_log_append() {
        let root = tempfile::tempdir().expect("root");
        let store = SupervisorStore::new(root.path()).expect("store");
        let lock = store.lock().expect("lock");
        let pending = pending_transaction(&store, "budget-before-log");
        store.write_pending_locked(&pending).expect("pending");
        drop(lock);

        let state = store.snapshot().expect("recovered snapshot");

        assert_eq!(state.disclosure_budgets[0].budget_id, "budget-before-log");
        assert_eq!(store.verify_control_log().expect("log").len(), 1);
        assert!(!store.paths.pending_transaction_path.exists());
    }

    #[test]
    fn snapshot_recovers_pending_transaction_after_log_append_without_duplication() {
        let root = tempfile::tempdir().expect("root");
        let store = SupervisorStore::new(root.path()).expect("store");
        let lock = store.lock().expect("lock");
        let pending = pending_transaction(&store, "budget-after-log");
        let events = store
            .verify_control_log_locked()
            .expect("control log before append");
        store.write_pending_locked(&pending).expect("pending");
        store
            .append_prepared_control_locked(&pending.event, &events)
            .expect("append");
        drop(lock);

        let state = store.snapshot().expect("recovered snapshot");

        assert_eq!(state.disclosure_budgets[0].budget_id, "budget-after-log");
        assert_eq!(store.verify_control_log().expect("log").len(), 1);
        assert!(!store.paths.pending_transaction_path.exists());
    }

    #[test]
    fn snapshot_repairs_an_interrupted_pending_log_append() {
        let root = tempfile::tempdir().expect("root");
        let store = SupervisorStore::new(root.path()).expect("store");
        let lock = store.lock().expect("lock");
        let pending = pending_transaction(&store, "budget-partial-log");
        store.write_pending_locked(&pending).expect("pending");
        let line = serde_json::to_vec(&pending.event).expect("event line");
        let mut log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&store.paths.control_log_path)
            .expect("control log");
        log.write_all(&line[..line.len() / 2])
            .and_then(|_| log.sync_all())
            .expect("partial append");
        drop(log);
        drop(lock);

        let state = store.snapshot().expect("recovered snapshot");

        assert_eq!(state.disclosure_budgets[0].budget_id, "budget-partial-log");
        assert_eq!(store.verify_control_log().expect("log").len(), 1);
        assert!(!store.paths.pending_transaction_path.exists());
    }

    #[test]
    fn tampered_pending_state_is_rejected() {
        let root = tempfile::tempdir().expect("root");
        let store = SupervisorStore::new(root.path()).expect("store");
        let lock = store.lock().expect("lock");
        let mut pending = pending_transaction(&store, "budget-tampered");
        pending.next_state.disclosure_budgets[0].query_count = 2;
        store.write_pending_locked(&pending).expect("pending");
        drop(lock);

        assert!(
            store
                .snapshot()
                .expect_err("tampered pending state must fail")
                .to_string()
                .contains("state digest")
        );
    }
}
