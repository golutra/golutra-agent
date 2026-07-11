use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::{Duration, Instant},
};

use golutra_policy::WorkspacePolicy;
use serde::{Deserialize, Serialize};
use sqlx::{SqlitePool, sqlite::SqlitePoolOptions};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum FileSearchError {
    #[error("policy rejected path: {0}")]
    Policy(String),
    #[error("file search io failed: {0}")]
    Io(String),
    #[error("rg execution failed: {0}")]
    Rg(String),
    #[error("file metadata index failed: {0}")]
    Metadata(String),
    #[error("file search limit exceeded: {0}")]
    Limit(String),
    #[error("file search timed out after {0} ms")]
    Timeout(u64),
    #[error("file search was cancelled")]
    Cancelled,
}

const MAX_INDEXED_FILES: usize = 100_000;
const MAX_RG_OUTPUT_BYTES: u64 = 8 * 1024 * 1024;
const DEFAULT_OPERATION_TIMEOUT: Duration = Duration::from_secs(30);
const PROCESS_POLL_INTERVAL: Duration = Duration::from_millis(10);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEntry {
    pub relative_path: String,
    pub size_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchMatch {
    pub relative_path: String,
    pub line_number: u64,
    pub line: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileMetadataRecord {
    pub relative_path: String,
    pub size_bytes: u64,
}

#[derive(Debug, Clone)]
pub struct FileMetadataIndex {
    pool: SqlitePool,
}

impl FileMetadataIndex {
    pub async fn in_memory() -> Result<Self, FileSearchError> {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .map_err(|error| FileSearchError::Metadata(error.to_string()))?;
        let index = Self { pool };
        index.migrate().await?;
        Ok(index)
    }

    pub async fn connect(database_url: &str) -> Result<Self, FileSearchError> {
        let pool = SqlitePool::connect(database_url)
            .await
            .map_err(|error| FileSearchError::Metadata(error.to_string()))?;
        let index = Self { pool };
        index.migrate().await?;
        Ok(index)
    }

    pub async fn index_workspace(&self, search: &FileSearch) -> Result<u64, FileSearchError> {
        let search = search.clone();
        let files = tokio::task::spawn_blocking(move || search.list_files())
            .await
            .map_err(|error| {
                FileSearchError::Io(format!("file indexing task failed: {error}"))
            })??;
        self.replace_all(&files).await
    }

    pub async fn list_metadata(&self) -> Result<Vec<FileMetadataRecord>, FileSearchError> {
        let rows = sqlx::query_as::<_, (String, i64)>(
            "SELECT relative_path, size_bytes FROM file_metadata ORDER BY relative_path",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|error| FileSearchError::Metadata(error.to_string()))?;

        rows.into_iter()
            .map(|(relative_path, size_bytes)| {
                u64::try_from(size_bytes)
                    .map(|size_bytes| FileMetadataRecord {
                        relative_path,
                        size_bytes,
                    })
                    .map_err(|error| FileSearchError::Metadata(error.to_string()))
            })
            .collect()
    }

    async fn migrate(&self) -> Result<(), FileSearchError> {
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS file_metadata (
                relative_path TEXT PRIMARY KEY NOT NULL,
                size_bytes INTEGER NOT NULL
            )",
        )
        .execute(&self.pool)
        .await
        .map_err(|error| FileSearchError::Metadata(error.to_string()))?;
        Ok(())
    }

    async fn replace_all(&self, files: &[FileEntry]) -> Result<u64, FileSearchError> {
        let mut transaction = self
            .pool
            .begin()
            .await
            .map_err(|error| FileSearchError::Metadata(error.to_string()))?;
        sqlx::query("DELETE FROM file_metadata")
            .execute(&mut *transaction)
            .await
            .map_err(|error| FileSearchError::Metadata(error.to_string()))?;

        for file in files {
            let size_bytes = i64::try_from(file.size_bytes)
                .map_err(|error| FileSearchError::Metadata(error.to_string()))?;
            sqlx::query(
                "INSERT INTO file_metadata (relative_path, size_bytes)
                 VALUES (?, ?)",
            )
            .bind(&file.relative_path)
            .bind(size_bytes)
            .execute(&mut *transaction)
            .await
            .map_err(|error| FileSearchError::Metadata(error.to_string()))?;
        }

        transaction
            .commit()
            .await
            .map_err(|error| FileSearchError::Metadata(error.to_string()))?;
        u64::try_from(files.len()).map_err(|error| FileSearchError::Metadata(error.to_string()))
    }
}

#[derive(Debug, Clone)]
pub struct FileSearch {
    policy: WorkspacePolicy,
}

impl FileSearch {
    #[must_use]
    pub fn new(policy: WorkspacePolicy) -> Self {
        Self { policy }
    }

    pub fn list_files(&self) -> Result<Vec<FileEntry>, FileSearchError> {
        self.list_files_with_cancellation(&AtomicBool::new(false))
    }

    pub fn list_files_with_cancellation(
        &self,
        cancellation: &AtomicBool,
    ) -> Result<Vec<FileEntry>, FileSearchError> {
        let mut entries = Vec::new();
        self.collect_files(
            self.policy.workspace_root(),
            &mut entries,
            cancellation,
            Instant::now() + DEFAULT_OPERATION_TIMEOUT,
        )?;
        entries.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
        Ok(entries)
    }

    pub fn search(
        &self,
        pattern: &str,
        path: impl AsRef<Path>,
    ) -> Result<Vec<SearchMatch>, FileSearchError> {
        self.search_with_cancellation(pattern, path, &AtomicBool::new(false))
    }

    pub fn search_with_cancellation(
        &self,
        pattern: &str,
        path: impl AsRef<Path>,
        cancellation: &AtomicBool,
    ) -> Result<Vec<SearchMatch>, FileSearchError> {
        let deadline = Instant::now() + DEFAULT_OPERATION_TIMEOUT;
        check_operation_state(cancellation, deadline)?;
        let evaluation = self
            .policy
            .evaluate_path("file_search", path.as_ref(), true);
        if evaluation.decision != golutra_core::PolicyDecision::Allow {
            return Err(FileSearchError::Policy(evaluation.reason));
        }
        let resolved_path = PathBuf::from(evaluation.resource);
        let mut child = Command::new("rg")
            .arg("--json")
            .arg("--line-number")
            .arg("--no-heading")
            .arg("--")
            .arg(pattern)
            .arg(&resolved_path)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| FileSearchError::Rg(error.to_string()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| FileSearchError::Rg("rg stdout is unavailable".to_owned()))?;
        let reader = thread::spawn(move || read_bounded_rg_output(stdout));
        let status = loop {
            if let Err(error) = check_operation_state(cancellation, deadline) {
                let _ = child.kill();
                let _ = child.wait();
                let _ = reader.join();
                return Err(error);
            }
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => thread::sleep(PROCESS_POLL_INTERVAL),
                Err(error) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = reader.join();
                    return Err(FileSearchError::Rg(error.to_string()));
                }
            }
        };
        let bytes = reader
            .join()
            .map_err(|_| FileSearchError::Rg("rg output reader panicked".to_owned()))??;
        if bytes.len() as u64 > MAX_RG_OUTPUT_BYTES {
            return Err(FileSearchError::Limit(format!(
                "rg output exceeds {MAX_RG_OUTPUT_BYTES} bytes"
            )));
        }

        if !(status.success() || status.code() == Some(1)) {
            return Err(FileSearchError::Rg(format!(
                "rg exited with status {status}"
            )));
        }

        let stdout = String::from_utf8_lossy(&bytes);
        stdout
            .lines()
            .filter_map(|line| parse_rg_json_line(line, self.policy.workspace_root()))
            .collect()
    }

    fn collect_files(
        &self,
        dir: &Path,
        entries: &mut Vec<FileEntry>,
        cancellation: &AtomicBool,
        deadline: Instant,
    ) -> Result<(), FileSearchError> {
        let mut pending_dirs = vec![dir.to_path_buf()];
        while let Some(directory) = pending_dirs.pop() {
            check_operation_state(cancellation, deadline)?;
            for entry in
                fs::read_dir(&directory).map_err(|error| FileSearchError::Io(error.to_string()))?
            {
                check_operation_state(cancellation, deadline)?;
                let entry = entry.map_err(|error| FileSearchError::Io(error.to_string()))?;
                let path = entry.path();
                if should_skip(&path) {
                    continue;
                }
                let metadata = fs::symlink_metadata(&path)
                    .map_err(|error| FileSearchError::Io(error.to_string()))?;
                if metadata.file_type().is_symlink() {
                    continue;
                }
                if metadata.is_dir() {
                    pending_dirs.push(path);
                    continue;
                }
                if !metadata.is_file() {
                    continue;
                }
                if entries.len() >= MAX_INDEXED_FILES {
                    return Err(FileSearchError::Limit(format!(
                        "workspace contains more than {MAX_INDEXED_FILES} indexable files"
                    )));
                }
                let relative_path = path
                    .strip_prefix(self.policy.workspace_root())
                    .map_err(|error| FileSearchError::Io(error.to_string()))?
                    .display()
                    .to_string();
                entries.push(FileEntry {
                    relative_path,
                    size_bytes: metadata.len(),
                });
            }
        }
        Ok(())
    }
}

fn read_bounded_rg_output(
    mut stdout: std::process::ChildStdout,
) -> Result<Vec<u8>, FileSearchError> {
    let mut bytes = Vec::new();
    stdout
        .by_ref()
        .take(MAX_RG_OUTPUT_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| FileSearchError::Rg(error.to_string()))?;
    Ok(bytes)
}

fn check_operation_state(
    cancellation: &AtomicBool,
    deadline: Instant,
) -> Result<(), FileSearchError> {
    if cancellation.load(Ordering::Acquire) {
        return Err(FileSearchError::Cancelled);
    }
    if Instant::now() >= deadline {
        return Err(FileSearchError::Timeout(
            u64::try_from(DEFAULT_OPERATION_TIMEOUT.as_millis()).unwrap_or(u64::MAX),
        ));
    }
    Ok(())
}

fn parse_rg_json_line(
    line: &str,
    workspace_root: &Path,
) -> Option<Result<SearchMatch, FileSearchError>> {
    let value: serde_json::Value = match serde_json::from_str(line) {
        Ok(value) => value,
        Err(error) => return Some(Err(FileSearchError::Rg(error.to_string()))),
    };
    if value.get("type").and_then(serde_json::Value::as_str) != Some("match") {
        return None;
    }
    let data = value.get("data")?;
    let path = PathBuf::from(data.get("path")?.get("text")?.as_str()?);
    let line_number = data.get("line_number")?.as_u64()?;
    let line = data.get("lines")?.get("text")?.as_str()?.to_owned();
    let relative_path = path
        .strip_prefix(workspace_root)
        .unwrap_or(&path)
        .display()
        .to_string();
    Some(Ok(SearchMatch {
        relative_path,
        line_number,
        line,
    }))
}

fn should_skip(path: &Path) -> bool {
    path.components().any(|component| {
        let std::path::Component::Normal(component) = component else {
            return false;
        };
        let name = component.to_string_lossy().to_ascii_lowercase();
        matches!(
            name.as_str(),
            ".git" | ".ssh" | ".golutra" | "secrets" | "target" | "node_modules"
        ) || name == ".env"
            || name.starts_with(".env.")
    })
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn lists_files_without_internal_dirs() {
        let workspace = tempdir().expect("workspace");
        fs::write(workspace.path().join("README.md"), "hello").expect("readme");
        fs::create_dir_all(workspace.path().join(".git")).expect("git dir");
        fs::write(workspace.path().join(".git/config"), "hidden").expect("hidden");
        let search = FileSearch::new(WorkspacePolicy::new(workspace.path()).expect("policy"));

        let files = search.list_files().expect("files");

        assert_eq!(files.len(), 1);
        assert_eq!(files[0].relative_path, "README.md");
    }

    #[test]
    fn file_listing_and_search_honor_preexisting_cancellation() {
        let workspace = tempdir().expect("workspace");
        fs::write(workspace.path().join("README.md"), "hello").expect("readme");
        let search = FileSearch::new(WorkspacePolicy::new(workspace.path()).expect("policy"));
        let cancellation = AtomicBool::new(true);

        assert!(matches!(
            search.list_files_with_cancellation(&cancellation),
            Err(FileSearchError::Cancelled)
        ));
        assert!(matches!(
            search.search_with_cancellation("hello", ".", &cancellation),
            Err(FileSearchError::Cancelled)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn file_index_does_not_follow_directory_symlinks_outside_workspace() {
        use std::os::unix::fs::symlink;

        let workspace = tempdir().expect("workspace");
        let outside = tempdir().expect("outside");
        fs::write(outside.path().join("secret.txt"), "secret").expect("outside file");
        symlink(outside.path(), workspace.path().join("linked")).expect("directory symlink");
        let search = FileSearch::new(WorkspacePolicy::new(workspace.path()).expect("policy"));

        assert!(search.list_files().expect("files").is_empty());
    }

    #[test]
    fn searches_with_rg_when_available() {
        let workspace = tempdir().expect("workspace");
        fs::write(workspace.path().join("README.md"), "hello\nworld").expect("readme");
        let search = FileSearch::new(WorkspacePolicy::new(workspace.path()).expect("policy"));

        let matches = search.search("world", ".").expect("search");

        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].relative_path, "README.md");
        assert_eq!(matches[0].line_number, 2);
    }

    #[test]
    fn search_pattern_starting_with_dash_is_not_treated_as_an_option() {
        let workspace = tempdir().expect("workspace");
        fs::write(workspace.path().join("README.md"), "--pre=sh\n").expect("readme");
        let search = FileSearch::new(WorkspacePolicy::new(workspace.path()).expect("policy"));

        let matches = search.search("--pre=sh", ".").expect("search");

        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].relative_path, "README.md");
    }

    #[tokio::test]
    async fn indexes_file_metadata_in_sqlite() {
        let workspace = tempdir().expect("workspace");
        fs::write(workspace.path().join("README.md"), "hello").expect("readme");
        fs::write(workspace.path().join(".env"), "secret").expect("env");
        let search = FileSearch::new(WorkspacePolicy::new(workspace.path()).expect("policy"));
        let index = FileMetadataIndex::in_memory().await.expect("index");

        let indexed = index.index_workspace(&search).await.expect("indexed");
        let metadata = index.list_metadata().await.expect("metadata");

        assert_eq!(indexed, 1);
        assert_eq!(
            metadata,
            vec![FileMetadataRecord {
                relative_path: "README.md".to_owned(),
                size_bytes: 5,
            }]
        );
    }
}
