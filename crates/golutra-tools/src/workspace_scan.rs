//! Bounded before/after workspace sampling for opaque process tools.

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use walkdir::{DirEntry, WalkDir};

use super::FileBeforeImage;

const MAX_TRACKED_FILES: usize = 5_000;
const MAX_TRACKED_BYTES: usize = 32 * 1024 * 1024;
const MAX_TRACKED_FILE_BYTES: u64 = 2 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileSample {
    content: Vec<u8>,
    unix_mode: Option<u32>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct WorkspaceSnapshot {
    files: BTreeMap<PathBuf, FileSample>,
    complete: bool,
}

impl WorkspaceSnapshot {
    pub(crate) fn before_images(&self) -> Vec<FileBeforeImage> {
        self.files
            .iter()
            .map(|(path, sample)| FileBeforeImage {
                path: path.clone(),
                content: Some(sample.content.clone()),
                unix_mode: sample.unix_mode,
            })
            .collect()
    }

    pub(crate) fn is_complete(&self) -> bool {
        self.complete
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
        complete: true,
        ..WorkspaceSnapshot::default()
    };
    let mut retained_bytes = 0_usize;
    let mut walker = WalkDir::new(root).follow_links(false).into_iter();
    while let Some(entry) = walker.next() {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => {
                snapshot.complete = false;
                continue;
            }
        };
        if entry.depth() > 0 && is_excluded_dir(&entry) {
            // The process can mutate an excluded subtree even though it is
            // intentionally outside the bounded snapshot. Preserve that
            // uncertainty instead of reporting a complete workspace scan.
            snapshot.complete = false;
            walker.skip_current_dir();
            continue;
        }
        if entry.file_type().is_symlink() {
            // A process can mutate a symlink target outside the sampled tree;
            // report the scan as incomplete instead of claiming no changes.
            snapshot.complete = false;
            continue;
        }
        if !entry.file_type().is_file() {
            continue;
        }
        if snapshot.files.len() >= MAX_TRACKED_FILES {
            snapshot.complete = false;
            break;
        }
        let metadata = match entry.metadata() {
            Ok(metadata) => metadata,
            Err(_) => {
                snapshot.complete = false;
                continue;
            }
        };
        if metadata.len() > MAX_TRACKED_FILE_BYTES {
            snapshot.complete = false;
            continue;
        }
        let file_bytes = usize::try_from(metadata.len()).unwrap_or(usize::MAX);
        if retained_bytes.saturating_add(file_bytes) > MAX_TRACKED_BYTES {
            snapshot.complete = false;
            break;
        }
        let content = match fs::read(entry.path()) {
            Ok(content) => content,
            Err(_) => {
                snapshot.complete = false;
                continue;
            }
        };
        retained_bytes = retained_bytes.saturating_add(content.len());
        snapshot.files.insert(
            entry.path().to_path_buf(),
            FileSample {
                content,
                unix_mode: unix_mode(&metadata),
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
        complete: before.complete && after.complete,
        ..WorkspaceMutationScan::default()
    };
    for path in paths {
        let old = before.files.get(&path);
        let new = after.files.get(&path);
        if old.map(|sample| (&sample.content, sample.unix_mode))
            == new.map(|sample| (&sample.content, sample.unix_mode))
        {
            continue;
        }
        scan.changed_files.push(path.clone());
        scan.before_images.push(FileBeforeImage {
            path: path.clone(),
            content: old.map(|sample| sample.content.clone()),
            unix_mode: old.and_then(|sample| sample.unix_mode),
        });
        scan.after_images.push(FileBeforeImage {
            path,
            content: new.map(|sample| sample.content.clone()),
            unix_mode: new.and_then(|sample| sample.unix_mode),
        });
    }
    scan
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
    async fn excluded_subtrees_make_the_scan_explicitly_incomplete() {
        let workspace = tempdir().expect("workspace");
        fs::create_dir(workspace.path().join("target")).expect("target");
        fs::write(workspace.path().join("target/output.bin"), "generated").expect("output");

        let snapshot = capture(workspace.path()).await;

        assert!(!snapshot.complete);
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

        assert!(!snapshot.complete);
    }
}
