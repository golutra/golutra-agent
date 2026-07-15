use std::path::PathBuf;

use chrono::{DateTime, Utc};
use golutra_core::SideEffectType;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const PLUGIN_MANIFEST_FILE: &str = "golutra-plugin.json";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginManifest {
    pub schema_version: u32,
    pub id: String,
    pub version: String,
    pub display_name: Option<String>,
    pub description: Option<String>,
    pub server: McpServerManifest,
    pub permissions: PluginPermissions,
    pub tools: Vec<PluginToolManifest>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServerManifest {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PluginWorkspaceAccess {
    #[default]
    ReadOnly,
    ReadWrite,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PluginPermissions {
    #[serde(default)]
    pub workspace_access: PluginWorkspaceAccess,
    #[serde(default)]
    pub allow_network: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginToolManifest {
    pub name: String,
    pub description: Option<String>,
    pub input_schema: Value,
    pub output_schema: Option<Value>,
    pub side_effect_type: SideEffectType,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginRevisionState {
    Staged,
    Reviewed,
    Enabled,
    Disabled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginRevision {
    pub revision_id: String,
    pub manifest: PluginManifest,
    pub package_dir: String,
    pub checksum: String,
    pub state: PluginRevisionState,
    pub staged_at: DateTime<Utc>,
    pub reviewed_at: Option<DateTime<Utc>>,
    pub enabled_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginRecord {
    pub plugin_id: String,
    pub active_revision_id: Option<String>,
    pub revisions: Vec<PluginRevision>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginRegistryState {
    pub schema_version: u32,
    pub plugins: Vec<PluginRecord>,
}

impl Default for PluginRegistryState {
    fn default() -> Self {
        Self {
            schema_version: 1,
            plugins: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnabledPlugin {
    pub revision_id: String,
    pub manifest: PluginManifest,
    pub package_root: PathBuf,
    pub checksum: String,
}
