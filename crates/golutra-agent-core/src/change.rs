use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FileChangeKind {
    Added,
    Modified,
    Deleted,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FileContentKind {
    Text,
    Binary,
    #[default]
    Unknown,
}

/// Immutable metadata captured for one side of a workspace file change.
///
/// `content_available=false` means the scanner retained metadata and a
/// checksum without copying the file body into the bounded before-image.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct FileStateMetadata {
    pub size_bytes: u64,
    pub checksum: Option<String>,
    pub unix_mode: Option<u32>,
    pub content_kind: FileContentKind,
    pub content_available: bool,
}

/// A content change for one workspace-relative file.
///
/// Line counts are absent when either side is binary, unavailable, or outside
/// the bounded diff budget. Consumers must not treat absent counts as zero.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct FileChangeSummary {
    pub path: String,
    pub kind: FileChangeKind,
    pub added_lines: Option<u64>,
    pub removed_lines: Option<u64>,
    #[serde(default)]
    pub before: Option<FileStateMetadata>,
    #[serde(default)]
    pub after: Option<FileStateMetadata>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct FileDiffPreview {
    pub path: String,
    pub lines: Vec<String>,
    pub truncated: bool,
}

/// The net workspace change from the beginning of a turn to its latest file
/// side effect. This is a durable event payload, not a live filesystem query.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TurnChangeSummary {
    pub files: Vec<FileChangeSummary>,
    pub added_lines: Option<u64>,
    pub removed_lines: Option<u64>,
    pub stats_complete: bool,
    #[serde(default)]
    pub file_count: u64,
    #[serde(default)]
    pub files_truncated: bool,
}
