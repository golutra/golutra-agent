//! Thread rollout 格式、脱敏、校验和与文件持久化。

use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

use fs2::FileExt;
use golutra_core::{SessionId, ThreadId};
use golutra_protocol::RuntimeEvent;
use golutra_store::ThreadRecord;
use golutra_tools::redact_sensitive_text;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::{
    ClientError, MAX_ROLLOUT_LINE_BYTES, ROLLOUT_FORMAT_VERSION, RuntimePaths, ensure_private_dir,
    set_owner_only_file, workspace_hash,
};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RolloutEnvelope {
    pub version: u32,
    pub thread_id: ThreadId,
    pub session_id: SessionId,
    pub sequence_no: u64,
    pub checksum: String,
    pub event: RuntimeEvent,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RolloutExport {
    pub thread_id: ThreadId,
    pub session_id: SessionId,
    pub path: String,
    pub event_count: usize,
    pub last_sequence_no: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadRebindResult {
    pub thread: ThreadRecord,
    pub previous_workspace_root: String,
    pub rollout_rebuilt: bool,
    pub checkpoint_compatibility: String,
}

pub(crate) fn rollout_line(
    thread: &ThreadRecord,
    event: &RuntimeEvent,
) -> Result<Vec<u8>, ClientError> {
    let mut event = event.clone();
    redact_rollout_value(&mut event.payload, None);
    let event_bytes = serde_json::to_vec(&event)?;
    let checksum = format!("sha256:{:x}", Sha256::digest(&event_bytes));
    let envelope = RolloutEnvelope {
        version: ROLLOUT_FORMAT_VERSION,
        thread_id: thread.thread_id,
        session_id: thread.session_id,
        sequence_no: event.sequence_no,
        checksum,
        event,
    };
    let line = serde_json::to_vec(&envelope)?;
    if line.len() > MAX_ROLLOUT_LINE_BYTES {
        return Err(ClientError::Io(format!(
            "rollout event exceeds {MAX_ROLLOUT_LINE_BYTES} byte limit"
        )));
    }
    Ok(line)
}

pub(crate) fn redact_rollout_value(value: &mut Value, key: Option<&str>) {
    let sensitive_key = key.is_some_and(is_sensitive_rollout_key);
    if sensitive_key {
        *value = Value::String("<redacted-secret>".to_owned());
        return;
    }
    match value {
        Value::String(content) => {
            *content = redact_sensitive_text(content).0;
        }
        Value::Array(values) => {
            for value in values {
                redact_rollout_value(value, None);
            }
        }
        Value::Object(values) => {
            for (key, value) in values {
                redact_rollout_value(value, Some(key));
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

/// Apply the canonical rollout redaction policy to a value exposed through a
/// developer-facing projection.
pub fn redact_runtime_value(value: &mut Value) {
    redact_rollout_value(value, None);
}

pub(crate) fn is_sensitive_rollout_key(key: &str) -> bool {
    let normalized = key.to_ascii_lowercase().replace('-', "_");
    matches!(
        normalized.as_str(),
        "api_key"
            | "apikey"
            | "authorization"
            | "token"
            | "access_token"
            | "refresh_token"
            | "id_token"
            | "bearer_token"
            | "secret"
            | "client_secret"
            | "password"
            | "credential"
            | "credentials"
    ) || normalized.ends_with("_api_key")
        || normalized.ends_with("_access_token")
        || normalized.ends_with("_refresh_token")
        || normalized.ends_with("_id_token")
        || normalized.ends_with("_secret")
        || normalized.ends_with("_password")
}

pub(crate) fn append_rollout_line(path: &Path, line: &[u8]) -> Result<(), ClientError> {
    let parent = path.parent().ok_or_else(|| {
        ClientError::Io(format!("rollout path has no parent: {}", path.display()))
    })?;
    ensure_private_dir(parent)?;
    if fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        return Err(ClientError::Io(format!(
            "rollout file cannot be a symbolic link: {}",
            path.display()
        )));
    }
    let lock = lock_rollout_file(path)?;
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|error| ClientError::Io(format!("{}: {error}", path.display())))?;
    set_owner_only_file(path)?;
    file.write_all(line)
        .and_then(|()| file.write_all(b"\n"))
        .and_then(|()| file.sync_data())
        .map_err(|error| ClientError::Io(format!("{}: {error}", path.display())))?;
    FileExt::unlock(&lock).map_err(|error| ClientError::Io(format!("{}: {error}", path.display())))
}

pub(crate) fn rebuild_rollout_file(path: &Path, lines: &[Vec<u8>]) -> Result<(), ClientError> {
    let parent = path.parent().ok_or_else(|| {
        ClientError::Io(format!("rollout path has no parent: {}", path.display()))
    })?;
    ensure_private_dir(parent)?;
    let lock = lock_rollout_file(path)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)
        .map_err(|error| ClientError::Io(format!("{}: {error}", parent.display())))?;
    for line in lines {
        temporary
            .write_all(line)
            .and_then(|()| temporary.write_all(b"\n"))
            .map_err(|error| ClientError::Io(format!("{}: {error}", path.display())))?;
    }
    temporary
        .as_file()
        .sync_all()
        .map_err(|error| ClientError::Io(format!("{}: {error}", path.display())))?;
    set_owner_only_file(temporary.path())?;
    temporary
        .persist(path)
        .map_err(|error| ClientError::Io(format!("{}: {}", path.display(), error.error)))?;
    set_owner_only_file(path)?;
    sync_runtime_directory(parent)?;
    FileExt::unlock(&lock).map_err(|error| ClientError::Io(format!("{}: {error}", path.display())))
}

pub(crate) fn lock_rollout_file(path: &Path) -> Result<File, ClientError> {
    let lock_path = rollout_lock_path(path);
    if fs::symlink_metadata(&lock_path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        return Err(ClientError::Io(format!(
            "rollout lock cannot be a symbolic link: {}",
            lock_path.display()
        )));
    }
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)
        .map_err(|error| ClientError::Io(format!("{}: {error}", lock_path.display())))?;
    set_owner_only_file(&lock_path)?;
    lock.lock_exclusive()
        .map_err(|error| ClientError::Io(format!("{}: {error}", lock_path.display())))?;
    Ok(lock)
}

pub(crate) fn rollout_lock_path(path: &Path) -> PathBuf {
    path.with_extension("jsonl.lock")
}

pub(crate) fn rollout_path_for_workspace(
    paths: &RuntimePaths,
    workspace_root: &Path,
    thread_id: ThreadId,
) -> PathBuf {
    paths
        .state_dir
        .join("workspaces")
        .join(workspace_hash(workspace_root))
        .join("rollouts")
        .join(format!("{thread_id}.jsonl"))
}

pub(crate) fn rollout_projection_files(
    directory: &Path,
) -> Result<Vec<(ThreadId, PathBuf)>, ClientError> {
    let mut projections = Vec::new();
    for entry in fs::read_dir(directory)
        .map_err(|error| ClientError::Io(format!("{}: {error}", directory.display())))?
    {
        let entry =
            entry.map_err(|error| ClientError::Io(format!("{}: {error}", directory.display())))?;
        let file_type = entry
            .file_type()
            .map_err(|error| ClientError::Io(format!("{}: {error}", entry.path().display())))?;
        if !file_type.is_file() {
            continue;
        }
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("jsonl") {
            continue;
        }
        let Some(thread_id) = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .and_then(|stem| stem.parse::<ThreadId>().ok())
        else {
            continue;
        };
        projections.push((thread_id, path));
    }
    Ok(projections)
}

pub(crate) fn remove_rollout_projection(path: &Path) -> Result<(), ClientError> {
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(ClientError::Io(format!("{}: {error}", path.display())));
        }
    }
    if let Some(parent) = path.parent() {
        sync_runtime_directory(parent)?;
    }
    Ok(())
}

#[cfg(unix)]
pub(crate) fn sync_runtime_directory(path: &Path) -> Result<(), ClientError> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| ClientError::Io(format!("{}: {error}", path.display())))
}

#[cfg(not(unix))]
pub(crate) fn sync_runtime_directory(_path: &Path) -> Result<(), ClientError> {
    Ok(())
}

pub(crate) fn normalize_rebind_source(path: &Path) -> Result<PathBuf, ClientError> {
    match path.canonicalize() {
        Ok(path) => return Ok(path),
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
            return Err(ClientError::Io(format!("{}: {error}", path.display())));
        }
        Err(_) => {}
    }
    if !path.is_absolute() {
        return Err(ClientError::InvalidSession(format!(
            "nonexistent rebind source must be absolute: {}",
            path.display()
        )));
    }
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::RootDir | std::path::Component::Prefix(_) => {
                normalized.push(component.as_os_str());
            }
            std::path::Component::Normal(component) => normalized.push(component),
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                return Err(ClientError::InvalidSession(format!(
                    "rebind source must not contain `..`: {}",
                    path.display()
                )));
            }
        }
    }
    Ok(normalized)
}
