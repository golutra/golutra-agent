//! 为不透明进程提供有界变更采样；不完整采样保留未知状态，不阻塞普通命令做全盘快照。

use std::{
    collections::BTreeMap,
    fs,
    io::Read,
    path::{Path, PathBuf},
    sync::{Arc, LazyLock},
    time::{Duration, Instant, UNIX_EPOCH},
};

use golutra_agent_core::{FileContentKind, FileStateMetadata};
use sha2::{Digest, Sha256};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use walkdir::{DirEntry, WalkDir};

use super::FileBeforeImage;

const MAX_TRACKED_FILES: usize = 5_000;
const MAX_TRACKED_FILE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_HASHED_FILE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_HASHED_TOTAL_BYTES: u64 = 32 * 1024 * 1024;
// 普通过程采样只保留少量可恢复内容；定向文件编辑仍使用完整 before-image 合同。
const MAX_SAMPLED_CONTENT_BYTES: usize = 2 * 1024 * 1024;
const HASH_BUFFER_BYTES: usize = 1024 * 1024;
pub(crate) const MAX_SCAN_DURATION: Duration = Duration::from_millis(250);
const MAX_CONCURRENT_SCANS: usize = 2;
static SCAN_PERMITS: LazyLock<Arc<Semaphore>> =
    LazyLock::new(|| Arc::new(Semaphore::new(MAX_CONCURRENT_SCANS)));

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileSample {
    content: Option<Vec<u8>>,
    metadata: FileStateMetadata,
    modified_nanos: u128,
}

#[derive(Debug, PartialEq, Eq)]
struct BoundedFileRead {
    content: Option<Vec<u8>>,
    checksum: Option<String>,
    content_kind: FileContentKind,
    bytes_read: u64,
    bytes_hashed: u64,
    complete: bool,
}

#[derive(Debug, PartialEq, Eq)]
struct BoundedReadError {
    bytes_hashed: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct WorkspaceSnapshot {
    pub(crate) capture_ms: u64,
    files: BTreeMap<PathBuf, FileSample>,
    scan_complete: bool,
    checkpoint_complete: bool,
}

impl WorkspaceSnapshot {
    pub(crate) fn before_images(&self) -> Vec<FileBeforeImage> {
        self.files
            .iter()
            .map(|(path, sample)| FileBeforeImage {
                path: path.clone(),
                content: sample.content.clone(),
                unix_mode: sample.metadata.unix_mode,
                metadata: Some(sample.metadata.clone()),
            })
            .collect()
    }

    pub(crate) fn is_complete(&self) -> bool {
        self.checkpoint_complete
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct WorkspaceMutationScan {
    pub(crate) duration_ms: u64,
    pub(crate) changed_files: Vec<PathBuf>,
    pub(crate) before_images: Vec<FileBeforeImage>,
    pub(crate) after_images: Vec<FileBeforeImage>,
    pub(crate) complete: bool,
    /// 完整快照证明只有源文件未变的派生缓存发生变化；不省略原始变动记录。
    pub(crate) only_derived_changes: bool,
}

pub(crate) async fn capture(root: &Path) -> WorkspaceSnapshot {
    let started = Instant::now();
    let mut snapshot = capture_with_budget(root, MAX_SCAN_DURATION).await;
    snapshot.capture_ms = super::elapsed_millis(started);
    snapshot
}

async fn capture_with_budget(root: &Path, budget: Duration) -> WorkspaceSnapshot {
    let root = root.to_path_buf();
    let cancellation = CancellationToken::new();
    let cancel_on_drop = CancelOnDrop(cancellation.clone());
    let deadline = Instant::now()
        .checked_add(budget)
        .unwrap_or_else(Instant::now);
    let Some(permit_budget) = deadline.checked_duration_since(Instant::now()) else {
        return WorkspaceSnapshot::default();
    };
    let Ok(Ok(permit)) =
        tokio::time::timeout(permit_budget, SCAN_PERMITS.clone().acquire_owned()).await
    else {
        return WorkspaceSnapshot::default();
    };
    let result = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        capture_blocking(&root, &cancellation, deadline)
    })
    .await
    .unwrap_or_default();
    drop(cancel_on_drop);
    result
}

pub(crate) async fn compare(root: &Path, before: WorkspaceSnapshot) -> WorkspaceMutationScan {
    let started = Instant::now();
    let after = capture(root).await;
    let mut scan = compare_snapshots(before, after);
    scan.duration_ms = super::elapsed_millis(started);
    scan
}

pub(crate) fn read_regular_file_bounded(
    path: &Path,
    root: &Path,
    max_bytes: u64,
    cancellation: &CancellationToken,
    deadline: Instant,
) -> Result<Vec<u8>, ()> {
    let canonical_root = root.canonicalize().map_err(|_| ())?;
    let (mut file, metadata) = open_regular_file(path, &canonical_root).map_err(|_| ())?;
    if metadata.len() > max_bytes {
        return Err(());
    }
    let sampled = read_bounded_with_control(
        &mut file,
        Some(max_bytes),
        max_bytes,
        cancellation,
        deadline,
    )
    .map_err(|_| ())?;
    let end_metadata = file.metadata().map_err(|_| ())?;
    let current_metadata = fs::symlink_metadata(path).map_err(|_| ())?;
    if !sampled.complete
        || sampled.bytes_read != metadata.len()
        || !same_file_state(&metadata, &end_metadata)
        || !current_metadata.file_type().is_file()
        || !same_file_state(&end_metadata, &current_metadata)
    {
        return Err(());
    }
    sampled.content.ok_or(())
}

struct CancelOnDrop(CancellationToken);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

fn capture_blocking(
    root: &Path,
    cancellation: &CancellationToken,
    deadline: Instant,
) -> WorkspaceSnapshot {
    let canonical_root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let mut snapshot = WorkspaceSnapshot {
        scan_complete: true,
        checkpoint_complete: true,
        ..WorkspaceSnapshot::default()
    };
    let mut retained_bytes = 0_usize;
    let mut hashed_bytes = 0_u64;
    let mut walker = WalkDir::new(root).follow_links(false).into_iter();
    while let Some(entry) = walker.next() {
        if cancellation.is_cancelled() || Instant::now() >= deadline {
            snapshot.scan_complete = false;
            snapshot.checkpoint_complete = false;
            break;
        }
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => {
                snapshot.scan_complete = false;
                snapshot.checkpoint_complete = false;
                continue;
            }
        };
        if entry.depth() > 0 && is_excluded_dir(&entry) {
            // The process can mutate an excluded subtree even though it is
            // intentionally outside the bounded snapshot. Preserve that
            // uncertainty instead of reporting a complete workspace scan.
            snapshot.scan_complete = false;
            snapshot.checkpoint_complete = false;
            walker.skip_current_dir();
            continue;
        }
        if entry.file_type().is_symlink() {
            // A process can mutate a symlink target outside the sampled tree;
            // report the scan as incomplete instead of claiming no changes.
            snapshot.scan_complete = false;
            snapshot.checkpoint_complete = false;
            continue;
        }
        if !entry.file_type().is_file() {
            if entry.depth() > 0 && !entry.file_type().is_dir() {
                snapshot.scan_complete = false;
                snapshot.checkpoint_complete = false;
            }
            continue;
        }
        if snapshot.files.len() >= MAX_TRACKED_FILES {
            snapshot.scan_complete = false;
            snapshot.checkpoint_complete = false;
            break;
        }
        let entry_metadata = match entry.metadata() {
            Ok(metadata) => metadata,
            Err(_) => {
                snapshot.scan_complete = false;
                snapshot.checkpoint_complete = false;
                continue;
            }
        };
        let (mut file, metadata) = match open_regular_file(entry.path(), &canonical_root) {
            Ok(opened) => opened,
            Err(_) => {
                snapshot.scan_complete = false;
                snapshot.checkpoint_complete = false;
                continue;
            }
        };
        if !same_file_identity(&entry_metadata, &metadata) {
            snapshot.scan_complete = false;
            snapshot.checkpoint_complete = false;
        }
        let file_bytes = usize::try_from(metadata.len()).unwrap_or(usize::MAX);
        let retain_content = metadata.len() <= MAX_TRACKED_FILE_BYTES
            && retained_bytes.saturating_add(file_bytes) <= MAX_SAMPLED_CONTENT_BYTES;
        if !retain_content {
            snapshot.checkpoint_complete = false;
        }
        let Some(hash_limit) = hash_limit_for_file(metadata.len(), hashed_bytes) else {
            snapshot.scan_complete = false;
            snapshot.checkpoint_complete = false;
            insert_file_sample(
                &mut snapshot,
                entry.path(),
                &metadata,
                None,
                None,
                FileContentKind::Unknown,
            );
            continue;
        };
        let retain_limit = retain_content.then(|| {
            let retained_remaining = MAX_SAMPLED_CONTENT_BYTES.saturating_sub(retained_bytes);
            MAX_TRACKED_FILE_BYTES.min(u64::try_from(retained_remaining).unwrap_or(u64::MAX))
        });
        let sampled = match read_bounded_with_control(
            &mut file,
            retain_limit,
            hash_limit,
            cancellation,
            deadline,
        ) {
            Ok(sampled) => sampled,
            Err(error) => {
                hashed_bytes = hashed_bytes.saturating_add(error.bytes_hashed);
                snapshot.scan_complete = false;
                snapshot.checkpoint_complete = false;
                if retain_content {
                    continue;
                }
                insert_file_sample(
                    &mut snapshot,
                    entry.path(),
                    &metadata,
                    None,
                    None,
                    FileContentKind::Unknown,
                );
                continue;
            }
        };
        let end_metadata = match file.metadata() {
            Ok(end_metadata)
                if same_file_identity(&metadata, &end_metadata)
                    && same_file_state(&metadata, &end_metadata) =>
            {
                end_metadata
            }
            _ => {
                snapshot.scan_complete = false;
                snapshot.checkpoint_complete = false;
                continue;
            }
        };
        let current_metadata = match fs::symlink_metadata(entry.path()) {
            Ok(metadata)
                if metadata.file_type().is_file() && same_file_state(&end_metadata, &metadata) =>
            {
                metadata
            }
            _ => {
                snapshot.scan_complete = false;
                snapshot.checkpoint_complete = false;
                continue;
            }
        };
        hashed_bytes = hashed_bytes.saturating_add(sampled.bytes_hashed);
        if !sampled.complete || sampled.bytes_read != current_metadata.len() {
            snapshot.scan_complete = false;
            snapshot.checkpoint_complete = false;
        }
        if retain_content && sampled.content.is_none() {
            snapshot.checkpoint_complete = false;
        }
        if let Some(content) = sampled.content.as_ref() {
            retained_bytes = retained_bytes.saturating_add(content.len());
        }
        insert_file_sample(
            &mut snapshot,
            entry.path(),
            &current_metadata,
            sampled.content,
            sampled.checksum,
            sampled.content_kind,
        );
    }
    snapshot
}

fn insert_file_sample(
    snapshot: &mut WorkspaceSnapshot,
    path: &Path,
    metadata: &fs::Metadata,
    content: Option<Vec<u8>>,
    checksum: Option<String>,
    content_kind: FileContentKind,
) {
    let content_available = content.is_some();
    snapshot.files.insert(
        path.to_path_buf(),
        FileSample {
            content,
            metadata: FileStateMetadata {
                size_bytes: metadata.len(),
                checksum,
                unix_mode: unix_mode(metadata),
                content_kind,
                content_available,
            },
            modified_nanos: modified_nanos(metadata),
        },
    );
}

fn compare_snapshots(before: WorkspaceSnapshot, after: WorkspaceSnapshot) -> WorkspaceMutationScan {
    let mut paths = before
        .files
        .keys()
        .chain(after.files.keys())
        .cloned()
        .collect::<Vec<_>>();
    paths.sort();
    paths.dedup();
    let mut scan = WorkspaceMutationScan {
        complete: before.scan_complete && after.scan_complete,
        ..WorkspaceMutationScan::default()
    };
    for path in paths {
        let old = before.files.get(&path);
        let new = after.files.get(&path);
        // 未采样不等于不存在，不能把预算截断投影成新增/删除或可恢复的缺失文件。
        if old.is_none() && !before.scan_complete || new.is_none() && !after.scan_complete {
            continue;
        }
        if samples_match(old, new) {
            continue;
        }
        scan.changed_files.push(path.clone());
        scan.before_images.push(FileBeforeImage {
            path: path.clone(),
            content: old.and_then(|sample| sample.content.clone()),
            unix_mode: old.and_then(|sample| sample.metadata.unix_mode),
            metadata: old.map(|sample| sample.metadata.clone()),
        });
        scan.after_images.push(FileBeforeImage {
            path,
            content: new.and_then(|sample| sample.content.clone()),
            unix_mode: new.and_then(|sample| sample.metadata.unix_mode),
            metadata: new.map(|sample| sample.metadata.clone()),
        });
    }
    scan.only_derived_changes = scan.complete
        && !scan.changed_files.is_empty()
        && scan.changed_files.iter().all(|path| {
            let Some(source) = derived_source_path(path) else {
                return false;
            };
            match (before.files.get(&source), after.files.get(&source)) {
                (Some(old), Some(new)) => {
                    old.metadata.checksum.is_some()
                        && new.metadata.checksum.is_some()
                        && samples_match(Some(old), Some(new))
                }
                _ => false,
            }
        });
    scan
}

/// 只识别具有标准源文件映射的派生产物，不按命令名、测试名或整个目录豁免。
/// PEP 3147/488 的缓存必须有同目录树中的稳定 .py 源文件；孤立字节码仍是行为输入。
fn derived_source_path(path: &Path) -> Option<PathBuf> {
    let directory = path.parent()?;
    if directory.file_name()? != "__pycache__" || path.extension()? != "pyc" {
        return None;
    }
    let stem = path.file_stem()?.to_str()?;
    let (stem, tag) = stem.rsplit_once('.')?;
    let (source, tag) = if let Some(optimization) = tag.strip_prefix("opt-") {
        if optimization.is_empty()
            || !optimization
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric())
        {
            return None;
        }
        stem.rsplit_once('.')?
    } else {
        (stem, tag)
    };
    if source.is_empty()
        || tag.is_empty()
        || !tag
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        return None;
    }
    Some(directory.parent()?.join(format!("{source}.py")))
}

fn samples_match(left: Option<&FileSample>, right: Option<&FileSample>) -> bool {
    match (left, right) {
        (None, None) => true,
        (Some(left), Some(right)) => {
            if left.metadata.unix_mode != right.metadata.unix_mode
                || left.metadata.size_bytes != right.metadata.size_bytes
            {
                return false;
            }
            match (&left.metadata.checksum, &right.metadata.checksum) {
                (Some(left), Some(right)) => left == right,
                _ => left.modified_nanos == right.modified_nanos,
            }
        }
        _ => false,
    }
}

fn hash_limit_for_file(file_bytes: u64, hashed_bytes: u64) -> Option<u64> {
    let remaining = MAX_HASHED_TOTAL_BYTES.checked_sub(hashed_bytes)?;
    if remaining == 0 || file_bytes > MAX_HASHED_FILE_BYTES || file_bytes > remaining {
        return None;
    }
    Some(MAX_HASHED_FILE_BYTES.min(remaining))
}

fn open_regular_file(
    path: &Path,
    canonical_root: &Path,
) -> Result<(fs::File, fs::Metadata), BoundedReadError> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(nix::libc::O_CLOEXEC | nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(0x0020_0000); // FILE_FLAG_OPEN_REPARSE_POINT
    }
    let file = options
        .open(path)
        .map_err(|_| BoundedReadError { bytes_hashed: 0 })?;
    let metadata = file
        .metadata()
        .map_err(|_| BoundedReadError { bytes_hashed: 0 })?;
    if !metadata.file_type().is_file() {
        return Err(BoundedReadError { bytes_hashed: 0 });
    }
    let canonical_parent = path
        .parent()
        .and_then(|parent| parent.canonicalize().ok())
        .ok_or(BoundedReadError { bytes_hashed: 0 })?;
    let current_metadata =
        fs::symlink_metadata(path).map_err(|_| BoundedReadError { bytes_hashed: 0 })?;
    if !canonical_parent.starts_with(canonical_root)
        || !current_metadata.file_type().is_file()
        || !same_file_identity(&metadata, &current_metadata)
    {
        return Err(BoundedReadError { bytes_hashed: 0 });
    }
    Ok((file, metadata))
}

#[cfg(unix)]
pub(crate) fn same_file_identity(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;

    left.dev() == right.dev() && left.ino() == right.ino()
}

#[cfg(windows)]
pub(crate) fn same_file_identity(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;

    left.file_type() == right.file_type()
        && left.file_attributes() == right.file_attributes()
        && left.creation_time() == right.creation_time()
        && left.last_write_time() == right.last_write_time()
        && left.file_size() == right.file_size()
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn same_file_identity(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    left.file_type() == right.file_type()
        && left.len() == right.len()
        && left.created().ok() == right.created().ok()
}

pub(crate) fn same_file_state(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    same_file_identity(left, right)
        && left.len() == right.len()
        && modified_nanos(left) == modified_nanos(right)
}

#[cfg(test)]
fn read_bounded(
    reader: &mut impl Read,
    retain_limit: Option<u64>,
    hash_limit: u64,
) -> Result<BoundedFileRead, BoundedReadError> {
    read_bounded_with_control(
        reader,
        retain_limit,
        hash_limit,
        &CancellationToken::new(),
        Instant::now()
            .checked_add(MAX_SCAN_DURATION)
            .unwrap_or_else(Instant::now),
    )
}

fn read_bounded_with_control(
    reader: &mut impl Read,
    retain_limit: Option<u64>,
    hash_limit: u64,
    cancellation: &CancellationToken,
    deadline: Instant,
) -> Result<BoundedFileRead, BoundedReadError> {
    let mut buffer = vec![0_u8; HASH_BUFFER_BYTES];
    let mut hasher = Sha256::new();
    let mut prefix = Vec::new();
    let mut content = retain_limit.map(|limit| {
        Vec::with_capacity(usize::try_from(limit).unwrap_or(0).min(HASH_BUFFER_BYTES))
    });
    let probe_limit = hash_limit.saturating_add(1);
    let mut bytes_read = 0_u64;
    let mut bytes_hashed = 0_u64;
    while bytes_read < probe_limit {
        if cancellation.is_cancelled() || Instant::now() >= deadline {
            return Ok(BoundedFileRead {
                content: None,
                checksum: None,
                content_kind: FileContentKind::Unknown,
                bytes_read,
                bytes_hashed,
                complete: false,
            });
        }
        let hash_remaining = probe_limit - bytes_read;
        let mut read_limit = hash_remaining.min(HASH_BUFFER_BYTES as u64);
        if let Some(retain_limit) = retain_limit.filter(|_| content.is_some()) {
            let retain_probe_limit = retain_limit.saturating_add(1);
            read_limit = read_limit.min(retain_probe_limit.saturating_sub(bytes_read));
        }
        let read_limit = usize::try_from(read_limit).unwrap_or(HASH_BUFFER_BYTES);
        let read = reader
            .read(&mut buffer[..read_limit])
            .map_err(|_| BoundedReadError { bytes_hashed })?;
        if read == 0 {
            let content_kind = content
                .as_deref()
                .map_or_else(|| content_kind(&prefix), content_kind);
            return Ok(BoundedFileRead {
                content,
                checksum: Some(format!("sha256:{:x}", hasher.finalize())),
                content_kind,
                bytes_read,
                bytes_hashed,
                complete: true,
            });
        }
        bytes_read = bytes_read.saturating_add(read as u64);
        let hashable = usize::try_from(hash_limit.saturating_sub(bytes_hashed))
            .unwrap_or(usize::MAX)
            .min(read);
        if prefix.len() < 8 * 1024 {
            let retained = (8 * 1024 - prefix.len()).min(hashable);
            prefix.extend_from_slice(&buffer[..retained]);
        }
        hasher.update(&buffer[..hashable]);
        bytes_hashed = bytes_hashed.saturating_add(hashable as u64);
        if let Some(retained) = content.as_mut() {
            if bytes_read <= retain_limit.unwrap_or_default() {
                retained.extend_from_slice(&buffer[..read]);
            } else {
                content = None;
            }
        }
    }

    Ok(BoundedFileRead {
        content: None,
        checksum: None,
        content_kind: FileContentKind::Unknown,
        bytes_read,
        bytes_hashed,
        complete: false,
    })
}

fn content_kind(bytes: &[u8]) -> FileContentKind {
    if std::str::from_utf8(bytes).is_ok() && !bytes.contains(&0) {
        FileContentKind::Text
    } else {
        FileContentKind::Binary
    }
}

fn modified_nanos(metadata: &fs::Metadata) -> u128 {
    metadata
        .modified()
        .ok()
        .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |duration| duration.as_nanos())
}

fn is_excluded_dir(entry: &DirEntry) -> bool {
    entry.file_type().is_dir()
        && matches!(
            entry.file_name().to_str(),
            Some(
                ".git"
                    | ".golutra"
                    | ".golutra-agent"
                    | "target"
                    | "node_modules"
                    | ".next"
                    | "dist"
                    | "build"
            )
        )
}

#[cfg(unix)]
fn unix_mode(metadata: &fs::Metadata) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;

    Some(metadata.permissions().mode() & 0o7777)
}

#[cfg(not(unix))]
fn unix_mode(_metadata: &fs::Metadata) -> Option<u32> {
    None
}

#[cfg(test)]
mod tests {
    use std::io::{Cursor, Read};

    use tempfile::tempdir;

    use super::*;

    #[tokio::test]
    async fn derived_changes_require_complete_unchanged_source_evidence() {
        let root = tempdir().unwrap();
        let source = root.path().join("module.py");
        let cache_dir = root.path().join("__pycache__");
        fs::create_dir(&cache_dir).unwrap();
        fs::write(&source, "value = 1\n").unwrap();
        let baseline = capture(root.path()).await;
        let cache = cache_dir.join("module.cpython-314.pyc");
        fs::write(&cache, b"derived output").unwrap();
        let after = capture(root.path()).await;
        let derived = compare_snapshots(baseline.clone(), after.clone());
        assert!(derived.only_derived_changes);
        assert_eq!(derived.changed_files, vec![cache.clone()]);
        assert_eq!(derived.before_images.len(), 1);
        assert_eq!(derived.after_images.len(), 1);

        let mut incomplete = baseline.clone();
        incomplete.scan_complete = false;
        assert!(!compare_snapshots(incomplete, after.clone()).only_derived_changes);
        let mut unknown_source = baseline.clone();
        unknown_source
            .files
            .get_mut(&source)
            .unwrap()
            .metadata
            .checksum = None;
        assert!(!compare_snapshots(unknown_source, after).only_derived_changes);

        fs::write(&source, "value = 2\n").unwrap();
        assert!(
            !compare(root.path(), baseline.clone())
                .await
                .only_derived_changes
        );
        fs::write(&source, "value = 1\n").unwrap();
        fs::write(cache_dir.join("orphan.cpython-314.pyc"), b"unknown source").unwrap();
        assert!(!compare(root.path(), baseline).await.only_derived_changes);
    }

    #[test]
    fn derived_source_mapping_is_not_a_directory_wide_exemption() {
        for (cache, source) in [
            ("pkg/__pycache__/module.cpython-314.pyc", "pkg/module.py"),
            (
                "pkg/__pycache__/module.cpython-313.opt-2.pyc",
                "pkg/module.py",
            ),
            ("pkg/__pycache__/module.pypy310.pyc", "pkg/module.py"),
        ] {
            assert_eq!(
                derived_source_path(Path::new(cache)),
                Some(PathBuf::from(source))
            );
        }
        for path in [
            "module.pyc",
            "__pycache__/module.pyc",
            "__pycache__/config.json",
            "__pycache__/module.tag.json",
            "__pycache__/module.cpython-314.opt-.pyc",
            "__pycache__/module.cpython-314.opt-!.pyc",
            "build/module.pyc",
            "dist/module.js",
        ] {
            assert!(derived_source_path(Path::new(path)).is_none(), "{path}");
        }
    }

    #[derive(Default)]
    struct EndlessReader {
        bytes_read: u64,
        requested: Vec<usize>,
    }

    impl Read for EndlessReader {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            self.requested.push(buffer.len());
            buffer.fill(b'x');
            self.bytes_read = self.bytes_read.saturating_add(buffer.len() as u64);
            Ok(buffer.len())
        }
    }

    #[test]
    fn bounded_read_retains_content_only_through_the_limit() {
        let mut exact = Cursor::new(b"abcd".to_vec());
        let exact = read_bounded(&mut exact, Some(4), 16).expect("read exact content");
        assert!(exact.complete);
        assert_eq!(exact.bytes_read, 4);
        assert_eq!(exact.bytes_hashed, 4);
        assert_eq!(exact.content.as_deref(), Some(b"abcd".as_slice()));
        assert!(exact.checksum.is_some());

        let mut over = Cursor::new(b"abcde".to_vec());
        let over = read_bounded(&mut over, Some(4), 16).expect("probe oversized content");
        assert!(over.complete);
        assert_eq!(over.bytes_read, 5);
        assert_eq!(over.bytes_hashed, 5);
        assert!(over.content.is_none());
        assert!(over.checksum.is_some());
    }

    #[test]
    fn bounded_read_stops_after_one_byte_beyond_the_hash_limit() {
        let mut reader = EndlessReader::default();

        let sampled = read_bounded(&mut reader, Some(3), 7).expect("bounded read");

        assert!(!sampled.complete);
        assert_eq!(sampled.bytes_read, 8);
        assert_eq!(sampled.bytes_hashed, 7);
        assert!(sampled.content.is_none());
        assert!(sampled.checksum.is_none());
        assert_eq!(sampled.content_kind, FileContentKind::Unknown);
        assert_eq!(reader.bytes_read, 8);
        assert_eq!(reader.requested.first(), Some(&4));
    }

    #[test]
    fn cumulative_hash_budget_is_rejected_before_reading() {
        assert_eq!(hash_limit_for_file(4, MAX_HASHED_TOTAL_BYTES - 4), Some(4));
        assert_eq!(hash_limit_for_file(5, MAX_HASHED_TOTAL_BYTES - 4), None);
        assert_eq!(hash_limit_for_file(MAX_HASHED_FILE_BYTES + 1, 0), None);
        assert_eq!(hash_limit_for_file(0, MAX_HASHED_TOTAL_BYTES), None);
        assert_eq!(hash_limit_for_file(0, MAX_HASHED_TOTAL_BYTES + 1), None);
    }

    #[tokio::test]
    async fn detects_added_modified_and_deleted_files() {
        let workspace = tempdir().expect("workspace");
        fs::write(workspace.path().join("modified.txt"), "before").expect("write before");
        fs::write(workspace.path().join("deleted.txt"), "gone").expect("write deleted");
        let before = capture(workspace.path()).await;

        fs::write(workspace.path().join("modified.txt"), "after").expect("modify");
        fs::remove_file(workspace.path().join("deleted.txt")).expect("delete");
        fs::write(workspace.path().join("added.txt"), "new").expect("add");
        let scan = compare(workspace.path(), before).await;

        assert!(scan.complete);
        let names = scan
            .changed_files
            .iter()
            .filter_map(|path| path.file_name().and_then(|name| name.to_str()))
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["added.txt", "deleted.txt", "modified.txt"]);
        assert!(
            scan.before_images
                .iter()
                .find(|image| image.path.ends_with("added.txt"))
                .is_some_and(|image| image.content.is_none())
        );
    }

    #[tokio::test]
    async fn incomplete_samples_do_not_invent_file_creation_or_deletion() {
        let workspace = tempdir().unwrap();
        let path = workspace.path().join("still-present.txt");
        fs::write(&path, "unchanged").unwrap();
        let complete = capture(workspace.path()).await;
        let missing_sample = WorkspaceSnapshot::default();
        for scan in [
            compare_snapshots(complete.clone(), missing_sample.clone()),
            compare_snapshots(missing_sample, complete),
        ] {
            assert!(!scan.complete);
            assert!(scan.changed_files.is_empty());
            assert!(scan.before_images.is_empty());
            assert!(scan.after_images.is_empty());
        }
    }

    #[tokio::test]
    async fn exhausted_scan_budget_preserves_unknown_state() {
        let workspace = tempdir().expect("workspace");
        fs::write(workspace.path().join("source.txt"), "before").unwrap();
        let before = capture_with_budget(workspace.path(), Duration::ZERO).await;
        assert!(!before.is_complete());
        fs::write(workspace.path().join("source.txt"), "after").unwrap();
        let after = capture(workspace.path()).await;
        let scan = compare_snapshots(before, after);
        assert!(!scan.complete);
        assert!(!scan.only_derived_changes);
    }

    #[tokio::test]
    async fn oversized_files_do_not_hide_small_file_changes() {
        let workspace = tempdir().expect("workspace");
        let large = workspace.path().join("archive.bin");
        fs::File::create(&large)
            .unwrap()
            .set_len(MAX_HASHED_FILE_BYTES + 1)
            .unwrap();
        let small = workspace.path().join("source.txt");
        fs::write(&small, "before").unwrap();
        let before = capture(workspace.path()).await;
        assert!(!before.is_complete());
        fs::write(&small, "after").unwrap();
        let scan = compare(workspace.path(), before).await;
        assert!(!scan.complete);
        assert!(scan.changed_files.contains(&small));
        assert!(
            scan.before_images.iter().any(|image| {
                image.path == small && image.content.as_deref() == Some(b"before")
            })
        );
    }

    #[tokio::test]
    async fn large_binary_changes_keep_checksums_without_retaining_content() {
        let workspace = tempdir().expect("workspace");
        let path = workspace.path().join("large.bin");
        let mut before_bytes = vec![0_u8; MAX_TRACKED_FILE_BYTES as usize + 1];
        before_bytes[1] = 1;
        fs::write(&path, &before_bytes).expect("write binary baseline");
        let before = capture(workspace.path()).await;
        assert!(!before.is_complete());

        let mut after_bytes = before_bytes;
        after_bytes[1] = 2;
        fs::write(&path, &after_bytes).expect("modify binary file");
        let scan = compare(workspace.path(), before).await;

        assert!(scan.complete);
        assert_eq!(scan.changed_files, vec![path]);
        let before = &scan.before_images[0];
        let after = &scan.after_images[0];
        assert!(before.content.is_none());
        assert!(after.content.is_none());
        assert_eq!(
            before.metadata.as_ref().map(|state| state.content_kind),
            Some(FileContentKind::Binary)
        );
        assert_eq!(
            after.metadata.as_ref().map(|state| state.content_kind),
            Some(FileContentKind::Binary)
        );
        assert_ne!(
            before
                .metadata
                .as_ref()
                .and_then(|state| state.checksum.as_deref()),
            after
                .metadata
                .as_ref()
                .and_then(|state| state.checksum.as_deref())
        );
        assert!(
            before
                .metadata
                .as_ref()
                .and_then(|state| state.checksum.as_deref())
                .is_some_and(|checksum| checksum.starts_with("sha256:"))
        );
        assert!(
            after
                .metadata
                .as_ref()
                .and_then(|state| state.checksum.as_deref())
                .is_some_and(|checksum| checksum.starts_with("sha256:"))
        );
    }

    #[tokio::test]
    async fn excluded_subtrees_make_the_scan_explicitly_incomplete() {
        let workspace = tempdir().expect("workspace");
        fs::create_dir(workspace.path().join("target")).expect("target");
        fs::write(workspace.path().join("target/output.bin"), "generated").expect("output");

        let snapshot = capture(workspace.path()).await;

        assert!(!snapshot.scan_complete);
        assert!(!snapshot.checkpoint_complete);
        assert!(snapshot.files.is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlink_entries_make_the_scan_explicitly_incomplete() {
        use std::os::unix::fs::symlink;

        let workspace = tempdir().expect("workspace");
        let outside = tempdir().expect("outside");
        fs::write(outside.path().join("outside.txt"), "outside").expect("outside file");
        symlink(outside.path(), workspace.path().join("linked")).expect("workspace symlink");

        let snapshot = capture(workspace.path()).await;

        assert!(!snapshot.scan_complete);
        assert!(!snapshot.checkpoint_complete);
    }
}
