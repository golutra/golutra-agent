//! 内容寻址 release、稳定指针和本地 blue-green 生命周期。
//!
//! 这个 crate 只负责受信任控制面的文件与状态边界。候选工作区不能调用
//! `promote`、`rollback` 或写入 stable pointer；它只能作为 `BuildRequest` 的输入。

use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
};

use chrono::{DateTime, Utc};
use fs2::FileExt;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use uuid::Uuid;
use walkdir::{DirEntry, WalkDir};

mod builder;

pub use builder::{BuildArtifact, BuildCheck, BuildReport, BuildStatus, TrustedBuilder};

pub const RELEASE_SCHEMA_VERSION: u32 = 1;
pub const STATE_SCHEMA_VERSION: u32 = 2;
const MAX_RELEASE_FILES: usize = 100_000;
const MAX_RELEASE_BYTES: u64 = 512 * 1024 * 1024;
const MAX_POINTER_BYTES: u64 = 16 * 1024;
const MAX_MANIFEST_BYTES: u64 = 4 * 1024 * 1024;
const MAX_DEPLOYMENT_STATE_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug, Error)]
pub enum ReleaseError {
    #[error("release IO failed: {0}")]
    Io(String),
    #[error("release JSON failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("release is invalid: {0}")]
    Invalid(String),
    #[error("release {0} was not found")]
    NotFound(String),
    #[error("release state transition is invalid: {0}")]
    InvalidTransition(String),
    #[error("release control log integrity failed: {0}")]
    Integrity(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ArtifactDigest {
    pub relative_path: String,
    pub checksum: String,
    pub size_bytes: u64,
    #[serde(default)]
    pub executable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ReleaseManifest {
    pub release_id: String,
    pub parent_release_id: Option<String>,
    pub candidate_id: String,
    pub source_commit: String,
    pub source_digest: String,
    pub dependency_lock_digest: String,
    pub toolchain_digest: String,
    pub artifact_digests: Vec<ArtifactDigest>,
    pub protocol_version_range: String,
    pub state_schema_version_range: String,
    pub migration_plan_ref: Option<String>,
    pub provenance_ref: String,
    pub update_metadata_ref: String,
    pub rollback_release_id: Option<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ReleaseBuildRequest {
    pub candidate_id: String,
    pub parent_release_id: Option<String>,
    pub source_root: PathBuf,
    pub source_commit: String,
    pub dependency_lock_digest: String,
    pub toolchain_digest: String,
    pub protocol_version_range: String,
    pub state_schema_version_range: String,
    pub migration_plan_ref: Option<String>,
    pub provenance_ref: String,
    pub update_metadata_ref: String,
    pub rollback_release_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ReleasePointer {
    pub release_id: String,
    pub generation: u64,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DeploymentPhase {
    Preview,
    Canary,
    Promoted,
    RolledBack,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DeploymentRecord {
    pub sequence: u64,
    pub phase: DeploymentPhase,
    pub release_id: String,
    pub previous_release_id: Option<String>,
    pub reason: String,
    pub at: DateTime<Utc>,
    pub previous_digest: String,
    pub digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct PendingDeployment {
    record: DeploymentRecord,
    pointers: BTreeMap<String, Option<ReleasePointer>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct StateSchemaManifest {
    pub current_version: u32,
    pub minimum_reader_version: u32,
    pub minimum_writer_version: u32,
    pub forward_migrations: Vec<String>,
    pub rollback_strategy: String,
    pub irreversible: bool,
}

#[derive(Debug, Clone)]
pub struct ReleaseStore {
    root: PathBuf,
    releases_root: PathBuf,
    lock_path: PathBuf,
    deployment_log: PathBuf,
    deployment_pending: PathBuf,
}

impl ReleaseStore {
    pub fn new(root: impl Into<PathBuf>) -> Result<Self, ReleaseError> {
        let root = root.into();
        ensure_private_dir(&root)?;
        let releases_root = root.join("releases");
        ensure_private_dir(&releases_root)?;
        let store = Self {
            lock_path: root.join("release.lock"),
            deployment_log: root.join("deployment.jsonl"),
            deployment_pending: root.join("deployment.pending.json"),
            root,
            releases_root,
        };
        ensure_private_file(&store.lock_path)?;
        Ok(store)
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn state_schema_manifest(&self) -> StateSchemaManifest {
        StateSchemaManifest {
            current_version: STATE_SCHEMA_VERSION,
            minimum_reader_version: 1,
            minimum_writer_version: 1,
            forward_migrations: vec!["expand_contract_only".to_owned()],
            rollback_strategy: "restore_state_snapshot_before_pointer_switch".to_owned(),
            irreversible: false,
        }
    }

    pub fn build(&self, request: ReleaseBuildRequest) -> Result<ReleaseManifest, ReleaseError> {
        validate_build_request(&request)?;
        let source_root = canonical_directory(&request.source_root)?;
        let _lock = self.lock()?;
        let files = collect_release_files(&source_root)?;
        if files.is_empty() {
            return Err(ReleaseError::Invalid(
                "release source contains no eligible files".to_owned(),
            ));
        }
        let (source_digest, artifacts, total_bytes) = digest_files(&source_root, &files)?;
        if total_bytes > MAX_RELEASE_BYTES {
            return Err(ReleaseError::Invalid(format!(
                "release source exceeds {MAX_RELEASE_BYTES} bytes"
            )));
        }
        let release_id = format!(
            "release-{}",
            hex_digest(&[source_digest.as_bytes(), request.candidate_id.as_bytes()])
        );
        let release_dir = self.releases_root.join(&release_id);
        if release_dir.exists() {
            match self.read_manifest_locked(&release_id) {
                Ok(existing) => {
                    if !manifest_matches_request(&existing, &request, &source_digest) {
                        return Err(ReleaseError::Integrity(format!(
                            "content-addressed release {release_id} already contains different content"
                        )));
                    }
                    return Ok(existing);
                }
                Err(ReleaseError::NotFound(_)) if !release_dir.join("release.json").exists() => {
                    let metadata = fs::symlink_metadata(&release_dir).map_err(|error| {
                        ReleaseError::Io(format!("{}: {error}", release_dir.display()))
                    })?;
                    if metadata.file_type().is_symlink() || !metadata.is_dir() {
                        return Err(ReleaseError::Integrity(format!(
                            "incomplete release path is not a directory: {}",
                            release_dir.display()
                        )));
                    }
                    fs::remove_dir_all(&release_dir).map_err(|error| {
                        ReleaseError::Io(format!("{}: {error}", release_dir.display()))
                    })?;
                }
                Err(error) => return Err(error),
            }
        }
        let manifest = ReleaseManifest {
            release_id: release_id.clone(),
            parent_release_id: request.parent_release_id,
            candidate_id: request.candidate_id,
            source_commit: request.source_commit,
            source_digest,
            dependency_lock_digest: request.dependency_lock_digest,
            toolchain_digest: request.toolchain_digest,
            artifact_digests: artifacts.clone(),
            protocol_version_range: request.protocol_version_range,
            state_schema_version_range: request.state_schema_version_range,
            migration_plan_ref: request.migration_plan_ref,
            provenance_ref: request.provenance_ref,
            update_metadata_ref: request.update_metadata_ref,
            rollback_release_id: request.rollback_release_id,
            created_at: Utc::now(),
        };
        let staging_guard = tempfile::Builder::new()
            .prefix(".release-staging-")
            .tempdir_in(&self.releases_root)
            .map_err(|error| ReleaseError::Io(error.to_string()))?;
        let staging = staging_guard.path().to_path_buf();
        let source_dir = staging.join("source");
        let publish = (|| -> Result<(), ReleaseError> {
            ensure_private_dir(&source_dir)?;
            for entry in &files {
                let relative = entry
                    .path()
                    .strip_prefix(&source_root)
                    .map_err(|error| ReleaseError::Io(error.to_string()))?;
                let target = source_dir.join(relative);
                if entry.file_type().is_dir() {
                    ensure_private_dir(&target)?;
                } else {
                    if let Some(parent) = target.parent() {
                        ensure_private_dir(parent)?;
                    }
                    fs::copy(entry.path(), &target).map_err(|error| {
                        ReleaseError::Io(format!("{}: {error}", entry.path().display()))
                    })?;
                    set_owner_source_file(entry.path(), &target)?;
                }
            }
            let staged_files = collect_release_files(&source_dir)?;
            let (staged_digest, staged_artifacts, staged_bytes) =
                digest_files(&source_dir, &staged_files)?;
            if staged_digest != manifest.source_digest
                || staged_artifacts != artifacts
                || staged_bytes != total_bytes
            {
                return Err(ReleaseError::Integrity(
                    "release source changed while it was staged".to_owned(),
                ));
            }
            write_private_atomic(
                &staging.join("release.json"),
                &serde_json::to_vec_pretty(&manifest)?,
            )?;
            fs::rename(&staging, &release_dir).map_err(|error| {
                ReleaseError::Io(format!(
                    "failed to publish release {}: {error}",
                    release_dir.display()
                ))
            })?;
            sync_parent(&release_dir)
        })();
        drop(staging_guard);
        publish?;
        Ok(manifest)
    }

    pub fn build_checked(
        &self,
        request: ReleaseBuildRequest,
        report: &BuildReport,
        artifact_root: impl AsRef<Path>,
    ) -> Result<ReleaseManifest, ReleaseError> {
        if !report.passed || !report.sandbox_enforced {
            return Err(ReleaseError::Invalid(
                "release requires a passed OS-enforced trusted build report".to_owned(),
            ));
        }
        if report.binary_artifacts.is_empty() {
            return Err(ReleaseError::Invalid(
                "trusted build report contains no binary artifacts".to_owned(),
            ));
        }
        let expected_request = request.clone();
        let source_root = canonical_directory(&request.source_root)?;
        let artifact_root = canonical_directory(artifact_root.as_ref())?;
        let files = collect_release_files(&source_root)?;
        let (source_digest, _, _) = digest_files(&source_root, &files)?;
        if source_digest != report.source_digest {
            return Err(ReleaseError::Integrity(
                "trusted build report source digest does not match release source".to_owned(),
            ));
        }
        let release_id = format!(
            "release-{}",
            hex_digest(&[
                source_digest.as_bytes(),
                expected_request.candidate_id.as_bytes(),
            ])
        );
        let release_dir = self.releases_root.join(&release_id);
        if !release_dir.join("release.json").exists() {
            let manifest = self.build(request)?;
            if manifest.source_digest != report.source_digest || manifest.release_id != release_id {
                return Err(ReleaseError::Integrity(
                    "published release source does not match the trusted build report".to_owned(),
                ));
            }
        }
        let binary_staging_guard = tempfile::Builder::new()
            .prefix(".bin-staging-")
            .tempdir_in(&self.releases_root)
            .map_err(|error| ReleaseError::Io(error.to_string()))?;
        let binary_staging = binary_staging_guard.path().to_path_buf();
        ensure_private_dir(&binary_staging)?;
        let mut expected_binary_digests = Vec::with_capacity(report.binary_artifacts.len());
        for artifact in &report.binary_artifacts {
            let relative = Path::new(&artifact.relative_path);
            let components = relative.components().collect::<Vec<_>>();
            if components.len() != 3
                || components[0].as_os_str() != "target"
                || components[1].as_os_str() != "release"
            {
                return Err(ReleaseError::Invalid(format!(
                    "trusted build artifact path is invalid: {}",
                    artifact.relative_path
                )));
            }
            let source = artifact_root.join(relative);
            let canonical_source = source
                .canonicalize()
                .map_err(|error| ReleaseError::Io(format!("{}: {error}", source.display())))?;
            if !canonical_source.starts_with(&artifact_root) {
                return Err(ReleaseError::Invalid(format!(
                    "trusted build artifact escapes its root: {}",
                    artifact.relative_path
                )));
            }
            let metadata = fs::symlink_metadata(&source)
                .map_err(|error| ReleaseError::Io(format!("{}: {error}", source.display())))?;
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Err(ReleaseError::Invalid(format!(
                    "trusted build artifact is not a regular file: {}",
                    source.display()
                )));
            }
            if metadata.len() != artifact.size_bytes || metadata.len() > MAX_RELEASE_BYTES {
                return Err(ReleaseError::Integrity(format!(
                    "trusted build artifact size changed after verification: {}",
                    source.display()
                )));
            }
            let checksum = sha256_file(&canonical_source, MAX_RELEASE_BYTES)?;
            if checksum != artifact.checksum {
                return Err(ReleaseError::Integrity(format!(
                    "trusted build artifact changed after verification: {}",
                    source.display()
                )));
            }
            let file_name = relative.file_name().ok_or_else(|| {
                ReleaseError::Invalid("trusted build artifact has no file name".to_owned())
            })?;
            let file_name = file_name.to_str().ok_or_else(|| {
                ReleaseError::Invalid(
                    "trusted build artifact file name is not valid UTF-8".to_owned(),
                )
            })?;
            validate_binary_name(file_name)?;
            let target = binary_staging.join(file_name);
            fs::copy(&canonical_source, &target)
                .map_err(|error| ReleaseError::Io(error.to_string()))?;
            set_owner_executable(&target)?;
            let staged_metadata = fs::symlink_metadata(&target)
                .map_err(|error| ReleaseError::Io(format!("{}: {error}", target.display())))?;
            if staged_metadata.file_type().is_symlink()
                || !staged_metadata.is_file()
                || staged_metadata.len() != artifact.size_bytes
                || sha256_file(&target, MAX_RELEASE_BYTES)? != checksum
            {
                return Err(ReleaseError::Integrity(format!(
                    "trusted build artifact changed while it was staged: {}",
                    source.display()
                )));
            }
            expected_binary_digests.push(ArtifactDigest {
                relative_path: format!("bin/{file_name}"),
                checksum,
                size_bytes: artifact.size_bytes,
                executable: true,
            });
        }
        expected_binary_digests.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
        if expected_binary_digests
            .windows(2)
            .any(|pair| pair[0].relative_path == pair[1].relative_path)
        {
            return Err(ReleaseError::Invalid(
                "trusted build report contains duplicate binary names".to_owned(),
            ));
        }

        let install = (|| -> Result<ReleaseManifest, ReleaseError> {
            let _lock = self.lock()?;
            let mut manifest = self.read_manifest_file_locked(&release_id)?;
            if !manifest_matches_request(&manifest, &expected_request, &report.source_digest) {
                return Err(ReleaseError::Integrity(
                    "release manifest does not match the trusted build source".to_owned(),
                ));
            }
            let declared_source = self.verify_source_artifacts_locked(&release_id, &manifest)?;
            let mut existing_binary_digests = manifest
                .artifact_digests
                .iter()
                .filter(|artifact| artifact.relative_path.starts_with("bin/"))
                .cloned()
                .collect::<Vec<_>>();
            existing_binary_digests
                .sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
            if !existing_binary_digests.is_empty() {
                self.verify_manifest_artifacts_locked(&release_id, &manifest)?;
                if existing_binary_digests != expected_binary_digests {
                    return Err(ReleaseError::Integrity(
                        "content-addressed release already contains different binary artifacts"
                            .to_owned(),
                    ));
                }
                return Ok(manifest);
            }
            if declared_source.len() != manifest.artifact_digests.len() {
                return Err(ReleaseError::Integrity(
                    "source-only release manifest contains unknown artifacts".to_owned(),
                ));
            }

            let bin_dir = release_dir.join("bin");
            if bin_dir.exists() {
                self.verify_binary_directory_locked(&release_id, &expected_binary_digests)?;
                for artifact in &expected_binary_digests {
                    self.verify_binary_file_locked(
                        &release_id,
                        artifact.relative_path.trim_start_matches("bin/"),
                        artifact,
                    )?;
                }
            } else {
                fs::rename(&binary_staging, &bin_dir).map_err(|error| {
                    ReleaseError::Io(format!(
                        "failed to publish release binaries {}: {error}",
                        bin_dir.display()
                    ))
                })?;
                sync_parent(&bin_dir)?;
            }
            manifest
                .artifact_digests
                .extend(expected_binary_digests.clone());
            manifest
                .artifact_digests
                .sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
            write_private_atomic(
                &release_dir.join("release.json"),
                &serde_json::to_vec_pretty(&manifest)?,
            )?;
            self.verify_manifest_artifacts_locked(&release_id, &manifest)?;
            Ok(manifest)
        })();
        drop(binary_staging_guard);
        install
    }

    fn verify_binary_file_locked(
        &self,
        release_id: &str,
        binary_name: &str,
        artifact: &ArtifactDigest,
    ) -> Result<(), ReleaseError> {
        validate_binary_name(binary_name)?;
        let path = self
            .releases_root
            .join(release_id)
            .join("bin")
            .join(binary_name);
        let metadata = fs::symlink_metadata(&path)
            .map_err(|error| ReleaseError::Io(format!("{}: {error}", path.display())))?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(ReleaseError::Integrity(format!(
                "release binary is not a regular file: {}",
                path.display()
            )));
        }
        if metadata.len() != artifact.size_bytes
            || metadata.len() > MAX_RELEASE_BYTES
            || !source_file_is_executable(&path)?
            || sha256_file(&path, MAX_RELEASE_BYTES)? != artifact.checksum
        {
            return Err(ReleaseError::Integrity(format!(
                "release binary checksum mismatch: {}",
                path.display()
            )));
        }
        Ok(())
    }

    pub fn manifest(&self, release_id: &str) -> Result<ReleaseManifest, ReleaseError> {
        let _lock = self.lock()?;
        self.read_manifest_locked(release_id)
    }

    pub fn release_source(&self, release_id: &str) -> Result<PathBuf, ReleaseError> {
        self.manifest(release_id)?;
        let source = self.releases_root.join(release_id).join("source");
        if !source.is_dir() {
            return Err(ReleaseError::Integrity(format!(
                "release {release_id} has no immutable source directory"
            )));
        }
        Ok(source)
    }

    pub fn binary_path(
        &self,
        release_id: &str,
        binary_name: &str,
    ) -> Result<PathBuf, ReleaseError> {
        validate_release_id(release_id)?;
        validate_binary_name(binary_name)?;
        let manifest = self.manifest(release_id)?;
        let relative = format!("bin/{binary_name}");
        let artifact = manifest
            .artifact_digests
            .iter()
            .find(|artifact| artifact.relative_path == relative)
            .ok_or_else(|| ReleaseError::NotFound(format!("{release_id}/{binary_name}")))?;
        let path = self
            .releases_root
            .join(release_id)
            .join("bin")
            .join(binary_name);
        let metadata = fs::symlink_metadata(&path)
            .map_err(|error| ReleaseError::Io(format!("{}: {error}", path.display())))?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(ReleaseError::Integrity(format!(
                "release binary is not a regular file: {}",
                path.display()
            )));
        }
        if metadata.len() != artifact.size_bytes
            || metadata.len() > MAX_RELEASE_BYTES
            || sha256_file(&path, MAX_RELEASE_BYTES)? != artifact.checksum
        {
            return Err(ReleaseError::Integrity(format!(
                "release binary checksum mismatch: {}",
                path.display()
            )));
        }
        Ok(path)
    }

    pub fn stable_binary(&self, binary_name: &str) -> Result<PathBuf, ReleaseError> {
        let stable = self.pointer("stable")?.ok_or_else(|| {
            ReleaseError::InvalidTransition("no stable release is selected".to_owned())
        })?;
        self.binary_path(&stable.release_id, binary_name)
    }

    pub fn pointer(&self, name: &str) -> Result<Option<ReleasePointer>, ReleaseError> {
        validate_pointer_name(name)?;
        let _lock = self.lock()?;
        self.recover_deployment_locked()?;
        self.read_pointer_locked(name)
    }

    pub fn set_preview(&self, release_id: &str) -> Result<ReleasePointer, ReleaseError> {
        self.transition_pointer("preview", release_id, DeploymentPhase::Preview, "preview")
    }

    pub fn start_canary(&self, release_id: &str) -> Result<ReleasePointer, ReleaseError> {
        let _lock = self.lock()?;
        self.recover_deployment_locked()?;
        self.ensure_manifest_locked(release_id)?;
        let preview = self
            .read_pointer_locked("preview")?
            .ok_or_else(|| ReleaseError::InvalidTransition("canary requires preview".to_owned()))?;
        if preview.release_id != release_id {
            return Err(ReleaseError::InvalidTransition(
                "canary release must match preview pointer".to_owned(),
            ));
        }
        let mut pointers = self.load_pointer_state_locked()?;
        let pointer = Self::next_pointer(&pointers, "canary", release_id);
        pointers.insert("canary".to_owned(), Some(pointer.clone()));
        self.commit_deployment_locked(
            DeploymentPhase::Canary,
            release_id,
            self.read_pointer_locked("stable")?
                .map(|pointer| pointer.release_id),
            "canary started",
            pointers,
        )?;
        Ok(pointer)
    }

    pub fn promote(&self, release_id: &str, reason: &str) -> Result<ReleasePointer, ReleaseError> {
        if reason.trim().is_empty() {
            return Err(ReleaseError::Invalid(
                "promotion reason is required".to_owned(),
            ));
        }
        let _lock = self.lock()?;
        self.recover_deployment_locked()?;
        self.ensure_manifest_locked(release_id)?;
        let preview = self.read_pointer_locked("preview")?;
        let canary = self.read_pointer_locked("canary")?;
        if preview.as_ref().map(|pointer| pointer.release_id.clone()) != Some(release_id.to_owned())
            && canary.as_ref().map(|pointer| pointer.release_id.clone())
                != Some(release_id.to_owned())
        {
            return Err(ReleaseError::InvalidTransition(
                "promotion requires the release to be previewed or canaried".to_owned(),
            ));
        }
        let previous = self.read_pointer_locked("stable")?;
        let mut pointers = self.load_pointer_state_locked()?;
        if let Some(previous) = &previous {
            let previous_pointer =
                Self::next_pointer(&pointers, "previous-stable", &previous.release_id);
            pointers.insert("previous-stable".to_owned(), Some(previous_pointer));
        }
        let pointer = Self::next_pointer(&pointers, "stable", release_id);
        pointers.insert("stable".to_owned(), Some(pointer.clone()));
        pointers.insert("preview".to_owned(), None);
        pointers.insert("canary".to_owned(), None);
        self.commit_deployment_locked(
            DeploymentPhase::Promoted,
            release_id,
            previous.map(|pointer| pointer.release_id),
            reason,
            pointers,
        )?;
        Ok(pointer)
    }

    pub fn cancel_canary(
        &self,
        release_id: &str,
        reason: &str,
    ) -> Result<ReleasePointer, ReleaseError> {
        if reason.trim().is_empty() {
            return Err(ReleaseError::Invalid(
                "rollback reason is required".to_owned(),
            ));
        }
        let _lock = self.lock()?;
        self.recover_deployment_locked()?;
        self.ensure_manifest_locked(release_id)?;
        let canary = self.read_pointer_locked("canary")?.ok_or_else(|| {
            ReleaseError::InvalidTransition("no active canary to cancel".to_owned())
        })?;
        if canary.release_id != release_id {
            return Err(ReleaseError::InvalidTransition(
                "rollback release must match the active canary pointer".to_owned(),
            ));
        }
        let stable = self.read_pointer_locked("stable")?.ok_or_else(|| {
            ReleaseError::InvalidTransition(
                "canary rollback requires an existing stable release".to_owned(),
            )
        })?;
        let mut pointers = self.load_pointer_state_locked()?;
        pointers.insert("preview".to_owned(), None);
        pointers.insert("canary".to_owned(), None);
        self.commit_deployment_locked(
            DeploymentPhase::RolledBack,
            &stable.release_id,
            Some(release_id.to_owned()),
            reason,
            pointers,
        )?;
        Ok(stable)
    }

    pub fn rollback(&self, reason: &str) -> Result<ReleasePointer, ReleaseError> {
        if reason.trim().is_empty() {
            return Err(ReleaseError::Invalid(
                "rollback reason is required".to_owned(),
            ));
        }
        let _lock = self.lock()?;
        self.recover_deployment_locked()?;
        let previous = self
            .read_pointer_locked("previous-stable")?
            .or(self.read_pointer_locked("stable")?)
            .ok_or_else(|| {
                ReleaseError::InvalidTransition("no stable release to restore".to_owned())
            })?;
        self.ensure_manifest_locked(&previous.release_id)?;
        let current = self.read_pointer_locked("stable")?;
        let mut pointers = self.load_pointer_state_locked()?;
        let pointer = Self::next_pointer(&pointers, "stable", &previous.release_id);
        pointers.insert("stable".to_owned(), Some(pointer.clone()));
        pointers.insert("preview".to_owned(), None);
        pointers.insert("canary".to_owned(), None);
        self.commit_deployment_locked(
            DeploymentPhase::RolledBack,
            &previous.release_id,
            current.map(|pointer| pointer.release_id),
            reason,
            pointers,
        )?;
        Ok(pointer)
    }

    pub fn verify_deployment_log(&self) -> Result<Vec<DeploymentRecord>, ReleaseError> {
        let _lock = self.lock()?;
        self.recover_deployment_locked()?;
        self.verify_deployment_log_locked()
    }

    fn verify_deployment_log_locked(&self) -> Result<Vec<DeploymentRecord>, ReleaseError> {
        if !self.deployment_log.exists() {
            return Ok(Vec::new());
        }
        let metadata = fs::symlink_metadata(&self.deployment_log)
            .map_err(|error| ReleaseError::Io(error.to_string()))?;
        if metadata.file_type().is_symlink()
            || !metadata.is_file()
            || metadata.len() > MAX_DEPLOYMENT_STATE_BYTES
        {
            return Err(ReleaseError::Integrity(
                "deployment log violates its file boundary".to_owned(),
            ));
        }
        let content = fs::read_to_string(&self.deployment_log)
            .map_err(|error| ReleaseError::Io(error.to_string()))?;
        verify_deployment_log_content(&content)
    }

    fn transition_pointer(
        &self,
        pointer_name: &str,
        release_id: &str,
        phase: DeploymentPhase,
        reason: &str,
    ) -> Result<ReleasePointer, ReleaseError> {
        let _lock = self.lock()?;
        self.recover_deployment_locked()?;
        self.ensure_manifest_locked(release_id)?;
        if pointer_name == "preview" {
            for active in ["preview", "canary"] {
                if self
                    .read_pointer_locked(active)?
                    .is_some_and(|pointer| pointer.release_id != release_id)
                {
                    return Err(ReleaseError::InvalidTransition(format!(
                        "cannot replace an active {active} release"
                    )));
                }
            }
        }
        let mut pointers = self.load_pointer_state_locked()?;
        let pointer = Self::next_pointer(&pointers, pointer_name, release_id);
        pointers.insert(pointer_name.to_owned(), Some(pointer.clone()));
        self.commit_deployment_locked(phase, release_id, None, reason, pointers)?;
        Ok(pointer)
    }

    fn lock(&self) -> Result<File, ReleaseError> {
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&self.lock_path)
            .map_err(|error| ReleaseError::Io(error.to_string()))?;
        file.lock_exclusive()
            .map_err(|error| ReleaseError::Io(error.to_string()))?;
        Ok(file)
    }

    fn ensure_manifest_locked(&self, release_id: &str) -> Result<ReleaseManifest, ReleaseError> {
        self.read_manifest_locked(release_id)
    }

    fn read_manifest_locked(&self, release_id: &str) -> Result<ReleaseManifest, ReleaseError> {
        let manifest = self.read_manifest_file_locked(release_id)?;
        self.verify_manifest_artifacts_locked(release_id, &manifest)?;
        Ok(manifest)
    }

    fn read_manifest_file_locked(&self, release_id: &str) -> Result<ReleaseManifest, ReleaseError> {
        validate_release_id(release_id)?;
        let path = self.releases_root.join(release_id).join("release.json");
        let metadata = fs::symlink_metadata(&path).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                ReleaseError::NotFound(release_id.to_owned())
            } else {
                ReleaseError::Io(format!("{}: {error}", path.display()))
            }
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(ReleaseError::Integrity(format!(
                "release manifest is not a regular file: {}",
                path.display()
            )));
        }
        if metadata.len() > MAX_MANIFEST_BYTES {
            return Err(ReleaseError::Integrity(format!(
                "release manifest exceeds its size limit: {}",
                path.display()
            )));
        }
        let bytes = fs::read(&path).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                ReleaseError::NotFound(release_id.to_owned())
            } else {
                ReleaseError::Io(format!("{}: {error}", path.display()))
            }
        })?;
        let manifest: ReleaseManifest = serde_json::from_slice(&bytes)?;
        if manifest.release_id != release_id {
            return Err(ReleaseError::Integrity(format!(
                "release manifest identity does not match directory {release_id}"
            )));
        }
        Ok(manifest)
    }

    fn verify_manifest_artifacts_locked(
        &self,
        release_id: &str,
        manifest: &ReleaseManifest,
    ) -> Result<(), ReleaseError> {
        let declared_source = self.verify_source_artifacts_locked(release_id, manifest)?;
        let mut declared_binaries = manifest
            .artifact_digests
            .iter()
            .filter(|artifact| artifact.relative_path.starts_with("bin/"))
            .cloned()
            .collect::<Vec<_>>();
        if declared_source
            .len()
            .saturating_add(declared_binaries.len())
            != manifest.artifact_digests.len()
        {
            return Err(ReleaseError::Integrity(
                "release manifest contains an unknown artifact namespace".to_owned(),
            ));
        }
        declared_binaries.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
        if declared_binaries
            .windows(2)
            .any(|pair| pair[0].relative_path == pair[1].relative_path)
        {
            return Err(ReleaseError::Integrity(
                "release manifest contains duplicate binary artifacts".to_owned(),
            ));
        }
        for artifact in &declared_binaries {
            let binary_name = artifact.relative_path.strip_prefix("bin/").ok_or_else(|| {
                ReleaseError::Integrity("release binary namespace is invalid".to_owned())
            })?;
            validate_binary_name(binary_name)?;
            self.verify_binary_file_locked(release_id, binary_name, artifact)?;
        }
        self.verify_binary_directory_locked(release_id, &declared_binaries)
    }

    fn verify_source_artifacts_locked(
        &self,
        release_id: &str,
        manifest: &ReleaseManifest,
    ) -> Result<Vec<ArtifactDigest>, ReleaseError> {
        let source = self.releases_root.join(release_id).join("source");
        let source_metadata = fs::symlink_metadata(&source)
            .map_err(|error| ReleaseError::Io(format!("{}: {error}", source.display())))?;
        if source_metadata.file_type().is_symlink() || !source_metadata.is_dir() {
            return Err(ReleaseError::Integrity(format!(
                "release source is not an immutable directory: {}",
                source.display()
            )));
        }
        let entries = collect_release_files(&source)?;
        let (source_digest, source_artifacts, total_bytes) = digest_files(&source, &entries)?;
        if total_bytes > MAX_RELEASE_BYTES || source_digest != manifest.source_digest {
            return Err(ReleaseError::Integrity(format!(
                "release source digest mismatch for {release_id}"
            )));
        }
        let mut declared_source = manifest
            .artifact_digests
            .iter()
            .filter(|artifact| artifact.relative_path.starts_with("source/"))
            .cloned()
            .collect::<Vec<_>>();
        declared_source.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
        if declared_source != source_artifacts {
            return Err(ReleaseError::Integrity(format!(
                "release source artifact manifest mismatch for {release_id}"
            )));
        }
        Ok(declared_source)
    }

    fn verify_binary_directory_locked(
        &self,
        release_id: &str,
        declared: &[ArtifactDigest],
    ) -> Result<(), ReleaseError> {
        let bin_dir = self.releases_root.join(release_id).join("bin");
        if !bin_dir.exists() {
            return if declared.is_empty() {
                Ok(())
            } else {
                Err(ReleaseError::Integrity(
                    "release binary directory is missing".to_owned(),
                ))
            };
        }
        let metadata = fs::symlink_metadata(&bin_dir)
            .map_err(|error| ReleaseError::Io(format!("{}: {error}", bin_dir.display())))?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(ReleaseError::Integrity(
                "release binary path is not a directory".to_owned(),
            ));
        }
        let mut actual = fs::read_dir(&bin_dir)
            .map_err(|error| ReleaseError::Io(error.to_string()))?
            .map(|entry| {
                let entry = entry.map_err(|error| ReleaseError::Io(error.to_string()))?;
                let metadata = entry
                    .file_type()
                    .map_err(|error| ReleaseError::Io(error.to_string()))?;
                if !metadata.is_file() || metadata.is_symlink() {
                    return Err(ReleaseError::Integrity(format!(
                        "release binary directory contains a non-file entry: {}",
                        entry.path().display()
                    )));
                }
                Ok(format!("bin/{}", entry.file_name().to_string_lossy()))
            })
            .collect::<Result<Vec<_>, ReleaseError>>()?;
        actual.sort();
        let expected = declared
            .iter()
            .map(|artifact| artifact.relative_path.clone())
            .collect::<Vec<_>>();
        if actual != expected {
            return Err(ReleaseError::Integrity(
                "release binary directory does not match its manifest".to_owned(),
            ));
        }
        Ok(())
    }

    fn read_pointer_locked(&self, name: &str) -> Result<Option<ReleasePointer>, ReleaseError> {
        self.read_pointer_file(name)
    }

    fn read_pointer_file(&self, name: &str) -> Result<Option<ReleasePointer>, ReleaseError> {
        validate_pointer_name(name)?;
        let path = self.root.join(name);
        if !path.exists() {
            return Ok(None);
        }
        let metadata = fs::symlink_metadata(&path)
            .map_err(|error| ReleaseError::Io(format!("{}: {error}", path.display())))?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(ReleaseError::Integrity(format!(
                "release pointer is not a regular file: {}",
                path.display()
            )));
        }
        if metadata.len() > MAX_POINTER_BYTES {
            return Err(ReleaseError::Integrity(format!(
                "release pointer is too large: {}",
                path.display()
            )));
        }
        let bytes = fs::read(&path).map_err(|error| ReleaseError::Io(error.to_string()))?;
        Ok(Some(serde_json::from_slice(&bytes)?))
    }

    fn remove_pointer_locked(&self, name: &str) -> Result<(), ReleaseError> {
        validate_pointer_name(name)?;
        let path = self.root.join(name);
        match fs::remove_file(path) {
            Ok(()) => sync_parent(&self.root.join(name)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(ReleaseError::Io(error.to_string())),
        }
    }

    fn load_pointer_state_locked(
        &self,
    ) -> Result<BTreeMap<String, Option<ReleasePointer>>, ReleaseError> {
        ["stable", "previous-stable", "preview", "canary"]
            .into_iter()
            .map(|name| Ok((name.to_owned(), self.read_pointer_locked(name)?)))
            .collect()
    }

    fn next_pointer(
        pointers: &BTreeMap<String, Option<ReleasePointer>>,
        name: &str,
        release_id: &str,
    ) -> ReleasePointer {
        ReleasePointer {
            release_id: release_id.to_owned(),
            generation: pointers
                .get(name)
                .and_then(Option::as_ref)
                .map_or(0, |pointer| pointer.generation)
                .saturating_add(1),
            updated_at: Utc::now(),
        }
    }

    fn commit_deployment_locked(
        &self,
        phase: DeploymentPhase,
        release_id: &str,
        previous_release_id: Option<String>,
        reason: &str,
        pointers: BTreeMap<String, Option<ReleasePointer>>,
    ) -> Result<(), ReleaseError> {
        let records = self.verify_deployment_log_locked()?;
        let previous_digest = records
            .last()
            .map_or_else(String::new, |record| record.digest.clone());
        let mut record = DeploymentRecord {
            sequence: u64::try_from(records.len())
                .unwrap_or(u64::MAX)
                .saturating_add(1),
            phase,
            release_id: release_id.to_owned(),
            previous_release_id,
            reason: bounded_text(reason, 512),
            at: Utc::now(),
            previous_digest,
            digest: String::new(),
        };
        record.digest = deployment_digest(&record, &record.previous_digest)?;
        let pending = PendingDeployment { record, pointers };
        let bytes = serde_json::to_vec(&pending)?;
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_DEPLOYMENT_STATE_BYTES {
            return Err(ReleaseError::Invalid(
                "pending deployment exceeds its size limit".to_owned(),
            ));
        }
        write_private_atomic(&self.deployment_pending, &bytes)?;
        self.append_prepared_deployment_locked(&pending.record, &records)?;
        self.apply_pointer_state_locked(&pending.pointers)?;
        self.remove_deployment_pending_locked()
    }

    fn append_prepared_deployment_locked(
        &self,
        record: &DeploymentRecord,
        records: &[DeploymentRecord],
    ) -> Result<(), ReleaseError> {
        let expected_sequence = u64::try_from(records.len())
            .unwrap_or(u64::MAX)
            .saturating_add(1);
        let expected_previous = records
            .last()
            .map_or_else(String::new, |record| record.digest.clone());
        if record.sequence != expected_sequence
            || record.previous_digest != expected_previous
            || deployment_digest(record, &record.previous_digest)? != record.digest
        {
            return Err(ReleaseError::Integrity(
                "pending deployment does not extend the deployment log".to_owned(),
            ));
        }
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.deployment_log)
            .map_err(|error| ReleaseError::Io(error.to_string()))?;
        set_owner_only_file(&self.deployment_log)?;
        let line = serde_json::to_vec(record)?;
        let current_size = file
            .metadata()
            .map_err(|error| ReleaseError::Io(error.to_string()))?
            .len();
        let appended_size = u64::try_from(line.len().saturating_add(1)).unwrap_or(u64::MAX);
        if current_size.saturating_add(appended_size) > MAX_DEPLOYMENT_STATE_BYTES {
            return Err(ReleaseError::Invalid(
                "deployment log exceeds its size limit".to_owned(),
            ));
        }
        file.write_all(&line)
            .and_then(|_| file.write_all(b"\n"))
            .and_then(|_| file.sync_all())
            .map_err(|error| ReleaseError::Io(error.to_string()))?;
        Ok(())
    }

    fn apply_pointer_state_locked(
        &self,
        pointers: &BTreeMap<String, Option<ReleasePointer>>,
    ) -> Result<(), ReleaseError> {
        for name in ["previous-stable", "stable", "preview", "canary"] {
            let pointer = pointers.get(name).ok_or_else(|| {
                ReleaseError::Integrity(format!("pending deployment has no {name} pointer"))
            })?;
            match pointer {
                Some(pointer) => {
                    self.ensure_manifest_locked(&pointer.release_id)?;
                    write_private_atomic(&self.root.join(name), &serde_json::to_vec(pointer)?)?;
                }
                None => self.remove_pointer_locked(name)?,
            }
        }
        Ok(())
    }

    fn recover_deployment_locked(&self) -> Result<(), ReleaseError> {
        let Some(pending) = read_pending_deployment(&self.deployment_pending)? else {
            return self.verify_pointer_state_against_log_locked();
        };
        if deployment_digest(&pending.record, &pending.record.previous_digest)?
            != pending.record.digest
        {
            return Err(ReleaseError::Integrity(
                "pending deployment digest is invalid".to_owned(),
            ));
        }
        let records = match self.verify_deployment_log_locked() {
            Ok(records) => records,
            Err(error) => self.recover_interrupted_deployment_append_locked(&pending, error)?,
        };
        if records
            .last()
            .is_some_and(|record| record.sequence == pending.record.sequence)
        {
            if records.last() != Some(&pending.record) {
                return Err(ReleaseError::Integrity(
                    "deployment log conflicts with pending deployment".to_owned(),
                ));
            }
        } else {
            self.append_prepared_deployment_locked(&pending.record, &records)?;
        }
        self.apply_pointer_state_locked(&pending.pointers)?;
        self.remove_deployment_pending_locked()?;
        self.verify_pointer_state_against_log_locked()
    }

    fn verify_pointer_state_against_log_locked(&self) -> Result<(), ReleaseError> {
        let records = self.verify_deployment_log_locked()?;
        let mut expected = ["stable", "previous-stable", "preview", "canary"]
            .into_iter()
            .map(|name| (name.to_owned(), None::<String>))
            .collect::<BTreeMap<_, _>>();
        for record in records {
            match record.phase {
                DeploymentPhase::Preview => {
                    expected.insert("preview".to_owned(), Some(record.release_id));
                }
                DeploymentPhase::Canary => {
                    expected.insert("canary".to_owned(), Some(record.release_id));
                }
                DeploymentPhase::Promoted => {
                    expected.insert("stable".to_owned(), Some(record.release_id));
                    expected.insert("previous-stable".to_owned(), record.previous_release_id);
                    expected.insert("preview".to_owned(), None);
                    expected.insert("canary".to_owned(), None);
                }
                DeploymentPhase::RolledBack => {
                    expected.insert("stable".to_owned(), Some(record.release_id));
                    expected.insert("preview".to_owned(), None);
                    expected.insert("canary".to_owned(), None);
                }
            }
        }
        for (name, expected_release_id) in expected {
            let actual = self
                .read_pointer_locked(&name)?
                .map(|pointer| pointer.release_id);
            if actual != expected_release_id {
                return Err(ReleaseError::Integrity(format!(
                    "release pointer {name} does not match the deployment log"
                )));
            }
        }
        Ok(())
    }

    fn recover_interrupted_deployment_append_locked(
        &self,
        pending: &PendingDeployment,
        original_error: ReleaseError,
    ) -> Result<Vec<DeploymentRecord>, ReleaseError> {
        let bytes =
            fs::read(&self.deployment_log).map_err(|error| ReleaseError::Io(error.to_string()))?;
        let prefix_len = bytes
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(0, |index| index.saturating_add(1));
        let trailing = &bytes[prefix_len..];
        let pending_line = serde_json::to_vec(&pending.record)?;
        if trailing.is_empty() || !pending_line.starts_with(trailing) {
            return Err(original_error);
        }
        let Ok(prefix) = std::str::from_utf8(&bytes[..prefix_len]) else {
            return Err(original_error);
        };
        let records = verify_deployment_log_content(prefix)?;
        let expected_sequence = u64::try_from(records.len())
            .unwrap_or(u64::MAX)
            .saturating_add(1);
        let expected_previous = records
            .last()
            .map_or_else(String::new, |record| record.digest.clone());
        if pending.record.sequence != expected_sequence
            || pending.record.previous_digest != expected_previous
        {
            return Err(original_error);
        }
        let file = OpenOptions::new()
            .write(true)
            .open(&self.deployment_log)
            .map_err(|error| ReleaseError::Io(error.to_string()))?;
        file.set_len(u64::try_from(prefix_len).unwrap_or(u64::MAX))
            .and_then(|_| file.sync_all())
            .map_err(|error| ReleaseError::Io(error.to_string()))?;
        Ok(records)
    }

    fn remove_deployment_pending_locked(&self) -> Result<(), ReleaseError> {
        match fs::remove_file(&self.deployment_pending) {
            Ok(()) => sync_parent(&self.deployment_pending),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(ReleaseError::Io(error.to_string())),
        }
    }
}

fn verify_deployment_log_content(content: &str) -> Result<Vec<DeploymentRecord>, ReleaseError> {
    let mut previous_digest = String::new();
    let mut records = Vec::new();
    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let record: DeploymentRecord = serde_json::from_str(line)?;
        let expected_sequence = u64::try_from(records.len())
            .unwrap_or(u64::MAX)
            .saturating_add(1);
        if record.sequence != expected_sequence || record.previous_digest != previous_digest {
            return Err(ReleaseError::Integrity(format!(
                "deployment log chain is broken at record {}",
                record.sequence
            )));
        }
        let digest = deployment_digest(&record, &record.previous_digest)?;
        if digest != record.digest {
            return Err(ReleaseError::Integrity(format!(
                "deployment log digest mismatch at record {}",
                record.sequence
            )));
        }
        previous_digest = record.digest.clone();
        records.push(record);
    }
    Ok(records)
}

fn read_pending_deployment(path: &Path) -> Result<Option<PendingDeployment>, ReleaseError> {
    if !path.exists() {
        return Ok(None);
    }
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| ReleaseError::Io(format!("{}: {error}", path.display())))?;
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.len() > MAX_DEPLOYMENT_STATE_BYTES
    {
        return Err(ReleaseError::Integrity(
            "pending deployment violates its file boundary".to_owned(),
        ));
    }
    let bytes =
        fs::read(path).map_err(|error| ReleaseError::Io(format!("{}: {error}", path.display())))?;
    Ok(Some(serde_json::from_slice(&bytes)?))
}

fn manifest_matches_request(
    manifest: &ReleaseManifest,
    request: &ReleaseBuildRequest,
    source_digest: &str,
) -> bool {
    manifest.source_digest == source_digest
        && manifest.parent_release_id == request.parent_release_id
        && manifest.candidate_id == request.candidate_id
        && manifest.source_commit == request.source_commit
        && manifest.dependency_lock_digest == request.dependency_lock_digest
        && manifest.toolchain_digest == request.toolchain_digest
        && manifest.protocol_version_range == request.protocol_version_range
        && manifest.state_schema_version_range == request.state_schema_version_range
        && manifest.migration_plan_ref == request.migration_plan_ref
        && manifest.provenance_ref == request.provenance_ref
        && manifest.update_metadata_ref == request.update_metadata_ref
        && manifest.rollback_release_id == request.rollback_release_id
}

fn validate_build_request(request: &ReleaseBuildRequest) -> Result<(), ReleaseError> {
    for (name, value) in [
        ("candidate_id", request.candidate_id.as_str()),
        ("source_commit", request.source_commit.as_str()),
        (
            "dependency_lock_digest",
            request.dependency_lock_digest.as_str(),
        ),
        ("toolchain_digest", request.toolchain_digest.as_str()),
        (
            "protocol_version_range",
            request.protocol_version_range.as_str(),
        ),
        (
            "state_schema_version_range",
            request.state_schema_version_range.as_str(),
        ),
        ("provenance_ref", request.provenance_ref.as_str()),
        ("update_metadata_ref", request.update_metadata_ref.as_str()),
    ] {
        if value.trim().is_empty() || value.len() > 512 {
            return Err(ReleaseError::Invalid(format!(
                "{name} must be non-empty and bounded"
            )));
        }
    }
    validate_identifier(&request.candidate_id, "candidate_id")?;
    Ok(())
}

fn validate_release_id(value: &str) -> Result<(), ReleaseError> {
    validate_identifier(value, "release_id")
}

fn validate_pointer_name(value: &str) -> Result<(), ReleaseError> {
    if !matches!(value, "stable" | "previous-stable" | "preview" | "canary") {
        return Err(ReleaseError::Invalid(format!(
            "unsupported release pointer `{value}`"
        )));
    }
    Ok(())
}

fn validate_binary_name(value: &str) -> Result<(), ReleaseError> {
    if value.is_empty()
        || value.len() > 128
        || value.contains('/')
        || value.contains('\\')
        || value.contains("..")
        || value.chars().any(char::is_whitespace)
    {
        return Err(ReleaseError::Invalid(
            "release binary name is unsafe".to_owned(),
        ));
    }
    Ok(())
}

fn validate_identifier(value: &str, name: &str) -> Result<(), ReleaseError> {
    if value.is_empty()
        || value.len() > 256
        || value.contains('/')
        || value.contains('\\')
        || value.contains("..")
        || value.chars().any(char::is_whitespace)
    {
        return Err(ReleaseError::Invalid(format!(
            "{name} is not a safe identifier"
        )));
    }
    Ok(())
}

fn canonical_directory(path: &Path) -> Result<PathBuf, ReleaseError> {
    let canonical = path
        .canonicalize()
        .map_err(|error| ReleaseError::Io(format!("{}: {error}", path.display())))?;
    if !canonical.is_dir() {
        return Err(ReleaseError::Invalid(format!(
            "source root is not a directory: {}",
            canonical.display()
        )));
    }
    Ok(canonical)
}

fn collect_release_files(source_root: &Path) -> Result<Vec<DirEntry>, ReleaseError> {
    let mut entries = WalkDir::new(source_root)
        .follow_links(false)
        .into_iter()
        .filter_entry(included_entry)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| ReleaseError::Io(error.to_string()))?;
    if entries.len() > MAX_RELEASE_FILES {
        return Err(ReleaseError::Invalid(format!(
            "release source exceeds {MAX_RELEASE_FILES} entries"
        )));
    }
    entries.sort_by_key(|entry| entry.path().to_path_buf());
    for entry in &entries {
        if entry.depth() > 0 && entry.file_type().is_symlink() {
            return Err(ReleaseError::Invalid(format!(
                "release source contains symlink: {}",
                entry.path().display()
            )));
        }
    }
    Ok(entries)
}

fn included_entry(entry: &DirEntry) -> bool {
    if entry.depth() == 0 {
        return true;
    }
    // 保留符号链接条目，让后续校验明确拒绝，而不是静默把越界引用当成不存在。
    if entry.file_type().is_symlink() {
        return true;
    }
    !matches!(
        entry.file_name().to_str(),
        Some(".git" | ".golutra" | "target" | "node_modules")
    )
}

fn digest_files(
    source_root: &Path,
    entries: &[DirEntry],
) -> Result<(String, Vec<ArtifactDigest>, u64), ReleaseError> {
    let mut digest = Sha256::new();
    let mut artifacts = Vec::new();
    let mut total_bytes = 0_u64;
    for entry in entries.iter().filter(|entry| entry.file_type().is_file()) {
        let relative = entry
            .path()
            .strip_prefix(source_root)
            .map_err(|error| ReleaseError::Io(error.to_string()))?;
        let relative_text = normalized_release_relative_path(relative)?;
        let (file_digest, size_bytes) = sha256_file_digest(entry.path(), MAX_RELEASE_BYTES)?;
        total_bytes = total_bytes.saturating_add(size_bytes);
        if total_bytes > MAX_RELEASE_BYTES {
            return Err(ReleaseError::Invalid(format!(
                "release source exceeds {MAX_RELEASE_BYTES} bytes"
            )));
        }
        digest.update(relative_text.as_bytes());
        digest.update([0]);
        digest.update(file_digest);
        artifacts.push(ArtifactDigest {
            relative_path: format!("source/{relative_text}"),
            checksum: sha256_label(&file_digest),
            size_bytes,
            executable: source_file_is_executable(entry.path())?,
        });
    }
    artifacts.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
    Ok((
        format!("sha256:{:x}", digest.finalize()),
        artifacts,
        total_bytes,
    ))
}

fn normalized_release_relative_path(path: &Path) -> Result<String, ReleaseError> {
    let mut components = Vec::new();
    for component in path.components() {
        let Component::Normal(component) = component else {
            return Err(ReleaseError::Invalid(format!(
                "release source contains a non-normal path: {}",
                path.display()
            )));
        };
        let component = component.to_str().ok_or_else(|| {
            ReleaseError::Invalid(format!(
                "release source path is not valid UTF-8: {}",
                path.display()
            ))
        })?;
        if component.contains('\\') || component.chars().any(char::is_control) {
            return Err(ReleaseError::Invalid(format!(
                "release source path contains ambiguous characters: {}",
                path.display()
            )));
        }
        components.push(component);
    }
    if components.is_empty() {
        return Err(ReleaseError::Invalid(
            "release source relative path is empty".to_owned(),
        ));
    }
    Ok(components.join("/"))
}

fn sha256_file(path: &Path, max_bytes: u64) -> Result<String, ReleaseError> {
    let (digest, _) = sha256_file_digest(path, max_bytes)?;
    Ok(sha256_label(&digest))
}

fn sha256_file_digest(path: &Path, max_bytes: u64) -> Result<([u8; 32], u64), ReleaseError> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| ReleaseError::Io(format!("{}: {error}", path.display())))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(ReleaseError::Integrity(format!(
            "artifact is not a regular file: {}",
            path.display()
        )));
    }
    if metadata.len() > max_bytes {
        return Err(ReleaseError::Invalid(format!(
            "artifact exceeds {max_bytes} bytes: {}",
            path.display()
        )));
    }
    let mut file = File::open(path)
        .map_err(|error| ReleaseError::Io(format!("{}: {error}", path.display())))?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    let mut total = 0_u64;
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| ReleaseError::Io(format!("{}: {error}", path.display())))?;
        if read == 0 {
            break;
        }
        total = total.saturating_add(u64::try_from(read).unwrap_or(u64::MAX));
        if total > max_bytes {
            return Err(ReleaseError::Invalid(format!(
                "artifact exceeds {max_bytes} bytes: {}",
                path.display()
            )));
        }
        digest.update(&buffer[..read]);
    }
    if total != metadata.len() {
        return Err(ReleaseError::Integrity(format!(
            "artifact size changed while hashing: {}",
            path.display()
        )));
    }
    Ok((digest.finalize().into(), total))
}

fn sha256_label(digest: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut value = String::with_capacity(7 + digest.len().saturating_mul(2));
    value.push_str("sha256:");
    for byte in digest {
        value.push(char::from(HEX[usize::from(byte >> 4)]));
        value.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    value
}

fn hex_digest(parts: &[&[u8]]) -> String {
    let mut digest = Sha256::new();
    for part in parts {
        digest.update(part);
        digest.update([0]);
    }
    format!("{:x}", digest.finalize())
}

fn deployment_digest(
    record: &DeploymentRecord,
    previous_digest: &str,
) -> Result<String, ReleaseError> {
    let mut unsigned = record.clone();
    unsigned.digest.clear();
    unsigned.previous_digest = previous_digest.to_owned();
    let bytes = serde_json::to_vec(&unsigned)?;
    Ok(format!("sha256:{:x}", Sha256::digest(bytes)))
}

fn bounded_text(value: &str, max: usize) -> String {
    value.chars().take(max).collect()
}

fn ensure_private_dir(path: &Path) -> Result<(), ReleaseError> {
    fs::create_dir_all(path).map_err(|error| ReleaseError::Io(error.to_string()))?;
    set_owner_only_dir(path)
}

fn ensure_private_file(path: &Path) -> Result<(), ReleaseError> {
    match OpenOptions::new().create_new(true).write(true).open(path) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(ReleaseError::Io(error.to_string())),
    }
    let metadata =
        fs::symlink_metadata(path).map_err(|error| ReleaseError::Io(error.to_string()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(ReleaseError::Integrity(format!(
            "private lock path is not a regular file: {}",
            path.display()
        )));
    }
    set_owner_only_file(path)
}

fn write_private_atomic(path: &Path, bytes: &[u8]) -> Result<(), ReleaseError> {
    if let Some(parent) = path.parent() {
        ensure_private_dir(parent)?;
    }
    let temp = path.with_extension(format!("tmp-{}", Uuid::now_v7()));
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temp)
        .map_err(|error| ReleaseError::Io(error.to_string()))?;
    set_owner_only_file(&temp)?;
    let write_result = file
        .write_all(bytes)
        .and_then(|_| file.sync_all())
        .map_err(|error| ReleaseError::Io(error.to_string()));
    if write_result.is_err() {
        let _ = fs::remove_file(&temp);
        return write_result;
    }
    #[cfg(windows)]
    if path.exists() {
        fs::remove_file(path).map_err(|error| ReleaseError::Io(error.to_string()))?;
    }
    fs::rename(&temp, path).map_err(|error| {
        let _ = fs::remove_file(&temp);
        ReleaseError::Io(error.to_string())
    })?;
    set_owner_only_file(path)?;
    sync_parent(path)
}

#[cfg(unix)]
fn sync_parent(path: &Path) -> Result<(), ReleaseError> {
    let parent = path
        .parent()
        .ok_or_else(|| ReleaseError::Io(format!("path has no parent: {}", path.display())))?;
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| ReleaseError::Io(error.to_string()))
}

#[cfg(not(unix))]
fn sync_parent(_path: &Path) -> Result<(), ReleaseError> {
    Ok(())
}

#[cfg(unix)]
fn set_owner_only_dir(path: &Path) -> Result<(), ReleaseError> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(|error| ReleaseError::Io(error.to_string()))
}

#[cfg(not(unix))]
fn set_owner_only_dir(_path: &Path) -> Result<(), ReleaseError> {
    Ok(())
}

#[cfg(unix)]
fn set_owner_only_file(path: &Path) -> Result<(), ReleaseError> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .map_err(|error| ReleaseError::Io(error.to_string()))
}

#[cfg(unix)]
fn source_file_is_executable(path: &Path) -> Result<bool, ReleaseError> {
    use std::os::unix::fs::PermissionsExt;

    let mode = fs::symlink_metadata(path)
        .map_err(|error| ReleaseError::Io(format!("{}: {error}", path.display())))?
        .permissions()
        .mode();
    Ok(mode & 0o111 != 0)
}

#[cfg(not(unix))]
fn source_file_is_executable(_path: &Path) -> Result<bool, ReleaseError> {
    Ok(false)
}

fn set_owner_source_file(source: &Path, destination: &Path) -> Result<(), ReleaseError> {
    if source_file_is_executable(source)? {
        set_owner_executable(destination)
    } else {
        set_owner_only_file(destination)
    }
}

#[cfg(unix)]
fn set_owner_executable(path: &Path) -> Result<(), ReleaseError> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(|error| ReleaseError::Io(error.to_string()))
}

#[cfg(not(unix))]
fn set_owner_executable(_path: &Path) -> Result<(), ReleaseError> {
    Ok(())
}

#[cfg(not(unix))]
fn set_owner_only_file(_path: &Path) -> Result<(), ReleaseError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use super::*;
    use tempfile::tempdir;

    fn build_request(source_root: &Path) -> ReleaseBuildRequest {
        ReleaseBuildRequest {
            candidate_id: "candidate-1".to_owned(),
            parent_release_id: None,
            source_root: source_root.to_owned(),
            source_commit: "commit-1".to_owned(),
            dependency_lock_digest: "sha256:lock".to_owned(),
            toolchain_digest: "sha256:toolchain".to_owned(),
            protocol_version_range: "2..=2".to_owned(),
            state_schema_version_range: "1..=2".to_owned(),
            migration_plan_ref: None,
            provenance_ref: "provenance://candidate-1".to_owned(),
            update_metadata_ref: "metadata://candidate-1".to_owned(),
            rollback_release_id: None,
        }
    }

    fn pending_preview(store: &ReleaseStore, release_id: &str) -> PendingDeployment {
        let records = store
            .verify_deployment_log_locked()
            .expect("deployment records");
        let mut pointers = store.load_pointer_state_locked().expect("pointer state");
        let preview = ReleaseStore::next_pointer(&pointers, "preview", release_id);
        pointers.insert("preview".to_owned(), Some(preview));
        let previous_digest = records
            .last()
            .map_or_else(String::new, |record| record.digest.clone());
        let mut record = DeploymentRecord {
            sequence: u64::try_from(records.len())
                .unwrap_or(u64::MAX)
                .saturating_add(1),
            phase: DeploymentPhase::Preview,
            release_id: release_id.to_owned(),
            previous_release_id: None,
            reason: "preview".to_owned(),
            at: Utc::now(),
            previous_digest,
            digest: String::new(),
        };
        record.digest =
            deployment_digest(&record, &record.previous_digest).expect("deployment digest");
        PendingDeployment { record, pointers }
    }

    #[test]
    fn deployment_journal_recovers_before_log_append() {
        let source = tempdir().expect("source");
        fs::write(source.path().join("main.txt"), "source").expect("source file");
        let root = tempdir().expect("release root");
        let store = ReleaseStore::new(root.path()).expect("store");
        let release = store.build(build_request(source.path())).expect("release");
        let lock = store.lock().expect("lock");
        let pending = pending_preview(&store, &release.release_id);
        write_private_atomic(
            &store.deployment_pending,
            &serde_json::to_vec(&pending).expect("pending bytes"),
        )
        .expect("pending deployment");
        drop(lock);

        let preview = store
            .pointer("preview")
            .expect("preview pointer")
            .expect("preview exists");

        assert_eq!(preview.release_id, release.release_id);
        assert_eq!(store.verify_deployment_log().expect("log").len(), 1);
        assert!(!store.deployment_pending.exists());
    }

    #[test]
    fn deployment_journal_repairs_partial_log_append() {
        let source = tempdir().expect("source");
        fs::write(source.path().join("main.txt"), "source").expect("source file");
        let root = tempdir().expect("release root");
        let store = ReleaseStore::new(root.path()).expect("store");
        let release = store.build(build_request(source.path())).expect("release");
        let lock = store.lock().expect("lock");
        let pending = pending_preview(&store, &release.release_id);
        write_private_atomic(
            &store.deployment_pending,
            &serde_json::to_vec(&pending).expect("pending bytes"),
        )
        .expect("pending deployment");
        let line = serde_json::to_vec(&pending.record).expect("record line");
        let mut log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&store.deployment_log)
            .expect("deployment log");
        log.write_all(&line[..line.len() / 2])
            .and_then(|_| log.sync_all())
            .expect("partial log");
        drop(log);
        drop(lock);

        let preview = store
            .pointer("preview")
            .expect("preview pointer")
            .expect("preview exists");

        assert_eq!(preview.release_id, release.release_id);
        assert_eq!(store.verify_deployment_log().expect("log").len(), 1);
        assert!(!store.deployment_pending.exists());
    }

    #[test]
    fn build_pointer_promote_and_rollback_preserve_immutable_content() {
        let source = tempdir().expect("source");
        fs::create_dir_all(source.path().join("crates/golutra-runtime/src")).expect("mkdir");
        fs::write(
            source.path().join("crates/golutra-runtime/src/lib.rs"),
            "version one",
        )
        .expect("source file");
        let root = tempdir().expect("release root");
        let store = ReleaseStore::new(root.path()).expect("store");
        let first = store.build(build_request(source.path())).expect("build");
        store.set_preview(&first.release_id).expect("preview");
        store.start_canary(&first.release_id).expect("canary");
        store
            .promote(&first.release_id, "first release")
            .expect("promote");
        assert_eq!(
            store
                .pointer("stable")
                .expect("pointer")
                .unwrap()
                .release_id,
            first.release_id
        );

        fs::write(
            source.path().join("crates/golutra-runtime/src/lib.rs"),
            "version two",
        )
        .expect("source update");
        let mut second_request = build_request(source.path());
        second_request.candidate_id = "candidate-2".to_owned();
        second_request.parent_release_id = Some(first.release_id.clone());
        second_request.rollback_release_id = Some(first.release_id.clone());
        let second = store.build(second_request).expect("second build");
        store.set_preview(&second.release_id).expect("preview two");
        store.start_canary(&second.release_id).expect("canary two");
        store
            .promote(&second.release_id, "second release")
            .expect("promote two");
        assert_eq!(
            fs::read_to_string(
                store
                    .release_source(&first.release_id)
                    .expect("first source")
                    .join("crates/golutra-runtime/src/lib.rs")
            )
            .expect("first content"),
            "version one"
        );
        store
            .rollback("canary health regression")
            .expect("rollback");
        assert_eq!(
            store
                .pointer("stable")
                .expect("pointer")
                .unwrap()
                .release_id,
            first.release_id
        );
        assert_eq!(store.verify_deployment_log().expect("log").len(), 7);
    }

    #[test]
    fn source_symlinks_and_unsafe_pointer_names_are_rejected() {
        let source = tempdir().expect("source");
        fs::write(source.path().join("main.txt"), "safe").expect("file");
        let root = tempdir().expect("root");
        let store = ReleaseStore::new(root.path()).expect("store");
        assert!(store.pointer("../stable").is_err());
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(source.path().join("main.txt"), source.path().join("link"))
                .expect("symlink");
            assert!(store.build(build_request(source.path())).is_err());
        }
    }

    #[test]
    fn manifest_identity_size_and_source_integrity_are_enforced() {
        let source = tempdir().expect("source");
        let source_file = source.path().join("main.txt");
        fs::write(&source_file, "immutable source").expect("source file");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&source_file, fs::Permissions::from_mode(0o700))
                .expect("source mode");
        }
        let root = tempdir().expect("release root");
        let store = ReleaseStore::new(root.path()).expect("store");
        let manifest = store.build(build_request(source.path())).expect("build");
        let manifest_path = root
            .path()
            .join("releases")
            .join(&manifest.release_id)
            .join("release.json");
        let original_manifest = fs::read(&manifest_path).expect("manifest bytes");

        let mut mismatched = manifest.clone();
        mismatched.release_id = "release-mismatched".to_owned();
        fs::write(
            &manifest_path,
            serde_json::to_vec_pretty(&mismatched).expect("mismatched manifest"),
        )
        .expect("write mismatched manifest");
        assert!(
            store
                .manifest(&manifest.release_id)
                .expect_err("manifest identity mismatch must fail")
                .to_string()
                .contains("identity")
        );

        fs::write(&manifest_path, &original_manifest).expect("restore manifest");
        let installed_source = root
            .path()
            .join("releases")
            .join(&manifest.release_id)
            .join("source/main.txt");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&installed_source, fs::Permissions::from_mode(0o600))
                .expect("tamper source mode");
            assert!(
                store
                    .manifest(&manifest.release_id)
                    .expect_err("source mode tampering must fail")
                    .to_string()
                    .contains("source artifact manifest")
            );
            fs::set_permissions(&installed_source, fs::Permissions::from_mode(0o700))
                .expect("restore source mode");
        }
        fs::write(&installed_source, "tampered source").expect("tamper source");
        assert!(
            store
                .manifest(&manifest.release_id)
                .expect_err("source tampering must fail")
                .to_string()
                .contains("source digest")
        );

        fs::write(
            &manifest_path,
            vec![b' '; usize::try_from(MAX_MANIFEST_BYTES + 1).expect("manifest size")],
        )
        .expect("oversized manifest");
        assert!(
            store
                .manifest(&manifest.release_id)
                .expect_err("oversized manifest must fail")
                .to_string()
                .contains("size limit")
        );
    }

    #[test]
    fn checked_build_copies_verified_binary_and_detects_tampering() {
        let source = tempdir().expect("source");
        fs::create_dir_all(source.path().join("crates/golutra-runtime/src")).expect("source dir");
        fs::write(
            source.path().join("crates/golutra-runtime/src/lib.rs"),
            "runtime source",
        )
        .expect("source");
        let artifacts = tempdir().expect("build artifacts");
        let binary = artifacts.path().join("target/release/golutra-cli");
        fs::create_dir_all(binary.parent().expect("binary parent")).expect("target dir");
        let binary_bytes = b"verified binary";
        fs::write(&binary, binary_bytes).expect("binary");
        let files = collect_release_files(source.path()).expect("files");
        let (source_digest, _, _) = digest_files(source.path(), &files).expect("digest");
        let report = BuildReport {
            builder_version: "test".to_owned(),
            source_digest,
            sandbox_backend: "test".to_owned(),
            sandbox_enforced: true,
            checks: vec![BuildCheck {
                name: "test".to_owned(),
                command: vec!["cargo test".to_owned()],
                status: BuildStatus::Pass,
                exit_code: Some(0),
                duration_ms: 1,
                output_digest: "sha256:test".to_owned(),
            }],
            binary_artifacts: vec![BuildArtifact {
                relative_path: "target/release/golutra-cli".to_owned(),
                checksum: format!("sha256:{:x}", Sha256::digest(binary_bytes)),
                size_bytes: u64::try_from(binary_bytes.len()).unwrap_or(u64::MAX),
            }],
            passed: true,
            completed_at: Utc::now(),
        };
        let root = tempdir().expect("release root");
        let store = ReleaseStore::new(root.path()).expect("store");
        let manifest = store
            .build_checked(build_request(source.path()), &report, artifacts.path())
            .expect("checked build");
        let installed = store
            .binary_path(&manifest.release_id, "golutra-cli")
            .expect("verified binary");
        assert_eq!(fs::read(&installed).expect("binary bytes"), binary_bytes);
        fs::write(&installed, b"tampered").expect("tamper");
        assert!(
            store
                .binary_path(&manifest.release_id, "golutra-cli")
                .is_err()
        );
    }

    #[test]
    fn checked_build_recovers_a_published_bin_before_manifest_update() {
        let source = tempdir().expect("source");
        fs::write(source.path().join("source.txt"), "source").expect("source");
        let artifacts = tempdir().expect("artifacts");
        let binary = artifacts.path().join("target/release/golutra-cli");
        fs::create_dir_all(binary.parent().expect("binary parent")).expect("binary directory");
        let binary_bytes = b"recoverable binary";
        fs::write(&binary, binary_bytes).expect("binary");
        let files = collect_release_files(source.path()).expect("source files");
        let (source_digest, _, _) = digest_files(source.path(), &files).expect("source digest");
        let report = BuildReport {
            builder_version: "test".to_owned(),
            source_digest,
            sandbox_backend: "test".to_owned(),
            sandbox_enforced: true,
            checks: Vec::new(),
            binary_artifacts: vec![BuildArtifact {
                relative_path: "target/release/golutra-cli".to_owned(),
                checksum: format!("sha256:{:x}", Sha256::digest(binary_bytes)),
                size_bytes: u64::try_from(binary_bytes.len()).unwrap_or(u64::MAX),
            }],
            passed: true,
            completed_at: Utc::now(),
        };
        let root = tempdir().expect("release root");
        let store = ReleaseStore::new(root.path()).expect("store");
        let request = build_request(source.path());
        let source_only = store.build(request.clone()).expect("source-only release");
        let published_bin = root
            .path()
            .join("releases")
            .join(&source_only.release_id)
            .join("bin");
        ensure_private_dir(&published_bin).expect("published bin directory");
        let published_binary = published_bin.join("golutra-cli");
        fs::copy(&binary, &published_binary).expect("published binary");
        set_owner_executable(&published_binary).expect("published binary mode");

        let recovered = store
            .build_checked(request, &report, artifacts.path())
            .expect("recovered checked release");
        assert!(
            recovered
                .artifact_digests
                .iter()
                .any(|artifact| artifact.relative_path == "bin/golutra-cli")
        );
        store
            .binary_path(&recovered.release_id, "golutra-cli")
            .expect("verified recovered binary");
    }

    #[cfg(unix)]
    #[test]
    fn checked_build_rejects_artifact_paths_that_escape_staging_root() {
        let source = tempdir().expect("source");
        fs::write(source.path().join("source.txt"), "source").expect("source file");
        let artifacts = tempdir().expect("artifacts");
        let outside = tempdir().expect("outside");
        let outside_binary = outside.path().join("golutra-cli");
        fs::write(&outside_binary, "outside binary").expect("outside binary");
        fs::create_dir_all(artifacts.path().join("target")).expect("artifact target");
        std::os::unix::fs::symlink(outside.path(), artifacts.path().join("target/release"))
            .expect("release symlink");
        let bytes = fs::read(&outside_binary).expect("binary bytes");
        let files = collect_release_files(source.path()).expect("files");
        let (source_digest, _, _) = digest_files(source.path(), &files).expect("digest");
        let report = BuildReport {
            builder_version: "test".to_owned(),
            source_digest,
            sandbox_backend: "test".to_owned(),
            sandbox_enforced: true,
            checks: vec![BuildCheck {
                name: "test".to_owned(),
                command: vec!["cargo test".to_owned()],
                status: BuildStatus::Pass,
                exit_code: Some(0),
                duration_ms: 1,
                output_digest: "sha256:test".to_owned(),
            }],
            binary_artifacts: vec![BuildArtifact {
                relative_path: "target/release/golutra-cli".to_owned(),
                checksum: format!("sha256:{:x}", Sha256::digest(&bytes)),
                size_bytes: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
            }],
            passed: true,
            completed_at: Utc::now(),
        };
        let releases = tempdir().expect("release root");
        let store = ReleaseStore::new(releases.path()).expect("release store");

        let error = store
            .build_checked(build_request(source.path()), &report, artifacts.path())
            .expect_err("artifact escape must be rejected");
        assert!(error.to_string().contains("escapes its root"));
    }
}
