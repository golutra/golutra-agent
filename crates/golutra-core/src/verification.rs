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
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct VerificationCheck {
    pub kind: VerificationCheckKind,
    pub name: String,
    pub command: Option<String>,
    pub passed: bool,
    pub evidence_refs: Vec<EvidenceId>,
    pub message: String,
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
