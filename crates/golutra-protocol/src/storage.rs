use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct StorageStats {
    pub artifact_records: u64,
    pub live_artifact_blobs: u64,
    pub expired_artifact_blobs: u64,
    pub live_artifact_bytes: u64,
    pub checkpoint_directories: u64,
    pub rollout_files: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct StorageMaintenanceReport {
    pub artifact_blobs_removed: u64,
    pub protected_artifacts_skipped: u64,
    pub temporary_artifacts_removed: u64,
    pub checkpoint_directories_removed: u64,
    pub completed_at: DateTime<Utc>,
    pub stats: StorageStats,
}
