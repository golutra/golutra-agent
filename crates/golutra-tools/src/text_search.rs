//! Small, bounded fallback for environments that do not ship `rg`.
//!
//! The normal path still uses ripgrep. This implementation exists for slim
//! benchmark/CI images and deliberately stays inside the already-resolved
//! workspace path; it is not a general-purpose process replacement.

use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use regex::Regex;
use tokio_util::sync::CancellationToken;
use walkdir::WalkDir;

use crate::workspace_scan;

const MAX_FILE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_OUTPUT_BYTES: usize = 2 * 1024 * 1024;
const MAX_FILES: usize = 100_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NativeSearchResult {
    pub(crate) output: String,
    pub(crate) matches: u64,
    pub(crate) scanned_files: u64,
    pub(crate) output_bytes: u64,
    pub(crate) output_truncated: bool,
    pub(crate) scan_truncated: bool,
    pub(crate) timed_out: bool,
    pub(crate) cancelled: bool,
}

pub(crate) fn search(
    pattern: &str,
    root: PathBuf,
    workspace_root: PathBuf,
    timeout_ms: u64,
    cancellation: CancellationToken,
) -> Result<NativeSearchResult, String> {
    let regex = Regex::new(pattern).map_err(|error| format!("invalid search pattern: {error}"))?;
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    let mut result = NativeSearchResult {
        output: String::new(),
        matches: 0,
        scanned_files: 0,
        output_bytes: 0,
        output_truncated: false,
        scan_truncated: false,
        timed_out: false,
        cancelled: false,
    };

    if root.is_file() {
        scan_file(
            &root,
            &regex,
            &workspace_root,
            &deadline,
            &cancellation,
            &mut result,
        );
    } else if root.is_dir() {
        for entry in WalkDir::new(&root)
            .follow_links(false)
            .into_iter()
            .filter_entry(|entry| !is_ignored_directory(entry.path(), &root))
        {
            if result.cancelled
                || result.timed_out
                || result.output_truncated
                || result.scan_truncated
            {
                break;
            }
            if cancellation.is_cancelled() {
                result.cancelled = true;
                break;
            }
            if Instant::now() >= deadline {
                result.timed_out = true;
                break;
            }
            let Ok(entry) = entry else {
                continue;
            };
            if entry.file_type().is_file() {
                scan_file(
                    entry.path(),
                    &regex,
                    &workspace_root,
                    &deadline,
                    &cancellation,
                    &mut result,
                );
            }
        }
    }

    result.output_bytes = result.output.len().try_into().unwrap_or(u64::MAX);
    Ok(result)
}

fn scan_file(
    path: &Path,
    regex: &Regex,
    workspace_root: &Path,
    deadline: &Instant,
    cancellation: &CancellationToken,
    result: &mut NativeSearchResult,
) {
    if result.scanned_files as usize >= MAX_FILES {
        result.scan_truncated = true;
        return;
    }
    if !path.starts_with(workspace_root) || is_hidden_path(path, workspace_root) {
        return;
    }
    if cancellation.is_cancelled() {
        result.cancelled = true;
        return;
    }
    if Instant::now() >= *deadline {
        result.timed_out = true;
        return;
    }
    let Ok(bytes) = workspace_scan::read_regular_file_bounded(
        path,
        workspace_root,
        MAX_FILE_BYTES,
        cancellation,
        *deadline,
    ) else {
        return;
    };
    result.scanned_files = result.scanned_files.saturating_add(1);
    if bytes.iter().take(8 * 1024).any(|byte| *byte == 0) {
        return;
    }
    let text = String::from_utf8_lossy(&bytes);
    for (line_number, line) in text.lines().enumerate() {
        if cancellation.is_cancelled() {
            result.cancelled = true;
            return;
        }
        if Instant::now() >= *deadline {
            result.timed_out = true;
            return;
        }
        if !regex.is_match(line) {
            continue;
        }
        result.matches = result.matches.saturating_add(1);
        let record = format!(
            "{}:{}:{}\n",
            path.display(),
            line_number.saturating_add(1),
            line.trim_end_matches('\r')
        );
        let remaining = MAX_OUTPUT_BYTES.saturating_sub(result.output.len());
        if record.len() > remaining {
            let mut boundary = remaining;
            while boundary > 0 && !record.is_char_boundary(boundary) {
                boundary = boundary.saturating_sub(1);
            }
            result.output.push_str(&record[..boundary]);
            result.output_truncated = true;
            return;
        }
        result.output.push_str(&record);
    }
}

fn is_ignored_directory(path: &Path, root: &Path) -> bool {
    if path == root {
        return false;
    }
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    name.starts_with('.')
        || matches!(
            name,
            "node_modules" | "target" | "__pycache__" | ".venv" | "venv"
        )
}

fn is_hidden_path(path: &Path, root: &Path) -> bool {
    path.strip_prefix(root)
        .ok()
        .into_iter()
        .flat_map(Path::components)
        .filter_map(|component| component.as_os_str().to_str())
        .any(|component| component.starts_with('.'))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn searches_workspace_without_following_hidden_directories() {
        let workspace = tempdir().expect("workspace");
        fs::write(workspace.path().join("visible.txt"), "needle here\n").expect("visible");
        fs::create_dir(workspace.path().join(".git")).expect("hidden directory");
        fs::write(workspace.path().join(".git").join("hidden.txt"), "needle\n").expect("hidden");

        let result = search(
            "needle",
            workspace.path().to_path_buf(),
            workspace.path().to_path_buf(),
            5_000,
            CancellationToken::new(),
        )
        .expect("search");

        assert_eq!(result.matches, 1);
        assert!(result.output.contains("visible.txt:1:needle here"));
        assert!(!result.output.contains("hidden.txt"));
    }

    #[test]
    fn cancellation_is_reported() {
        let workspace = tempdir().expect("workspace");
        fs::write(workspace.path().join("visible.txt"), "needle\n").expect("visible");
        let cancellation = CancellationToken::new();
        cancellation.cancel();

        let result = search(
            "needle",
            workspace.path().to_path_buf(),
            workspace.path().to_path_buf(),
            5_000,
            cancellation,
        )
        .expect("search");

        assert!(result.cancelled);
    }
}
