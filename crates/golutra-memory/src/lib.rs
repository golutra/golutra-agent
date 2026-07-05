use golutra_core::{EvidenceId, TaskId};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WorkingSummary {
    pub objective: String,
    pub completion_criteria: Vec<String>,
    pub done: Vec<String>,
    pub in_progress: Vec<String>,
    pub blocked: Vec<String>,
    pub key_files: Vec<String>,
    pub key_evidence: Vec<EvidenceId>,
    pub unresolved_items: Vec<String>,
    pub next_steps: Vec<String>,
    pub risks: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct MemoryCandidate {
    pub source_task_id: TaskId,
    pub evidence_ids: Vec<EvidenceId>,
    pub proposed_scope: String,
    pub confidence: u8,
    pub contradiction_ids: Vec<String>,
    pub expiry: Option<String>,
    pub promotion_status: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ContextProjectionCacheEntry {
    pub cache_key: String,
    pub task_id: TaskId,
    pub token_count: u64,
    pub invalidation_refs: Vec<String>,
}

#[must_use]
pub fn propose_project_memory(
    source_task_id: TaskId,
    evidence_ids: Vec<EvidenceId>,
) -> MemoryCandidate {
    MemoryCandidate {
        source_task_id,
        evidence_ids,
        proposed_scope: "project".to_owned(),
        confidence: 70,
        contradiction_ids: Vec::new(),
        expiry: None,
        promotion_status: "proposed".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proposes_project_scoped_memory_candidate() {
        let candidate = propose_project_memory(TaskId::new(), vec![EvidenceId::new()]);

        assert_eq!(candidate.proposed_scope, "project");
        assert_eq!(candidate.promotion_status, "proposed");
    }
}
