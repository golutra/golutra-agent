use golutra_core::{
    ArtifactId, ArtifactRecord, EvidenceRecord, LoopDecision, SessionId, TaskId, TaskStatus,
    ThreadId, Timestamp, ToolResultEnvelope, VerificationRecord,
};
use golutra_protocol::{
    CommandAck, DebugProjection, RuntimeEvent, RuntimeEventType, StateProjection, UserProjection,
    VisibleStep,
};
use sha2::{Digest, Sha256};
use sqlx::{
    Row, SqlitePool,
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
};
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    str::FromStr,
    sync::Arc,
};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("sqlite operation failed")]
    Sqlx(#[from] sqlx::Error),
    #[error("json serialization failed")]
    Json(#[from] serde_json::Error),
    #[error("stored id is invalid: {0}")]
    InvalidId(String),
    #[error("artifact IO failed: {0}")]
    ArtifactIo(String),
    #[error("artifact checksum mismatch for {0}")]
    ArtifactChecksum(String),
}

pub type StoreResult<T> = Result<T, StoreError>;

const MAX_PROJECTION_VISIBLE_STEPS: usize = 512;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ThreadRecord {
    pub thread_id: ThreadId,
    pub session_id: SessionId,
    pub parent_thread_id: Option<ThreadId>,
    pub workspace_root: Option<String>,
    pub title: String,
    pub preview: String,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    pub recency_at: Timestamp,
    pub archived: bool,
}

#[derive(Debug, Clone)]
pub struct RuntimeStore {
    pool: SqlitePool,
    artifact_root: PathBuf,
    _temporary_artifact_root: Option<Arc<tempfile::TempDir>>,
}

impl RuntimeStore {
    pub async fn connect(database_url: &str) -> StoreResult<Self> {
        let artifact_root = artifact_root_for_database_url(database_url);
        Self::connect_with_artifact_root(database_url, artifact_root).await
    }

    pub async fn connect_with_artifact_root(
        database_url: &str,
        artifact_root: impl Into<PathBuf>,
    ) -> StoreResult<Self> {
        let options = SqliteConnectOptions::from_str(database_url)?.create_if_missing(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await?;
        let store = Self {
            pool,
            artifact_root: artifact_root.into(),
            _temporary_artifact_root: None,
        };
        store.initialize().await?;
        Ok(store)
    }

    pub async fn in_memory() -> StoreResult<Self> {
        let temporary = Arc::new(
            tempfile::tempdir().map_err(|error| StoreError::ArtifactIo(error.to_string()))?,
        );
        let options = SqliteConnectOptions::from_str("sqlite::memory:")?;
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await?;
        let store = Self {
            pool,
            artifact_root: temporary.path().join("artifacts"),
            _temporary_artifact_root: Some(temporary),
        };
        store.initialize().await?;
        Ok(store)
    }

    pub async fn initialize(&self) -> StoreResult<()> {
        for statement in MIGRATIONS {
            sqlx::query(statement).execute(&self.pool).await?;
        }
        Ok(())
    }

    pub async fn command_ack(&self, idempotency_key: &str) -> StoreResult<Option<CommandAck>> {
        let row = sqlx::query("SELECT ack_json FROM command_acks WHERE idempotency_key = ?")
            .bind(idempotency_key)
            .fetch_optional(&self.pool)
            .await?;
        row.map(|row| {
            let ack_json: String = row.try_get("ack_json")?;
            Ok(serde_json::from_str(&ack_json)?)
        })
        .transpose()
    }

    pub async fn store_command_ack(
        &self,
        idempotency_key: &str,
        ack: &CommandAck,
    ) -> StoreResult<()> {
        sqlx::query(
            r#"
            INSERT INTO command_acks (idempotency_key, command_id, ack_json, created_at)
            VALUES (?, ?, ?, ?)
            ON CONFLICT(idempotency_key) DO UPDATE SET
                ack_json = excluded.ack_json
            "#,
        )
        .bind(idempotency_key)
        .bind(ack.command_id.to_string())
        .bind(serde_json::to_string(ack)?)
        .bind(chrono::Utc::now().to_rfc3339())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn append_event(&self, event: &RuntimeEvent) -> StoreResult<()> {
        let event_json = serde_json::to_string(event)?;
        let mut transaction = self.pool.begin().await?;
        sqlx::query(
            r#"
            INSERT INTO runtime_events (
                event_id, sequence_no, session_id, task_id, turn_id, event_type, source,
                durable, payload_json, event_json
            )
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            "#,
        )
        .bind(event.id.to_string())
        .bind(i64::try_from(event.sequence_no).unwrap_or(i64::MAX))
        .bind(event.session_id.to_string())
        .bind(event.task_id.map(|id| id.to_string()))
        .bind(event.turn_id.map(|id| id.to_string()))
        .bind(format!("{:?}", event.event_type))
        .bind(format!("{:?}", event.source))
        .bind(event.durable)
        .bind(serde_json::to_string(&event.payload)?)
        .bind(event_json)
        .execute(&mut *transaction)
        .await?;
        let projection_row =
            sqlx::query("SELECT projection_json FROM state_projections WHERE session_id = ?")
                .bind(event.session_id.to_string())
                .fetch_optional(&mut *transaction)
                .await?;
        let mut projection = match projection_row {
            Some(row) => {
                let projection_json: String = row.try_get("projection_json")?;
                serde_json::from_str(&projection_json)?
            }
            None => initial_projection(event.session_id),
        };
        apply_event_to_state(&mut projection, event);
        persist_runtime_indexes(&mut transaction, event, &projection).await?;
        transaction.commit().await?;
        Ok(())
    }

    pub async fn list_session_states(&self) -> StoreResult<Vec<StateProjection>> {
        let rows = sqlx::query(
            "SELECT projection_json FROM state_projections ORDER BY last_sequence_no ASC",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| {
                let projection_json: String = row.try_get("projection_json")?;
                Ok(serde_json::from_str(&projection_json)?)
            })
            .collect()
    }

    pub async fn max_sequence_no(&self) -> StoreResult<u64> {
        let row = sqlx::query(
            "SELECT COALESCE(MAX(sequence_no), 0) AS max_sequence_no FROM runtime_events",
        )
        .fetch_one(&self.pool)
        .await?;
        let max_sequence_no: i64 = row.try_get("max_sequence_no")?;
        Ok(u64::try_from(max_sequence_no).unwrap_or_default())
    }

    pub async fn load_events(
        &self,
        session_id: SessionId,
        task_id: Option<TaskId>,
        after_sequence_no: Option<u64>,
    ) -> StoreResult<Vec<RuntimeEvent>> {
        let rows = sqlx::query(
            r#"
            SELECT event_json
            FROM runtime_events
            WHERE session_id = ?
              AND (? IS NULL OR task_id = ?)
              AND sequence_no > ?
            ORDER BY sequence_no ASC
            "#,
        )
        .bind(session_id.to_string())
        .bind(task_id.map(|id| id.to_string()))
        .bind(task_id.map(|id| id.to_string()))
        .bind(i64::try_from(after_sequence_no.unwrap_or_default()).unwrap_or(i64::MAX))
        .fetch_all(&self.pool)
        .await?;

        rows.into_iter()
            .map(|row| {
                let event_json: String = row.try_get("event_json")?;
                Ok(serde_json::from_str(&event_json)?)
            })
            .collect()
    }

    pub fn reduce_state(session_id: SessionId, events: &[RuntimeEvent]) -> StateProjection {
        let mut projection = initial_projection(session_id);

        for event in events {
            apply_event_to_state(&mut projection, event);
        }

        projection
    }

    pub async fn query_state(
        &self,
        session_id: SessionId,
        task_id: Option<TaskId>,
    ) -> StoreResult<StateProjection> {
        if task_id.is_none()
            && let Some(row) =
                sqlx::query("SELECT projection_json FROM state_projections WHERE session_id = ?")
                    .bind(session_id.to_string())
                    .fetch_optional(&self.pool)
                    .await?
        {
            let projection_json: String = row.try_get("projection_json")?;
            return Ok(serde_json::from_str(&projection_json)?);
        }
        let events = self.load_events(session_id, task_id, None).await?;
        Ok(Self::reduce_state(session_id, &events))
    }

    pub async fn user_projection(
        &self,
        session_id: SessionId,
        task_id: Option<TaskId>,
    ) -> StoreResult<UserProjection> {
        let state = self.query_state(session_id, task_id).await?;
        Ok(UserProjection {
            session_id: state.session_id,
            task_id: state.active_task_id,
            status: state.task_status,
            visible_steps: state.visible_steps,
            pending_approval: state.pending_approval,
            final_message: state.final_message,
            residual_risks: state
                .last_verification
                .map(|record| record.residual_risks)
                .unwrap_or_default(),
        })
    }

    pub async fn debug_projection(
        &self,
        session_id: SessionId,
        task_id: Option<TaskId>,
    ) -> StoreResult<DebugProjection> {
        let events = self.load_events(session_id, task_id, None).await?;
        let tool_results = events
            .iter()
            .filter(|event| event.event_type == RuntimeEventType::ToolCompleted)
            .filter_map(|event| {
                event
                    .payload
                    .get("envelope")
                    .cloned()
                    .and_then(|value| serde_json::from_value::<ToolResultEnvelope>(value).ok())
            })
            .collect::<Vec<_>>();
        let referenced_artifacts = tool_results
            .iter()
            .filter_map(|result| result.raw_artifact_ref)
            .chain(events.iter().filter_map(|event| event.payload_ref))
            .collect::<HashSet<_>>();
        let artifacts = self
            .load_artifacts_for_session(session_id)
            .await?
            .into_iter()
            .filter(|artifact| {
                task_id.is_none() || referenced_artifacts.contains(&artifact.artifact_id)
            })
            .collect::<Vec<_>>();
        let artifact_ids = artifacts
            .iter()
            .map(|artifact| artifact.artifact_id)
            .collect::<HashSet<_>>();
        let evidence = self.load_evidence_records(&artifact_ids).await?;
        let verification = events
            .iter()
            .rev()
            .find(|event| event.event_type == RuntimeEventType::VerificationCompleted)
            .and_then(verification_from_event);
        let loop_decisions = events
            .iter()
            .filter(|event| event.event_type == RuntimeEventType::LoopDecided)
            .filter_map(loop_decision_from_event)
            .collect::<Vec<_>>();
        Ok(DebugProjection {
            session_id,
            task_id,
            events,
            busy_policy_decisions: Vec::new(),
            tool_results,
            artifacts,
            evidence,
            verification,
            loop_decisions,
        })
    }

    pub async fn store_artifact(&self, artifact: &ArtifactRecord, bytes: &[u8]) -> StoreResult<()> {
        verify_artifact_checksum(artifact, bytes)?;
        tokio::fs::create_dir_all(&self.artifact_root)
            .await
            .map_err(|error| StoreError::ArtifactIo(error.to_string()))?;
        set_owner_only_dir(&self.artifact_root).await?;
        let final_path = self.artifact_blob_path(artifact.artifact_id);
        let temporary_path = final_path.with_extension(format!("tmp-{}", uuid::Uuid::now_v7()));
        tokio::fs::write(&temporary_path, bytes)
            .await
            .map_err(|error| StoreError::ArtifactIo(error.to_string()))?;
        set_owner_only_file(&temporary_path).await?;
        tokio::fs::rename(&temporary_path, &final_path)
            .await
            .map_err(|error| StoreError::ArtifactIo(error.to_string()))?;
        sqlx::query(
            r#"
            INSERT INTO artifact_records (artifact_id, session_id, uri, checksum, artifact_json)
            VALUES (?, ?, ?, ?, ?)
            "#,
        )
        .bind(artifact.artifact_id.to_string())
        .bind(artifact.session_id.to_string())
        .bind(&artifact.uri)
        .bind(&artifact.checksum)
        .bind(serde_json::to_string(artifact)?)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn load_artifact(
        &self,
        artifact_id: ArtifactId,
    ) -> StoreResult<Option<ArtifactRecord>> {
        let row = sqlx::query(
            r#"
            SELECT artifact_json
            FROM artifact_records
            WHERE artifact_id = ?
            "#,
        )
        .bind(artifact_id.to_string())
        .fetch_optional(&self.pool)
        .await?;

        row.map(|row| {
            let artifact_json: String = row.try_get("artifact_json")?;
            Ok(serde_json::from_str(&artifact_json)?)
        })
        .transpose()
    }

    pub async fn load_artifact_bytes(
        &self,
        artifact_id: ArtifactId,
    ) -> StoreResult<Option<Vec<u8>>> {
        let Some(artifact) = self.load_artifact(artifact_id).await? else {
            return Ok(None);
        };
        let path = self.artifact_blob_path(artifact_id);
        let bytes = tokio::fs::read(&path)
            .await
            .map_err(|error| StoreError::ArtifactIo(format!("{}: {error}", path.display())))?;
        verify_artifact_checksum(&artifact, &bytes)?;
        Ok(Some(bytes))
    }

    fn artifact_blob_path(&self, artifact_id: ArtifactId) -> PathBuf {
        self.artifact_root.join(format!("{artifact_id}.blob"))
    }

    pub async fn store_evidence(&self, evidence: &EvidenceRecord) -> StoreResult<()> {
        sqlx::query(
            r#"
            INSERT INTO evidence_records (evidence_id, claim, evidence_json)
            VALUES (?, ?, ?)
            "#,
        )
        .bind(evidence.evidence_id.to_string())
        .bind(&evidence.claim)
        .bind(serde_json::to_string(evidence)?)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn upsert_thread(&self, thread: &ThreadRecord) -> StoreResult<()> {
        sqlx::query(
            r#"
            INSERT INTO threads (
                thread_id, session_id, parent_thread_id, workspace_root, title, preview,
                created_at, updated_at, recency_at, archived
            )
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT(thread_id) DO UPDATE SET
                session_id = excluded.session_id,
                parent_thread_id = excluded.parent_thread_id,
                workspace_root = excluded.workspace_root,
                title = excluded.title,
                preview = excluded.preview,
                updated_at = excluded.updated_at,
                recency_at = excluded.recency_at,
                archived = excluded.archived
            "#,
        )
        .bind(thread.thread_id.to_string())
        .bind(thread.session_id.to_string())
        .bind(thread.parent_thread_id.map(|id| id.to_string()))
        .bind(&thread.workspace_root)
        .bind(&thread.title)
        .bind(&thread.preview)
        .bind(thread.created_at)
        .bind(thread.updated_at)
        .bind(thread.recency_at)
        .bind(thread.archived)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn thread_by_id(&self, thread_id: ThreadId) -> StoreResult<Option<ThreadRecord>> {
        let row = sqlx::query(
            r#"
            SELECT thread_id, session_id, parent_thread_id, workspace_root, title, preview,
                   created_at, updated_at, recency_at, archived
            FROM threads
            WHERE thread_id = ?
            "#,
        )
        .bind(thread_id.to_string())
        .fetch_optional(&self.pool)
        .await?;

        row.map(thread_from_row).transpose()
    }

    pub async fn thread_by_session(
        &self,
        session_id: SessionId,
    ) -> StoreResult<Option<ThreadRecord>> {
        let row = sqlx::query(
            r#"
            SELECT thread_id, session_id, parent_thread_id, workspace_root, title, preview,
                   created_at, updated_at, recency_at, archived
            FROM threads
            WHERE session_id = ?
            ORDER BY recency_at DESC
            LIMIT 1
            "#,
        )
        .bind(session_id.to_string())
        .fetch_optional(&self.pool)
        .await?;

        row.map(thread_from_row).transpose()
    }

    pub async fn list_threads(
        &self,
        workspace_root: Option<&str>,
        limit: u32,
    ) -> StoreResult<Vec<ThreadRecord>> {
        let rows = sqlx::query(
            r#"
            SELECT thread_id, session_id, parent_thread_id, workspace_root, title, preview,
                   created_at, updated_at, recency_at, archived
            FROM threads
            WHERE archived = 0
              AND (? IS NULL OR workspace_root = ?)
            ORDER BY recency_at DESC
            LIMIT ?
            "#,
        )
        .bind(workspace_root)
        .bind(workspace_root)
        .bind(i64::from(limit))
        .fetch_all(&self.pool)
        .await?;

        rows.into_iter().map(thread_from_row).collect()
    }

    async fn load_artifacts_for_session(
        &self,
        session_id: SessionId,
    ) -> StoreResult<Vec<ArtifactRecord>> {
        let rows = sqlx::query(
            r#"
            SELECT artifact_json
            FROM artifact_records
            WHERE session_id = ?
            ORDER BY artifact_id ASC
            "#,
        )
        .bind(session_id.to_string())
        .fetch_all(&self.pool)
        .await?;

        rows.into_iter()
            .map(|row| {
                let artifact_json: String = row.try_get("artifact_json")?;
                Ok(serde_json::from_str(&artifact_json)?)
            })
            .collect()
    }

    async fn load_evidence_records(
        &self,
        artifact_ids: &HashSet<ArtifactId>,
    ) -> StoreResult<Vec<EvidenceRecord>> {
        let rows = sqlx::query(
            r#"
            SELECT evidence_json
            FROM evidence_records
            ORDER BY evidence_id ASC
            "#,
        )
        .fetch_all(&self.pool)
        .await?;

        let records = rows
            .into_iter()
            .map(|row| {
                let evidence_json: String = row.try_get("evidence_json")?;
                Ok(serde_json::from_str(&evidence_json)?)
            })
            .collect::<StoreResult<Vec<EvidenceRecord>>>()?;
        Ok(records
            .into_iter()
            .filter(|record| {
                record
                    .artifact_refs
                    .iter()
                    .any(|artifact_id| artifact_ids.contains(artifact_id))
            })
            .collect())
    }
}

fn initial_projection(session_id: SessionId) -> StateProjection {
    StateProjection {
        session_id,
        active_task_id: None,
        task_status: TaskStatus::Idle,
        runtime_lane: None,
        last_sequence_no: 0,
        visible_steps: Vec::new(),
        pending_approval: None,
        final_message: None,
        last_loop_decision: None,
        last_verification: None,
    }
}

fn apply_event_to_state(projection: &mut StateProjection, event: &RuntimeEvent) {
    projection.last_sequence_no = projection.last_sequence_no.max(event.sequence_no);
    if let Some(task_id) = event.task_id {
        projection.active_task_id = Some(task_id);
    }
    apply_event_to_projection(projection, event);
}

fn apply_event_to_projection(projection: &mut StateProjection, event: &RuntimeEvent) {
    match event.event_type {
        RuntimeEventType::TaskCreated
        | RuntimeEventType::TurnStarted
        | RuntimeEventType::TaskResumed => {
            projection.task_status = TaskStatus::Running;
        }
        RuntimeEventType::TaskCompleted => {
            projection.task_status = event
                .payload
                .get("status")
                .cloned()
                .and_then(|value| serde_json::from_value(value).ok())
                .unwrap_or(TaskStatus::Completed);
        }
        RuntimeEventType::TaskAbortRequested => {
            projection.task_status = TaskStatus::Aborting;
        }
        RuntimeEventType::TaskAborted => {
            projection.task_status = TaskStatus::Cancelled;
        }
        RuntimeEventType::TaskPaused => {
            projection.task_status = TaskStatus::Paused;
        }
        RuntimeEventType::ApprovalRequested => {
            projection.task_status = TaskStatus::WaitingApproval;
            projection.pending_approval = event
                .payload
                .get("approval_id")
                .and_then(|value| value.as_str())
                .map(ToOwned::to_owned);
        }
        RuntimeEventType::ApprovalResolved => {
            projection.pending_approval = None;
            projection.task_status = TaskStatus::Running;
        }
        RuntimeEventType::VerificationCompleted => {
            projection.last_verification = verification_from_event(event);
        }
        RuntimeEventType::LoopDecided => {
            projection.last_loop_decision = loop_decision_from_event(event);
        }
        RuntimeEventType::AssistantMessage => {
            projection.final_message = event
                .payload
                .get("content")
                .and_then(|content| content.as_str())
                .map(ToOwned::to_owned);
        }
        _ => {}
    }

    projection.visible_steps.push(VisibleStep {
        label: format!("{:?}", event.event_type),
        status: format!("{:?}", projection.task_status),
        summary: event
            .payload
            .get("summary")
            .and_then(|summary| summary.as_str())
            .unwrap_or("runtime event recorded")
            .to_owned(),
    });
    let overflow = projection
        .visible_steps
        .len()
        .saturating_sub(MAX_PROJECTION_VISIBLE_STEPS);
    if overflow > 0 {
        projection.visible_steps.drain(..overflow);
    }
}

fn verification_from_event(event: &RuntimeEvent) -> Option<VerificationRecord> {
    event
        .payload
        .get("record")
        .cloned()
        .or_else(|| Some(event.payload.clone()))
        .and_then(|value| serde_json::from_value(value).ok())
}

fn loop_decision_from_event(event: &RuntimeEvent) -> Option<LoopDecision> {
    event
        .payload
        .get("record")
        .cloned()
        .or_else(|| Some(event.payload.clone()))
        .and_then(|value| serde_json::from_value(value).ok())
}

async fn persist_runtime_indexes(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    event: &RuntimeEvent,
    projection: &StateProjection,
) -> StoreResult<()> {
    let status = serde_json::to_value(projection.task_status)?
        .as_str()
        .unwrap_or("idle")
        .to_owned();
    let timestamp = event.timestamp.to_rfc3339();
    sqlx::query(
        r#"
        INSERT INTO sessions (session_id, status, active_task_id, last_sequence_no, created_at, updated_at)
        VALUES (?, ?, ?, ?, ?, ?)
        ON CONFLICT(session_id) DO UPDATE SET
            status = excluded.status,
            active_task_id = excluded.active_task_id,
            last_sequence_no = excluded.last_sequence_no,
            updated_at = excluded.updated_at
        "#,
    )
    .bind(event.session_id.to_string())
    .bind(&status)
    .bind(projection.active_task_id.map(|id| id.to_string()))
    .bind(i64::try_from(projection.last_sequence_no).unwrap_or(i64::MAX))
    .bind(&timestamp)
    .bind(&timestamp)
    .execute(&mut **transaction)
    .await?;
    sqlx::query(
        r#"
        INSERT INTO state_projections (session_id, last_sequence_no, projection_json, updated_at)
        VALUES (?, ?, ?, ?)
        ON CONFLICT(session_id) DO UPDATE SET
            last_sequence_no = excluded.last_sequence_no,
            projection_json = excluded.projection_json,
            updated_at = excluded.updated_at
        "#,
    )
    .bind(event.session_id.to_string())
    .bind(i64::try_from(projection.last_sequence_no).unwrap_or(i64::MAX))
    .bind(serde_json::to_string(projection)?)
    .bind(&timestamp)
    .execute(&mut **transaction)
    .await?;
    if let Some(task_id) = event.task_id {
        sqlx::query(
            r#"
            INSERT INTO tasks (task_id, session_id, status, last_sequence_no, created_at, updated_at)
            VALUES (?, ?, ?, ?, ?, ?)
            ON CONFLICT(task_id) DO UPDATE SET
                status = excluded.status,
                last_sequence_no = excluded.last_sequence_no,
                updated_at = excluded.updated_at
            "#,
        )
        .bind(task_id.to_string())
        .bind(event.session_id.to_string())
        .bind(&status)
        .bind(i64::try_from(projection.last_sequence_no).unwrap_or(i64::MAX))
        .bind(&timestamp)
        .bind(&timestamp)
        .execute(&mut **transaction)
        .await?;
    }
    if let Some(turn_id) = event.turn_id {
        sqlx::query(
            r#"
            INSERT INTO turns (turn_id, session_id, task_id, status, last_sequence_no, created_at, updated_at)
            VALUES (?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT(turn_id) DO UPDATE SET
                status = excluded.status,
                last_sequence_no = excluded.last_sequence_no,
                updated_at = excluded.updated_at
            "#,
        )
        .bind(turn_id.to_string())
        .bind(event.session_id.to_string())
        .bind(event.task_id.map(|id| id.to_string()))
        .bind(&status)
        .bind(i64::try_from(projection.last_sequence_no).unwrap_or(i64::MAX))
        .bind(&timestamp)
        .bind(&timestamp)
        .execute(&mut **transaction)
        .await?;
    }
    Ok(())
}

const MIGRATIONS: &[&str] = &[
    r#"
    CREATE TABLE IF NOT EXISTS runtime_events (
        event_id TEXT PRIMARY KEY,
        sequence_no INTEGER NOT NULL,
        session_id TEXT NOT NULL,
        task_id TEXT,
        turn_id TEXT,
        event_type TEXT NOT NULL,
        source TEXT NOT NULL,
        durable INTEGER NOT NULL,
        payload_json TEXT NOT NULL,
        event_json TEXT NOT NULL
    )
    "#,
    r#"
    CREATE INDEX IF NOT EXISTS idx_runtime_events_session_sequence
    ON runtime_events (session_id, sequence_no)
    "#,
    r#"
    CREATE INDEX IF NOT EXISTS idx_runtime_events_task_sequence
    ON runtime_events (task_id, sequence_no)
    "#,
    r#"
    CREATE UNIQUE INDEX IF NOT EXISTS idx_runtime_events_sequence_no
    ON runtime_events (sequence_no)
    "#,
    r#"
    CREATE TABLE IF NOT EXISTS command_acks (
        idempotency_key TEXT PRIMARY KEY,
        command_id TEXT NOT NULL,
        ack_json TEXT NOT NULL,
        created_at TEXT NOT NULL
    )
    "#,
    r#"
    CREATE TABLE IF NOT EXISTS sessions (
        session_id TEXT PRIMARY KEY,
        status TEXT NOT NULL,
        active_task_id TEXT,
        last_sequence_no INTEGER NOT NULL,
        created_at TEXT NOT NULL,
        updated_at TEXT NOT NULL
    )
    "#,
    r#"
    CREATE TABLE IF NOT EXISTS tasks (
        task_id TEXT PRIMARY KEY,
        session_id TEXT NOT NULL,
        status TEXT NOT NULL,
        last_sequence_no INTEGER NOT NULL,
        created_at TEXT NOT NULL,
        updated_at TEXT NOT NULL
    )
    "#,
    r#"
    CREATE INDEX IF NOT EXISTS idx_tasks_session_updated
    ON tasks (session_id, updated_at DESC)
    "#,
    r#"
    CREATE TABLE IF NOT EXISTS turns (
        turn_id TEXT PRIMARY KEY,
        session_id TEXT NOT NULL,
        task_id TEXT,
        status TEXT NOT NULL,
        last_sequence_no INTEGER NOT NULL,
        created_at TEXT NOT NULL,
        updated_at TEXT NOT NULL
    )
    "#,
    r#"
    CREATE INDEX IF NOT EXISTS idx_turns_session_updated
    ON turns (session_id, updated_at DESC)
    "#,
    r#"
    CREATE TABLE IF NOT EXISTS state_projections (
        session_id TEXT PRIMARY KEY,
        last_sequence_no INTEGER NOT NULL,
        projection_json TEXT NOT NULL,
        updated_at TEXT NOT NULL
    )
    "#,
    r#"
    CREATE TABLE IF NOT EXISTS artifact_records (
        artifact_id TEXT PRIMARY KEY,
        session_id TEXT NOT NULL,
        uri TEXT NOT NULL,
        checksum TEXT NOT NULL,
        artifact_json TEXT NOT NULL
    )
    "#,
    r#"
    CREATE TABLE IF NOT EXISTS evidence_records (
        evidence_id TEXT PRIMARY KEY,
        claim TEXT NOT NULL,
        evidence_json TEXT NOT NULL
    )
    "#,
    r#"
    CREATE TABLE IF NOT EXISTS threads (
        thread_id TEXT PRIMARY KEY,
        session_id TEXT NOT NULL,
        parent_thread_id TEXT,
        workspace_root TEXT,
        title TEXT NOT NULL,
        preview TEXT NOT NULL,
        created_at TEXT NOT NULL,
        updated_at TEXT NOT NULL,
        recency_at TEXT NOT NULL,
        archived INTEGER NOT NULL DEFAULT 0
    )
    "#,
    r#"
    CREATE INDEX IF NOT EXISTS idx_threads_workspace_recency
    ON threads (workspace_root, recency_at DESC)
    "#,
    r#"
    CREATE INDEX IF NOT EXISTS idx_threads_session
    ON threads (session_id)
    "#,
];

fn artifact_root_for_database_url(database_url: &str) -> PathBuf {
    database_url
        .strip_prefix("sqlite://")
        .or_else(|| database_url.strip_prefix("sqlite:"))
        .filter(|path| !path.is_empty() && *path != ":memory:")
        .map(PathBuf::from)
        .and_then(|path| path.parent().map(Path::to_path_buf))
        .unwrap_or_else(std::env::temp_dir)
        .join("artifacts")
}

fn verify_artifact_checksum(artifact: &ArtifactRecord, bytes: &[u8]) -> StoreResult<()> {
    let checksum = artifact_checksum(bytes);
    if checksum == artifact.checksum {
        Ok(())
    } else {
        Err(StoreError::ArtifactChecksum(
            artifact.artifact_id.to_string(),
        ))
    }
}

fn artifact_checksum(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    format!("sha256:{digest:x}")
}

#[cfg(unix)]
async fn set_owner_only_dir(path: &Path) -> StoreResult<()> {
    use std::os::unix::fs::PermissionsExt;

    tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
        .await
        .map_err(|error| StoreError::ArtifactIo(error.to_string()))
}

#[cfg(not(unix))]
async fn set_owner_only_dir(_path: &Path) -> StoreResult<()> {
    Ok(())
}

#[cfg(unix)]
async fn set_owner_only_file(path: &Path) -> StoreResult<()> {
    use std::os::unix::fs::PermissionsExt;

    tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .await
        .map_err(|error| StoreError::ArtifactIo(error.to_string()))
}

#[cfg(not(unix))]
async fn set_owner_only_file(_path: &Path) -> StoreResult<()> {
    Ok(())
}

fn thread_from_row(row: sqlx::sqlite::SqliteRow) -> StoreResult<ThreadRecord> {
    let thread_id: String = row.try_get("thread_id")?;
    let session_id: String = row.try_get("session_id")?;
    let parent_thread_id: Option<String> = row.try_get("parent_thread_id")?;
    Ok(ThreadRecord {
        thread_id: ThreadId::from_str(&thread_id)
            .map_err(|error| StoreError::InvalidId(error.to_string()))?,
        session_id: SessionId::from_str(&session_id)
            .map_err(|error| StoreError::InvalidId(error.to_string()))?,
        parent_thread_id: parent_thread_id
            .map(|value| {
                ThreadId::from_str(&value).map_err(|error| StoreError::InvalidId(error.to_string()))
            })
            .transpose()?,
        workspace_root: row.try_get("workspace_root")?,
        title: row.try_get("title")?,
        preview: row.try_get("preview")?,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
        recency_at: row.try_get("recency_at")?,
        archived: row.try_get("archived")?,
    })
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use golutra_core::{ArtifactId, CommandId, RedactionStatus, ToolCallId, TurnId};
    use golutra_protocol::{RuntimeEventSource, RuntimeEventType};
    use serde_json::json;
    use tempfile::tempdir;

    use super::*;

    #[tokio::test]
    async fn command_ack_is_durable_by_idempotency_key() {
        let store = RuntimeStore::in_memory().await.expect("store");
        let ack = CommandAck {
            command_id: CommandId::new(),
            accepted: true,
            reason: Some("accepted".to_owned()),
        };

        store
            .store_command_ack("same-command", &ack)
            .await
            .expect("store ack");

        assert_eq!(
            store.command_ack("same-command").await.expect("load ack"),
            Some(ack)
        );
    }

    #[tokio::test]
    async fn appends_events_and_reduces_state() {
        let store = RuntimeStore::in_memory().await.expect("store opens");
        let session_id = SessionId::new();
        let task_id = TaskId::new();
        let event = RuntimeEvent {
            id: golutra_core::EventId::new(),
            sequence_no: 1,
            session_id,
            turn_id: Some(TurnId::new()),
            task_id: Some(task_id),
            parent_event_id: None,
            event_type: RuntimeEventType::TaskCreated,
            timestamp: Utc::now(),
            source: RuntimeEventSource::Runtime,
            payload: json!({"summary": "task created"}),
            payload_ref: None,
            durable: true,
        };

        store.append_event(&event).await.expect("event appended");

        let events = store
            .load_events(session_id, Some(task_id), None)
            .await
            .expect("events load");
        let projection = RuntimeStore::reduce_state(session_id, &events);

        assert_eq!(projection.active_task_id, Some(task_id));
        assert_eq!(projection.task_status, TaskStatus::Running);
        assert_eq!(projection.last_sequence_no, 1);
    }

    #[tokio::test]
    async fn assistant_message_becomes_user_projection_final_message() {
        let store = RuntimeStore::in_memory().await.expect("store opens");
        let session_id = SessionId::new();
        let task_id = TaskId::new();
        let event = RuntimeEvent {
            id: golutra_core::EventId::new(),
            sequence_no: 1,
            session_id,
            turn_id: Some(TurnId::new()),
            task_id: Some(task_id),
            parent_event_id: None,
            event_type: RuntimeEventType::AssistantMessage,
            timestamp: Utc::now(),
            source: RuntimeEventSource::Runtime,
            payload: json!({
                "summary": "Completed: file written",
                "content": "Completed: file written",
            }),
            payload_ref: None,
            durable: true,
        };

        store.append_event(&event).await.expect("event appended");

        let projection = store
            .user_projection(session_id, None)
            .await
            .expect("projection loads");

        assert_eq!(
            projection.final_message,
            Some("Completed: file written".to_owned())
        );
    }

    #[tokio::test]
    async fn stores_artifact_metadata() {
        let store = RuntimeStore::in_memory().await.expect("store opens");
        let session_id = SessionId::new();
        let bytes = b"fixture";
        let artifact = ArtifactRecord {
            artifact_id: ArtifactId::new(),
            session_id,
            turn_id: Some(TurnId::new()),
            tool_call_id: Some(ToolCallId::new()),
            artifact_type: "stdout".to_owned(),
            uri: "artifact://fixture/stdout".to_owned(),
            checksum: artifact_checksum(bytes),
            size_bytes: bytes.len() as u64,
            created_at: Utc::now(),
            producer: "test".to_owned(),
            redaction_status: RedactionStatus::NotRequired,
            retention_policy: "test".to_owned(),
            provenance_refs: Vec::new(),
        };

        store
            .store_artifact(&artifact, bytes)
            .await
            .expect("artifact stored");
        let loaded = store
            .load_artifact(artifact.artifact_id)
            .await
            .expect("artifact loads");

        assert_eq!(loaded, Some(artifact.clone()));
        assert_eq!(
            store
                .load_artifact_bytes(loaded.expect("artifact").artifact_id)
                .await
                .expect("blob loads"),
            Some(bytes.to_vec())
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let directory_mode = std::fs::metadata(&store.artifact_root)
                .expect("artifact directory")
                .permissions()
                .mode()
                & 0o777;
            let file_mode = std::fs::metadata(store.artifact_blob_path(artifact.artifact_id))
                .expect("artifact blob")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(directory_mode, 0o700);
            assert_eq!(file_mode, 0o600);
        }
    }

    #[tokio::test]
    async fn debug_projection_includes_events_and_artifacts() {
        let store = RuntimeStore::in_memory().await.expect("store opens");
        let session_id = SessionId::new();
        let event = RuntimeEvent {
            id: golutra_core::EventId::new(),
            sequence_no: 1,
            session_id,
            turn_id: Some(TurnId::new()),
            task_id: Some(TaskId::new()),
            parent_event_id: None,
            event_type: RuntimeEventType::TaskCreated,
            timestamp: Utc::now(),
            source: RuntimeEventSource::Runtime,
            payload: json!({"summary": "task created"}),
            payload_ref: None,
            durable: true,
        };
        let bytes = b"x";
        let artifact = ArtifactRecord {
            artifact_id: ArtifactId::new(),
            session_id,
            turn_id: Some(TurnId::new()),
            tool_call_id: Some(ToolCallId::new()),
            artifact_type: "log".to_owned(),
            uri: "artifact://fixture/log".to_owned(),
            checksum: artifact_checksum(bytes),
            size_bytes: 1,
            created_at: Utc::now(),
            producer: "test".to_owned(),
            redaction_status: RedactionStatus::NotRequired,
            retention_policy: "test".to_owned(),
            provenance_refs: Vec::new(),
        };
        store.append_event(&event).await.expect("event");
        store
            .store_artifact(&artifact, bytes)
            .await
            .expect("artifact");

        let projection = store
            .debug_projection(session_id, None)
            .await
            .expect("debug projection");

        assert_eq!(projection.events.len(), 1);
        assert_eq!(projection.artifacts.len(), 1);
    }

    #[tokio::test]
    async fn stores_and_lists_thread_metadata() {
        let store = RuntimeStore::in_memory().await.expect("store opens");
        let now = Utc::now();
        let thread = ThreadRecord {
            thread_id: ThreadId::new(),
            session_id: SessionId::new(),
            parent_thread_id: None,
            workspace_root: Some("/workspace".to_owned()),
            title: "Implement provider setup".to_owned(),
            preview: "Implement provider setup and persistence".to_owned(),
            created_at: now,
            updated_at: now,
            recency_at: now,
            archived: false,
        };

        store.upsert_thread(&thread).await.expect("thread stored");
        let loaded = store
            .thread_by_id(thread.thread_id)
            .await
            .expect("thread loads")
            .expect("thread exists");
        let listed = store
            .list_threads(Some("/workspace"), 10)
            .await
            .expect("threads list");

        assert_eq!(loaded.thread_id, thread.thread_id);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].session_id, thread.session_id);
    }

    #[tokio::test]
    async fn durable_projection_and_runtime_indexes_survive_reopen() {
        let directory = tempdir().expect("directory");
        let database_url = format!(
            "sqlite://{}",
            directory.path().join("runtime.sqlite").display()
        );
        let session_id = SessionId::new();
        let task_id = TaskId::new();
        let event = RuntimeEvent {
            id: golutra_core::EventId::new(),
            sequence_no: 1,
            session_id,
            turn_id: Some(TurnId::new()),
            task_id: Some(task_id),
            parent_event_id: None,
            event_type: RuntimeEventType::TaskCreated,
            timestamp: Utc::now(),
            source: RuntimeEventSource::Runtime,
            payload: json!({"summary": "durable task"}),
            payload_ref: None,
            durable: true,
        };
        let store = RuntimeStore::connect(&database_url).await.expect("store");
        store.append_event(&event).await.expect("event");
        drop(store);

        let reopened = RuntimeStore::connect(&database_url)
            .await
            .expect("reopened");
        let state = reopened
            .query_state(session_id, None)
            .await
            .expect("projection");
        let states = reopened.list_session_states().await.expect("states");

        assert_eq!(state.active_task_id, Some(task_id));
        assert_eq!(state.task_status, TaskStatus::Running);
        assert_eq!(states, vec![state]);
    }
}
