use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use golutra_policy::WorkspacePolicy;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
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
}

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
        let pool = SqlitePool::connect("sqlite::memory:")
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
        let files = search.list_files()?;
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
        let mut entries = Vec::new();
        self.collect_files(self.policy.workspace_root(), &mut entries)?;
        entries.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
        Ok(entries)
    }

    pub fn search(
        &self,
        pattern: &str,
        path: impl AsRef<Path>,
    ) -> Result<Vec<SearchMatch>, FileSearchError> {
        let evaluation = self
            .policy
            .evaluate_path("file_search", path.as_ref(), true);
        if evaluation.decision != golutra_core::PolicyDecision::Allow {
            return Err(FileSearchError::Policy(evaluation.reason));
        }
        let resolved_path = self
            .policy
            .resolve_path(path, true)
            .map_err(|error| FileSearchError::Policy(error.to_string()))?;
        let output = Command::new("rg")
            .arg("--line-number")
            .arg("--no-heading")
            .arg(pattern)
            .arg(&resolved_path)
            .output()
            .map_err(|error| FileSearchError::Rg(error.to_string()))?;

        if !(output.status.success() || output.status.code() == Some(1)) {
            return Err(FileSearchError::Rg(
                String::from_utf8_lossy(&output.stderr).to_string(),
            ));
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        Ok(stdout
            .lines()
            .filter_map(|line| parse_rg_line(line, self.policy.workspace_root()))
            .collect())
    }

    fn collect_files(
        &self,
        dir: &Path,
        entries: &mut Vec<FileEntry>,
    ) -> Result<(), FileSearchError> {
        for entry in fs::read_dir(dir).map_err(|error| FileSearchError::Io(error.to_string()))? {
            let entry = entry.map_err(|error| FileSearchError::Io(error.to_string()))?;
            let path = entry.path();
            if should_skip(&path) {
                continue;
            }
            if path.is_dir() {
                self.collect_files(&path, entries)?;
            } else if path.is_file() {
                let metadata = entry
                    .metadata()
                    .map_err(|error| FileSearchError::Io(error.to_string()))?;
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

fn parse_rg_line(line: &str, workspace_root: &Path) -> Option<SearchMatch> {
    let mut parts = line.splitn(3, ':');
    let path = PathBuf::from(parts.next()?);
    let line_number = parts.next()?.parse().ok()?;
    let line = parts.next()?.to_owned();
    let relative_path = path
        .strip_prefix(workspace_root)
        .unwrap_or(&path)
        .display()
        .to_string();
    Some(SearchMatch {
        relative_path,
        line_number,
        line,
    })
}

fn should_skip(path: &Path) -> bool {
    let text = path.to_string_lossy();
    text.contains("/.git/")
        || text.ends_with("/.git")
        || text.contains("/.ssh/")
        || text.ends_with("/.ssh")
        || text.contains("/.env")
        || text.contains("/secrets/")
        || text.ends_with("/secrets")
        || text.contains("/target/")
        || text.ends_with("/target")
        || text.contains("/node_modules/")
        || text.ends_with("/node_modules")
        || text.contains("/.golutra/")
        || text.ends_with("/.golutra")
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
    fn searches_with_rg_when_available() {
        let workspace = tempdir().expect("workspace");
        fs::write(workspace.path().join("README.md"), "hello\nworld").expect("readme");
        let search = FileSearch::new(WorkspacePolicy::new(workspace.path()).expect("policy"));

        let matches = search.search("world", ".").expect("search");

        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].relative_path, "README.md");
        assert_eq!(matches[0].line_number, 2);
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
