use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{EvidenceId, TaskId, VerificationId};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum VerificationResult {
    Pass,
    Fail,
    Partial,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum VerificationCheckKind {
    ToolExecution,
    WorkspaceChange,
    ObjectiveValidation,
    AssistantResponse,
    Schema,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct VerificationCheck {
    pub kind: VerificationCheckKind,
    pub name: String,
    pub command: Option<String>,
    pub passed: bool,
    pub evidence_refs: Vec<EvidenceId>,
    pub message: String,
}

#[derive(Deserialize)]
struct VerificationCheckWire {
    #[serde(default)]
    kind: Option<VerificationCheckKind>,
    name: String,
    command: Option<String>,
    passed: bool,
    evidence_refs: Vec<EvidenceId>,
    message: String,
}

impl<'de> Deserialize<'de> for VerificationCheck {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = VerificationCheckWire::deserialize(deserializer)?;
        // `kind` 加入前已持久化的事实不能阻断 runtime 启动；稳定的检查名称足以恢复原语义。
        let kind = wire
            .kind
            .unwrap_or_else(|| infer_legacy_verification_check_kind(&wire.name));
        Ok(Self {
            kind,
            name: wire.name,
            command: wire.command,
            passed: wire.passed,
            evidence_refs: wire.evidence_refs,
            message: wire.message,
        })
    }
}

fn infer_legacy_verification_check_kind(name: &str) -> VerificationCheckKind {
    if name == "assistant_response" {
        VerificationCheckKind::AssistantResponse
    } else if name.starts_with("output_schema") {
        VerificationCheckKind::Schema
    } else if name == "workspace_diff" {
        VerificationCheckKind::WorkspaceChange
    } else if name.starts_with("objective:") {
        VerificationCheckKind::ObjectiveValidation
    } else if name.starts_with("tool:") {
        VerificationCheckKind::ToolExecution
    } else {
        VerificationCheckKind::ObjectiveValidation
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct VerificationRecord {
    pub verification_id: VerificationId,
    pub task_id: TaskId,
    pub objective: String,
    pub completion_criteria: Vec<String>,
    pub checks: Vec<VerificationCheck>,
    pub evidence_refs: Vec<EvidenceId>,
    pub result: VerificationResult,
    pub policy_status: String,
    pub residual_risks: Vec<String>,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn legacy_verification_check_infers_assistant_response_kind() {
        let check: VerificationCheck = serde_json::from_value(json!({
            "name": "assistant_response",
            "command": null,
            "passed": true,
            "evidence_refs": [],
            "message": "assistant response produced"
        }))
        .expect("legacy verification check");

        assert_eq!(check.kind, VerificationCheckKind::AssistantResponse);
    }

    #[test]
    fn legacy_verification_check_infers_tool_execution_kind() {
        let check: VerificationCheck = serde_json::from_value(json!({
            "name": "tool:write_file",
            "command": null,
            "passed": true,
            "evidence_refs": [],
            "message": "file written"
        }))
        .expect("legacy verification check");

        assert_eq!(check.kind, VerificationCheckKind::ToolExecution);
    }
}
