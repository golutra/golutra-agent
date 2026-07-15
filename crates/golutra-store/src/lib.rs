use golutra_core::{
    ArtifactId, ArtifactRecord, BusyPolicyDecision, EventId, EvidenceRecord, SessionId, TaskId,
    ThreadId, Timestamp, ToolResultEnvelope, TurnId,
};
use golutra_protocol::{
    CommandAck, DebugProjection, RuntimeEvent, RuntimeEventType, StateProjection, UserProjection,
};
use sha2::{Digest, Sha256};
use sqlx::{
    Row, SqlitePool,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions},
};
use std::{
    collections::{HashMap, HashSet},
    io::ErrorKind,
    path::{Path, PathBuf},
    str::FromStr,
    sync::Arc,
    time::Duration,
};
use thiserror::Error;
use tokio::io::AsyncWriteExt;

mod projection;

pub(crate) use projection::{
    apply_event_to_state, initial_projection, loop_decision_from_event, verification_from_event,
};

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

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ThreadRecord {
    pub thread_id: ThreadId,
    pub session_id: SessionId,
    pub parent_thread_id: Option<ThreadId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forked_from_turn_id: Option<TurnId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forked_from_sequence_no: Option<u64>,
    pub workspace_root: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rebound_from_workspace_root: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rollout_path: Option<String>,
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
        let options = SqliteConnectOptions::from_str(database_url)?
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .busy_timeout(Duration::from_secs(5));
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
        self.ensure_thread_columns().await?;
        Ok(())
    }

    async fn ensure_thread_columns(&self) -> StoreResult<()> {
        let rows = sqlx::query("PRAGMA table_info(threads)")
            .fetch_all(&self.pool)
            .await?;
        let existing = rows
            .into_iter()
            .map(|row| row.try_get::<String, _>("name"))
            .collect::<Result<HashSet<_>, _>>()?;
        for (name, declaration) in [
            ("forked_from_turn_id", "TEXT"),
            ("forked_from_sequence_no", "INTEGER"),
            ("rebound_from_workspace_root", "TEXT"),
            ("rollout_path", "TEXT"),
        ] {
            if !existing.contains(name) {
                sqlx::query(&format!(
                    "ALTER TABLE threads ADD COLUMN {name} {declaration}"
                ))
                .execute(&self.pool)
                .await?;
            }
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
                command_id = excluded.command_id,
                ack_json = excluded.ack_json,
                created_at = excluded.created_at
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
        let mut transaction = self.pool.begin().await?;
        append_event_in_transaction(&mut transaction, event).await?;
        sqlx::query(
            "UPDATE runtime_sequence
             SET last_sequence_no = MAX(last_sequence_no, ?)
             WHERE singleton = 1",
        )
        .bind(i64::try_from(event.sequence_no).unwrap_or(i64::MAX))
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok(())
    }

    pub async fn append_event_assigning_sequence(
        &self,
        mut event: RuntimeEvent,
    ) -> StoreResult<RuntimeEvent> {
        let mut transaction = self.pool.begin().await?;
        let row = sqlx::query(
            "UPDATE runtime_sequence
             SET last_sequence_no = last_sequence_no + 1
             WHERE singleton = 1
             RETURNING last_sequence_no",
        )
        .fetch_one(&mut *transaction)
        .await?;
        let sequence_no: i64 = row.try_get("last_sequence_no")?;
        event.sequence_no = u64::try_from(sequence_no).unwrap_or(u64::MAX);
        append_event_in_transaction(&mut transaction, &event).await?;
        transaction.commit().await?;
        Ok(event)
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

    pub async fn load_events_page(
        &self,
        session_id: SessionId,
        task_id: Option<TaskId>,
        after_sequence_no: Option<u64>,
        limit: u32,
    ) -> StoreResult<Vec<RuntimeEvent>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let rows = sqlx::query(
            r#"
            SELECT event_json
            FROM runtime_events
            WHERE session_id = ?
              AND (? IS NULL OR task_id = ?)
              AND sequence_no > ?
            ORDER BY sequence_no ASC
            LIMIT ?
            "#,
        )
        .bind(session_id.to_string())
        .bind(task_id.map(|id| id.to_string()))
        .bind(task_id.map(|id| id.to_string()))
        .bind(i64::try_from(after_sequence_no.unwrap_or_default()).unwrap_or(i64::MAX))
        .bind(i64::from(limit))
        .fetch_all(&self.pool)
        .await?;

        rows.into_iter()
            .map(|row| {
                let event_json: String = row.try_get("event_json")?;
                Ok(serde_json::from_str(&event_json)?)
            })
            .collect()
    }

    pub async fn load_event_by_sequence(
        &self,
        session_id: SessionId,
        sequence_no: u64,
    ) -> StoreResult<Option<RuntimeEvent>> {
        let row = sqlx::query(
            r#"
            SELECT event_json
            FROM runtime_events
            WHERE session_id = ? AND sequence_no = ?
            LIMIT 1
            "#,
        )
        .bind(session_id.to_string())
        .bind(i64::try_from(sequence_no).unwrap_or(i64::MAX))
        .fetch_optional(&self.pool)
        .await?;
        row.map(|row| {
            let event_json: String = row.try_get("event_json")?;
            Ok(serde_json::from_str(&event_json)?)
        })
        .transpose()
    }

    pub async fn load_recent_events(
        &self,
        session_id: SessionId,
        task_id: Option<TaskId>,
        after_sequence_no: Option<u64>,
        limit: u32,
    ) -> StoreResult<Vec<RuntimeEvent>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let rows = sqlx::query(
            r#"
            SELECT event_json
            FROM runtime_events
            WHERE session_id = ?
              AND (? IS NULL OR task_id = ?)
              AND sequence_no > ?
            ORDER BY sequence_no DESC
            LIMIT ?
            "#,
        )
        .bind(session_id.to_string())
        .bind(task_id.map(|id| id.to_string()))
        .bind(task_id.map(|id| id.to_string()))
        .bind(i64::try_from(after_sequence_no.unwrap_or_default()).unwrap_or(i64::MAX))
        .bind(i64::from(limit))
        .fetch_all(&self.pool)
        .await?;

        let mut events = rows
            .into_iter()
            .map(|row| {
                let event_json: String = row.try_get("event_json")?;
                Ok(serde_json::from_str(&event_json)?)
            })
            .collect::<StoreResult<Vec<RuntimeEvent>>>()?;
        events.reverse();
        Ok(events)
    }

    pub async fn load_latest_explicit_compaction(
        &self,
        session_id: SessionId,
    ) -> StoreResult<Option<RuntimeEvent>> {
        let row = sqlx::query(
            r#"
            SELECT event_json
            FROM runtime_events
            WHERE session_id = ?
              AND event_type = 'CompactionCompleted'
              AND json_extract(payload_json, '$.mode') = 'explicit'
            ORDER BY sequence_no DESC
            LIMIT 1
            "#,
        )
        .bind(session_id.to_string())
        .fetch_optional(&self.pool)
        .await?;
        row.map(|row| {
            let event_json: String = row.try_get("event_json")?;
            Ok(serde_json::from_str(&event_json)?)
        })
        .transpose()
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
        let mut artifacts = self.load_artifacts_for_session(session_id).await?;
        for artifact_id in &referenced_artifacts {
            if !artifacts
                .iter()
                .any(|artifact| artifact.artifact_id == *artifact_id)
                && let Some(artifact) = self.load_artifact(*artifact_id).await?
            {
                artifacts.push(artifact);
            }
        }
        if task_id.is_some() {
            artifacts.retain(|artifact| referenced_artifacts.contains(&artifact.artifact_id));
        }
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
        let busy_policy_decisions = events
            .iter()
            .filter(|event| event.event_type == RuntimeEventType::BusyPolicyDecided)
            .filter_map(|event| {
                event
                    .payload
                    .get("decision")
                    .cloned()
                    .and_then(|value| serde_json::from_value::<BusyPolicyDecision>(value).ok())
            })
            .collect();
        Ok(DebugProjection {
            session_id,
            task_id,
            events,
            busy_policy_decisions,
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
        let mut temporary = tokio::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary_path)
            .await
            .map_err(|error| StoreError::ArtifactIo(error.to_string()))?;
        temporary
            .write_all(bytes)
            .await
            .map_err(|error| StoreError::ArtifactIo(error.to_string()))?;
        set_owner_only_file(&temporary_path).await?;
        temporary
            .sync_all()
            .await
            .map_err(|error| StoreError::ArtifactIo(error.to_string()))?;
        drop(temporary);
        let linked = match tokio::fs::hard_link(&temporary_path, &final_path).await {
            Ok(()) => true,
            Err(error) if error.kind() == ErrorKind::AlreadyExists => false,
            Err(error) => return Err(StoreError::ArtifactIo(error.to_string())),
        };
        tokio::fs::remove_file(&temporary_path)
            .await
            .map_err(|error| StoreError::ArtifactIo(error.to_string()))?;
        sync_artifact_directory(&self.artifact_root).await?;
        if !linked {
            let existing_bytes = tokio::fs::read(&final_path)
                .await
                .map_err(|error| StoreError::ArtifactIo(error.to_string()))?;
            if existing_bytes != bytes {
                return Err(StoreError::ArtifactIo(format!(
                    "artifact {} already has different blob content",
                    artifact.artifact_id
                )));
            }
        }
        let result = sqlx::query(
            r#"
            INSERT OR IGNORE INTO artifact_records (
                artifact_id, session_id, uri, checksum, artifact_json
            )
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
        if result.rows_affected() == 0
            && self.load_artifact(artifact.artifact_id).await?.as_ref() != Some(artifact)
        {
            return Err(StoreError::ArtifactIo(format!(
                "artifact {} already has different metadata",
                artifact.artifact_id
            )));
        }
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
        let mut transaction = self.pool.begin().await?;
        upsert_thread_in_transaction(&mut transaction, thread).await?;
        transaction.commit().await?;
        Ok(())
    }

    pub async fn create_forked_thread(
        &self,
        child: &ThreadRecord,
        parent_session_id: SessionId,
        through_sequence_no: u64,
    ) -> StoreResult<Vec<RuntimeEvent>> {
        let mut transaction = self.pool.begin().await?;
        let rows = sqlx::query(
            r#"
            SELECT event_json
            FROM runtime_events
            WHERE session_id = ? AND sequence_no <= ?
            ORDER BY sequence_no ASC
            "#,
        )
        .bind(parent_session_id.to_string())
        .bind(i64::try_from(through_sequence_no).unwrap_or(i64::MAX))
        .fetch_all(&mut *transaction)
        .await?;
        let parent_events = rows
            .into_iter()
            .map(|row| {
                let event_json: String = row.try_get("event_json")?;
                Ok(serde_json::from_str::<RuntimeEvent>(&event_json)?)
            })
            .collect::<StoreResult<Vec<_>>>()?;

        let event_ids = parent_events
            .iter()
            .map(|event| (event.id, EventId::new()))
            .collect::<HashMap<_, _>>();
        let task_ids = parent_events
            .iter()
            .filter_map(|event| event.task_id)
            .map(|task_id| (task_id, TaskId::new()))
            .collect::<HashMap<_, _>>();
        let turn_ids = parent_events
            .iter()
            .filter_map(|event| event.turn_id)
            .map(|turn_id| (turn_id, TurnId::new()))
            .collect::<HashMap<_, _>>();
        let replacements = fork_id_replacements(
            parent_session_id,
            child.session_id,
            &event_ids,
            &task_ids,
            &turn_ids,
        );

        upsert_thread_in_transaction(&mut transaction, child).await?;
        let mut forked_events = Vec::with_capacity(parent_events.len());
        for mut event in parent_events {
            event.id = event_ids[&event.id];
            event.sequence_no = next_sequence_in_transaction(&mut transaction).await?;
            event.session_id = child.session_id;
            event.task_id = event.task_id.map(|task_id| task_ids[&task_id]);
            event.turn_id = event.turn_id.map(|turn_id| turn_ids[&turn_id]);
            event.parent_event_id = event
                .parent_event_id
                .and_then(|event_id| event_ids.get(&event_id).copied());
            remap_json_ids(&mut event.payload, &replacements);
            append_event_in_transaction(&mut transaction, &event).await?;
            forked_events.push(event);
        }
        transaction.commit().await?;
        Ok(forked_events)
    }

    pub async fn thread_by_id(&self, thread_id: ThreadId) -> StoreResult<Option<ThreadRecord>> {
        let row = sqlx::query(
            r#"
            SELECT thread_id, session_id, parent_thread_id, forked_from_turn_id,
                   forked_from_sequence_no, workspace_root, rebound_from_workspace_root,
                   rollout_path, title, preview, created_at, updated_at, recency_at, archived
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
            SELECT thread_id, session_id, parent_thread_id, forked_from_turn_id,
                   forked_from_sequence_no, workspace_root, rebound_from_workspace_root,
                   rollout_path, title, preview, created_at, updated_at, recency_at, archived
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
            SELECT thread_id, session_id, parent_thread_id, forked_from_turn_id,
                   forked_from_sequence_no, workspace_root, rebound_from_workspace_root,
                   rollout_path, title, preview, created_at, updated_at, recency_at, archived
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

async fn upsert_thread_in_transaction(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    thread: &ThreadRecord,
) -> StoreResult<()> {
    sqlx::query(
        r#"
        INSERT INTO threads (
            thread_id, session_id, parent_thread_id, forked_from_turn_id,
            forked_from_sequence_no, workspace_root, rebound_from_workspace_root,
            rollout_path, title, preview, created_at, updated_at, recency_at, archived
        )
        VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        ON CONFLICT(thread_id) DO UPDATE SET
            session_id = excluded.session_id,
            parent_thread_id = excluded.parent_thread_id,
            forked_from_turn_id = excluded.forked_from_turn_id,
            forked_from_sequence_no = excluded.forked_from_sequence_no,
            workspace_root = excluded.workspace_root,
            rebound_from_workspace_root = excluded.rebound_from_workspace_root,
            rollout_path = excluded.rollout_path,
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
    .bind(thread.forked_from_turn_id.map(|id| id.to_string()))
    .bind(
        thread
            .forked_from_sequence_no
            .map(|value| i64::try_from(value).unwrap_or(i64::MAX)),
    )
    .bind(&thread.workspace_root)
    .bind(&thread.rebound_from_workspace_root)
    .bind(&thread.rollout_path)
    .bind(&thread.title)
    .bind(&thread.preview)
    .bind(thread.created_at)
    .bind(thread.updated_at)
    .bind(thread.recency_at)
    .bind(thread.archived)
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

async fn next_sequence_in_transaction(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
) -> StoreResult<u64> {
    let row = sqlx::query(
        "UPDATE runtime_sequence
         SET last_sequence_no = last_sequence_no + 1
         WHERE singleton = 1
         RETURNING last_sequence_no",
    )
    .fetch_one(&mut **transaction)
    .await?;
    let sequence_no: i64 = row.try_get("last_sequence_no")?;
    Ok(u64::try_from(sequence_no).unwrap_or(u64::MAX))
}

fn fork_id_replacements(
    parent_session_id: SessionId,
    child_session_id: SessionId,
    event_ids: &HashMap<EventId, EventId>,
    task_ids: &HashMap<TaskId, TaskId>,
    turn_ids: &HashMap<TurnId, TurnId>,
) -> HashMap<String, String> {
    std::iter::once((parent_session_id.to_string(), child_session_id.to_string()))
        .chain(
            event_ids
                .iter()
                .map(|(source, target)| (source.to_string(), target.to_string())),
        )
        .chain(
            task_ids
                .iter()
                .map(|(source, target)| (source.to_string(), target.to_string())),
        )
        .chain(
            turn_ids
                .iter()
                .map(|(source, target)| (source.to_string(), target.to_string())),
        )
        .collect()
}

fn remap_json_ids(value: &mut serde_json::Value, replacements: &HashMap<String, String>) {
    match value {
        serde_json::Value::String(value) => {
            if let Some(replacement) = replacements.get(value) {
                *value = replacement.clone();
            }
        }
        serde_json::Value::Array(values) => {
            for value in values {
                remap_json_ids(value, replacements);
            }
        }
        serde_json::Value::Object(values) => {
            for value in values.values_mut() {
                remap_json_ids(value, replacements);
            }
        }
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {}
    }
}

async fn append_event_in_transaction(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    event: &RuntimeEvent,
) -> StoreResult<()> {
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
    .bind(serde_json::to_string(event)?)
    .execute(&mut **transaction)
    .await?;
    let projection_row =
        sqlx::query("SELECT projection_json FROM state_projections WHERE session_id = ?")
            .bind(event.session_id.to_string())
            .fetch_optional(&mut **transaction)
            .await?;
    let mut projection = match projection_row {
        Some(row) => {
            let projection_json: String = row.try_get("projection_json")?;
            serde_json::from_str(&projection_json)?
        }
        None => initial_projection(event.session_id),
    };
    apply_event_to_state(&mut projection, event);
    persist_runtime_indexes(transaction, event, &projection).await
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
    CREATE TABLE IF NOT EXISTS runtime_sequence (
        singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
        last_sequence_no INTEGER NOT NULL
    )
    "#,
    r#"
    INSERT OR IGNORE INTO runtime_sequence (singleton, last_sequence_no)
    SELECT 1, COALESCE(MAX(sequence_no), 0) FROM runtime_events
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
        forked_from_turn_id TEXT,
        forked_from_sequence_no INTEGER,
        workspace_root TEXT,
        rebound_from_workspace_root TEXT,
        rollout_path TEXT,
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
    DELETE FROM threads
    WHERE EXISTS (
        SELECT 1
        FROM threads AS newer
        WHERE newer.session_id = threads.session_id
          AND (
              newer.recency_at > threads.recency_at
              OR (newer.recency_at = threads.recency_at AND newer.updated_at > threads.updated_at)
              OR (
                  newer.recency_at = threads.recency_at
                  AND newer.updated_at = threads.updated_at
                  AND newer.thread_id > threads.thread_id
              )
          )
    )
    "#,
    r#"
    DROP INDEX IF EXISTS idx_threads_session
    "#,
    r#"
    CREATE UNIQUE INDEX IF NOT EXISTS idx_threads_session_unique
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
async fn sync_artifact_directory(path: &Path) -> StoreResult<()> {
    tokio::fs::File::open(path)
        .await
        .map_err(|error| StoreError::ArtifactIo(error.to_string()))?
        .sync_all()
        .await
        .map_err(|error| StoreError::ArtifactIo(error.to_string()))
}

#[cfg(not(unix))]
async fn sync_artifact_directory(_path: &Path) -> StoreResult<()> {
    Ok(())
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
    let forked_from_turn_id: Option<String> = row.try_get("forked_from_turn_id")?;
    let forked_from_sequence_no: Option<i64> = row.try_get("forked_from_sequence_no")?;
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
        forked_from_turn_id: forked_from_turn_id
            .map(|value| {
                TurnId::from_str(&value).map_err(|error| StoreError::InvalidId(error.to_string()))
            })
            .transpose()?,
        forked_from_sequence_no: forked_from_sequence_no
            .map(|value| {
                u64::try_from(value).map_err(|_| {
                    StoreError::InvalidId("negative forked_from_sequence_no".to_owned())
                })
            })
            .transpose()?,
        workspace_root: row.try_get("workspace_root")?,
        rebound_from_workspace_root: row.try_get("rebound_from_workspace_root")?,
        rollout_path: row.try_get("rollout_path")?,
        title: row.try_get("title")?,
        preview: row.try_get("preview")?,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
        recency_at: row.try_get("recency_at")?,
        archived: row.try_get("archived")?,
    })
}

#[cfg(test)]
mod tests;
