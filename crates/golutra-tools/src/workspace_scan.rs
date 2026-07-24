//! Bounded before/after workspace sampling for opaque process tools.

use std::{
    collections::BTreeMap,
    fs,
    io::Read,
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};

use golutra_core::{FileContentKind, FileStateMetadata};
use sha2::{Digest, Sha256};
use walkdir::{DirEntry, WalkDir};

use super::FileBeforeImage;

const MAX_TRACKED_FILES: usize = 5_000;
const MAX_TRACKED_BYTES: usize = 32 * 1024 * 1024;
const MAX_TRACKED_FILE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_HASHED_FILE_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_HASHED_TOTAL_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const HASH_BUFFER_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileSample {
    content: Option<Vec<u8>>,
    metadata: FileStateMetadata,
    modified_nanos: u128,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct WorkspaceSnapshot {
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
    pub(crate) changed_files: Vec<PathBuf>,
    pub(crate) before_images: Vec<FileBeforeImage>,
    pub(crate) after_images: Vec<FileBeforeImage>,
    pub(crate) complete: bool,
}

pub(crate) async fn capture(root: &Path) -> WorkspaceSnapshot {
    let root = root.to_path_buf();
    tokio::task::spawn_blocking(move || capture_blocking(&root))
        .await
        .unwrap_or_default()
}

pub(crate) async fn compare(root: &Path, before: WorkspaceSnapshot) -> WorkspaceMutationScan {
    let after = capture(root).await;
    compare_snapshots(before, after)
}

fn capture_blocking(root: &Path) -> WorkspaceSnapshot {
    let mut snapshot = WorkspaceSnapshot {
        scan_complete: true,
        checkpoint_complete: true,
        ..WorkspaceSnapshot::default()
    };
    let mut retained_bytes = 0_usize;
    let mut hashed_bytes = 0_u64;
    let mut walker = WalkDir::new(root).follow_links(false).into_iter();
    while let Some(entry) = walker.next() {
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
            continue;
        }
        if snapshot.files.len() >= MAX_TRACKED_FILES {
            snapshot.scan_complete = false;
            snapshot.checkpoint_complete = false;
            break;
        }
        let metadata = match entry.metadata() {
            Ok(metadata) => metadata,
            Err(_) => {
                snapshot.scan_complete = false;
                snapshot.checkpoint_complete = false;
                continue;
            }
        };
        let file_bytes = usize::try_from(metadata.len()).unwrap_or(usize::MAX);
        let retain_content = metadata.len() <= MAX_TRACKED_FILE_BYTES
            && retained_bytes.saturating_add(file_bytes) <= MAX_TRACKED_BYTES;
        let (content, checksum, content_kind) = if retain_content {
            match fs::read(entry.path()) {
                Ok(content) => {
                    retained_bytes = retained_bytes.saturating_add(content.len());
                    hashed_bytes = hashed_bytes.saturating_add(metadata.len());
                    let checksum = checksum_bytes(&content);
                    let kind = content_kind(&content);
                    (Some(content), Some(checksum), kind)
                }
                Err(_) => {
                    snapshot.scan_complete = false;
                    snapshot.checkpoint_complete = false;
                    continue;
                }
            }
        } else {
            snapshot.checkpoint_complete = false;
            if metadata.len() <= MAX_HASHED_FILE_BYTES
                && hashed_bytes.saturating_add(metadata.len()) <= MAX_HASHED_TOTAL_BYTES
            {
                match hash_file(entry.path()) {
                    Ok((checksum, kind)) => {
                        hashed_bytes = hashed_bytes.saturating_add(metadata.len());
                        (None, Some(checksum), kind)
                    }
                    Err(_) => {
                        snapshot.scan_complete = false;
                        (None, None, FileContentKind::Unknown)
                    }
                }
            } else {
                snapshot.scan_complete = false;
                (None, None, FileContentKind::Unknown)
            }
        };
        let unix_mode = unix_mode(&metadata);
        snapshot.files.insert(
            entry.path().to_path_buf(),
            FileSample {
                content,
                metadata: FileStateMetadata {
                    size_bytes: metadata.len(),
                    checksum,
                    unix_mode,
                    content_kind,
                    content_available: retain_content,
                },
                modified_nanos: modified_nanos(&metadata),
            },
        );
    }
    snapshot
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
    scan
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

fn hash_file(path: &Path) -> std::io::Result<(String, FileContentKind)> {
    let mut file = fs::File::open(path)?;
    let mut buffer = vec![0_u8; HASH_BUFFER_BYTES];
    let mut hasher = Sha256::new();
    let mut prefix = Vec::new();
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        if prefix.len() < 8 * 1024 {
            let retained = (8 * 1024 - prefix.len()).min(read);
            prefix.extend_from_slice(&buffer[..retained]);
        }
        hasher.update(&buffer[..read]);
    }
    Ok((
        format!("sha256:{:x}", hasher.finalize()),
        content_kind(&prefix),
    ))
}

fn checksum_bytes(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
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
            Some(".git" | ".golutra" | "target" | "node_modules" | ".next" | "dist" | "build")
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
    use tempfile::tempdir;

    use super::*;

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
    async fn large_binary_changes_keep_checksums_without_retaining_content() {
        let workspace = tempdir().expect("workspace");
        let path = workspace.path().join("large.bin");
        let mut before_bytes = vec![0_u8; MAX_TRACKED_FILE_BYTES as usize + 1];
        before_bytes[1] = 1;
        fs::write(&path, &before_bytes).expect("write binary baseline");
        let before = capture(workspace.path()).await;

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
