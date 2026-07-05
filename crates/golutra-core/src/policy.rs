use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{EvidenceId, PolicyId};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PolicyDecision {
    Allow,
    Ask,
    Deny,
    Block,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PolicyEvaluation {
    pub policy_ref: PolicyId,
    pub subject: String,
    pub action: String,
    pub resource: String,
    pub decision: PolicyDecision,
    pub reason: String,
    pub evidence_refs: Vec<EvidenceId>,
}
