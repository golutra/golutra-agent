use std::{
    collections::BTreeSet,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
};

use chrono::Utc;
use fs2::FileExt;
use sha2::{Digest, Sha256};
use uuid::Uuid;
use walkdir::WalkDir;

use crate::{
    EnabledPlugin, PLUGIN_MANIFEST_FILE, PluginError, PluginManifest, PluginRecord,
    PluginRegistryState, PluginRevision, PluginRevisionState, validate::validate_manifest,
};

const MAX_REGISTRY_BYTES: u64 = 4 * 1024 * 1024;
const MAX_PACKAGE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_PACKAGE_FILE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_PACKAGE_FILES: usize = 2_048;
const MAX_PLUGINS: usize = 128;
const MAX_REVISIONS_PER_PLUGIN: usize = 20;

#[derive(Debug, Clone)]
pub struct PluginStore {
    root: PathBuf,
    packages: PathBuf,
    state_path: PathBuf,
    lock_path: PathBuf,
}

impl PluginStore {
    pub fn new(home: impl AsRef<Path>) -> Result<Self, PluginError> {
        let home = absolute_path(home.as_ref())?;
        ensure_private_dir(&home)?;
        let home = home
            .canonicalize()
            .map_err(|error| PluginError::Io(error.to_string()))?;
        let root = home.join("plugins");
        let packages = root.join("packages");
        ensure_private_dir(&root)?;
        ensure_private_dir(&packages)?;
        Ok(Self {
            state_path: root.join("registry.json"),
            lock_path: root.join("registry.lock"),
            root,
            packages,
        })
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn state(&self) -> Result<PluginRegistryState, PluginError> {
        let _lock = self.acquire_lock()?;
        self.load_state_unlocked()
    }

    pub fn stage(&self, source: impl AsRef<Path>) -> Result<PluginRevision, PluginError> {
        let source = source
            .as_ref()
            .canonicalize()
            .map_err(|error| PluginError::Io(error.to_string()))?;
        if !source.is_dir() {
            return Err(PluginError::InvalidManifest(format!(
                "plugin package is not a directory: {}",
                source.display()
            )));
        }
        let packages = self
            .packages
            .canonicalize()
            .map_err(|error| PluginError::Io(error.to_string()))?;
        if source.starts_with(&packages) {
            return Err(PluginError::InvalidManifest(
                "cannot stage a package from the managed plugin store".to_owned(),
            ));
        }
        let manifest = load_manifest(&source)?;
        validate_manifest(&manifest)?;

        let _lock = self.acquire_lock()?;
        let mut state = self.load_state_unlocked()?;
        validate_stage_capacity(&state, &manifest.id)?;
        let revision_id = Uuid::now_v7().to_string();
        let plugin_dir = self.packages.join(&manifest.id);
        ensure_private_dir(&plugin_dir)?;
        let staging_dir = plugin_dir.join(format!(".stage-{revision_id}"));
        let package_dir = plugin_dir.join(&revision_id);
        if staging_dir.exists() || package_dir.exists() {
            return Err(PluginError::Io(format!(
                "plugin revision path already exists: {}",
                package_dir.display()
            )));
        }
        if let Err(error) = copy_package(&source, &staging_dir) {
            let _ = fs::remove_dir_all(&staging_dir);
            return Err(error);
        }
        let checksum = match package_checksum(&staging_dir) {
            Ok(checksum) => checksum,
            Err(error) => {
                let _ = fs::remove_dir_all(&staging_dir);
                return Err(error);
            }
        };
        fs::rename(&staging_dir, &package_dir)
            .map_err(|error| PluginError::Io(error.to_string()))?;
        sync_directory(&plugin_dir)?;
        let relative_dir = package_dir
            .strip_prefix(&self.root)
            .expect("package is inside plugin root")
            .to_str()
            .ok_or_else(|| PluginError::Io("plugin package path is not UTF-8".to_owned()))?
            .to_owned();
        let revision = PluginRevision {
            revision_id: revision_id.clone(),
            manifest: manifest.clone(),
            package_dir: relative_dir,
            checksum,
            state: PluginRevisionState::Staged,
            staged_at: Utc::now(),
            reviewed_at: None,
            enabled_at: None,
        };
        let record = state
            .plugins
            .iter_mut()
            .find(|record| record.plugin_id == manifest.id);
        if let Some(record) = record {
            record.revisions.push(revision.clone());
        } else {
            state.plugins.push(PluginRecord {
                plugin_id: manifest.id,
                active_revision_id: None,
                revisions: vec![revision.clone()],
            });
        }
        state
            .plugins
            .sort_by(|left, right| left.plugin_id.cmp(&right.plugin_id));
        if let Err(error) = self.save_state_unlocked(&state) {
            let _ = fs::remove_dir_all(&package_dir);
            return Err(error);
        }
        Ok(revision)
    }

    pub fn review(
        &self,
        plugin_id: &str,
        revision_id: &str,
    ) -> Result<PluginRevision, PluginError> {
        let _lock = self.acquire_lock()?;
        let mut state = self.load_state_unlocked()?;
        let revision = find_revision(&state, plugin_id, revision_id)?.clone();
        if revision.state != PluginRevisionState::Staged {
            return Err(PluginError::InvalidState(format!(
                "revision `{revision_id}` is {:?}, expected staged",
                revision.state
            )));
        }
        self.verify_revision(&revision)?;
        let revision = find_revision_mut(&mut state, plugin_id, revision_id)?;
        revision.state = PluginRevisionState::Reviewed;
        revision.reviewed_at = Some(Utc::now());
        let revision = revision.clone();
        self.save_state_unlocked(&state)?;
        Ok(revision)
    }

    pub fn enable(
        &self,
        plugin_id: &str,
        revision_id: &str,
    ) -> Result<PluginRevision, PluginError> {
        let _lock = self.acquire_lock()?;
        let mut state = self.load_state_unlocked()?;
        self.enable_unlocked(&mut state, plugin_id, revision_id)?;
        let revision = find_revision(&state, plugin_id, revision_id)?.clone();
        self.save_state_unlocked(&state)?;
        Ok(revision)
    }

    pub fn disable(&self, plugin_id: &str) -> Result<PluginRevision, PluginError> {
        let _lock = self.acquire_lock()?;
        let mut state = self.load_state_unlocked()?;
        let record = find_record_mut(&mut state, plugin_id)?;
        let active = record.active_revision_id.take().ok_or_else(|| {
            PluginError::InvalidState(format!("plugin `{plugin_id}` is not enabled"))
        })?;
        let revision = record
            .revisions
            .iter_mut()
            .find(|revision| revision.revision_id == active)
            .ok_or_else(|| PluginError::RevisionNotFound(active.clone()))?;
        revision.state = PluginRevisionState::Disabled;
        let revision = revision.clone();
        self.save_state_unlocked(&state)?;
        Ok(revision)
    }

    pub fn rollback(&self, plugin_id: &str) -> Result<PluginRevision, PluginError> {
        let _lock = self.acquire_lock()?;
        let mut state = self.load_state_unlocked()?;
        let record = find_record(&state, plugin_id)?;
        let active = record.active_revision_id.as_deref();
        let target = record
            .revisions
            .iter()
            .filter(|revision| Some(revision.revision_id.as_str()) != active)
            .filter(|revision| revision.reviewed_at.is_some())
            .filter(|revision| revision.state != PluginRevisionState::Staged)
            .max_by_key(|revision| revision.staged_at)
            .map(|revision| revision.revision_id.clone())
            .ok_or_else(|| {
                PluginError::InvalidState(format!(
                    "plugin `{plugin_id}` has no reviewed revision to roll back to"
                ))
            })?;
        self.enable_unlocked(&mut state, plugin_id, &target)?;
        let revision = find_revision(&state, plugin_id, &target)?.clone();
        self.save_state_unlocked(&state)?;
        Ok(revision)
    }

    pub fn enabled(&self) -> Result<Vec<EnabledPlugin>, PluginError> {
        let _lock = self.acquire_lock()?;
        let state = self.load_state_unlocked()?;
        let mut enabled = Vec::new();
        for record in &state.plugins {
            let Some(revision_id) = record.active_revision_id.as_deref() else {
                continue;
            };
            let revision = find_revision(&state, &record.plugin_id, revision_id)?;
            if revision.state != PluginRevisionState::Enabled {
                return Err(PluginError::InvalidState(format!(
                    "active revision `{revision_id}` is not enabled"
                )));
            }
            let package_root = self.verify_revision(revision)?;
            enabled.push(EnabledPlugin {
                revision_id: revision.revision_id.clone(),
                manifest: revision.manifest.clone(),
                package_root,
                checksum: revision.checksum.clone(),
            });
        }
        enabled.sort_by(|left, right| left.manifest.id.cmp(&right.manifest.id));
        Ok(enabled)
    }

    fn enable_unlocked(
        &self,
        state: &mut PluginRegistryState,
        plugin_id: &str,
        revision_id: &str,
    ) -> Result<(), PluginError> {
        let target = find_revision(state, plugin_id, revision_id)?.clone();
        if !matches!(
            target.state,
            PluginRevisionState::Reviewed | PluginRevisionState::Disabled
        ) {
            return Err(PluginError::InvalidState(format!(
                "revision `{revision_id}` must be reviewed before it can be enabled"
            )));
        }
        self.verify_revision(&target)?;
        let record = find_record_mut(state, plugin_id)?;
        if let Some(active) = record.active_revision_id.as_deref()
            && active != revision_id
            && let Some(revision) = record
                .revisions
                .iter_mut()
                .find(|revision| revision.revision_id == active)
        {
            revision.state = PluginRevisionState::Disabled;
        }
        let revision = record
            .revisions
            .iter_mut()
            .find(|revision| revision.revision_id == revision_id)
            .ok_or_else(|| PluginError::RevisionNotFound(revision_id.to_owned()))?;
        revision.state = PluginRevisionState::Enabled;
        revision.enabled_at = Some(Utc::now());
        record.active_revision_id = Some(revision_id.to_owned());
        Ok(())
    }

    fn verify_revision(&self, revision: &PluginRevision) -> Result<PathBuf, PluginError> {
        validate_manifest(&revision.manifest)?;
        let package_root = managed_path(&self.root, &revision.package_dir)?;
        let checksum = package_checksum(&package_root)?;
        if checksum != revision.checksum {
            return Err(PluginError::Integrity(format!(
                "revision `{}` checksum changed",
                revision.revision_id
            )));
        }
        let manifest = load_manifest(&package_root)?;
        if manifest != revision.manifest {
            return Err(PluginError::Integrity(format!(
                "revision `{}` manifest changed",
                revision.revision_id
            )));
        }
        Ok(package_root)
    }

    fn acquire_lock(&self) -> Result<File, PluginError> {
        reject_symlink(&self.lock_path)?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&self.lock_path)
            .map_err(|error| PluginError::Io(error.to_string()))?;
        set_owner_only_file(&self.lock_path, false)?;
        file.lock_exclusive()
            .map_err(|error| PluginError::Io(error.to_string()))?;
        Ok(file)
    }

    fn load_state_unlocked(&self) -> Result<PluginRegistryState, PluginError> {
        reject_symlink(&self.state_path)?;
        if !self.state_path.exists() {
            return Ok(PluginRegistryState::default());
        }
        let file =
            File::open(&self.state_path).map_err(|error| PluginError::Io(error.to_string()))?;
        let metadata = file
            .metadata()
            .map_err(|error| PluginError::Io(error.to_string()))?;
        if !metadata.is_file() || metadata.len() > MAX_REGISTRY_BYTES {
            return Err(PluginError::Limit(
                "plugin registry is not a bounded regular file".to_owned(),
            ));
        }
        let mut bytes = Vec::new();
        file.take(MAX_REGISTRY_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| PluginError::Io(error.to_string()))?;
        if bytes.len() as u64 > MAX_REGISTRY_BYTES {
            return Err(PluginError::Limit(
                "plugin registry grew while reading".to_owned(),
            ));
        }
        let state: PluginRegistryState = serde_json::from_slice(&bytes)?;
        validate_state(&state)?;
        Ok(state)
    }

    fn save_state_unlocked(&self, state: &PluginRegistryState) -> Result<(), PluginError> {
        validate_state(state)?;
        let mut bytes = serde_json::to_vec_pretty(state)?;
        bytes.push(b'\n');
        if bytes.len() as u64 > MAX_REGISTRY_BYTES {
            return Err(PluginError::Limit(
                "plugin registry exceeds its size limit".to_owned(),
            ));
        }
        reject_symlink(&self.state_path)?;
        let mut temporary = tempfile::NamedTempFile::new_in(&self.root)
            .map_err(|error| PluginError::Io(error.to_string()))?;
        temporary
            .write_all(&bytes)
            .map_err(|error| PluginError::Io(error.to_string()))?;
        set_owner_only_file(temporary.path(), false)?;
        temporary
            .as_file()
            .sync_all()
            .map_err(|error| PluginError::Io(error.to_string()))?;
        temporary
            .persist(&self.state_path)
            .map_err(|error| PluginError::Io(error.error.to_string()))?;
        set_owner_only_file(&self.state_path, false)?;
        sync_directory(&self.root)
    }
}

fn load_manifest(package_root: &Path) -> Result<PluginManifest, PluginError> {
    let path = package_root.join(PLUGIN_MANIFEST_FILE);
    reject_symlink(&path)?;
    let metadata = fs::metadata(&path).map_err(|error| PluginError::Io(error.to_string()))?;
    if !metadata.is_file() || metadata.len() > MAX_PACKAGE_FILE_BYTES {
        return Err(PluginError::Limit(
            "plugin manifest is not a bounded regular file".to_owned(),
        ));
    }
    let bytes = fs::read(path).map_err(|error| PluginError::Io(error.to_string()))?;
    let manifest = serde_json::from_slice(&bytes)?;
    validate_manifest(&manifest)?;
    Ok(manifest)
}

fn copy_package(source: &Path, destination: &Path) -> Result<(), PluginError> {
    ensure_private_dir(destination)?;
    let mut file_count = 0_usize;
    let mut total_bytes = 0_u64;
    for entry in WalkDir::new(source).follow_links(false).sort_by_file_name() {
        let entry = entry.map_err(|error| PluginError::Io(error.to_string()))?;
        let relative = entry
            .path()
            .strip_prefix(source)
            .map_err(|error| PluginError::Io(error.to_string()))?;
        if relative.as_os_str().is_empty() {
            continue;
        }
        let target = destination.join(relative);
        let file_type = entry.file_type();
        if file_type.is_symlink() {
            return Err(PluginError::InvalidManifest(format!(
                "plugin packages cannot contain symbolic links: {}",
                relative.display()
            )));
        }
        if file_type.is_dir() {
            ensure_private_dir(&target)?;
            continue;
        }
        if !file_type.is_file() {
            return Err(PluginError::InvalidManifest(format!(
                "plugin packages can only contain files and directories: {}",
                relative.display()
            )));
        }
        file_count = file_count.saturating_add(1);
        let metadata = entry
            .metadata()
            .map_err(|error| PluginError::Io(error.to_string()))?;
        total_bytes = total_bytes.saturating_add(metadata.len());
        if file_count > MAX_PACKAGE_FILES
            || metadata.len() > MAX_PACKAGE_FILE_BYTES
            || total_bytes > MAX_PACKAGE_BYTES
        {
            return Err(PluginError::Limit(
                "plugin package exceeds file count or byte limits".to_owned(),
            ));
        }
        if let Some(parent) = target.parent() {
            ensure_private_dir(parent)?;
        }
        fs::copy(entry.path(), &target).map_err(|error| PluginError::Io(error.to_string()))?;
        set_owner_only_file(&target, executable(&metadata))?;
    }
    if file_count == 0 {
        return Err(PluginError::InvalidManifest(
            "plugin package is empty".to_owned(),
        ));
    }
    Ok(())
}

fn package_checksum(root: &Path) -> Result<String, PluginError> {
    let root = root
        .canonicalize()
        .map_err(|error| PluginError::Io(error.to_string()))?;
    let mut hasher = Sha256::new();
    let mut file_count = 0_usize;
    let mut total_bytes = 0_u64;
    for entry in WalkDir::new(&root).follow_links(false).sort_by_file_name() {
        let entry = entry.map_err(|error| PluginError::Io(error.to_string()))?;
        let relative = entry
            .path()
            .strip_prefix(&root)
            .map_err(|error| PluginError::Io(error.to_string()))?;
        if relative.as_os_str().is_empty() {
            continue;
        }
        if entry.file_type().is_symlink() {
            return Err(PluginError::Integrity(format!(
                "plugin package contains a symbolic link: {}",
                relative.display()
            )));
        }
        if entry.file_type().is_dir() {
            continue;
        }
        if !entry.file_type().is_file() {
            return Err(PluginError::Integrity(format!(
                "plugin package contains a special file: {}",
                relative.display()
            )));
        }
        let relative = relative
            .to_str()
            .ok_or_else(|| PluginError::Integrity("package path is not UTF-8".to_owned()))?;
        let metadata = entry
            .metadata()
            .map_err(|error| PluginError::Io(error.to_string()))?;
        file_count = file_count.saturating_add(1);
        total_bytes = total_bytes.saturating_add(metadata.len());
        if file_count > MAX_PACKAGE_FILES
            || metadata.len() > MAX_PACKAGE_FILE_BYTES
            || total_bytes > MAX_PACKAGE_BYTES
        {
            return Err(PluginError::Limit(
                "plugin package exceeds file count or byte limits".to_owned(),
            ));
        }
        hasher.update((relative.len() as u64).to_le_bytes());
        hasher.update(relative.as_bytes());
        hasher.update(metadata.len().to_le_bytes());
        hasher.update([u8::from(executable(&metadata))]);
        let mut file =
            File::open(entry.path()).map_err(|error| PluginError::Io(error.to_string()))?;
        let mut buffer = [0_u8; 16 * 1024];
        loop {
            let read = file
                .read(&mut buffer)
                .map_err(|error| PluginError::Io(error.to_string()))?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
        }
    }
    Ok(format!("sha256:{:x}", hasher.finalize()))
}

fn validate_stage_capacity(
    state: &PluginRegistryState,
    plugin_id: &str,
) -> Result<(), PluginError> {
    if state.plugins.len() >= MAX_PLUGINS
        && !state
            .plugins
            .iter()
            .any(|record| record.plugin_id == plugin_id)
    {
        return Err(PluginError::Limit(format!(
            "plugin registry cannot exceed {MAX_PLUGINS} plugins"
        )));
    }
    if state
        .plugins
        .iter()
        .find(|record| record.plugin_id == plugin_id)
        .is_some_and(|record| record.revisions.len() >= MAX_REVISIONS_PER_PLUGIN)
    {
        return Err(PluginError::Limit(format!(
            "plugin `{plugin_id}` cannot exceed {MAX_REVISIONS_PER_PLUGIN} revisions"
        )));
    }
    Ok(())
}

fn validate_state(state: &PluginRegistryState) -> Result<(), PluginError> {
    if state.schema_version != 1 {
        return Err(PluginError::InvalidState(
            "registry schema_version must be 1".to_owned(),
        ));
    }
    if state.plugins.len() > MAX_PLUGINS {
        return Err(PluginError::Limit("too many plugins".to_owned()));
    }
    let mut plugin_ids = BTreeSet::new();
    for record in &state.plugins {
        if !plugin_ids.insert(&record.plugin_id) {
            return Err(PluginError::InvalidState(format!(
                "duplicate plugin `{}`",
                record.plugin_id
            )));
        }
        if record.revisions.len() > MAX_REVISIONS_PER_PLUGIN {
            return Err(PluginError::Limit(format!(
                "plugin `{}` has too many revisions",
                record.plugin_id
            )));
        }
        let mut revision_ids = BTreeSet::new();
        let mut enabled = Vec::new();
        for revision in &record.revisions {
            validate_manifest(&revision.manifest)?;
            if revision.manifest.id != record.plugin_id {
                return Err(PluginError::InvalidState(format!(
                    "revision manifest id does not match `{}`",
                    record.plugin_id
                )));
            }
            if !revision_ids.insert(&revision.revision_id) {
                return Err(PluginError::InvalidState(format!(
                    "duplicate revision `{}`",
                    revision.revision_id
                )));
            }
            if revision.state == PluginRevisionState::Enabled {
                enabled.push(revision.revision_id.as_str());
            }
        }
        match record.active_revision_id.as_deref() {
            Some(active) if enabled == [active] => {}
            None if enabled.is_empty() => {}
            _ => {
                return Err(PluginError::InvalidState(format!(
                    "plugin `{}` active revision is inconsistent",
                    record.plugin_id
                )));
            }
        }
    }
    Ok(())
}

fn managed_path(root: &Path, relative: &str) -> Result<PathBuf, PluginError> {
    let relative = Path::new(relative);
    if relative.is_absolute()
        || relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(PluginError::Integrity(
            "managed package path is invalid".to_owned(),
        ));
    }
    let root = root
        .canonicalize()
        .map_err(|error| PluginError::Io(error.to_string()))?;
    let path = root.join(relative);
    let canonical = path
        .canonicalize()
        .map_err(|error| PluginError::Io(error.to_string()))?;
    if !canonical.starts_with(&root) || !canonical.is_dir() {
        return Err(PluginError::Integrity(
            "managed package path escapes the plugin store".to_owned(),
        ));
    }
    Ok(canonical)
}

fn find_record<'a>(
    state: &'a PluginRegistryState,
    plugin_id: &str,
) -> Result<&'a PluginRecord, PluginError> {
    state
        .plugins
        .iter()
        .find(|record| record.plugin_id == plugin_id)
        .ok_or_else(|| PluginError::NotFound(plugin_id.to_owned()))
}

fn find_record_mut<'a>(
    state: &'a mut PluginRegistryState,
    plugin_id: &str,
) -> Result<&'a mut PluginRecord, PluginError> {
    state
        .plugins
        .iter_mut()
        .find(|record| record.plugin_id == plugin_id)
        .ok_or_else(|| PluginError::NotFound(plugin_id.to_owned()))
}

fn find_revision<'a>(
    state: &'a PluginRegistryState,
    plugin_id: &str,
    revision_id: &str,
) -> Result<&'a PluginRevision, PluginError> {
    find_record(state, plugin_id)?
        .revisions
        .iter()
        .find(|revision| revision.revision_id == revision_id)
        .ok_or_else(|| PluginError::RevisionNotFound(revision_id.to_owned()))
}

fn find_revision_mut<'a>(
    state: &'a mut PluginRegistryState,
    plugin_id: &str,
    revision_id: &str,
) -> Result<&'a mut PluginRevision, PluginError> {
    find_record_mut(state, plugin_id)?
        .revisions
        .iter_mut()
        .find(|revision| revision.revision_id == revision_id)
        .ok_or_else(|| PluginError::RevisionNotFound(revision_id.to_owned()))
}

fn reject_symlink(path: &Path) -> Result<(), PluginError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(PluginError::Io(format!(
            "plugin path cannot be a symbolic link: {}",
            path.display()
        ))),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(PluginError::Io(error.to_string())),
    }
}

fn absolute_path(path: &Path) -> Result<PathBuf, PluginError> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .map_err(|error| PluginError::Io(error.to_string()))
    }
}

fn ensure_private_dir(path: &Path) -> Result<(), PluginError> {
    reject_symlink(path)?;
    fs::create_dir_all(path).map_err(|error| PluginError::Io(error.to_string()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .map_err(|error| PluginError::Io(error.to_string()))?;
    }
    Ok(())
}

fn set_owner_only_file(path: &Path, executable: bool) -> Result<(), PluginError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = if executable { 0o700 } else { 0o600 };
        fs::set_permissions(path, fs::Permissions::from_mode(mode))
            .map_err(|error| PluginError::Io(error.to_string()))?;
    }
    #[cfg(not(unix))]
    let _ = (path, executable);
    Ok(())
}

#[cfg(unix)]
fn executable(metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn executable(_metadata: &fs::Metadata) -> bool {
    false
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<(), PluginError> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| PluginError::Io(error.to_string()))
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> Result<(), PluginError> {
    Ok(())
}
