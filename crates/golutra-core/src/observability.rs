use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{
    EventId, ProviderRequestId, ProviderResponseId, RegressionCampaignId, RunId, SessionId, TaskId,
    Timestamp, ToolCallId, ToolResultStatus, TurnId, UserStepId, VerificationId, WorkspaceId,
};

pub const RUNTIME_EVENT_SCHEMA_VERSION: u32 = 3;
pub const BUILD_PROVENANCE_SCHEMA_VERSION: u32 = 1;
pub const RUN_PROVENANCE_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CausalRelation {
    Parent,
    TriggeredBy,
    RespondsTo,
    DerivedFrom,
    Verifies,
    Compares,
    Supersedes,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CausalLink {
    pub event_id: EventId,
    pub relation: CausalRelation,
}

/// Correlation identifiers propagated through one governed runtime execution.
///
/// The event envelope remains authoritative for session/task/turn ownership.
/// Repeating those identifiers here makes detached facts self-describing and
/// lets integrity validation reject mismatched context rather than guessing.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CausalContext {
    pub run_id: Option<RunId>,
    pub workspace_id: Option<WorkspaceId>,
    pub session_id: Option<SessionId>,
    pub task_id: Option<TaskId>,
    pub turn_id: Option<TurnId>,
    pub step_id: Option<String>,
    pub step_no: Option<u32>,
    pub provider_round_id: Option<String>,
    pub provider_request_id: Option<ProviderRequestId>,
    pub provider_response_id: Option<ProviderResponseId>,
    pub provider_tool_call_id: Option<String>,
    pub tool_call_id: Option<ToolCallId>,
    pub verification_id: Option<VerificationId>,
    pub candidate_id: Option<String>,
    pub regression_campaign_id: Option<RegressionCampaignId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct BuildProvenance {
    pub schema_version: u32,
    pub package_version: String,
    pub git_commit: Option<String>,
    pub dirty: bool,
    pub source_digest: Option<String>,
    pub cargo_lock_digest: Option<String>,
    pub target: String,
    pub profile: String,
    pub features: Vec<String>,
    pub rustc_version: String,
    pub binary_checksum: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RunProvenance {
    pub schema_version: u32,
    pub run_id: RunId,
    pub runtime_identity: String,
    pub build: BuildProvenance,
    pub runtime_config_digest: Option<String>,
    pub provider_config_digest: Option<String>,
    pub tool_manifest_digest: Option<String>,
    pub policy_digest: Option<String>,
    pub verifier_digest: Option<String>,
    pub workspace_initial_digest: Option<String>,
    pub captured_at: Timestamp,
}

/// 冻结后的用户可见步骤。reasoning 不得写入这里，否则 TUI 会把内部思考当成解释。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct UserStep {
    pub step_id: UserStepId,
    pub turn_id: TurnId,
    pub kind: UserStepKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum UserStepKind {
    AssistantText {
        text: String,
    },
    ToolBatch {
        summary: String,
        tools: Vec<UserStepTool>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct UserStepTool {
    pub tool_call_id: ToolCallId,
    pub tool_name: String,
    pub status: ToolResultStatus,
    pub object: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolSummaryKind {
    Read,
    Listed,
    Searched,
    Edited,
    Ran,
    Other,
}

impl ToolSummaryKind {
    fn from_tool_name(tool_name: &str) -> Self {
        match tool_name {
            "read_file" => Self::Read,
            "list_dir" => Self::Listed,
            "rg_search" | "symbol_search" | "find_references" => Self::Searched,
            "write_file" | "edit_file" => Self::Edited,
            "shell" => Self::Ran,
            _ => Self::Other,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Listed => "listed",
            Self::Searched => "searched",
            Self::Edited => "edited",
            Self::Ran => "ran",
            Self::Other => "used",
        }
    }

    fn noun(self, count: usize) -> &'static str {
        match (self, count) {
            (Self::Read, 1) => "file",
            (Self::Read, _) => "files",
            (Self::Listed, 1) => "directory",
            (Self::Listed, _) => "directories",
            (Self::Searched, 1) => "pattern",
            (Self::Searched, _) => "patterns",
            (Self::Edited, 1) => "file",
            (Self::Edited, _) => "files",
            (Self::Ran, 1) => "shell command",
            (Self::Ran, _) => "shell commands",
            (Self::Other, 1) => "other tool",
            (Self::Other, _) => "other tools",
        }
    }
}

fn display_file_name(path: &str) -> String {
    std::path::Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or(path)
        .to_owned()
}

fn tool_object(tool_name: &str, facts: &serde_json::Value) -> Option<String> {
    let string = |key: &str| facts.get(key).and_then(serde_json::Value::as_str);
    let object = match tool_name {
        "shell" => string("command").map(ToOwned::to_owned),
        "shell_session" => {
            let command = string("command").unwrap_or_default();
            let stdin = string("stdin")
                .or_else(|| string("input"))
                .unwrap_or_default();
            if stdin.trim().is_empty() {
                (!command.trim().is_empty()).then(|| command.to_owned())
            } else if command.trim().is_empty() {
                Some(stdin.to_owned())
            } else {
                Some(format!("{command}\n{stdin}"))
            }
        }
        "subagent" | "delegate_task" => string("task")
            .or_else(|| string("child_status"))
            .map(ToOwned::to_owned),
        "read_file" | "write_file" | "edit_file" => string("path").map(display_file_name),
        "list_dir" => string("path").map(ToOwned::to_owned),
        "rg_search" => match (string("pattern"), string("path")) {
            (Some(pattern), Some(path)) => Some(format!("{pattern} in {path}")),
            (Some(pattern), None) => Some(pattern.to_owned()),
            _ => None,
        },
        "symbol_search" => string("query").map(ToOwned::to_owned),
        "find_references" => string("symbol").map(ToOwned::to_owned),
        _ => None,
    };
    object.filter(|value| !value.trim().is_empty())
}

/// 把同一 provider 回合里的成功工具收成一句用户可见摘要；失败工具不并进成功句。
#[must_use]
pub fn summarize_user_tool_batch(tools: &[UserStepTool]) -> String {
    let successful = tools
        .iter()
        .filter(|tool| tool.status == ToolResultStatus::Ok)
        .collect::<Vec<_>>();
    if successful.is_empty() {
        let failed = tools
            .iter()
            .filter(|tool| tool.status != ToolResultStatus::Ok)
            .count();
        return if failed == 1 {
            "Failed".to_owned()
        } else {
            format!("Failed {failed} tools")
        };
    }

    let mut counts = [
        (ToolSummaryKind::Read, Vec::new()),
        (ToolSummaryKind::Listed, Vec::new()),
        (ToolSummaryKind::Searched, Vec::new()),
        (ToolSummaryKind::Edited, Vec::new()),
        (ToolSummaryKind::Ran, Vec::new()),
        (ToolSummaryKind::Other, Vec::new()),
    ];
    for tool in successful {
        let kind = ToolSummaryKind::from_tool_name(&tool.tool_name);
        if let Some((_, objects)) = counts.iter_mut().find(|(candidate, _)| *candidate == kind) {
            objects.push(tool.object.clone().unwrap_or_default());
        }
    }
    counts
        .iter()
        .filter(|(_, objects)| !objects.is_empty())
        .map(|(kind, objects)| {
            if objects.len() == 1 && !objects[0].is_empty() && *kind != ToolSummaryKind::Ran {
                format!("{} {}", kind.label(), objects[0])
            } else if objects.len() == 1 && *kind == ToolSummaryKind::Ran {
                kind.label().to_owned()
            } else {
                format!(
                    "{} {} {}",
                    kind.label(),
                    objects.len(),
                    kind.noun(objects.len())
                )
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

#[must_use]
pub fn user_step_tool_from_envelope(
    tool_call_id: ToolCallId,
    tool_name: String,
    status: ToolResultStatus,
    facts: &serde_json::Value,
) -> UserStepTool {
    UserStepTool {
        object: tool_object(&tool_name, facts),
        tool_call_id,
        tool_name,
        status,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ToolCallId;
    use serde_json::json;

    #[test]
    fn tool_batch_summary_keeps_single_path_and_collapses_multiple_reads() {
        let read_readme = user_step_tool_from_envelope(
            ToolCallId::new(),
            "read_file".to_owned(),
            ToolResultStatus::Ok,
            &json!({"path": "README.md"}),
        );
        let read_cargo = user_step_tool_from_envelope(
            ToolCallId::new(),
            "read_file".to_owned(),
            ToolResultStatus::Ok,
            &json!({"path": "Cargo.toml"}),
        );
        let listed = user_step_tool_from_envelope(
            ToolCallId::new(),
            "list_dir".to_owned(),
            ToolResultStatus::Ok,
            &json!({"path": "crates"}),
        );
        assert_eq!(
            summarize_user_tool_batch(&[read_readme.clone()]),
            "read README.md"
        );
        assert_eq!(
            summarize_user_tool_batch(&[read_readme, read_cargo, listed]),
            "read 2 files, listed crates"
        );
    }

    #[test]
    fn shell_batch_keeps_the_command_as_the_visible_object() {
        let ran = user_step_tool_from_envelope(
            ToolCallId::new(),
            "shell".to_owned(),
            ToolResultStatus::Ok,
            &json!({"command": "git status --short"}),
        );
        assert_eq!(ran.object.as_deref(), Some("git status --short"));
        assert_eq!(summarize_user_tool_batch(&[ran.clone()]), "ran");
        assert_eq!(
            user_step_tool_from_envelope(
                ToolCallId::new(),
                "shell".to_owned(),
                ToolResultStatus::Ok,
                &json!({}),
            )
            .object,
            None
        );
    }
}
