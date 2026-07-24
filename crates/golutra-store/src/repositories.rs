//! Explicit repository boundaries over the canonical SQLite store.
//!
//! `RuntimeStore` remains the migration and connection owner.  These small
//! handles make the fact-plane dependencies visible to application services:
//! event/projection/artifact/job/thread access can be passed independently and
//! can later be backed by separate physical stores without changing use cases.

use std::collections::HashSet;

use golutra_core::{
    ArtifactId, ArtifactRecord, CommandId, ContextSnapshot, EvidenceId, EvidenceRecord,
    PostTaskJob, PostTaskJobId, PostTaskJobStatus, SessionId, TaskId, ThreadId, VerificationPlan,
};
use golutra_protocol::{
    ArtifactReadRequest, CommandAck, RuntimeEvent, SessionCursor, SessionRangeDirection,
};

use super::{
    ArtifactRange, CommandClaim, EventIntegrity, RuntimeStore, StateProjection, StoreResult,
    ThreadRecord,
};

/// The five logical repository seams used by the Runtime OS application layer.
#[derive(Debug, Clone)]
pub struct RuntimeRepositories {
    pub events: EventRepository,
    pub projections: ProjectionRepository,
    pub artifacts: ArtifactRepository,
    pub jobs: DurableJobRepository,
    pub threads: ThreadRepository,
}

impl RuntimeStore {
    #[must_use]
    pub fn repositories(&self) -> RuntimeRepositories {
        RuntimeRepositories {
            events: EventRepository {
                store: self.clone(),
            },
            projections: ProjectionRepository {
                store: self.clone(),
            },
            artifacts: ArtifactRepository {
                store: self.clone(),
            },
            jobs: DurableJobRepository {
                store: self.clone(),
            },
            threads: ThreadRepository {
                store: self.clone(),
            },
        }
    }
}

#[derive(Debug, Clone)]
pub struct EventRepository {
    store: RuntimeStore,
}

impl EventRepository {
    pub async fn append(&self, event: &RuntimeEvent) -> StoreResult<()> {
        self.store.append_event(event).await
    }

    pub async fn append_assigning_sequence(
        &self,
        event: RuntimeEvent,
    ) -> StoreResult<RuntimeEvent> {
        self.store.append_event_assigning_sequence(event).await
    }

    pub async fn max_sequence(&self) -> StoreResult<u64> {
        self.store.max_sequence_no().await
    }

    pub async fn load(
        &self,
        session_id: SessionId,
        task_id: Option<TaskId>,
        after_sequence_no: Option<u64>,
    ) -> StoreResult<Vec<RuntimeEvent>> {
        self.store
            .load_events(session_id, task_id, after_sequence_no)
            .await
    }

    pub async fn load_page(
        &self,
        session_id: SessionId,
        task_id: Option<TaskId>,
        after_sequence_no: Option<u64>,
        limit: u32,
    ) -> StoreResult<Vec<RuntimeEvent>> {
        self.store
            .load_events_page(session_id, task_id, after_sequence_no, limit)
            .await
    }

    pub async fn load_before(
        &self,
        session_id: SessionId,
        task_id: Option<TaskId>,
        before_sequence_no: Option<u64>,
        limit: u32,
    ) -> StoreResult<Vec<RuntimeEvent>> {
        self.store
            .load_events_before(session_id, task_id, before_sequence_no, limit)
            .await
    }

    pub async fn integrity(
        &self,
        session_id: SessionId,
        task_id: TaskId,
    ) -> StoreResult<EventIntegrity> {
        self.store.event_integrity(session_id, task_id).await
    }

    pub async fn integrity_before(
        &self,
        session_id: SessionId,
        task_id: TaskId,
        before_sequence_no: u64,
    ) -> StoreResult<EventIntegrity> {
        self.store
            .event_integrity_before(session_id, task_id, Some(before_sequence_no))
            .await
    }

    pub async fn load_by_sequence(
        &self,
        session_id: SessionId,
        sequence_no: u64,
    ) -> StoreResult<Option<RuntimeEvent>> {
        self.store
            .load_event_by_sequence(session_id, sequence_no)
            .await
    }

    pub async fn load_recent(
        &self,
        session_id: SessionId,
        task_id: Option<TaskId>,
        after_sequence_no: Option<u64>,
        limit: u32,
    ) -> StoreResult<Vec<RuntimeEvent>> {
        self.store
            .load_recent_events(session_id, task_id, after_sequence_no, limit)
            .await
    }

    pub async fn latest_explicit_compaction(
        &self,
        session_id: SessionId,
    ) -> StoreResult<Option<RuntimeEvent>> {
        self.store.load_latest_explicit_compaction(session_id).await
    }

    pub async fn latest_context_compaction(
        &self,
        session_id: SessionId,
    ) -> StoreResult<Option<RuntimeEvent>> {
        self.store.load_latest_context_compaction(session_id).await
    }

    pub async fn claim_command(
        &self,
        idempotency_key: &str,
        command_id: CommandId,
        provisional_ack: &CommandAck,
        receipt_event: RuntimeEvent,
    ) -> StoreResult<CommandClaim> {
        self.store
            .claim_command(idempotency_key, command_id, provisional_ack, receipt_event)
            .await
    }

    pub async fn complete_command(
        &self,
        idempotency_key: &str,
        command_id: CommandId,
        ack: &CommandAck,
        completion_event: RuntimeEvent,
    ) -> StoreResult<RuntimeEvent> {
        self.store
            .complete_command(idempotency_key, command_id, ack, completion_event)
            .await
    }
}

#[derive(Debug, Clone)]
pub struct ProjectionRepository {
    store: RuntimeStore,
}

impl ProjectionRepository {
    pub async fn state(
        &self,
        session_id: SessionId,
        task_id: Option<TaskId>,
    ) -> StoreResult<StateProjection> {
        self.store.query_state(session_id, task_id).await
    }

    pub async fn user(
        &self,
        session_id: SessionId,
        task_id: Option<TaskId>,
    ) -> StoreResult<golutra_protocol::UserProjection> {
        self.store.user_projection(session_id, task_id).await
    }

    pub async fn debug(
        &self,
        session_id: SessionId,
        task_id: Option<TaskId>,
    ) -> StoreResult<golutra_protocol::DebugProjection> {
        self.store.debug_projection(session_id, task_id).await
    }

    pub async fn all_states(&self) -> StoreResult<Vec<StateProjection>> {
        self.store.list_session_states().await
    }
}

#[derive(Debug, Clone)]
pub struct ArtifactRepository {
    store: RuntimeStore,
}

impl ArtifactRepository {
    pub async fn store(&self, artifact: &ArtifactRecord, bytes: &[u8]) -> StoreResult<()> {
        self.store.store_artifact(artifact, bytes).await
    }

    pub async fn get(&self, artifact_id: ArtifactId) -> StoreResult<Option<ArtifactRecord>> {
        self.store.load_artifact(artifact_id).await
    }

    pub async fn find_by_content(
        &self,
        session_id: SessionId,
        artifact_type: &str,
        checksum: &str,
        size_bytes: u64,
    ) -> StoreResult<Option<ArtifactRecord>> {
        self.store
            .find_artifact_by_content(session_id, artifact_type, checksum, size_bytes)
            .await
    }

    pub async fn bytes(&self, artifact_id: ArtifactId) -> StoreResult<Option<Vec<u8>>> {
        self.store.load_artifact_bytes(artifact_id).await
    }

    pub async fn range(&self, request: &ArtifactReadRequest) -> StoreResult<Option<ArtifactRange>> {
        self.store.read_artifact_range(request).await
    }

    pub async fn store_context(&self, snapshot: &ContextSnapshot) -> StoreResult<()> {
        self.store.store_context_snapshot(snapshot).await
    }

    pub async fn contexts(&self, task_id: TaskId) -> StoreResult<Vec<ContextSnapshot>> {
        self.store.load_context_snapshots(task_id).await
    }

    pub async fn store_verification(&self, plan: &VerificationPlan) -> StoreResult<()> {
        self.store.store_verification_plan(plan).await
    }

    pub async fn verification(&self, task_id: TaskId) -> StoreResult<Option<VerificationPlan>> {
        self.store.load_verification_plan(task_id).await
    }

    pub async fn evidence_by_ids(
        &self,
        evidence_ids: &HashSet<EvidenceId>,
    ) -> StoreResult<Vec<EvidenceRecord>> {
        self.store.load_evidence_by_ids(evidence_ids).await
    }

    pub async fn evidence_for_artifacts(
        &self,
        artifact_ids: &HashSet<ArtifactId>,
    ) -> StoreResult<Vec<EvidenceRecord>> {
        self.store.load_evidence_records(artifact_ids).await
    }

    pub async fn store_evidence(&self, evidence: &EvidenceRecord) -> StoreResult<()> {
        self.store.store_evidence(evidence).await
    }
}

#[derive(Debug, Clone)]
pub struct DurableJobRepository {
    store: RuntimeStore,
}

impl DurableJobRepository {
    pub async fn get_for_task(&self, task_id: TaskId) -> StoreResult<Option<PostTaskJob>> {
        self.store.post_task_job(task_id).await
    }

    pub async fn by_id(&self, job_id: PostTaskJobId) -> StoreResult<Option<PostTaskJob>> {
        self.store.post_task_job_by_id(job_id).await
    }

    pub async fn list_for_task(&self, task_id: TaskId) -> StoreResult<Vec<PostTaskJob>> {
        self.store.list_post_task_jobs(task_id).await
    }

    pub async fn enqueue_with_event(
        &self,
        job: &PostTaskJob,
        event: RuntimeEvent,
    ) -> StoreResult<RuntimeEvent> {
        self.store
            .enqueue_post_task_job_with_event(job, event)
            .await
    }

    pub async fn recover_expired(
        &self,
        workspace_id: &str,
        now: chrono::DateTime<chrono::Utc>,
    ) -> StoreResult<u64> {
        self.store
            .recover_expired_post_task_jobs(workspace_id, now)
            .await
    }

    pub async fn claim(
        &self,
        worker_id: &str,
        workspace_id: &str,
        now: chrono::DateTime<chrono::Utc>,
        lease_for: chrono::Duration,
    ) -> StoreResult<Option<PostTaskJob>> {
        self.store
            .claim_post_task_job_for_workspace(worker_id, workspace_id, now, lease_for)
            .await
    }

    pub async fn start(
        &self,
        job_id: PostTaskJobId,
        worker_id: &str,
        started_at: chrono::DateTime<chrono::Utc>,
    ) -> StoreResult<bool> {
        self.store
            .start_post_task_job(job_id, worker_id, started_at)
            .await
    }

    pub async fn requeue(
        &self,
        job_id: PostTaskJobId,
        worker_id: &str,
        error: &str,
    ) -> StoreResult<bool> {
        self.store
            .requeue_post_task_job(job_id, worker_id, error)
            .await
    }

    pub async fn retry(&self, job_id: PostTaskJobId) -> StoreResult<bool> {
        self.store.retry_post_task_job(job_id).await
    }

    pub async fn finish(
        &self,
        job_id: PostTaskJobId,
        worker_id: &str,
        status: PostTaskJobStatus,
        result_refs: &[String],
        error: Option<&str>,
        completed_at: chrono::DateTime<chrono::Utc>,
    ) -> StoreResult<bool> {
        self.store
            .finish_post_task_job(job_id, worker_id, status, result_refs, error, completed_at)
            .await
    }
}

#[derive(Debug, Clone)]
pub struct ThreadRepository {
    store: RuntimeStore,
}

impl ThreadRepository {
    pub async fn list(
        &self,
        workspace_root: Option<&str>,
        limit: u32,
    ) -> StoreResult<Vec<ThreadRecord>> {
        self.store.list_threads(workspace_root, limit).await
    }

    pub async fn by_id(&self, thread_id: ThreadId) -> StoreResult<Option<ThreadRecord>> {
        self.store.thread_by_id(thread_id).await
    }

    pub async fn page(
        &self,
        workspace_root: Option<&str>,
        cursor: Option<&SessionCursor>,
        limit: u32,
    ) -> StoreResult<Vec<ThreadRecord>> {
        self.store
            .list_threads_page(workspace_root, cursor, limit)
            .await
    }

    pub async fn window(
        &self,
        workspace_root: Option<&str>,
        anchor: &ThreadRecord,
        direction: SessionRangeDirection,
        count: u32,
    ) -> StoreResult<Vec<ThreadRecord>> {
        self.store
            .thread_window(workspace_root, anchor, direction, count)
            .await
    }

    pub async fn by_session(&self, session_id: SessionId) -> StoreResult<Option<ThreadRecord>> {
        self.store.thread_by_session(session_id).await
    }

    pub async fn upsert(&self, thread: &ThreadRecord) -> StoreResult<()> {
        self.store.upsert_thread(thread).await
    }

    pub async fn fork(
        &self,
        child: &ThreadRecord,
        parent_session_id: SessionId,
        through_sequence_no: u64,
    ) -> StoreResult<Vec<RuntimeEvent>> {
        self.store
            .create_forked_thread(child, parent_session_id, through_sequence_no)
            .await
    }
}
