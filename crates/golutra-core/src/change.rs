use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FileChangeKind {
    Added,
    Modified,
    Deleted,
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
}

/// The net workspace change from the beginning of a turn to its latest file
/// side effect. This is a durable event payload, not a live filesystem query.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TurnChangeSummary {
    pub files: Vec<FileChangeSummary>,
    pub added_lines: Option<u64>,
    pub removed_lines: Option<u64>,
    pub stats_complete: bool,
}
