//! Durable background facts and wake admission. Notifications are only scheduling hints.

use chrono::{Duration, Utc};
use golutra_agent_core::{
    ArtifactId, BackgroundBatch, BackgroundBatchId, BackgroundResult, BackgroundResultId,
    BackgroundTaskId, BackgroundTaskRecord, BackgroundTaskStatus, BackgroundWakeClaim, EventId,
    MAX_BACKGROUND_BATCH_TASKS, MAX_BACKGROUND_PENDING_RESULTS, MAX_BACKGROUND_RESULT_BYTES,
    MAX_BACKGROUND_SUMMARY_BYTES, SessionId, TaskId, Timestamp, ToolCallId, ToolResultEnvelope,
    ToolResultStatus, TurnId, WorkspaceId,
};
use golutra_agent_protocol::{RuntimeEvent, RuntimeEventType};
use sqlx::{Row, Sqlite, Transaction, sqlite::SqliteRow};

use crate::{RuntimeStore, StoreError, StoreResult};

pub(crate) const SCHEMA: &[&str] = &[
    "CREATE TABLE background_batches (
        batch_id TEXT PRIMARY KEY, workspace_id TEXT NOT NULL, session_id TEXT NOT NULL,
        parent_task_id TEXT NOT NULL, turn_id TEXT NOT NULL, generation INTEGER NOT NULL,
        created_at TEXT NOT NULL, deadline_at TEXT NOT NULL, sealed INTEGER NOT NULL,
        suppressed INTEGER NOT NULL DEFAULT 0, wake_state TEXT NOT NULL DEFAULT 'pending',
        wake_owner TEXT, wake_expires_at TEXT, batch_json TEXT NOT NULL
    )",
    "CREATE INDEX idx_background_batches_session ON background_batches (workspace_id, session_id, created_at)",
    "CREATE TABLE background_tasks (
        background_task_id TEXT PRIMARY KEY, batch_id TEXT NOT NULL,
        workspace_id TEXT NOT NULL, session_id TEXT NOT NULL, parent_task_id TEXT NOT NULL,
        tool_call_id TEXT NOT NULL, status TEXT NOT NULL, record_json TEXT NOT NULL,
        UNIQUE(workspace_id, session_id, tool_call_id)
    )",
    "CREATE INDEX idx_background_tasks_batch ON background_tasks (batch_id, status)",
    "CREATE TABLE background_results (
        result_id TEXT PRIMARY KEY, background_task_id TEXT NOT NULL UNIQUE,
        batch_id TEXT NOT NULL, workspace_id TEXT NOT NULL, session_id TEXT NOT NULL,
        created_at TEXT NOT NULL, result_json TEXT NOT NULL, delivered_turn_id TEXT,
        delivered_at TEXT
    )",
    "CREATE INDEX idx_background_results_pending ON background_results (workspace_id, session_id, delivered_at, created_at)",
];

#[derive(Debug, Clone)]
pub struct BackgroundRepository {
    store: RuntimeStore,
}

impl RuntimeStore {
    #[must_use]
    pub fn background(&self) -> BackgroundRepository {
        BackgroundRepository {
            store: self.clone(),
        }
    }
}

fn invalid(message: impl Into<String>) -> StoreError {
    StoreError::InvalidBackgroundOperation(message.into())
}

fn validate_batch(batch: &BackgroundBatch) -> StoreResult<()> {
    if batch.generation > i64::MAX as u64 || batch.deadline_at <= batch.created_at {
        return Err(invalid("batch generation or deadline is invalid"));
    }
    Ok(())
}

fn decode_batch(row: &SqliteRow) -> StoreResult<BackgroundBatch> {
    let mut batch: BackgroundBatch = serde_json::from_str(row.try_get("batch_json")?)?;
    batch.sealed = row.try_get::<i64, _>("sealed")? != 0;
    Ok(batch)
}

fn decode_task(row: &SqliteRow) -> StoreResult<BackgroundTaskRecord> {
    let mut task: BackgroundTaskRecord = serde_json::from_str(row.try_get("record_json")?)?;
    task.status = serde_json::from_value(serde_json::Value::String(row.try_get("status")?))?;
    Ok(task)
}

pub(super) async fn lock_batch(
    transaction: &mut Transaction<'_, Sqlite>,
    batch_id: BackgroundBatchId,
) -> StoreResult<()> {
    // Acquire the SQLite writer before reading admission state. Two deferred readers must
    // not both decide to admit and then race while upgrading their transactions.
    sqlx::query("UPDATE background_batches SET sealed=sealed WHERE batch_id=?")
        .bind(batch_id.to_string())
        .execute(&mut **transaction)
        .await?;
    Ok(())
}

impl BackgroundRepository {
    /// Create an open finite batch. A sealed empty batch is allowed but never wakes.
    pub async fn create_batch(&self, batch: &BackgroundBatch) -> StoreResult<bool> {
        validate_batch(batch)?;
        let json = serde_json::to_string(batch)?;
        let inserted = sqlx::query("INSERT OR IGNORE INTO background_batches
            (batch_id,workspace_id,session_id,parent_task_id,turn_id,generation,created_at,deadline_at,sealed,batch_json)
            VALUES (?,?,?,?,?,?,?,?,?,?)")
            .bind(batch.batch_id.to_string()).bind(batch.scope.workspace_id.to_string())
            .bind(batch.scope.session_id.to_string()).bind(batch.scope.task_id.to_string())
            .bind(batch.scope.turn_id.to_string()).bind(batch.generation as i64)
            .bind(batch.created_at.to_rfc3339()).bind(batch.deadline_at.to_rfc3339())
            .bind(batch.sealed).bind(&json).execute(&self.store.pool).await?;
        if inserted.rows_affected() == 0 {
            let existing: String =
                sqlx::query_scalar("SELECT batch_json FROM background_batches WHERE batch_id=?")
                    .bind(batch.batch_id.to_string())
                    .fetch_one(&self.store.pool)
                    .await?;
            if existing != json {
                return Err(invalid("batch ID reused with different input"));
            }
        }
        Ok(inserted.rows_affected() != 0)
    }

    pub async fn batch(&self, batch_id: BackgroundBatchId) -> StoreResult<Option<BackgroundBatch>> {
        sqlx::query("SELECT batch_json,sealed FROM background_batches WHERE batch_id=?")
            .bind(batch_id.to_string())
            .fetch_optional(&self.store.pool)
            .await?
            .as_ref()
            .map(decode_batch)
            .transpose()
    }

    pub async fn open_batch(
        &self,
        workspace_id: WorkspaceId,
        session_id: SessionId,
        task_id: TaskId,
        turn_id: TurnId,
        generation: u64,
    ) -> StoreResult<Option<BackgroundBatch>> {
        if generation > i64::MAX as u64 {
            return Err(invalid("generation is out of range"));
        }
        sqlx::query("SELECT batch_json,sealed FROM background_batches WHERE workspace_id=? AND session_id=?
            AND parent_task_id=? AND turn_id=? AND generation=? AND sealed=0 AND suppressed=0 ORDER BY created_at DESC LIMIT 1")
            .bind(workspace_id.to_string()).bind(session_id.to_string()).bind(task_id.to_string())
            .bind(turn_id.to_string()).bind(generation as i64).fetch_optional(&self.store.pool).await?
            .as_ref().map(decode_batch).transpose()
    }

    pub async fn session_batches(
        &self,
        workspace_id: WorkspaceId,
        session_id: SessionId,
        limit: usize,
    ) -> StoreResult<Vec<BackgroundBatch>> {
        // Recovery must not lose an older unfinished batch behind recent delivered history.
        sqlx::query(
            "SELECT b.batch_json,b.sealed FROM background_batches b WHERE b.workspace_id=? AND b.session_id=?
            ORDER BY EXISTS(SELECT 1 FROM background_tasks t WHERE t.batch_id=b.batch_id AND t.status IN ('queued','running')) DESC,
            b.created_at DESC,b.batch_id DESC LIMIT ?",
        )
        .bind(workspace_id.to_string())
        .bind(session_id.to_string())
        .bind(limit.min(MAX_BACKGROUND_PENDING_RESULTS) as i64)
        .fetch_all(&self.store.pool)
        .await?
        .iter()
        .map(decode_batch)
        .collect()
    }

    pub async fn register_task(&self, task: &BackgroundTaskRecord) -> StoreResult<bool> {
        if task.status != BackgroundTaskStatus::Queued
            || task.tool_name.is_empty()
            || task.tool_name.len() > 256
            || task.backend.len() > 128
            || task.deadline_at <= task.created_at
        {
            return Err(invalid("new background task has invalid state or bounds"));
        }
        let json = serde_json::to_string(task)?;
        let mut tx = self.store.pool.begin().await?;
        lock_batch(&mut tx, task.batch_id).await?;
        if let Some(existing) = sqlx::query_scalar::<_, String>(
            "SELECT record_json FROM background_tasks WHERE background_task_id=?",
        )
        .bind(task.background_task_id.to_string())
        .fetch_optional(&mut *tx)
        .await?
        {
            if existing != json {
                return Err(invalid("background task ID reused with different input"));
            }
            tx.commit().await?;
            return Ok(false);
        }
        let row = sqlx::query(
            "SELECT batch_json,sealed,suppressed FROM background_batches WHERE batch_id=?",
        )
        .bind(task.batch_id.to_string())
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| invalid("unknown background batch"))?;
        let batch = decode_batch(&row)?;
        if batch.sealed
            || row.try_get::<i64, _>("suppressed")? != 0
            || batch.scope != task.scope
            || task.deadline_at > batch.deadline_at
        {
            return Err(invalid(
                "batch is closed, scope mismatched, or deadline exceeded",
            ));
        }
        let count: i64 =
            sqlx::query_scalar("SELECT count(*) FROM background_tasks WHERE batch_id=?")
                .bind(task.batch_id.to_string())
                .fetch_one(&mut *tx)
                .await?;
        if count >= MAX_BACKGROUND_BATCH_TASKS as i64 {
            return Err(invalid("background batch capacity exceeded"));
        }
        let active: i64 = sqlx::query_scalar("SELECT count(*) FROM background_tasks t LEFT JOIN background_results r
            ON r.background_task_id=t.background_task_id WHERE t.workspace_id=? AND t.session_id=? AND r.delivered_at IS NULL")
            .bind(task.scope.workspace_id.to_string()).bind(task.scope.session_id.to_string()).fetch_one(&mut *tx).await?;
        if active >= MAX_BACKGROUND_PENDING_RESULTS as i64 {
            return Err(invalid("background pending capacity exceeded"));
        }
        sqlx::query("INSERT INTO background_tasks
            (background_task_id,batch_id,workspace_id,session_id,parent_task_id,tool_call_id,status,record_json) VALUES (?,?,?,?,?,?,?,?)")
            .bind(task.background_task_id.to_string()).bind(task.batch_id.to_string())
            .bind(task.scope.workspace_id.to_string()).bind(task.scope.session_id.to_string())
            .bind(task.scope.task_id.to_string()).bind(task.tool_call_id.to_string())
            .bind(task.status.as_str()).bind(json).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(true)
    }

    pub async fn seal_batch(&self, batch_id: BackgroundBatchId) -> StoreResult<bool> {
        Ok(
            sqlx::query("UPDATE background_batches SET sealed=1 WHERE batch_id=? AND sealed=0")
                .bind(batch_id.to_string())
                .execute(&self.store.pool)
                .await?
                .rows_affected()
                != 0,
        )
    }

    pub async fn tasks(
        &self,
        batch_id: BackgroundBatchId,
    ) -> StoreResult<Vec<BackgroundTaskRecord>> {
        sqlx::query(
            "SELECT record_json,status FROM background_tasks WHERE batch_id=? ORDER BY rowid",
        )
        .bind(batch_id.to_string())
        .fetch_all(&self.store.pool)
        .await?
        .iter()
        .map(decode_task)
        .collect()
    }

    pub async fn find_task_by_tool_call(
        &self,
        workspace_id: WorkspaceId,
        session_id: SessionId,
        call_id: ToolCallId,
    ) -> StoreResult<Option<BackgroundTaskRecord>> {
        sqlx::query("SELECT record_json,status FROM background_tasks WHERE workspace_id=? AND session_id=? AND tool_call_id=?")
            .bind(workspace_id.to_string()).bind(session_id.to_string()).bind(call_id.to_string())
            .fetch_optional(&self.store.pool).await?.as_ref().map(decode_task).transpose()
    }

    pub async fn start_task(&self, id: BackgroundTaskId) -> StoreResult<bool> {
        self.set_task_status(id, BackgroundTaskStatus::Running)
            .await
    }

    /// Terminal states require `finish_task`, so no task can finish without a result.
    pub async fn set_task_status(
        &self,
        id: BackgroundTaskId,
        status: BackgroundTaskStatus,
    ) -> StoreResult<bool> {
        if !matches!(
            status,
            BackgroundTaskStatus::Running | BackgroundTaskStatus::WaitingForUser
        ) {
            return Err(invalid(
                "nonterminal transition must be running or waiting_for_user",
            ));
        }
        Ok(sqlx::query("UPDATE background_tasks SET status=? WHERE background_task_id=?
            AND status IN ('queued','running','waiting_for_user') AND status<>?
            AND EXISTS (SELECT 1 FROM background_batches b WHERE b.batch_id=background_tasks.batch_id AND b.suppressed=0)
            AND NOT EXISTS (SELECT 1 FROM background_results r WHERE r.background_task_id=background_tasks.background_task_id)")
            .bind(status.as_str()).bind(id.to_string()).bind(status.as_str())
            .execute(&self.store.pool).await?.rows_affected()!=0)
    }

    /// Atomically settle or hand off an operation and publish its pending result.
    /// The first observation wins; a user-mediated resumption starts a new operation.
    pub async fn finish_task(&self, result: &BackgroundResult) -> StoreResult<bool> {
        validate_result(result)?;
        let repository = self.clone();
        let result = result.clone();
        // Once an external operation has completed, cancellation of its caller must
        // not discard the only result. The bounded commit outlives that caller.
        crate::await_store_operation(tokio::spawn(async move {
            let mut tx = repository.store.pool.begin().await?;
            lock_batch(&mut tx, result.batch_id).await?;
            let inserted = finish_in_transaction(&mut tx, &result).await?;
            tx.commit().await?;
            Ok(inserted)
        }))
        .await
    }

    pub async fn pending_results(
        &self,
        workspace_id: WorkspaceId,
        session_id: SessionId,
        limit: usize,
    ) -> StoreResult<Vec<BackgroundResult>> {
        let rows = sqlx::query(
            "SELECT result_json FROM background_results WHERE workspace_id=? AND session_id=?
            AND delivered_at IS NULL ORDER BY created_at,result_id LIMIT ?",
        )
        .bind(workspace_id.to_string())
        .bind(session_id.to_string())
        .bind(limit.min(MAX_BACKGROUND_PENDING_RESULTS) as i64)
        .fetch_all(&self.store.pool)
        .await?;
        rows.into_iter()
            .map(|row| Ok(serde_json::from_str(row.try_get("result_json")?)?))
            .collect()
    }

    pub async fn result_for_task(
        &self,
        task_id: BackgroundTaskId,
    ) -> StoreResult<Option<BackgroundResult>> {
        let json: Option<String> = sqlx::query_scalar(
            "SELECT result_json FROM background_results WHERE background_task_id=?",
        )
        .bind(task_id.to_string())
        .fetch_optional(&self.store.pool)
        .await?;
        json.map(|json| Ok(serde_json::from_str(&json)?))
            .transpose()
    }

    /// Resolve a prior observation without crossing workspace or session ownership.
    pub async fn result_by_id(
        &self,
        workspace_id: WorkspaceId,
        session_id: SessionId,
        result_id: BackgroundResultId,
    ) -> StoreResult<Option<BackgroundResult>> {
        let json: Option<String> = sqlx::query_scalar(
            "SELECT result_json FROM background_results WHERE result_id=? AND workspace_id=? AND session_id=?",
        )
        .bind(result_id.to_string())
        .bind(workspace_id.to_string())
        .bind(session_id.to_string())
        .fetch_optional(&self.store.pool)
        .await?;
        json.map(|json| Ok(serde_json::from_str(&json)?))
            .transpose()
    }

    /// Call only after the matching observation is durable in conversation history.
    /// A provider failure leaves that history available; it must not replay the operation.
    pub async fn acknowledge_results(
        &self,
        workspace_id: WorkspaceId,
        session_id: SessionId,
        ids: &[BackgroundResultId],
        turn_id: TurnId,
    ) -> StoreResult<u64> {
        if ids.len() > MAX_BACKGROUND_PENDING_RESULTS {
            return Err(invalid("too many delivery acknowledgements"));
        }
        let mut tx = self.store.pool.begin().await?;
        let mut count = 0;
        let now = Utc::now().to_rfc3339();
        for id in ids {
            count += sqlx::query(
                "UPDATE background_results SET delivered_at=?,delivered_turn_id=?
                WHERE result_id=? AND workspace_id=? AND session_id=? AND delivered_at IS NULL",
            )
            .bind(&now)
            .bind(turn_id.to_string())
            .bind(id.to_string())
            .bind(workspace_id.to_string())
            .bind(session_id.to_string())
            .execute(&mut *tx)
            .await?
            .rows_affected();
        }
        tx.commit().await?;
        Ok(count)
    }

    /// Reserve one sealed and settled batch. A reservation does not consume any result.
    pub async fn claim_wake(
        &self,
        workspace_id: WorkspaceId,
        session_id: SessionId,
        generation: u64,
        owner: &str,
        now: Timestamp,
        lease_duration: Duration,
    ) -> StoreResult<Option<BackgroundWakeClaim>> {
        self.claim_wake_inner(
            workspace_id,
            session_id,
            generation,
            owner,
            now,
            lease_duration,
            None,
        )
        .await
    }

    pub async fn claim_batch_wake(
        &self,
        batch_id: BackgroundBatchId,
        generation: u64,
        owner: &str,
        now: Timestamp,
        lease_duration: Duration,
    ) -> StoreResult<Option<BackgroundWakeClaim>> {
        let Some(batch) = self.batch(batch_id).await? else {
            return Ok(None);
        };
        self.claim_wake_inner(
            batch.scope.workspace_id,
            batch.scope.session_id,
            generation,
            owner,
            now,
            lease_duration,
            Some(batch_id),
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn claim_wake_inner(
        &self,
        workspace_id: WorkspaceId,
        session_id: SessionId,
        generation: u64,
        owner: &str,
        now: Timestamp,
        lease_duration: Duration,
        target_batch: Option<BackgroundBatchId>,
    ) -> StoreResult<Option<BackgroundWakeClaim>> {
        if owner.is_empty()
            || owner.len() > 256
            || generation > i64::MAX as u64
            || lease_duration <= Duration::zero()
            || lease_duration > Duration::minutes(5)
        {
            return Err(invalid("invalid wake lease"));
        }
        let expires = now
            .checked_add_signed(lease_duration)
            .ok_or_else(|| invalid("wake lease expiry is out of range"))?;
        let mut tx = self.store.pool.begin().await?;
        // This first write also serializes competing wake claims across connections.
        sqlx::query("UPDATE background_batches SET wake_state='pending',wake_owner=NULL,wake_expires_at=NULL
            WHERE workspace_id=? AND session_id=? AND wake_state='claimed' AND wake_expires_at<=?")
            .bind(workspace_id.to_string()).bind(session_id.to_string()).bind(now.to_rfc3339()).execute(&mut *tx).await?;
        let row = sqlx::query("SELECT b.batch_id,b.batch_json,b.sealed FROM background_batches b
            WHERE b.workspace_id=? AND b.session_id=? AND b.generation=? AND b.sealed=1 AND b.suppressed=0 AND b.wake_state='pending'
            AND (? IS NULL OR b.batch_id=?)
            AND NOT EXISTS (SELECT 1 FROM background_batches c WHERE c.workspace_id=b.workspace_id AND c.session_id=b.session_id AND c.wake_state='claimed')
            AND EXISTS (SELECT 1 FROM background_tasks t WHERE t.batch_id=b.batch_id)
            AND NOT EXISTS (SELECT 1 FROM background_tasks t WHERE t.batch_id=b.batch_id AND t.status IN ('queued','running','waiting_for_user'))
            AND EXISTS (SELECT 1 FROM background_results r WHERE r.batch_id=b.batch_id AND r.delivered_at IS NULL)
            ORDER BY b.created_at,b.batch_id LIMIT 1")
            .bind(workspace_id.to_string()).bind(session_id.to_string()).bind(generation as i64)
            .bind(target_batch.map(|id|id.to_string())).bind(target_batch.map(|id|id.to_string()))
            .fetch_optional(&mut *tx).await?;
        let Some(row) = row else {
            tx.commit().await?;
            return Ok(None);
        };
        let batch = decode_batch(&row)?;
        sqlx::query("UPDATE background_batches SET wake_state='claimed',wake_owner=?,wake_expires_at=? WHERE batch_id=?")
            .bind(owner).bind(expires.to_rfc3339()).bind(batch.batch_id.to_string()).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(Some(BackgroundWakeClaim {
            batch,
            owner: owner.to_owned(),
            lease_expires_at: expires,
        }))
    }

    pub async fn commit_wake(&self, claim: &BackgroundWakeClaim) -> StoreResult<bool> {
        Ok(sqlx::query("UPDATE background_batches SET wake_state='committed',wake_owner=NULL,wake_expires_at=NULL
            WHERE batch_id=? AND wake_state='claimed' AND wake_owner=? AND wake_expires_at=? AND suppressed=0 AND wake_expires_at>?
            AND EXISTS (SELECT 1 FROM background_results r WHERE r.batch_id=background_batches.batch_id AND r.delivered_at IS NULL)")
            .bind(claim.batch.batch_id.to_string()).bind(&claim.owner).bind(claim.lease_expires_at.to_rfc3339())
            .bind(Utc::now().to_rfc3339())
            .execute(&self.store.pool).await?.rows_affected()!=0)
    }

    pub async fn release_wake(&self, claim: &BackgroundWakeClaim) -> StoreResult<bool> {
        Ok(sqlx::query("UPDATE background_batches SET wake_state='pending',wake_owner=NULL,wake_expires_at=NULL
            WHERE batch_id=? AND wake_state='claimed' AND wake_owner=? AND wake_expires_at=?")
            .bind(claim.batch.batch_id.to_string()).bind(&claim.owner).bind(claim.lease_expires_at.to_rfc3339())
            .execute(&self.store.pool).await?.rows_affected()!=0)
    }

    pub async fn suppress_session(
        &self,
        workspace_id: WorkspaceId,
        session_id: SessionId,
    ) -> StoreResult<u64> {
        Ok(sqlx::query("UPDATE background_batches SET sealed=1,suppressed=1,wake_owner=NULL,wake_expires_at=NULL,wake_state='suppressed'
            WHERE workspace_id=? AND session_id=? AND suppressed=0")
            .bind(workspace_id.to_string()).bind(session_id.to_string()).execute(&self.store.pool).await?.rows_affected())
    }

    /// Revoke one batch without suppressing later work admitted in the same session.
    pub async fn suppress_batch(
        &self,
        workspace_id: WorkspaceId,
        session_id: SessionId,
        batch_id: BackgroundBatchId,
    ) -> StoreResult<bool> {
        Ok(sqlx::query("UPDATE background_batches SET sealed=1,suppressed=1,wake_owner=NULL,wake_expires_at=NULL,wake_state='suppressed'
            WHERE workspace_id=? AND session_id=? AND batch_id=? AND suppressed=0")
            .bind(workspace_id.to_string()).bind(session_id.to_string()).bind(batch_id.to_string())
            .execute(&self.store.pool).await?.rows_affected()!=0)
    }

    /// The host must identify orphaned parent tasks; never assume every workspace task
    /// belongs to this process. Recovery records uncertainty and suppresses auto-wake.
    pub async fn recover_abandoned(
        &self,
        workspace_id: WorkspaceId,
        parent_tasks: &[TaskId],
    ) -> StoreResult<u64> {
        let mut tx = self.store.pool.begin().await?;
        let mut count = 0;
        for task_id in parent_tasks {
            sqlx::query("UPDATE background_batches SET sealed=1,suppressed=1,wake_state='suppressed',wake_owner=NULL,wake_expires_at=NULL
                WHERE workspace_id=? AND parent_task_id=?")
                .bind(workspace_id.to_string()).bind(task_id.to_string()).execute(&mut *tx).await?;
            let rows = sqlx::query("SELECT record_json,status FROM background_tasks WHERE workspace_id=? AND parent_task_id=?
                AND status IN ('queued','running')")
                .bind(workspace_id.to_string()).bind(task_id.to_string()).fetch_all(&mut *tx).await?;
            for row in rows {
                let task = decode_task(&row)?;
                let result = abandoned_result(&task);
                count += u64::from(finish_in_transaction(&mut tx, &result).await?);
            }
        }
        tx.commit().await?;
        Ok(count)
    }
}

fn validate_result(result: &BackgroundResult) -> StoreResult<()> {
    if (!result.status.is_terminal() && result.status != BackgroundTaskStatus::WaitingForUser)
        || result.summary.len() > MAX_BACKGROUND_SUMMARY_BYTES
        || result.output.summary.len() > MAX_BACKGROUND_SUMMARY_BYTES
        || result.output.tool_call_id != result.tool_call_id
        || result.output.tool_name != result.tool_name
        || result.artifact_refs.len() > 64
        || result
            .artifact_refs
            .iter()
            .any(|reference| reference.len() > 2048)
        || serde_json::to_vec(result)?.len() > MAX_BACKGROUND_RESULT_BYTES
    {
        return Err(invalid(
            "background result identity, status, or size is invalid",
        ));
    }
    Ok(())
}

pub(super) fn validate_completion_bundle(
    event: &RuntimeEvent,
    result: &BackgroundResult,
) -> StoreResult<()> {
    validate_result(result)?;
    let envelope: ToolResultEnvelope = serde_json::from_value(event.payload["envelope"].clone())?;
    if event.event_type != RuntimeEventType::BackgroundTaskUpdated
        || event.id
            != EventId(uuid::Uuid::new_v5(
                &result.result_id.0,
                b"background-result-event",
            ))
        || event.session_id != result.scope.session_id
        || event.task_id != Some(result.scope.task_id)
        || event.turn_id != Some(result.scope.turn_id)
        || event.timestamp != result.created_at
        || event.payload["background_task_id"] != serde_json::to_value(result.background_task_id)?
        || event.payload["batch_id"] != serde_json::to_value(result.batch_id)?
        || event.payload["result_id"] != serde_json::to_value(result.result_id)?
        || event.payload["status"] != serde_json::to_value(result.status)?
        || envelope.tool_call_id != result.tool_call_id
        || envelope.tool_name != result.tool_name
        || envelope.status != result.output.status
        || envelope.raw_artifact_ref != result.output.raw_artifact_ref
    {
        return Err(invalid(
            "completion audit and background result have different identities or scope",
        ));
    }
    Ok(())
}

pub(super) async fn validate_completion_references(
    transaction: &mut Transaction<'_, Sqlite>,
    result: &BackgroundResult,
) -> StoreResult<()> {
    for reference in &result.artifact_refs {
        let artifact_id = reference
            .parse::<ArtifactId>()
            .map_err(|error| invalid(format!("invalid background artifact reference: {error}")))?;
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM artifact_records WHERE artifact_id=? AND session_id=? AND blob_deleted_at IS NULL)",
        )
        .bind(artifact_id.to_string())
        .bind(result.scope.session_id.to_string())
        .fetch_one(&mut **transaction)
        .await?;
        if !exists {
            return Err(invalid(format!(
                "background artifact {artifact_id} has no live metadata in this session"
            )));
        }
    }
    Ok(())
}

pub(super) async fn finish_in_transaction(
    tx: &mut Transaction<'_, Sqlite>,
    result: &BackgroundResult,
) -> StoreResult<bool> {
    let row =
        sqlx::query("SELECT record_json,status FROM background_tasks WHERE background_task_id=?")
            .bind(result.background_task_id.to_string())
            .fetch_optional(&mut **tx)
            .await?
            .ok_or_else(|| invalid("unknown background task"))?;
    let task = decode_task(&row)?;
    if task.scope != result.scope
        || task.batch_id != result.batch_id
        || task.tool_call_id != result.tool_call_id
        || task.tool_name != result.tool_name
    {
        return Err(invalid("result belongs to a different background task"));
    }
    if task.status.is_terminal()
        || sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM background_results WHERE background_task_id=?",
        )
        .bind(task.background_task_id.to_string())
        .fetch_one(&mut **tx)
        .await?
            != 0
    {
        return Ok(false);
    }
    sqlx::query("INSERT INTO background_results (result_id,background_task_id,batch_id,workspace_id,session_id,created_at,result_json)
        VALUES (?,?,?,?,?,?,?)")
        .bind(result.result_id.to_string()).bind(result.background_task_id.to_string()).bind(result.batch_id.to_string())
        .bind(result.scope.workspace_id.to_string()).bind(result.scope.session_id.to_string()).bind(result.created_at.to_rfc3339())
        .bind(serde_json::to_string(result)?).execute(&mut **tx).await?;
    sqlx::query("UPDATE background_tasks SET status=? WHERE background_task_id=?")
        .bind(result.status.as_str())
        .bind(result.background_task_id.to_string())
        .execute(&mut **tx)
        .await?;
    Ok(true)
}

fn abandoned_result(task: &BackgroundTaskRecord) -> BackgroundResult {
    let summary = "Execution host was lost; inspect external effects before retrying.".to_owned();
    BackgroundResult {
        result_id: BackgroundResultId::new(),
        background_task_id: task.background_task_id,
        batch_id: task.batch_id,
        scope: task.scope,
        tool_call_id: task.tool_call_id,
        tool_name: task.tool_name.clone(),
        status: BackgroundTaskStatus::OutcomeUnknown,
        summary: summary.clone(),
        artifact_refs: Vec::new(),
        created_at: Utc::now(),
        output: ToolResultEnvelope {
            tool_call_id: task.tool_call_id,
            tool_name: task.tool_name.clone(),
            status: ToolResultStatus::Error,
            summary,
            structured_facts: serde_json::json!({"status":"outcome_unknown","automatic_replay_allowed":false}),
            model_visible_excerpt: None,
            raw_artifact_ref: None,
            evidence_refs: Vec::new(),
            risk: "unknown external effects".to_owned(),
            verification_hint: Some(
                "Inspect the operation's current state before retrying.".to_owned(),
            ),
        },
    }
}

#[cfg(test)]
mod tests;
