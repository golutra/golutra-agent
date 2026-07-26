use golutra_core::{
    ArtifactId, ArtifactRecord, BusyPolicyDecision, CommandId, ContextSnapshot, EventId,
    EvidenceRecord, PostTaskJob, PostTaskJobId, PostTaskJobKind, PostTaskJobStatus, SessionId,
    TaskId, ThreadId, Timestamp, ToolResultEnvelope, TurnId, VerificationPlan,
};
use golutra_protocol::{
    ArtifactReadRequest, CommandAck, DebugEventWindow, DebugProjection, RuntimeEvent,
    RuntimeEventType, SessionCursor, SessionRangeDirection, StateProjection, StorageStats,
    UserProjection,
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
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

mod projection;
mod repositories;

pub use repositories::{
    ArtifactRepository, DurableJobRepository, EventRepository, ProjectionRepository,
    RuntimeRepositories, ThreadRepository,
};

const DEBUG_PROJECTION_EVENT_LIMIT: u32 = 512;
const DEBUG_ARTIFACT_RETENTION_DAYS: i64 = 30;
const CHECKPOINT_ARTIFACT_RETENTION_DAYS: i64 = 30;
const EPHEMERAL_ARTIFACT_RETENTION_DAYS: i64 = 1;
const TEMPORARY_ARTIFACT_RETENTION_HOURS: u64 = 1;
pub const MAX_ARTIFACT_READ_BYTES: u64 = 1024 * 1024;

pub(crate) use projection::{
    apply_event_to_state, initial_projection, loop_decision_from_event, verification_from_event,
};

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("sqlite operation failed: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("json serialization failed: {0}")]
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

#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq)]
pub enum CommandClaim {
    Claimed { receipt_event: Option<RuntimeEvent> },
    Existing(CommandAck),
    Conflict { existing_command_id: String },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ArtifactMaintenanceReport {
    pub artifact_blobs_removed: u64,
    pub protected_artifacts_skipped: u64,
    pub temporary_artifacts_removed: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactRange {
    pub artifact: ArtifactRecord,
    pub offset: u64,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventIntegrity {
    pub event_count: u64,
    pub first_sequence: Option<u64>,
    pub last_sequence: Option<u64>,
    pub event_chain_digest: String,
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
        self.ensure_command_columns().await?;
        self.ensure_artifact_columns().await?;
        Ok(())
    }

    async fn ensure_artifact_columns(&self) -> StoreResult<()> {
        let rows = sqlx::query("PRAGMA table_info(artifact_records)")
            .fetch_all(&self.pool)
            .await?;
        let existing = rows
            .into_iter()
            .map(|row| row.try_get::<String, _>("name"))
            .collect::<Result<HashSet<_>, _>>()?;
        for (name, declaration) in [
            ("created_at", "TEXT"),
            ("retention_policy", "TEXT"),
            ("size_bytes", "INTEGER"),
            ("expires_at", "TEXT"),
            ("blob_deleted_at", "TEXT"),
        ] {
            if !existing.contains(name) {
                sqlx::query(&format!(
                    "ALTER TABLE artifact_records ADD COLUMN {name} {declaration}"
                ))
                .execute(&self.pool)
                .await?;
            }
        }
        let rows = sqlx::query(
            "SELECT artifact_id, artifact_json FROM artifact_records
             WHERE created_at IS NULL OR retention_policy IS NULL OR size_bytes IS NULL",
        )
        .fetch_all(&self.pool)
        .await?;
        for row in rows {
            let artifact_id: String = row.try_get("artifact_id")?;
            let artifact_json: String = row.try_get("artifact_json")?;
            let artifact: ArtifactRecord = serde_json::from_str(&artifact_json)?;
            sqlx::query(
                "UPDATE artifact_records
                 SET created_at = ?, retention_policy = ?, size_bytes = ?, expires_at = ?
                 WHERE artifact_id = ?",
            )
            .bind(artifact.created_at.to_rfc3339())
            .bind(&artifact.retention_policy)
            .bind(i64::try_from(artifact.size_bytes).unwrap_or(i64::MAX))
            .bind(artifact_expiration(&artifact).map(|value| value.to_rfc3339()))
            .bind(artifact_id)
            .execute(&self.pool)
            .await?;
        }
        Ok(())
    }

    async fn ensure_command_columns(&self) -> StoreResult<()> {
        let rows = sqlx::query("PRAGMA table_info(command_acks)")
            .fetch_all(&self.pool)
            .await?;
        let existing = rows
            .into_iter()
            .map(|row| row.try_get::<String, _>("name"))
            .collect::<Result<HashSet<_>, _>>()?;
        for (name, declaration) in [
            ("status", "TEXT NOT NULL DEFAULT 'completed'"),
            ("updated_at", "TEXT"),
        ] {
            if !existing.contains(name) {
                sqlx::query(&format!(
                    "ALTER TABLE command_acks ADD COLUMN {name} {declaration}"
                ))
                .execute(&self.pool)
                .await?;
            }
        }
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

    pub async fn claim_command(
        &self,
        idempotency_key: &str,
        command_id: CommandId,
        provisional_ack: &CommandAck,
        mut receipt_event: RuntimeEvent,
    ) -> StoreResult<CommandClaim> {
        let mut transaction = self.pool.begin().await?;
        let timestamp = chrono::Utc::now().to_rfc3339();
        let insert = sqlx::query(
            r#"
            INSERT OR IGNORE INTO command_acks (
                idempotency_key, command_id, ack_json, status, created_at, updated_at
            )
            VALUES (?, ?, ?, 'processing', ?, ?)
            "#,
        )
        .bind(idempotency_key)
        .bind(command_id.to_string())
        .bind(serde_json::to_string(provisional_ack)?)
        .bind(&timestamp)
        .bind(&timestamp)
        .execute(&mut *transaction)
        .await?;
        if insert.rows_affected() == 0 {
            let row = sqlx::query(
                "SELECT command_id, ack_json, status FROM command_acks WHERE idempotency_key = ?",
            )
            .bind(idempotency_key)
            .fetch_one(&mut *transaction)
            .await?;
            let existing_command_id: String = row.try_get("command_id")?;
            if existing_command_id != command_id.to_string() {
                transaction.commit().await?;
                return Ok(CommandClaim::Conflict {
                    existing_command_id,
                });
            }
            let ack_json: String = row.try_get("ack_json")?;
            let ack = serde_json::from_str(&ack_json)?;
            let status: String = row.try_get("status")?;
            transaction.commit().await?;
            return if status == "processing" {
                Ok(CommandClaim::Claimed {
                    receipt_event: None,
                })
            } else {
                Ok(CommandClaim::Existing(ack))
            };
        }

        receipt_event.sequence_no = next_sequence_in_transaction(&mut transaction).await?;
        append_event_in_transaction(&mut transaction, &receipt_event).await?;
        transaction.commit().await?;
        Ok(CommandClaim::Claimed {
            receipt_event: Some(receipt_event),
        })
    }

    pub async fn complete_command(
        &self,
        idempotency_key: &str,
        command_id: CommandId,
        ack: &CommandAck,
        mut completion_event: RuntimeEvent,
    ) -> StoreResult<RuntimeEvent> {
        let mut transaction = self.pool.begin().await?;
        let update = sqlx::query(
            r#"
            UPDATE command_acks
            SET ack_json = ?, status = ?, updated_at = ?
            WHERE idempotency_key = ? AND command_id = ?
            "#,
        )
        .bind(serde_json::to_string(ack)?)
        .bind(if ack.accepted {
            "completed"
        } else {
            "rejected"
        })
        .bind(chrono::Utc::now().to_rfc3339())
        .bind(idempotency_key)
        .bind(command_id.to_string())
        .execute(&mut *transaction)
        .await?;
        if update.rows_affected() == 0 {
            let existing =
                sqlx::query("SELECT command_id FROM command_acks WHERE idempotency_key = ?")
                    .bind(idempotency_key)
                    .fetch_optional(&mut *transaction)
                    .await?;
            let detail = match existing {
                None => format!("command journal entry not found for `{idempotency_key}`"),
                Some(row) => {
                    let existing_command_id: String = row.try_get("command_id")?;
                    format!(
                        "idempotency key belongs to command {existing_command_id}, not {command_id}"
                    )
                }
            };
            return Err(StoreError::InvalidId(detail));
        }
        completion_event.sequence_no = next_sequence_in_transaction(&mut transaction).await?;
        append_event_in_transaction(&mut transaction, &completion_event).await?;
        transaction.commit().await?;
        Ok(completion_event)
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

    pub async fn session_for_task(&self, task_id: TaskId) -> StoreResult<Option<SessionId>> {
        let row = sqlx::query(
            "SELECT session_id FROM runtime_events
             WHERE task_id = ? ORDER BY sequence_no ASC LIMIT 1",
        )
        .bind(task_id.to_string())
        .fetch_optional(&self.pool)
        .await?;
        row.map(|row| {
            let value: String = row.try_get("session_id")?;
            value
                .parse()
                .map_err(|error: uuid::Error| StoreError::InvalidId(error.to_string()))
        })
        .transpose()
    }

    pub async fn event_integrity(
        &self,
        session_id: SessionId,
        task_id: TaskId,
    ) -> StoreResult<EventIntegrity> {
        self.event_integrity_before(session_id, task_id, None).await
    }

    pub async fn event_integrity_before(
        &self,
        session_id: SessionId,
        task_id: TaskId,
        before_sequence_no: Option<u64>,
    ) -> StoreResult<EventIntegrity> {
        let rows = sqlx::query(
            "SELECT sequence_no, event_json FROM runtime_events
             WHERE session_id = ? AND task_id = ?
               AND (? IS NULL OR sequence_no < ?)
             ORDER BY sequence_no ASC",
        )
        .bind(session_id.to_string())
        .bind(task_id.to_string())
        .bind(before_sequence_no.and_then(|value| i64::try_from(value).ok()))
        .bind(before_sequence_no.and_then(|value| i64::try_from(value).ok()))
        .fetch_all(&self.pool)
        .await?;
        let mut digest = Sha256::new();
        let first_sequence = rows.first().map(|row| row.try_get::<i64, _>("sequence_no"));
        let last_sequence = rows.last().map(|row| row.try_get::<i64, _>("sequence_no"));
        for row in &rows {
            let sequence_no: i64 = row.try_get("sequence_no")?;
            let event_json: String = row.try_get("event_json")?;
            digest.update(sequence_no.to_be_bytes());
            digest.update(event_json.as_bytes());
        }
        Ok(EventIntegrity {
            event_count: u64::try_from(rows.len()).unwrap_or(u64::MAX),
            first_sequence: first_sequence
                .transpose()?
                .and_then(|value| u64::try_from(value).ok()),
            last_sequence: last_sequence
                .transpose()?
                .and_then(|value| u64::try_from(value).ok()),
            event_chain_digest: format!("sha256:{:x}", digest.finalize()),
        })
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

    pub async fn load_events_before(
        &self,
        session_id: SessionId,
        task_id: Option<TaskId>,
        before_sequence_no: Option<u64>,
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
              AND sequence_no < ?
            ORDER BY sequence_no DESC
            LIMIT ?
            "#,
        )
        .bind(session_id.to_string())
        .bind(task_id.map(|id| id.to_string()))
        .bind(task_id.map(|id| id.to_string()))
        .bind(
            before_sequence_no
                .and_then(|value| i64::try_from(value).ok())
                .unwrap_or(i64::MAX),
        )
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

    pub async fn load_latest_context_compaction(
        &self,
        session_id: SessionId,
    ) -> StoreResult<Option<RuntimeEvent>> {
        let row = sqlx::query(
            r#"
            SELECT event_json
            FROM runtime_events
            WHERE session_id = ?
              AND event_type = 'CompactionCompleted'
              AND json_extract(payload_json, '$.content') IS NOT NULL
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
        let mut events = self
            .load_events_before(
                session_id,
                task_id,
                None,
                DEBUG_PROJECTION_EVENT_LIMIT.saturating_add(1),
            )
            .await?;
        let has_more_before = events.len() > DEBUG_PROJECTION_EVENT_LIMIT as usize;
        if has_more_before {
            events.remove(0);
        }
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
        let mut referenced_artifacts = tool_results
            .iter()
            .filter_map(|result| result.raw_artifact_ref)
            .chain(events.iter().filter_map(|event| event.payload_ref))
            .collect::<HashSet<_>>();
        for event in &events {
            collect_structured_artifact_refs(&event.payload, None, &mut referenced_artifacts);
        }
        let mut artifacts = Vec::with_capacity(referenced_artifacts.len());
        for artifact_id in &referenced_artifacts {
            if let Some(artifact) = self.load_artifact(*artifact_id).await? {
                artifacts.push(artifact);
            }
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
        let post_task_jobs = match task_id {
            Some(task_id) => self.list_post_task_jobs(task_id).await?,
            None => Vec::new(),
        };
        let mut missing_sections = Vec::new();
        if task_id.is_some() && verification.is_none() {
            missing_sections.push("verification_record".to_owned());
        }
        if task_id.is_some() && post_task_jobs.is_empty() {
            missing_sections.push("post_task_job".to_owned());
        }
        if post_task_jobs.iter().any(|job| {
            !matches!(
                job.status,
                PostTaskJobStatus::Succeeded
                    | PostTaskJobStatus::Failed
                    | PostTaskJobStatus::Cancelled
            )
        }) {
            missing_sections.push("post_task_job_terminal".to_owned());
        }
        if has_more_before {
            missing_sections.push("event_window".to_owned());
        }
        let trace_complete = missing_sections.is_empty();
        Ok(DebugProjection {
            session_id,
            task_id,
            event_window: DebugEventWindow {
                start_cursor: events.first().map(|event| event.sequence_no),
                end_cursor: events.last().map(|event| event.sequence_no),
                has_more_before,
                limit: DEBUG_PROJECTION_EVENT_LIMIT,
            },
            events,
            busy_policy_decisions,
            tool_results,
            artifacts,
            evidence,
            verification,
            loop_decisions,
            post_task_jobs,
            failure_diagnosis: None,
            failure_episodes: Vec::new(),
            diagnostic_slice: None,
            replay_execution: None,
            external_evaluations: Vec::new(),
            causal_comparisons: Vec::new(),
            trace_complete,
            missing_sections,
            retention_losses: Vec::new(),
        })
    }

    pub async fn store_artifact(&self, artifact: &ArtifactRecord, bytes: &[u8]) -> StoreResult<()> {
        verify_artifact_checksum(artifact, bytes)?;
        tokio::fs::create_dir_all(&self.artifact_root)
            .await
            .map_err(|error| StoreError::ArtifactIo(error.to_string()))?;
        set_owner_only_dir(&self.artifact_root).await?;
        let final_path = self.artifact_blob_path(artifact.artifact_id);
        let duplicate_artifact_id: Option<String> = sqlx::query_scalar(
            "SELECT artifact_id FROM artifact_records
             WHERE checksum = ? AND size_bytes = ? AND blob_deleted_at IS NULL
             ORDER BY created_at ASC LIMIT 1",
        )
        .bind(&artifact.checksum)
        .bind(i64::try_from(artifact.size_bytes).unwrap_or(i64::MAX))
        .fetch_optional(&self.pool)
        .await?;
        let linked = if let Some(existing_id) = duplicate_artifact_id {
            let existing_id = existing_id.parse::<ArtifactId>().map_err(|error| {
                StoreError::ArtifactIo(format!("stored artifact id is invalid: {error}"))
            })?;
            let existing_path = self.artifact_blob_path(existing_id);
            let existing_bytes = tokio::fs::read(&existing_path)
                .await
                .map_err(|error| StoreError::ArtifactIo(error.to_string()))?;
            if existing_bytes != bytes {
                return Err(StoreError::ArtifactIo(format!(
                    "artifact checksum collision for {}",
                    artifact.checksum
                )));
            }
            match tokio::fs::hard_link(&existing_path, &final_path).await {
                Ok(()) => true,
                Err(error) if error.kind() == ErrorKind::AlreadyExists => false,
                Err(error) => return Err(StoreError::ArtifactIo(error.to_string())),
            }
        } else {
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
            linked
        };
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
        let expires_at = artifact_expiration(artifact).map(|value| value.to_rfc3339());
        let result = sqlx::query(
            r#"
            INSERT OR IGNORE INTO artifact_records (
                artifact_id, session_id, uri, checksum, artifact_json,
                created_at, retention_policy, size_bytes, expires_at, blob_deleted_at
            )
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, NULL)
            "#,
        )
        .bind(artifact.artifact_id.to_string())
        .bind(artifact.session_id.to_string())
        .bind(&artifact.uri)
        .bind(&artifact.checksum)
        .bind(serde_json::to_string(artifact)?)
        .bind(artifact.created_at.to_rfc3339())
        .bind(&artifact.retention_policy)
        .bind(i64::try_from(artifact.size_bytes).unwrap_or(i64::MAX))
        .bind(expires_at)
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
        if result.rows_affected() == 0 {
            sqlx::query("UPDATE artifact_records SET blob_deleted_at = NULL WHERE artifact_id = ?")
                .bind(artifact.artifact_id.to_string())
                .execute(&self.pool)
                .await?;
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

    pub async fn find_artifact_by_content(
        &self,
        session_id: SessionId,
        artifact_type: &str,
        checksum: &str,
        size_bytes: u64,
    ) -> StoreResult<Option<ArtifactRecord>> {
        let rows = sqlx::query(
            r#"
            SELECT artifact_json
            FROM artifact_records
            WHERE session_id = ? AND checksum = ? AND size_bytes = ?
              AND blob_deleted_at IS NULL
            ORDER BY created_at ASC
            "#,
        )
        .bind(session_id.to_string())
        .bind(checksum)
        .bind(i64::try_from(size_bytes).unwrap_or(i64::MAX))
        .fetch_all(&self.pool)
        .await?;
        for row in rows {
            let artifact_json: String = row.try_get("artifact_json")?;
            let artifact: ArtifactRecord = serde_json::from_str(&artifact_json)?;
            if artifact.artifact_type == artifact_type {
                return Ok(Some(artifact));
            }
        }
        Ok(None)
    }

    pub async fn load_artifact_bytes(
        &self,
        artifact_id: ArtifactId,
    ) -> StoreResult<Option<Vec<u8>>> {
        let Some(artifact) = self.load_artifact(artifact_id).await? else {
            return Ok(None);
        };
        let blob_deleted_at: Option<String> = sqlx::query_scalar(
            "SELECT blob_deleted_at FROM artifact_records WHERE artifact_id = ?",
        )
        .bind(artifact_id.to_string())
        .fetch_one(&self.pool)
        .await?;
        if blob_deleted_at.is_some() {
            return Ok(None);
        }
        let path = self.artifact_blob_path(artifact_id);
        let bytes = tokio::fs::read(&path)
            .await
            .map_err(|error| StoreError::ArtifactIo(format!("{}: {error}", path.display())))?;
        verify_artifact_checksum(&artifact, &bytes)?;
        Ok(Some(bytes))
    }

    pub async fn read_artifact_range(
        &self,
        request: &ArtifactReadRequest,
    ) -> StoreResult<Option<ArtifactRange>> {
        if request.length == 0 || request.length > MAX_ARTIFACT_READ_BYTES {
            return Err(StoreError::ArtifactIo(format!(
                "artifact read length must be between 1 and {MAX_ARTIFACT_READ_BYTES} bytes"
            )));
        }
        let Some(artifact) = self.load_artifact(request.artifact_id).await? else {
            return Ok(None);
        };
        let blob_deleted_at: Option<String> = sqlx::query_scalar(
            "SELECT blob_deleted_at FROM artifact_records WHERE artifact_id = ?",
        )
        .bind(request.artifact_id.to_string())
        .fetch_one(&self.pool)
        .await?;
        if blob_deleted_at.is_some() {
            return Ok(None);
        }
        let path = self.artifact_blob_path(request.artifact_id);
        let mut file = tokio::fs::File::open(&path)
            .await
            .map_err(|error| StoreError::ArtifactIo(format!("{}: {error}", path.display())))?;
        let actual_size = file
            .metadata()
            .await
            .map_err(|error| StoreError::ArtifactIo(format!("{}: {error}", path.display())))?
            .len();
        if actual_size != artifact.size_bytes {
            return Err(StoreError::ArtifactIo(format!(
                "artifact {} size mismatch: metadata={} recorded={}",
                request.artifact_id, actual_size, artifact.size_bytes
            )));
        }
        if request.offset >= artifact.size_bytes {
            return Ok(Some(ArtifactRange {
                artifact,
                offset: request.offset,
                bytes: Vec::new(),
            }));
        }
        file.seek(std::io::SeekFrom::Start(request.offset))
            .await
            .map_err(|error| StoreError::ArtifactIo(format!("{}: {error}", path.display())))?;
        let length = request
            .length
            .min(artifact.size_bytes.saturating_sub(request.offset));
        let mut bytes = vec![0_u8; usize::try_from(length).unwrap_or(usize::MAX)];
        file.read_exact(&mut bytes)
            .await
            .map_err(|error| StoreError::ArtifactIo(format!("{}: {error}", path.display())))?;
        Ok(Some(ArtifactRange {
            artifact,
            offset: request.offset,
            bytes,
        }))
    }

    pub async fn store_context_snapshot(&self, snapshot: &ContextSnapshot) -> StoreResult<()> {
        sqlx::query(
            "INSERT OR IGNORE INTO context_snapshots
             (snapshot_id, session_id, task_id, turn_id, created_at, snapshot_json)
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(snapshot.snapshot_id.to_string())
        .bind(snapshot.session_id.to_string())
        .bind(snapshot.task_id.to_string())
        .bind(snapshot.turn_id.to_string())
        .bind(snapshot.created_at.to_rfc3339())
        .bind(serde_json::to_string(snapshot)?)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn load_context_snapshots(
        &self,
        task_id: TaskId,
    ) -> StoreResult<Vec<ContextSnapshot>> {
        let rows = sqlx::query(
            "SELECT snapshot_json FROM context_snapshots
             WHERE task_id = ? ORDER BY created_at ASC",
        )
        .bind(task_id.to_string())
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| {
                let snapshot_json: String = row.try_get("snapshot_json")?;
                Ok(serde_json::from_str(&snapshot_json)?)
            })
            .collect()
    }

    pub async fn store_verification_plan(&self, plan: &VerificationPlan) -> StoreResult<()> {
        sqlx::query(
            "INSERT OR REPLACE INTO verification_plans
             (plan_id, task_id, revision, created_at, plan_json)
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(plan.plan_id.to_string())
        .bind(plan.task_id.to_string())
        .bind(i64::from(plan.revision))
        .bind(plan.created_at.to_rfc3339())
        .bind(serde_json::to_string(plan)?)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn load_verification_plan(
        &self,
        task_id: TaskId,
    ) -> StoreResult<Option<VerificationPlan>> {
        let row = sqlx::query(
            "SELECT plan_json FROM verification_plans
             WHERE task_id = ? ORDER BY revision DESC LIMIT 1",
        )
        .bind(task_id.to_string())
        .fetch_optional(&self.pool)
        .await?;
        row.map(|row| {
            let plan_json: String = row.try_get("plan_json")?;
            Ok(serde_json::from_str(&plan_json)?)
        })
        .transpose()
    }

    pub async fn enqueue_post_task_job(&self, job: &PostTaskJob) -> StoreResult<()> {
        sqlx::query(
            "INSERT OR IGNORE INTO post_task_jobs
             (job_id, kind, workspace_id, session_id, task_id, input_refs_json, status,
              attempt, max_attempts, lease_owner, lease_expires_at, result_refs_json,
              last_error, created_at, started_at, completed_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(job.job_id.to_string())
        .bind(enum_json(job.kind)?)
        .bind(&job.workspace_id)
        .bind(&job.session_id)
        .bind(job.task_id.to_string())
        .bind(serde_json::to_string(&job.input_refs)?)
        .bind(enum_json(job.status)?)
        .bind(i64::from(job.attempt))
        .bind(i64::from(job.max_attempts))
        .bind(&job.lease_owner)
        .bind(job.lease_expires_at.map(|value| value.to_rfc3339()))
        .bind(serde_json::to_string(&job.result_refs)?)
        .bind(&job.last_error)
        .bind(job.created_at.to_rfc3339())
        .bind(job.started_at.map(|value| value.to_rfc3339()))
        .bind(job.completed_at.map(|value| value.to_rfc3339()))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// 在同一个 SQLite 事务中写入后台作业和排队事件，避免恢复时出现“有事件无作业”或反向的半状态。
    pub async fn enqueue_post_task_job_with_event(
        &self,
        job: &PostTaskJob,
        mut event: RuntimeEvent,
    ) -> StoreResult<Option<RuntimeEvent>> {
        let mut transaction = self.pool.begin().await?;
        // Acquire the SQLite writer lock before checking for an existing job.
        // This keeps independent RuntimeHost processes from scheduling the
        // same task concurrently in the terminal-event/enqueue recovery window.
        sqlx::query(
            "UPDATE runtime_sequence SET last_sequence_no = last_sequence_no WHERE singleton = 1",
        )
        .execute(&mut *transaction)
        .await?;
        let kind = enum_json(job.kind)?;
        let existing =
            sqlx::query("SELECT job_id FROM post_task_jobs WHERE task_id = ? AND kind = ? LIMIT 1")
                .bind(job.task_id.to_string())
                .bind(&kind)
                .fetch_optional(&mut *transaction)
                .await?;
        if existing.is_some() {
            transaction.commit().await?;
            return Ok(None);
        }
        sqlx::query(
            "INSERT INTO post_task_jobs
             (job_id, kind, workspace_id, session_id, task_id, input_refs_json, status,
              attempt, max_attempts, lease_owner, lease_expires_at, result_refs_json,
              last_error, created_at, started_at, completed_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(job.job_id.to_string())
        .bind(kind)
        .bind(&job.workspace_id)
        .bind(&job.session_id)
        .bind(job.task_id.to_string())
        .bind(serde_json::to_string(&job.input_refs)?)
        .bind(enum_json(job.status)?)
        .bind(i64::from(job.attempt))
        .bind(i64::from(job.max_attempts))
        .bind(&job.lease_owner)
        .bind(job.lease_expires_at.map(|value| value.to_rfc3339()))
        .bind(serde_json::to_string(&job.result_refs)?)
        .bind(&job.last_error)
        .bind(job.created_at.to_rfc3339())
        .bind(job.started_at.map(|value| value.to_rfc3339()))
        .bind(job.completed_at.map(|value| value.to_rfc3339()))
        .execute(&mut *transaction)
        .await?;
        event.sequence_no = next_sequence_in_transaction(&mut transaction).await?;
        append_event_in_transaction(&mut transaction, &event).await?;
        transaction.commit().await?;
        Ok(Some(event))
    }

    pub async fn post_task_job(&self, task_id: TaskId) -> StoreResult<Option<PostTaskJob>> {
        let row = sqlx::query(
            "SELECT job_id, kind, workspace_id, session_id, task_id, input_refs_json, status,
                    attempt, max_attempts, lease_owner, lease_expires_at, result_refs_json,
                    last_error, created_at, started_at, completed_at
             FROM post_task_jobs WHERE task_id = ? ORDER BY created_at DESC LIMIT 1",
        )
        .bind(task_id.to_string())
        .fetch_optional(&self.pool)
        .await?;
        row.map(post_task_job_from_row).transpose()
    }

    pub async fn post_task_job_by_id(
        &self,
        job_id: PostTaskJobId,
    ) -> StoreResult<Option<PostTaskJob>> {
        let row = sqlx::query(
            "SELECT job_id, kind, workspace_id, session_id, task_id, input_refs_json, status,
                    attempt, max_attempts, lease_owner, lease_expires_at, result_refs_json,
                    last_error, created_at, started_at, completed_at
             FROM post_task_jobs WHERE job_id = ?",
        )
        .bind(job_id.to_string())
        .fetch_optional(&self.pool)
        .await?;
        row.map(post_task_job_from_row).transpose()
    }

    pub async fn list_post_task_jobs(&self, task_id: TaskId) -> StoreResult<Vec<PostTaskJob>> {
        let rows = sqlx::query(
            "SELECT job_id, kind, workspace_id, session_id, task_id, input_refs_json, status,
                    attempt, max_attempts, lease_owner, lease_expires_at, result_refs_json,
                    last_error, created_at, started_at, completed_at
             FROM post_task_jobs WHERE task_id = ? ORDER BY created_at ASC",
        )
        .bind(task_id.to_string())
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(post_task_job_from_row).collect()
    }

    /// Return terminal tasks whose durable event declares pending governance,
    /// but which have neither a deep-evaluation job nor a terminal scheduling
    /// failure. This closes the crash window between terminal event commit and
    /// atomic job enqueue.
    pub async fn unscheduled_post_task_terminal_events(
        &self,
        workspace_root: Option<&str>,
    ) -> StoreResult<Vec<RuntimeEvent>> {
        let rows = sqlx::query(
            r#"
            WITH scoped_events AS (
                SELECT event.*
                FROM runtime_events AS event
                JOIN threads AS thread ON thread.session_id = event.session_id
                WHERE (? IS NULL OR thread.workspace_root = ?)
            ),
            latest_terminal AS (
                SELECT task_id, MAX(sequence_no) AS sequence_no
                FROM scoped_events
                WHERE task_id IS NOT NULL
                  AND event_type IN ('TaskCompleted', 'TaskAborted', 'TaskInterrupted', 'TaskUncertain')
                GROUP BY task_id
            )
            SELECT terminal.event_json,
                   (
                       SELECT failure.event_json
                       FROM scoped_events AS failure
                       WHERE failure.task_id = terminal.task_id
                         AND failure.event_type = 'PostTaskStageFailed'
                       ORDER BY failure.sequence_no DESC
                       LIMIT 1
                   ) AS stage_failure_json
            FROM latest_terminal
            JOIN scoped_events AS terminal
              ON terminal.task_id = latest_terminal.task_id
             AND terminal.sequence_no = latest_terminal.sequence_no
            LEFT JOIN post_task_jobs AS job
              ON job.task_id = terminal.task_id
             AND job.kind = 'deep_evaluation'
            WHERE job.job_id IS NULL
            ORDER BY terminal.sequence_no ASC
            "#,
        )
        .bind(workspace_root)
        .bind(workspace_root)
        .fetch_all(&self.pool)
        .await?;

        let mut events = Vec::with_capacity(rows.len());
        for row in rows {
            let event_json: String = row.try_get("event_json")?;
            let event = serde_json::from_str::<RuntimeEvent>(&event_json)?;
            if event
                .payload
                .pointer("/post_task_governance/status")
                .and_then(serde_json::Value::as_str)
                != Some("pending")
            {
                continue;
            }
            let stage_failure_json: Option<String> = row.try_get("stage_failure_json")?;
            let scheduling_terminal = stage_failure_json
                .as_deref()
                .map(serde_json::from_str::<RuntimeEvent>)
                .transpose()?
                .is_some_and(|failure| {
                    failure
                        .payload
                        .get("terminal")
                        .and_then(serde_json::Value::as_bool)
                        == Some(true)
                });
            if !scheduling_terminal {
                events.push(event);
            }
        }
        Ok(events)
    }

    pub async fn recover_expired_post_task_jobs(
        &self,
        workspace_id: &str,
        now: chrono::DateTime<chrono::Utc>,
    ) -> StoreResult<u64> {
        let requeued = sqlx::query(
            "UPDATE post_task_jobs SET status = 'queued', lease_owner = NULL,
             lease_expires_at = NULL, last_error = 'worker lease expired'
             WHERE workspace_id = ? AND status IN ('leased', 'running')
             AND lease_expires_at IS NOT NULL
             AND lease_expires_at <= ? AND attempt < max_attempts",
        )
        .bind(workspace_id)
        .bind(now.to_rfc3339())
        .execute(&self.pool)
        .await?;
        let failed = sqlx::query(
            "UPDATE post_task_jobs SET status = 'failed', lease_owner = NULL,
             lease_expires_at = NULL, last_error = 'worker lease expired and retry budget exhausted',
             completed_at = ?
             WHERE workspace_id = ? AND status IN ('leased', 'running')
             AND lease_expires_at IS NOT NULL
             AND lease_expires_at <= ? AND attempt >= max_attempts",
        )
        .bind(now.to_rfc3339())
        .bind(workspace_id)
        .bind(now.to_rfc3339())
        .execute(&self.pool)
        .await?;
        Ok(requeued
            .rows_affected()
            .saturating_add(failed.rows_affected()))
    }

    pub async fn claim_post_task_job(
        &self,
        worker_id: &str,
        now: chrono::DateTime<chrono::Utc>,
        lease_for: chrono::Duration,
    ) -> StoreResult<Option<PostTaskJob>> {
        self.claim_post_task_job_matching(worker_id, None, now, lease_for)
            .await
    }

    pub async fn claim_post_task_job_for_workspace(
        &self,
        worker_id: &str,
        workspace_id: &str,
        now: chrono::DateTime<chrono::Utc>,
        lease_for: chrono::Duration,
    ) -> StoreResult<Option<PostTaskJob>> {
        self.claim_post_task_job_matching(worker_id, Some(workspace_id), now, lease_for)
            .await
    }

    async fn claim_post_task_job_matching(
        &self,
        worker_id: &str,
        workspace_id: Option<&str>,
        now: chrono::DateTime<chrono::Utc>,
        lease_for: chrono::Duration,
    ) -> StoreResult<Option<PostTaskJob>> {
        let mut transaction = self.pool.begin().await?;
        let row = sqlx::query(
            "SELECT job_id FROM post_task_jobs
             WHERE status = 'queued' AND attempt < max_attempts
             AND (? IS NULL OR workspace_id = ?)
             ORDER BY created_at ASC LIMIT 1",
        )
        .bind(workspace_id)
        .bind(workspace_id)
        .fetch_optional(&mut *transaction)
        .await?;
        let Some(row) = row else {
            transaction.commit().await?;
            return Ok(None);
        };
        let job_id: String = row.try_get("job_id")?;
        let expires_at = now
            .checked_add_signed(lease_for)
            .unwrap_or(now + chrono::Duration::minutes(5));
        let updated = sqlx::query(
            "UPDATE post_task_jobs SET status = 'leased', attempt = attempt + 1,
             lease_owner = ?, lease_expires_at = ?
             WHERE job_id = ? AND status = 'queued'",
        )
        .bind(worker_id)
        .bind(expires_at.to_rfc3339())
        .bind(&job_id)
        .execute(&mut *transaction)
        .await?;
        if updated.rows_affected() == 0 {
            transaction.commit().await?;
            return Ok(None);
        }
        let row = sqlx::query(
            "SELECT job_id, kind, workspace_id, session_id, task_id, input_refs_json, status,
                    attempt, max_attempts, lease_owner, lease_expires_at, result_refs_json,
                    last_error, created_at, started_at, completed_at
             FROM post_task_jobs WHERE job_id = ?",
        )
        .bind(&job_id)
        .fetch_one(&mut *transaction)
        .await?;
        let job = post_task_job_from_row(row)?;
        transaction.commit().await?;
        Ok(Some(job))
    }

    pub async fn start_post_task_job(
        &self,
        job_id: PostTaskJobId,
        worker_id: &str,
        started_at: chrono::DateTime<chrono::Utc>,
    ) -> StoreResult<bool> {
        let result = sqlx::query(
            "UPDATE post_task_jobs SET status = 'running', started_at = ?
             WHERE job_id = ? AND status = 'leased' AND lease_owner = ?",
        )
        .bind(started_at.to_rfc3339())
        .bind(job_id.to_string())
        .bind(worker_id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    pub async fn requeue_post_task_job(
        &self,
        job_id: PostTaskJobId,
        worker_id: &str,
        error: &str,
    ) -> StoreResult<bool> {
        let result = sqlx::query(
            "UPDATE post_task_jobs SET status = 'queued', last_error = ?,
             lease_owner = NULL, lease_expires_at = NULL
             WHERE job_id = ? AND lease_owner = ?
             AND status IN ('leased', 'running') AND attempt < max_attempts",
        )
        .bind(error)
        .bind(job_id.to_string())
        .bind(worker_id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    pub async fn retry_post_task_job(&self, job_id: PostTaskJobId) -> StoreResult<bool> {
        let result = sqlx::query(
            "UPDATE post_task_jobs SET status = 'queued', attempt = 0,
             lease_owner = NULL, lease_expires_at = NULL, last_error = NULL,
             started_at = NULL, completed_at = NULL
             WHERE job_id = ? AND status IN ('failed', 'cancelled')",
        )
        .bind(job_id.to_string())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    pub async fn finish_post_task_job(
        &self,
        job_id: PostTaskJobId,
        worker_id: &str,
        status: PostTaskJobStatus,
        result_refs: &[String],
        error: Option<&str>,
        completed_at: chrono::DateTime<chrono::Utc>,
    ) -> StoreResult<bool> {
        let result = sqlx::query(
            "UPDATE post_task_jobs SET status = ?, result_refs_json = ?, last_error = ?,
             completed_at = ?, lease_owner = NULL, lease_expires_at = NULL
             WHERE job_id = ? AND lease_owner = ? AND status IN ('leased', 'running')",
        )
        .bind(enum_json(status)?)
        .bind(serde_json::to_string(result_refs)?)
        .bind(error)
        .bind(completed_at.to_rfc3339())
        .bind(job_id.to_string())
        .bind(worker_id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    pub async fn storage_stats(&self) -> StoreResult<StorageStats> {
        let row = sqlx::query(
            r#"
            SELECT
                COUNT(*) AS artifact_records,
                COALESCE(SUM(CASE WHEN blob_deleted_at IS NULL THEN 1 ELSE 0 END), 0)
                    AS live_artifact_blobs,
                COALESCE(SUM(CASE WHEN blob_deleted_at IS NOT NULL THEN 1 ELSE 0 END), 0)
                    AS expired_artifact_blobs,
                COALESCE(SUM(CASE WHEN blob_deleted_at IS NULL THEN size_bytes ELSE 0 END), 0)
                    AS live_artifact_bytes
            FROM artifact_records
            "#,
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(StorageStats {
            artifact_records: non_negative_database_count(&row, "artifact_records")?,
            live_artifact_blobs: non_negative_database_count(&row, "live_artifact_blobs")?,
            expired_artifact_blobs: non_negative_database_count(&row, "expired_artifact_blobs")?,
            live_artifact_bytes: non_negative_database_count(&row, "live_artifact_bytes")?,
            checkpoint_directories: 0,
            rollout_files: 0,
        })
    }

    pub async fn run_artifact_maintenance(
        &self,
        now: chrono::DateTime<chrono::Utc>,
    ) -> StoreResult<ArtifactMaintenanceReport> {
        tokio::fs::create_dir_all(&self.artifact_root)
            .await
            .map_err(|error| StoreError::ArtifactIo(error.to_string()))?;
        set_owner_only_dir(&self.artifact_root).await?;
        let protected = self.protected_artifact_ids().await?;
        let active_sessions = self.active_session_ids().await?;
        let rows = sqlx::query(
            "SELECT artifact_id, session_id, artifact_json, expires_at
             FROM artifact_records WHERE blob_deleted_at IS NULL",
        )
        .fetch_all(&self.pool)
        .await?;
        let mut report = ArtifactMaintenanceReport::default();
        for row in rows {
            let artifact_id_text: String = row.try_get("artifact_id")?;
            let artifact_id = artifact_id_text
                .parse::<uuid::Uuid>()
                .map(ArtifactId)
                .map_err(|_| StoreError::InvalidId(artifact_id_text.clone()))?;
            let session_id: String = row.try_get("session_id")?;
            let artifact_json: String = row.try_get("artifact_json")?;
            let artifact: ArtifactRecord = serde_json::from_str(&artifact_json)?;
            let expires_at = row
                .try_get::<Option<String>, _>("expires_at")?
                .and_then(|value| chrono::DateTime::parse_from_rfc3339(&value).ok())
                .map(|value| value.with_timezone(&chrono::Utc))
                .or_else(|| artifact_expiration(&artifact));
            if expires_at.is_none_or(|expires_at| expires_at > now) {
                continue;
            }
            if protected.contains(&artifact_id) || active_sessions.contains(&session_id) {
                report.protected_artifacts_skipped =
                    report.protected_artifacts_skipped.saturating_add(1);
                continue;
            }
            let path = self.artifact_blob_path(artifact_id);
            match tokio::fs::remove_file(&path).await {
                Ok(()) => {}
                Err(error) if error.kind() == ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(StoreError::ArtifactIo(format!(
                        "{}: {error}",
                        path.display()
                    )));
                }
            }
            sqlx::query("UPDATE artifact_records SET blob_deleted_at = ? WHERE artifact_id = ?")
                .bind(now.to_rfc3339())
                .bind(&artifact_id_text)
                .execute(&self.pool)
                .await?;
            report.artifact_blobs_removed = report.artifact_blobs_removed.saturating_add(1);
        }
        report.temporary_artifacts_removed = self.prune_temporary_artifacts().await?;
        sync_artifact_directory(&self.artifact_root).await?;
        Ok(report)
    }

    async fn protected_artifact_ids(&self) -> StoreResult<HashSet<ArtifactId>> {
        let rows = sqlx::query("SELECT evidence_json FROM evidence_records")
            .fetch_all(&self.pool)
            .await?;
        let mut protected = HashSet::new();
        for row in rows {
            let evidence_json: String = row.try_get("evidence_json")?;
            let evidence: EvidenceRecord = serde_json::from_str(&evidence_json)?;
            protected.extend(evidence.artifact_refs);
        }
        Ok(protected)
    }

    async fn active_session_ids(&self) -> StoreResult<HashSet<String>> {
        let rows = sqlx::query(
            "SELECT session_id FROM sessions
             WHERE status IN (
                'running', 'waiting_approval', 'waiting_authentication',
                'pausing', 'paused', 'aborting'
             )",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| row.try_get::<String, _>("session_id"))
            .collect::<Result<HashSet<_>, _>>()
            .map_err(StoreError::Sqlx)
    }

    async fn prune_temporary_artifacts(&self) -> StoreResult<u64> {
        let mut removed = 0_u64;
        let mut entries = match tokio::fs::read_dir(&self.artifact_root).await {
            Ok(entries) => entries,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(0),
            Err(error) => return Err(StoreError::ArtifactIo(error.to_string())),
        };
        while let Some(entry) = entries
            .next_entry()
            .await
            .map_err(|error| StoreError::ArtifactIo(error.to_string()))?
        {
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if !name.contains(".tmp-") {
                continue;
            }
            let metadata = entry
                .metadata()
                .await
                .map_err(|error| StoreError::ArtifactIo(error.to_string()))?;
            let old_enough = metadata
                .modified()
                .ok()
                .and_then(|modified| modified.elapsed().ok())
                .is_some_and(|age| {
                    age >= Duration::from_secs(
                        TEMPORARY_ARTIFACT_RETENTION_HOURS.saturating_mul(60 * 60),
                    )
                });
            if metadata.is_file() && old_enough {
                tokio::fs::remove_file(entry.path())
                    .await
                    .map_err(|error| StoreError::ArtifactIo(error.to_string()))?;
                removed = removed.saturating_add(1);
            }
        }
        Ok(removed)
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

    pub async fn list_threads_page(
        &self,
        workspace_root: Option<&str>,
        cursor: Option<&SessionCursor>,
        limit: u32,
    ) -> StoreResult<Vec<ThreadRecord>> {
        let cursor_at = cursor.map(|cursor| cursor.recency_at.to_rfc3339());
        let cursor_thread = cursor.map(|cursor| cursor.thread_id.to_string());
        let rows = sqlx::query(
            r#"
            SELECT thread_id, session_id, parent_thread_id, forked_from_turn_id,
                   forked_from_sequence_no, workspace_root, rebound_from_workspace_root,
                   rollout_path, title, preview, created_at, updated_at, recency_at, archived
            FROM threads
            WHERE archived = 0
              AND (? IS NULL OR workspace_root = ?)
              AND (
                    ? IS NULL
                    OR recency_at < ?
                    OR (recency_at = ? AND thread_id < ?)
              )
            ORDER BY recency_at DESC, thread_id DESC
            LIMIT ?
            "#,
        )
        .bind(workspace_root)
        .bind(workspace_root)
        .bind(cursor_at.as_deref())
        .bind(cursor_at.as_deref())
        .bind(cursor_at.as_deref())
        .bind(cursor_thread.as_deref())
        .bind(i64::from(limit))
        .fetch_all(&self.pool)
        .await?;

        rows.into_iter().map(thread_from_row).collect()
    }

    pub async fn thread_window(
        &self,
        workspace_root: Option<&str>,
        anchor: &ThreadRecord,
        direction: SessionRangeDirection,
        count: u32,
    ) -> StoreResult<Vec<ThreadRecord>> {
        if count <= 1 || direction == SessionRangeDirection::Single {
            return Ok(vec![anchor.clone()]);
        }
        let adjacent_count = count.saturating_sub(1);
        let anchor_at = anchor.recency_at.to_rfc3339();
        let anchor_thread = anchor.thread_id.to_string();
        let (comparison, order) = match direction {
            SessionRangeDirection::Newer => (">", "ASC"),
            SessionRangeDirection::Older => ("<", "DESC"),
            SessionRangeDirection::Single => unreachable!("single returned above"),
        };
        let sql = format!(
            r#"
            SELECT thread_id, session_id, parent_thread_id, forked_from_turn_id,
                   forked_from_sequence_no, workspace_root, rebound_from_workspace_root,
                   rollout_path, title, preview, created_at, updated_at, recency_at, archived
            FROM threads
            WHERE archived = 0
              AND (? IS NULL OR workspace_root = ?)
              AND (
                    recency_at {comparison} ?
                    OR (recency_at = ? AND thread_id {comparison} ?)
              )
            ORDER BY recency_at {order}, thread_id {order}
            LIMIT ?
            "#
        );
        let rows = sqlx::query(&sql)
            .bind(workspace_root)
            .bind(workspace_root)
            .bind(&anchor_at)
            .bind(&anchor_at)
            .bind(&anchor_thread)
            .bind(i64::from(adjacent_count))
            .fetch_all(&self.pool)
            .await?;
        let mut adjacent = rows
            .into_iter()
            .map(thread_from_row)
            .collect::<StoreResult<Vec<_>>>()?;
        match direction {
            SessionRangeDirection::Newer => {
                adjacent.reverse();
                adjacent.push(anchor.clone());
                Ok(adjacent)
            }
            SessionRangeDirection::Older => {
                let mut records = Vec::with_capacity(adjacent.len().saturating_add(1));
                records.push(anchor.clone());
                records.extend(adjacent);
                Ok(records)
            }
            SessionRangeDirection::Single => unreachable!("single returned above"),
        }
    }

    pub async fn load_evidence_records(
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

    pub async fn load_evidence_by_ids(
        &self,
        evidence_ids: &HashSet<golutra_core::EvidenceId>,
    ) -> StoreResult<Vec<EvidenceRecord>> {
        if evidence_ids.is_empty() {
            return Ok(Vec::new());
        }
        let rows = sqlx::query("SELECT evidence_json FROM evidence_records")
            .fetch_all(&self.pool)
            .await?;
        let records = rows
            .into_iter()
            .map(|row| {
                let evidence_json: String = row.try_get("evidence_json")?;
                Ok(serde_json::from_str::<EvidenceRecord>(&evidence_json)?)
            })
            .collect::<StoreResult<Vec<_>>>()?;
        Ok(records
            .into_iter()
            .filter(|record| evidence_ids.contains(&record.evidence_id))
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
        status TEXT NOT NULL,
        created_at TEXT NOT NULL,
        updated_at TEXT NOT NULL
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
        artifact_json TEXT NOT NULL,
        created_at TEXT,
        retention_policy TEXT,
        size_bytes INTEGER,
        expires_at TEXT,
        blob_deleted_at TEXT
    )
    "#,
    r#"
    CREATE INDEX IF NOT EXISTS idx_artifact_records_content
    ON artifact_records (checksum, size_bytes, blob_deleted_at)
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
    r#"
    CREATE TABLE IF NOT EXISTS context_snapshots (
        snapshot_id TEXT PRIMARY KEY,
        session_id TEXT NOT NULL,
        task_id TEXT NOT NULL,
        turn_id TEXT NOT NULL,
        created_at TEXT NOT NULL,
        snapshot_json TEXT NOT NULL
    )
    "#,
    r#"
    CREATE INDEX IF NOT EXISTS idx_context_snapshots_task_created
    ON context_snapshots (task_id, created_at ASC)
    "#,
    r#"
    CREATE TABLE IF NOT EXISTS verification_plans (
        plan_id TEXT PRIMARY KEY,
        task_id TEXT NOT NULL,
        revision INTEGER NOT NULL,
        created_at TEXT NOT NULL,
        plan_json TEXT NOT NULL
    )
    "#,
    r#"
    CREATE UNIQUE INDEX IF NOT EXISTS idx_verification_plans_task_revision
    ON verification_plans (task_id, revision)
    "#,
    r#"
    CREATE TABLE IF NOT EXISTS post_task_jobs (
        job_id TEXT PRIMARY KEY,
        kind TEXT NOT NULL,
        workspace_id TEXT NOT NULL,
        session_id TEXT NOT NULL,
        task_id TEXT NOT NULL,
        input_refs_json TEXT NOT NULL,
        status TEXT NOT NULL,
        attempt INTEGER NOT NULL,
        max_attempts INTEGER NOT NULL,
        lease_owner TEXT,
        lease_expires_at TEXT,
        result_refs_json TEXT NOT NULL,
        last_error TEXT,
        created_at TEXT NOT NULL,
        started_at TEXT,
        completed_at TEXT
    )
    "#,
    r#"
    CREATE INDEX IF NOT EXISTS idx_post_task_jobs_status_created
    ON post_task_jobs (status, created_at ASC)
    "#,
    r#"
    CREATE INDEX IF NOT EXISTS idx_post_task_jobs_task_created
    ON post_task_jobs (task_id, created_at ASC)
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

fn artifact_expiration(artifact: &ArtifactRecord) -> Option<chrono::DateTime<chrono::Utc>> {
    let retention_days = match artifact.retention_policy.as_str() {
        "debug_default" => DEBUG_ARTIFACT_RETENTION_DAYS,
        "restore_only_owner_access" => CHECKPOINT_ARTIFACT_RETENTION_DAYS,
        "ephemeral" => EPHEMERAL_ARTIFACT_RETENTION_DAYS,
        _ => return None,
    };
    artifact
        .created_at
        .checked_add_signed(chrono::Duration::days(retention_days))
}

fn non_negative_database_count(row: &sqlx::sqlite::SqliteRow, column: &str) -> StoreResult<u64> {
    let value: i64 = row.try_get(column)?;
    u64::try_from(value).map_err(|_| {
        StoreError::InvalidId(format!("database aggregate `{column}` cannot be negative"))
    })
}

fn enum_json<T: serde::Serialize>(value: T) -> StoreResult<String> {
    let value = serde_json::to_value(value)?;
    value.as_str().map(ToOwned::to_owned).ok_or_else(|| {
        StoreError::Json(serde_json::Error::io(std::io::Error::other(
            "enum is not a string",
        )))
    })
}

fn parse_timestamp(value: Option<String>) -> StoreResult<Option<chrono::DateTime<chrono::Utc>>> {
    value
        .map(|value| {
            chrono::DateTime::parse_from_rfc3339(&value)
                .map(|parsed| parsed.with_timezone(&chrono::Utc))
                .map_err(|error| StoreError::InvalidId(error.to_string()))
        })
        .transpose()
}

fn collect_structured_artifact_refs(
    value: &serde_json::Value,
    key: Option<&str>,
    artifact_ids: &mut HashSet<ArtifactId>,
) {
    match value {
        serde_json::Value::Object(object) => {
            for (key, value) in object {
                collect_structured_artifact_refs(value, Some(key), artifact_ids);
            }
        }
        serde_json::Value::Array(values) => {
            for value in values {
                collect_structured_artifact_refs(value, key, artifact_ids);
            }
        }
        serde_json::Value::String(value)
            if key.is_some_and(|key| {
                key == "artifact" || key == "uri" || key.ends_with("_ref") || key.ends_with("_refs")
            }) =>
        {
            if let Some(artifact_id) = parse_structured_artifact_ref(
                value,
                key.is_some_and(|key| key.contains("artifact")),
            ) {
                artifact_ids.insert(artifact_id);
            }
        }
        _ => {}
    }
}

fn parse_structured_artifact_ref(value: &str, allow_plain_id: bool) -> Option<ArtifactId> {
    let candidate = match value.strip_prefix("artifact://") {
        Some(path) => path.split('?').next()?.rsplit('/').next()?,
        None if allow_plain_id => value,
        None => return None,
    };
    candidate.parse::<ArtifactId>().ok()
}

fn parse_required_timestamp(value: String) -> StoreResult<chrono::DateTime<chrono::Utc>> {
    parse_timestamp(Some(value))?
        .ok_or_else(|| StoreError::InvalidId("required timestamp was empty".to_owned()))
}

fn post_task_job_from_row(row: sqlx::sqlite::SqliteRow) -> StoreResult<PostTaskJob> {
    let job_id_text: String = row.try_get("job_id")?;
    let task_id_text: String = row.try_get("task_id")?;
    let kind_text: String = row.try_get("kind")?;
    let status_text: String = row.try_get("status")?;
    let input_refs_json: String = row.try_get("input_refs_json")?;
    let result_refs_json: String = row.try_get("result_refs_json")?;
    let attempt: i64 = row.try_get("attempt")?;
    let max_attempts: i64 = row.try_get("max_attempts")?;
    let parse_enum = |value: String| {
        serde_json::from_value::<PostTaskJobKind>(serde_json::Value::String(value.clone()))
            .map_err(|error| StoreError::InvalidId(error.to_string()))
    };
    let parse_status = |value: String| {
        serde_json::from_value::<PostTaskJobStatus>(serde_json::Value::String(value.clone()))
            .map_err(|error| StoreError::InvalidId(error.to_string()))
    };
    Ok(PostTaskJob {
        job_id: job_id_text
            .parse()
            .map_err(|error: uuid::Error| StoreError::InvalidId(error.to_string()))?,
        kind: parse_enum(kind_text)?,
        workspace_id: row.try_get("workspace_id")?,
        session_id: row.try_get("session_id")?,
        task_id: task_id_text
            .parse()
            .map_err(|error: uuid::Error| StoreError::InvalidId(error.to_string()))?,
        input_refs: serde_json::from_str(&input_refs_json)?,
        status: parse_status(status_text)?,
        attempt: u32::try_from(attempt)
            .map_err(|_| StoreError::InvalidId("negative job attempt".to_owned()))?,
        max_attempts: u32::try_from(max_attempts)
            .map_err(|_| StoreError::InvalidId("negative job max_attempts".to_owned()))?,
        lease_owner: row.try_get("lease_owner")?,
        lease_expires_at: parse_timestamp(row.try_get("lease_expires_at")?)?,
        result_refs: serde_json::from_str(&result_refs_json)?,
        last_error: row.try_get("last_error")?,
        created_at: parse_required_timestamp(row.try_get("created_at")?)?,
        started_at: parse_timestamp(row.try_get("started_at")?)?,
        completed_at: parse_timestamp(row.try_get("completed_at")?)?,
    })
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
