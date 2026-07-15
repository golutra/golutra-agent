mod model;
mod parser;

use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};

use chrono::Utc;
use fs2::FileExt;
use ignore::WalkBuilder;
pub use model::*;
use parser::{graph_edges, parse_source};
use sha2::{Digest, Sha256};
use thiserror::Error;

const MAX_SOURCE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_INDEX_BYTES: u64 = 128 * 1024 * 1024;

#[derive(Debug, Error)]
pub enum CodeIntelligenceError {
    #[error("code intelligence IO failed: {0}")]
    Io(String),
    #[error("code intelligence JSON failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("code intelligence parser failed: {0}")]
    Parser(String),
    #[error("code intelligence workspace is invalid: {0}")]
    InvalidWorkspace(String),
    #[error("code intelligence index limit exceeded: {0}")]
    Limit(String),
}

#[derive(Debug, Clone)]
pub struct CodeIntelligence {
    workspace_root: PathBuf,
}

impl CodeIntelligence {
    pub fn new(workspace_root: impl AsRef<Path>) -> Result<Self, CodeIntelligenceError> {
        let workspace_root = workspace_root
            .as_ref()
            .canonicalize()
            .map_err(|error| CodeIntelligenceError::Io(error.to_string()))?;
        if !workspace_root.is_dir() {
            return Err(CodeIntelligenceError::InvalidWorkspace(
                workspace_root.display().to_string(),
            ));
        }
        Ok(Self { workspace_root })
    }

    pub fn build(&self) -> Result<CodeGraph, CodeIntelligenceError> {
        let mut files = WalkBuilder::new(&self.workspace_root)
            .hidden(false)
            .git_ignore(true)
            .git_global(true)
            .git_exclude(true)
            .build()
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_type()
                    .is_some_and(|file_type| file_type.is_file())
            })
            .filter_map(|entry| {
                let path = entry.into_path();
                supported_source_path(&path).then_some(path)
            })
            .collect::<Vec<_>>();
        files.sort();
        let mut symbols = Vec::new();
        let mut references = Vec::new();
        let mut dependencies = Vec::new();
        let mut digest = Sha256::new();
        let mut files_indexed = 0_u32;
        for path in files {
            let metadata = fs::symlink_metadata(&path)
                .map_err(|error| CodeIntelligenceError::Io(error.to_string()))?;
            if metadata.file_type().is_symlink() || metadata.len() > MAX_SOURCE_BYTES {
                continue;
            }
            let source = fs::read_to_string(&path)
                .map_err(|error| CodeIntelligenceError::Io(error.to_string()))?;
            let relative_path = path
                .strip_prefix(&self.workspace_root)
                .map_err(|_| CodeIntelligenceError::InvalidWorkspace(path.display().to_string()))?;
            let Some(parsed) = parse_source(relative_path, &source)
                .map_err(|error| CodeIntelligenceError::Parser(error.to_string()))?
            else {
                continue;
            };
            let normalized_path = relative_path.to_string_lossy().replace('\\', "/");
            digest.update(normalized_path.as_bytes());
            digest.update(Sha256::digest(source.as_bytes()));
            dependencies.extend(
                parsed
                    .dependencies
                    .into_iter()
                    .map(|dependency| (normalized_path.clone(), dependency)),
            );
            symbols.extend(parsed.symbols);
            references.extend(parsed.references);
            files_indexed = files_indexed.saturating_add(1);
        }
        let edges = graph_edges(&symbols, &references, &dependencies);
        Ok(CodeGraph {
            version: CODE_GRAPH_VERSION,
            workspace_root: self.workspace_root.display().to_string(),
            generated_at: Utc::now(),
            source_digest: format!("sha256:{:x}", digest.finalize()),
            files_indexed,
            symbols,
            references,
            edges,
        })
    }

    #[must_use]
    pub fn query_symbols(graph: &CodeGraph, query: &str, limit: usize) -> SymbolQueryResult {
        let query_lower = query.trim().to_ascii_lowercase();
        let mut matches = graph
            .symbols
            .iter()
            .filter_map(|symbol| {
                let name = symbol.name.to_ascii_lowercase();
                let score = if name == query_lower {
                    0
                } else if name.starts_with(&query_lower) {
                    1
                } else if name.contains(&query_lower) {
                    2
                } else {
                    return None;
                };
                Some((score, symbol.clone()))
            })
            .collect::<Vec<_>>();
        matches.sort_by(|(left_score, left), (right_score, right)| {
            left_score
                .cmp(right_score)
                .then(left.name.cmp(&right.name))
                .then(left.path.cmp(&right.path))
        });
        SymbolQueryResult {
            query: query.to_owned(),
            matches: matches
                .into_iter()
                .take(limit.max(1))
                .map(|(_, symbol)| symbol)
                .collect(),
        }
    }

    #[must_use]
    pub fn query_references(
        graph: &CodeGraph,
        symbol_name: &str,
        limit: usize,
    ) -> ReferenceQueryResult {
        ReferenceQueryResult {
            symbol_name: symbol_name.to_owned(),
            references: graph
                .references
                .iter()
                .filter(|reference| reference.symbol_name == symbol_name)
                .take(limit.max(1))
                .cloned()
                .collect(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct CodeIndexStore {
    path: PathBuf,
}

impl CodeIndexStore {
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn save(&self, graph: &CodeGraph) -> Result<(), CodeIntelligenceError> {
        let encoded = serde_json::to_vec(graph)?;
        if u64::try_from(encoded.len()).unwrap_or(u64::MAX) > MAX_INDEX_BYTES {
            return Err(CodeIntelligenceError::Limit(format!(
                "serialized graph exceeds {MAX_INDEX_BYTES} bytes"
            )));
        }
        let parent = self.path.parent().ok_or_else(|| {
            CodeIntelligenceError::Io(format!("{} has no parent", self.path.display()))
        })?;
        ensure_private_dir(parent)?;
        let lock_path = self.path.with_extension("lock");
        let lock = open_private_file(&lock_path, false)?;
        lock.lock_exclusive()
            .map_err(|error| CodeIntelligenceError::Io(error.to_string()))?;
        reject_symlink(&self.path)?;
        let temporary = self.path.with_extension("json.tmp");
        let mut file = open_private_file(&temporary, true)?;
        file.write_all(&encoded)
            .map_err(|error| CodeIntelligenceError::Io(error.to_string()))?;
        file.sync_all()
            .map_err(|error| CodeIntelligenceError::Io(error.to_string()))?;
        replace_file(&temporary, &self.path)?;
        set_owner_only_file(&self.path)?;
        File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|error| CodeIntelligenceError::Io(error.to_string()))
    }

    pub fn load(&self) -> Result<Option<CodeGraph>, CodeIntelligenceError> {
        reject_symlink(&self.path)?;
        let file = match File::open(&self.path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(CodeIntelligenceError::Io(error.to_string())),
        };
        let metadata = file
            .metadata()
            .map_err(|error| CodeIntelligenceError::Io(error.to_string()))?;
        if metadata.len() > MAX_INDEX_BYTES {
            return Err(CodeIntelligenceError::Limit(format!(
                "{} exceeds {MAX_INDEX_BYTES} bytes",
                self.path.display()
            )));
        }
        let mut bytes = Vec::new();
        file.take(MAX_INDEX_BYTES.saturating_add(1))
            .read_to_end(&mut bytes)
            .map_err(|error| CodeIntelligenceError::Io(error.to_string()))?;
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_INDEX_BYTES {
            return Err(CodeIntelligenceError::Limit(
                "index grew while reading".to_owned(),
            ));
        }
        let graph: CodeGraph = serde_json::from_slice(&bytes)?;
        if graph.version != CODE_GRAPH_VERSION {
            return Err(CodeIntelligenceError::InvalidWorkspace(format!(
                "unsupported code graph version {}",
                graph.version
            )));
        }
        Ok(Some(graph))
    }
}

fn supported_source_path(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|extension| extension.to_str()),
        Some("rs" | "ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs" | "py" | "pyi")
    )
}

fn reject_symlink(path: &Path) -> Result<(), CodeIntelligenceError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            Err(CodeIntelligenceError::InvalidWorkspace(format!(
                "index path cannot be a symlink: {}",
                path.display()
            )))
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(CodeIntelligenceError::Io(error.to_string())),
    }
}

fn open_private_file(path: &Path, truncate: bool) -> Result<File, CodeIntelligenceError> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(truncate)
        .read(!truncate)
        .write(true)
        .open(path)
        .map_err(|error| CodeIntelligenceError::Io(error.to_string()))?;
    set_owner_only_file(path)?;
    Ok(file)
}

fn replace_file(source: &Path, target: &Path) -> Result<(), CodeIntelligenceError> {
    #[cfg(windows)]
    if target.exists() {
        fs::remove_file(target).map_err(|error| CodeIntelligenceError::Io(error.to_string()))?;
    }
    fs::rename(source, target).map_err(|error| CodeIntelligenceError::Io(error.to_string()))
}

fn ensure_private_dir(path: &Path) -> Result<(), CodeIntelligenceError> {
    fs::create_dir_all(path).map_err(|error| CodeIntelligenceError::Io(error.to_string()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .map_err(|error| CodeIntelligenceError::Io(error.to_string()))?;
    }
    Ok(())
}

fn set_owner_only_file(path: &Path) -> Result<(), CodeIntelligenceError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .map_err(|error| CodeIntelligenceError::Io(error.to_string()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
