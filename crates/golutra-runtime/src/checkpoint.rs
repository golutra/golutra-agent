//! 工作区 checkpoint 的持久化、校验与恢复。

use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Component, Path, PathBuf},
};

use golutra_core::{
    CheckpointId, CheckpointType, TaskId, ToolCallId, TurnId, WorkspaceCheckpoint, WorkspaceId,
};
use golutra_tools::FileBeforeImage;
use ignore::gitignore::GitignoreBuilder;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

// Opaque commands can touch an unbounded tree. Once recovery is already partial,
// cap durable before-images so checkpoint I/O cannot consume the task deadline.
const MAX_PARTIAL_CHECKPOINT_FILES: usize = 128;

#[derive(Debug, Error)]
pub enum CheckpointError {
    #[error("checkpoint io failed: {0}")]
    Io(String),
    #[error("changed file is outside workspace: {0}")]
    OutsideWorkspace(String),
    #[error("changed file is excluded from checkpoint: {0}")]
    Excluded(String),
    #[error("checkpoint manifest is invalid: {0}")]
    InvalidManifest(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceCheckpointManager {
    workspace_root: PathBuf,
    checkpoint_root: PathBuf,
}

impl WorkspaceCheckpointManager {
    #[must_use]
    pub fn new(workspace_root: impl Into<PathBuf>, checkpoint_root: impl Into<PathBuf>) -> Self {
        Self {
            workspace_root: workspace_root.into(),
            checkpoint_root: checkpoint_root.into(),
        }
    }

    pub fn create_checkpoint(
        &self,
        workspace_id: WorkspaceId,
        task_id: TaskId,
        turn_id: TurnId,
        before_images: &[FileBeforeImage],
        created_before_tool_call_id: ToolCallId,
    ) -> Result<WorkspaceCheckpoint, CheckpointError> {
        let checkpoint_id = CheckpointId::new();
        let checkpoint_dir = self.checkpoint_root.join(checkpoint_id.to_string());
        fs::create_dir_all(&self.checkpoint_root)
            .map_err(|error| CheckpointError::Io(error.to_string()))?;
        set_owner_only_checkpoint_dir(&self.checkpoint_root)?;
        fs::create_dir_all(&checkpoint_dir)
            .map_err(|error| CheckpointError::Io(error.to_string()))?;
        set_owner_only_checkpoint_dir(&checkpoint_dir)?;

        let mut entries = Vec::new();
        for before_image in before_images {
            let relative_path = self.relative_checkpoint_path(&before_image.path)?;
            if let Some(content) = &before_image.content {
                let target_path = checkpoint_dir.join("files").join(&relative_path);
                if let Some(parent) = target_path.parent() {
                    fs::create_dir_all(parent)
                        .map_err(|error| CheckpointError::Io(error.to_string()))?;
                }
                let object_path = self.store_checkpoint_object(content)?;
                fs::hard_link(&object_path, &target_path).map_err(|error| {
                    CheckpointError::Io(format!(
                        "failed to link checkpoint object {} to {}: {error}",
                        object_path.display(),
                        target_path.display()
                    ))
                })?;
                set_owner_only_checkpoint_file(&target_path)?;
                sync_checkpoint_ancestors(
                    target_path.parent().unwrap_or(&checkpoint_dir),
                    &checkpoint_dir,
                )?;
            }
            entries.push(CheckpointManifestEntry {
                path: relative_path.display().to_string(),
                existed: before_image.content.is_some(),
                checksum: before_image.content.as_deref().map(checksum_bytes),
                unix_mode: before_image.unix_mode,
            });
        }
        let manifest = CheckpointManifest { entries };
        let manifest_path = checkpoint_dir.join("manifest.json");
        write_checkpoint_file(
            &manifest_path,
            &serde_json::to_vec_pretty(&manifest)
                .map_err(|error| CheckpointError::InvalidManifest(error.to_string()))?,
        )?;
        sync_checkpoint_directory(&checkpoint_dir)?;
        sync_checkpoint_directory(&self.checkpoint_root)?;
        let changed_files = manifest
            .entries
            .iter()
            .map(|entry| entry.path.clone())
            .collect();

        Ok(WorkspaceCheckpoint {
            checkpoint_id,
            workspace_id,
            task_id,
            turn_id,
            checkpoint_type: CheckpointType::Snapshot,
            changed_files,
            artifact_refs: Vec::new(),
            created_before_tool_call_id,
            restore_hint: format!(
                "restore files using manifest {}",
                checkpoint_dir.join("manifest.json").display()
            ),
            retention_policy: "p0_keep_until_task_cleanup".to_owned(),
        })
    }

    /// Select the before-images that may be persisted in a partial checkpoint.
    ///
    /// Opaque process tools take a bounded workspace snapshot before execution.
    /// That snapshot can contain gitignored files while already being marked
    /// incomplete because internal or generated subtrees were omitted. An
    /// unrestricted task can also report paths outside the workspace; those
    /// paths are observable but cannot be represented by a workspace rollback
    /// checkpoint. Callers may use this selection only when they explicitly
    /// accept an incomplete checkpoint.
    pub fn filter_checkpointable_before_images(
        &self,
        before_images: &[FileBeforeImage],
    ) -> Result<(Vec<FileBeforeImage>, usize), CheckpointError> {
        let mut retained =
            Vec::with_capacity(before_images.len().min(MAX_PARTIAL_CHECKPOINT_FILES));
        let mut excluded_count = 0_usize;
        for (index, before_image) in before_images.iter().enumerate() {
            if retained.len() >= MAX_PARTIAL_CHECKPOINT_FILES {
                excluded_count =
                    excluded_count.saturating_add(before_images.len().saturating_sub(index));
                break;
            }
            if before_image.content.is_none() && before_image.metadata.is_some() {
                excluded_count = excluded_count.saturating_add(1);
                continue;
            }
            match self.relative_checkpoint_path(&before_image.path) {
                Ok(_) => retained.push(before_image.clone()),
                Err(CheckpointError::Excluded(_) | CheckpointError::OutsideWorkspace(_)) => {
                    excluded_count = excluded_count.saturating_add(1);
                }
                Err(error) => return Err(error),
            }
        }
        Ok((retained, excluded_count))
    }

    pub fn restore_checkpoint(&self, checkpoint_id: CheckpointId) -> Result<(), CheckpointError> {
        let checkpoint_dir = self.checkpoint_root.join(checkpoint_id.to_string());
        let manifest_bytes = fs::read(checkpoint_dir.join("manifest.json"))
            .map_err(|error| CheckpointError::Io(error.to_string()))?;
        let manifest: CheckpointManifest = serde_json::from_slice(&manifest_bytes)
            .map_err(|error| CheckpointError::InvalidManifest(error.to_string()))?;
        let mut prepared = Vec::with_capacity(manifest.entries.len());
        for entry in manifest.entries {
            let declared_path = Path::new(&entry.path);
            if declared_path.as_os_str().is_empty()
                || declared_path
                    .components()
                    .any(|component| !matches!(component, Component::Normal(_)))
            {
                return Err(CheckpointError::InvalidManifest(format!(
                    "checkpoint path must be workspace-relative without traversal: {}",
                    entry.path
                )));
            }
            validate_workspace_file_mode(entry.unix_mode)?;
            let relative_path = self.relative_checkpoint_path(declared_path)?;
            let target = self.workspace_root.join(&relative_path);
            if entry.existed {
                let source = checkpoint_dir.join("files").join(&relative_path);
                let content =
                    fs::read(&source).map_err(|error| CheckpointError::Io(error.to_string()))?;
                let actual_checksum = checksum_bytes(&content);
                if entry.checksum.as_deref() != Some(actual_checksum.as_str()) {
                    return Err(CheckpointError::InvalidManifest(format!(
                        "checkpoint content checksum mismatch: {}",
                        entry.path
                    )));
                }
                prepared.push((target, Some(content), entry.unix_mode));
            } else {
                prepared.push((target, None, None));
            }
        }
        for (target, content, unix_mode) in prepared {
            if let Some(content) = content {
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent)
                        .map_err(|error| CheckpointError::Io(error.to_string()))?;
                }
                write_workspace_restore_file(&target, &content, unix_mode)?;
            } else {
                match fs::symlink_metadata(&target) {
                    Ok(_) => {
                        fs::remove_file(&target)
                            .map_err(|error| CheckpointError::Io(error.to_string()))?;
                        if let Some(parent) = target.parent() {
                            sync_checkpoint_directory(parent)?;
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(CheckpointError::Io(error.to_string())),
                }
            }
        }
        Ok(())
    }

    pub fn checkpoint_count(&self) -> Result<u64, CheckpointError> {
        Ok(self.checkpoint_directories()?.len() as u64)
    }

    pub fn prune_checkpoints(&self, keep_latest: usize) -> Result<u64, CheckpointError> {
        let mut checkpoints = self.checkpoint_directories()?;
        checkpoints.sort_by(|left, right| right.1.cmp(&left.1));
        let mut removed = 0_u64;
        for (path, _) in checkpoints.into_iter().skip(keep_latest) {
            fs::remove_dir_all(&path)
                .map_err(|error| CheckpointError::Io(format!("{}: {error}", path.display())))?;
            removed = removed.saturating_add(1);
        }
        if removed > 0 {
            self.prune_unreferenced_objects()?;
            sync_checkpoint_directory(&self.checkpoint_root)?;
        }
        Ok(removed)
    }

    fn checkpoint_directories(
        &self,
    ) -> Result<Vec<(PathBuf, std::time::SystemTime)>, CheckpointError> {
        let entries = match fs::read_dir(&self.checkpoint_root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(CheckpointError::Io(error.to_string())),
        };
        let mut checkpoints = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|error| CheckpointError::Io(error.to_string()))?;
            let metadata = fs::symlink_metadata(entry.path())
                .map_err(|error| CheckpointError::Io(error.to_string()))?;
            if metadata.file_type().is_symlink() {
                return Err(CheckpointError::Io(format!(
                    "checkpoint entry cannot be a symbolic link: {}",
                    entry.path().display()
                )));
            }
            if !metadata.is_dir()
                || entry
                    .file_name()
                    .to_str()
                    .and_then(|value| uuid::Uuid::parse_str(value).ok())
                    .is_none()
            {
                continue;
            }
            let modified = metadata
                .modified()
                .map_err(|error| CheckpointError::Io(error.to_string()))?;
            checkpoints.push((entry.path(), modified));
        }
        Ok(checkpoints)
    }

    fn store_checkpoint_object(&self, content: &[u8]) -> Result<PathBuf, CheckpointError> {
        let object_root = self.checkpoint_root.join(".objects");
        fs::create_dir_all(&object_root).map_err(|error| CheckpointError::Io(error.to_string()))?;
        set_owner_only_checkpoint_dir(&object_root)?;
        let checksum = checksum_bytes(content);
        let object_path = object_root.join(checksum.trim_start_matches("sha256:"));
        match fs::read(&object_path) {
            Ok(existing) if existing == content => return Ok(object_path),
            Ok(_) => {
                return Err(CheckpointError::InvalidManifest(format!(
                    "checkpoint object checksum collision: {checksum}"
                )));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(CheckpointError::Io(error.to_string())),
        }
        write_checkpoint_file(&object_path, content)?;
        sync_checkpoint_directory(&object_root)?;
        Ok(object_path)
    }

    fn prune_unreferenced_objects(&self) -> Result<(), CheckpointError> {
        let object_root = self.checkpoint_root.join(".objects");
        let entries = match fs::read_dir(&object_root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(CheckpointError::Io(error.to_string())),
        };
        for entry in entries {
            let entry = entry.map_err(|error| CheckpointError::Io(error.to_string()))?;
            let metadata = entry
                .metadata()
                .map_err(|error| CheckpointError::Io(error.to_string()))?;
            if metadata.is_file() && checkpoint_object_is_unreferenced(&metadata) {
                fs::remove_file(entry.path())
                    .map_err(|error| CheckpointError::Io(error.to_string()))?;
            }
        }
        sync_checkpoint_directory(&object_root)
    }

    fn relative_checkpoint_path(&self, changed_file: &Path) -> Result<PathBuf, CheckpointError> {
        let path = if changed_file.is_absolute() {
            changed_file.to_path_buf()
        } else {
            self.workspace_root.join(changed_file)
        };
        let canonical_workspace = self
            .workspace_root
            .canonicalize()
            .map_err(|error| CheckpointError::Io(error.to_string()))?;
        let canonical_path = if path.exists() {
            path.canonicalize()
                .map_err(|error| CheckpointError::Io(error.to_string()))?
        } else {
            let parent = path.parent().ok_or_else(|| {
                CheckpointError::Io(format!("changed file has no parent: {}", path.display()))
            })?;
            let canonical_parent = parent
                .canonicalize()
                .map_err(|error| CheckpointError::Io(error.to_string()))?;
            let file_name = path.file_name().ok_or_else(|| {
                CheckpointError::Io(format!("changed file has no name: {}", path.display()))
            })?;
            canonical_parent.join(file_name)
        };
        let relative = canonical_path
            .strip_prefix(&canonical_workspace)
            .map_err(|_| CheckpointError::OutsideWorkspace(path.display().to_string()))?;

        if is_checkpoint_excluded(relative) {
            return Err(CheckpointError::Excluded(relative.display().to_string()));
        }
        if self.is_gitignored(relative)? {
            return Err(CheckpointError::Excluded(relative.display().to_string()));
        }
        Ok(relative.to_path_buf())
    }

    fn is_gitignored(&self, relative_path: &Path) -> Result<bool, CheckpointError> {
        let mut builder = GitignoreBuilder::new(&self.workspace_root);
        let mut directory = self.workspace_root.clone();
        add_gitignore_file(&mut builder, &directory)?;
        if let Some(parent) = relative_path.parent() {
            for component in parent.components() {
                directory.push(component.as_os_str());
                add_gitignore_file(&mut builder, &directory)?;
            }
        }
        let matcher = builder
            .build()
            .map_err(|error| CheckpointError::Io(error.to_string()))?;
        Ok(matcher
            .matched_path_or_any_parents(relative_path, false)
            .is_ignore())
    }
}

fn add_gitignore_file(
    builder: &mut GitignoreBuilder,
    directory: &Path,
) -> Result<(), CheckpointError> {
    let path = directory.join(".gitignore");
    if path.exists()
        && let Some(error) = builder.add(path)
    {
        return Err(CheckpointError::Io(error.to_string()));
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct CheckpointManifest {
    entries: Vec<CheckpointManifestEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct CheckpointManifestEntry {
    path: String,
    existed: bool,
    checksum: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    unix_mode: Option<u32>,
}

#[must_use]
pub fn checkpoint_fingerprint(checkpoint: &WorkspaceCheckpoint) -> String {
    let mut hasher = Sha256::new();
    hasher.update(checkpoint.checkpoint_id.to_string().as_bytes());
    for changed_file in &checkpoint.changed_files {
        hasher.update(changed_file.as_bytes());
    }
    format!("sha256:{:x}", hasher.finalize())
}

fn checksum_bytes(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    format!("sha256:{digest:x}")
}

#[cfg(unix)]
fn checkpoint_object_is_unreferenced(metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;

    metadata.nlink() <= 1
}

#[cfg(not(unix))]
fn checkpoint_object_is_unreferenced(_metadata: &fs::Metadata) -> bool {
    false
}

fn is_checkpoint_excluded(relative_path: &Path) -> bool {
    let path_text = relative_path.to_string_lossy();
    relative_path.components().any(|component| {
        matches!(
            component,
            std::path::Component::Normal(value) if matches!(value.to_str(), Some(".git" | ".golutra"))
        )
    })
        || path_text.contains(".env")
        || path_text.contains(".ssh")
        || path_text.contains("id_rsa")
        || path_text.contains("id_ed25519")
}

fn write_checkpoint_file(path: &Path, bytes: &[u8]) -> Result<(), CheckpointError> {
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(path)
        .map_err(|error| CheckpointError::Io(error.to_string()))?;
    set_owner_only_checkpoint_file(path)?;
    file.write_all(bytes)
        .map_err(|error| CheckpointError::Io(error.to_string()))?;
    file.sync_all()
        .map_err(|error| CheckpointError::Io(error.to_string()))
}

fn write_workspace_restore_file(
    path: &Path,
    bytes: &[u8],
    unix_mode: Option<u32>,
) -> Result<(), CheckpointError> {
    let parent = path.parent().ok_or_else(|| {
        CheckpointError::Io(format!("restore path has no parent: {}", path.display()))
    })?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)
        .map_err(|error| CheckpointError::Io(error.to_string()))?;
    temporary
        .write_all(bytes)
        .map_err(|error| CheckpointError::Io(error.to_string()))?;
    set_workspace_file_mode(temporary.path(), unix_mode)?;
    temporary
        .as_file()
        .sync_all()
        .map_err(|error| CheckpointError::Io(error.to_string()))?;
    temporary
        .persist(path)
        .map_err(|error| CheckpointError::Io(error.error.to_string()))?;
    sync_checkpoint_directory(parent)
}

fn sync_checkpoint_ancestors(start: &Path, stop: &Path) -> Result<(), CheckpointError> {
    let mut directory = Some(start);
    while let Some(current) = directory {
        sync_checkpoint_directory(current)?;
        if current == stop {
            return Ok(());
        }
        directory = current.parent();
    }
    Err(CheckpointError::Io(format!(
        "checkpoint directory {} is not below {}",
        start.display(),
        stop.display()
    )))
}

#[cfg(unix)]
fn sync_checkpoint_directory(path: &Path) -> Result<(), CheckpointError> {
    fs::File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| CheckpointError::Io(error.to_string()))
}

#[cfg(not(unix))]
fn sync_checkpoint_directory(_path: &Path) -> Result<(), CheckpointError> {
    Ok(())
}

#[cfg(unix)]
fn set_workspace_file_mode(path: &Path, unix_mode: Option<u32>) -> Result<(), CheckpointError> {
    use std::os::unix::fs::PermissionsExt;

    validate_workspace_file_mode(unix_mode)?;
    if let Some(mode) = unix_mode {
        fs::set_permissions(path, fs::Permissions::from_mode(mode))
            .map_err(|error| CheckpointError::Io(error.to_string()))?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn set_workspace_file_mode(_path: &Path, _unix_mode: Option<u32>) -> Result<(), CheckpointError> {
    Ok(())
}

fn validate_workspace_file_mode(unix_mode: Option<u32>) -> Result<(), CheckpointError> {
    if unix_mode.is_some_and(|mode| mode > 0o7777) {
        return Err(CheckpointError::InvalidManifest(format!(
            "checkpoint file mode is invalid: {:o}",
            unix_mode.unwrap_or_default()
        )));
    }
    Ok(())
}

#[cfg(unix)]
fn set_owner_only_checkpoint_dir(path: &Path) -> Result<(), CheckpointError> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(|error| CheckpointError::Io(error.to_string()))
}

#[cfg(not(unix))]
fn set_owner_only_checkpoint_dir(_path: &Path) -> Result<(), CheckpointError> {
    Ok(())
}

#[cfg(unix)]
fn set_owner_only_checkpoint_file(path: &Path) -> Result<(), CheckpointError> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .map_err(|error| CheckpointError::Io(error.to_string()))
}

#[cfg(not(unix))]
fn set_owner_only_checkpoint_file(_path: &Path) -> Result<(), CheckpointError> {
    Ok(())
}
