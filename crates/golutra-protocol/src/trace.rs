use golutra_core::{
    ArtifactId, ArtifactRecord, ContextSnapshot, EvidenceRecord, PostTaskJob, RedactionStatus,
    RunProvenance, SessionId, TaskId, TraceIntegrity, TraceView, VerificationPlan,
    VerificationRecord,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{EvaluationProjection, RuntimeEvent};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TaskTraceRequest {
    pub session_id: SessionId,
    pub task_id: TaskId,
    pub view: TraceView,
    pub cursor: Option<u64>,
    pub limit: u32,
    pub wait_for_evaluation: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct TaskTracePage {
    pub session_id: SessionId,
    pub task_id: TaskId,
    pub runtime_identity: String,
    #[serde(default)]
    pub run_provenance: Option<RunProvenance>,
    pub view: TraceView,
    pub events: Vec<RuntimeEvent>,
    pub context_snapshots: Vec<ContextSnapshot>,
    pub artifacts: Vec<ArtifactRecord>,
    pub evidence: Vec<EvidenceRecord>,
    pub verification_plan: Option<VerificationPlan>,
    pub verification: Option<VerificationRecord>,
    pub post_task_jobs: Vec<PostTaskJob>,
    pub evaluation: EvaluationProjection,
    pub integrity: TraceIntegrity,
    pub next_cursor: Option<u64>,
    pub has_more: bool,
}

/// Sealed stdin contract used by the standalone runtime evaluation worker.
/// Assertions and holdout answers intentionally do not cross this boundary.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RuntimeEvaluationWorkerRequest {
    pub objective: String,
    #[serde(default)]
    pub payload: Value,
}

/// Full trace produced by a baseline or candidate runtime binary. The
/// Supervisor still validates the trace and workspace outcome independently.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RuntimeEvaluationWorkerResponse {
    pub elapsed_ms: u64,
    pub trace: TaskTracePage,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ArtifactReadRequest {
    pub artifact_id: ArtifactId,
    pub offset: u64,
    pub length: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ArtifactChunk {
    pub artifact_id: ArtifactId,
    pub offset: u64,
    pub length: u64,
    pub total_size: u64,
    pub checksum: String,
    #[serde(default = "raw_redaction_status")]
    pub redaction_status: RedactionStatus,
    pub content_base64: String,
    pub eof: bool,
}

fn raw_redaction_status() -> RedactionStatus {
    RedactionStatus::Raw
}
