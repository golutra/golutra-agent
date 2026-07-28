use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{ArtifactId, EvidenceId, PolicyId, ToolCallId};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SideEffectType {
    None,
    File,
    Process,
    Network,
    ExternalSystem,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ToolResultStatus {
    Ok,
    Error,
    Blocked,
    Cancelled,
    Timeout,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ToolProgressPhase {
    Started,
    Output,
    Completed,
}

/// Bounded, presentation-safe progress for one tool call.
///
/// Progress is diagnostic and may be sampled. Durable completion facts live in
/// [`ToolExecutionMetrics`], so consumers must not infer success from the last
/// progress event they happened to receive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ToolProgress {
    pub tool_call_id: ToolCallId,
    pub tool_name: String,
    pub phase: ToolProgressPhase,
    pub elapsed_ms: u64,
    pub output_bytes: u64,
    pub output_lines: u64,
    pub detail: Option<String>,
}

/// Stable execution metrics attached to every terminal tool report.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ToolExecutionMetrics {
    pub duration_ms: u64,
    pub output_bytes: u64,
    pub output_lines: u64,
    pub output_truncated: bool,
    pub exit_code: Option<i32>,
    pub item_count: Option<u64>,
    pub match_count: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ToolContract {
    pub tool_name: String,
    pub input_schema: Value,
    pub output_schema: Value,
    pub error_schema: Value,
    pub side_effect_type: SideEffectType,
    pub idempotency_key_policy: String,
    pub timeout_policy: String,
    pub cancellation_policy: String,
    pub retry_policy: String,
    pub artifact_policy: String,
    pub permission_policy_ref: Option<PolicyId>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ToolResultEnvelope {
    pub tool_call_id: ToolCallId,
    pub tool_name: String,
    pub status: ToolResultStatus,
    pub summary: String,
    pub structured_facts: Value,
    pub model_visible_excerpt: Option<String>,
    pub raw_artifact_ref: Option<ArtifactId>,
    pub evidence_refs: Vec<EvidenceId>,
    pub risk: String,
    pub verification_hint: Option<String>,
}

/// Returns a stable operational family for strategies that should share retry
/// and diagnosis state across superficially different tool calls.
#[must_use]
pub fn semantic_tool_failure_family(tool_name: &str, facts: &Value) -> Option<String> {
    let command = facts
        .get("command")
        .map(|command| match command {
            Value::String(command) => command.clone(),
            Value::Array(parts) => parts
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(" "),
            _ => String::new(),
        })
        .unwrap_or_default()
        .to_ascii_lowercase();
    if tool_name == "shell" {
        if command.contains("apt-get") && command.contains("install") {
            return Some("dependency_install:apt".to_owned());
        }
        if (command.contains("pip install") || command.contains("pip3 install"))
            || (command.contains("-m pip") && command.contains("install"))
        {
            return Some("dependency_install:pip".to_owned());
        }
        if command.contains("apt-get") && command.contains("update") {
            return Some("dependency_index:apt".to_owned());
        }
    }
    if matches!(tool_name, "process_poll" | "process_reconnect") {
        let process_id = facts
            .get("process_id")
            .map(|value| match value {
                Value::String(value) => value.clone(),
                value => value.to_string(),
            })
            .unwrap_or_else(|| "unknown".to_owned());
        return Some(format!("process_wait:{process_id}"));
    }
    None
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::semantic_tool_failure_family;

    #[test]
    fn semantic_failure_family_accepts_string_and_argv_commands() {
        assert_eq!(
            semantic_tool_failure_family(
                "shell",
                &json!({"command": "sudo apt-get install parquet-tools"}),
            )
            .as_deref(),
            Some("dependency_install:apt")
        );
        assert_eq!(
            semantic_tool_failure_family(
                "shell",
                &json!({"command": ["python", "-m", "pip", "install", "pyarrow"]}),
            )
            .as_deref(),
            Some("dependency_install:pip")
        );
    }
}
