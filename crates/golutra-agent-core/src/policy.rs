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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PolicyBlockDisposition {
    /// Reject only the current invocation and let the model submit a safer one.
    Recoverable,
    /// Reject the invocation and stop the task at the policy boundary.
    Terminal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PolicyEvaluation {
    pub policy_ref: PolicyId,
    pub subject: String,
    pub action: String,
    pub resource: String,
    pub decision: PolicyDecision,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub block_disposition: Option<PolicyBlockDisposition>,
    pub reason: String,
    pub evidence_refs: Vec<EvidenceId>,
}

impl PolicyEvaluation {
    /// Missing disposition on legacy blocked records is terminal by default.
    #[must_use]
    pub fn effective_block_disposition(&self) -> Option<PolicyBlockDisposition> {
        (self.decision == PolicyDecision::Block).then_some(
            self.block_disposition
                .unwrap_or(PolicyBlockDisposition::Terminal),
        )
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn legacy_block_without_disposition_remains_terminal() {
        let evaluation: PolicyEvaluation = serde_json::from_value(json!({
            "policy_ref": PolicyId::new(),
            "subject": "tool",
            "action": "shell",
            "resource": "rm -rf /",
            "decision": "block",
            "reason": "legacy block",
            "evidence_refs": []
        }))
        .expect("legacy policy evaluation");

        assert_eq!(evaluation.block_disposition, None);
        assert_eq!(
            evaluation.effective_block_disposition(),
            Some(PolicyBlockDisposition::Terminal)
        );
    }
}
