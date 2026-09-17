//! 当前 SQLite 格式的事务初始化与严格校验；不迁移、不修复任何历史格式。
use sha2::{Digest, Sha256};
use sqlx::{Row, SqliteConnection, SqlitePool};
use tokio::time::sleep;

// 单一基线高于所有旧账本版本，旧 reader 也会拒绝打开，避免误把新库当旧库。
pub(crate) const CURRENT_VERSION: i64 = 7;
const SCHEMA_NAME: &str = "current_runtime_schema";
const EVENT_CONTRACT: &str = "runtime_event:user_step;runtime_usage_lease:v1";
const MIGRATION_LOCK_RETRIES: usize = 40;
const MIGRATION_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(25);

const CURRENT_SCHEMA: &[&str] = &[
    r#"
    CREATE TABLE runtime_events (
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
    CREATE INDEX idx_runtime_events_session_sequence
    ON runtime_events (session_id, sequence_no)
    "#,
    r#"
    CREATE INDEX idx_runtime_events_task_sequence
    ON runtime_events (task_id, sequence_no)
    "#,
    r#"
    CREATE UNIQUE INDEX idx_runtime_events_sequence_no
    ON runtime_events (sequence_no)
    "#,
    r#"
    CREATE TABLE runtime_sequence (
        singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
        last_sequence_no INTEGER NOT NULL
    )
    "#,
    r#"
    INSERT OR IGNORE INTO runtime_sequence (singleton, last_sequence_no)
    SELECT 1, COALESCE(MAX(sequence_no), 0) FROM runtime_events
    "#,
    r#"
    CREATE TABLE command_acks (
        idempotency_key TEXT PRIMARY KEY,
        command_id TEXT NOT NULL,
        ack_json TEXT NOT NULL,
        status TEXT NOT NULL DEFAULT 'completed',
        created_at TEXT NOT NULL,
        updated_at TEXT
    )
    "#,
    r#"
    CREATE TABLE sessions (
        session_id TEXT PRIMARY KEY,
        status TEXT NOT NULL,
        active_task_id TEXT,
        last_sequence_no INTEGER NOT NULL,
        created_at TEXT NOT NULL,
        updated_at TEXT NOT NULL
    )
    "#,
    r#"
    CREATE TABLE tasks (
        task_id TEXT PRIMARY KEY,
        session_id TEXT NOT NULL,
        status TEXT NOT NULL,
        last_sequence_no INTEGER NOT NULL,
        created_at TEXT NOT NULL,
        updated_at TEXT NOT NULL
    )
    "#,
    r#"
    CREATE INDEX idx_tasks_session_updated
    ON tasks (session_id, updated_at DESC)
    "#,
    r#"
    CREATE TABLE turns (
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
    CREATE INDEX idx_turns_session_updated
    ON turns (session_id, updated_at DESC)
    "#,
    r#"
    CREATE TABLE state_projections (
        session_id TEXT PRIMARY KEY,
        last_sequence_no INTEGER NOT NULL,
        projection_json TEXT NOT NULL,
        updated_at TEXT NOT NULL
    )
    "#,
    r#"
    CREATE TABLE artifact_records (
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
    CREATE TABLE evidence_records (
        evidence_id TEXT PRIMARY KEY,
        claim TEXT NOT NULL,
        evidence_json TEXT NOT NULL
    )
    "#,
    r#"
    CREATE TABLE threads (
        thread_id TEXT PRIMARY KEY,
        session_id TEXT NOT NULL,
        parent_thread_id TEXT,
        workspace_root TEXT,
        title TEXT NOT NULL,
        preview TEXT NOT NULL,
        created_at TEXT NOT NULL,
        updated_at TEXT NOT NULL,
        recency_at TEXT NOT NULL,
        archived INTEGER NOT NULL DEFAULT 0,
        forked_from_turn_id TEXT,
        forked_from_sequence_no INTEGER,
        rebound_from_workspace_root TEXT,
        rollout_path TEXT,
        removed INTEGER NOT NULL DEFAULT 0
    )
    "#,
    r#"
    CREATE INDEX idx_threads_workspace_recency
    ON threads (workspace_root, recency_at DESC)
    "#,
    r#"
    CREATE TABLE context_snapshots (
        snapshot_id TEXT PRIMARY KEY,
        session_id TEXT NOT NULL,
        task_id TEXT NOT NULL,
        turn_id TEXT NOT NULL,
        created_at TEXT NOT NULL,
        snapshot_json TEXT NOT NULL
    )
    "#,
    r#"
    CREATE INDEX idx_context_snapshots_task_created
    ON context_snapshots (task_id, created_at ASC)
    "#,
    r#"
    CREATE TABLE verification_plans (
        plan_id TEXT PRIMARY KEY,
        task_id TEXT NOT NULL,
        revision INTEGER NOT NULL,
        created_at TEXT NOT NULL,
        plan_json TEXT NOT NULL
    )
    "#,
    r#"
    CREATE UNIQUE INDEX idx_verification_plans_task_revision
    ON verification_plans (task_id, revision)
    "#,
    r#"
    CREATE TABLE post_task_jobs (
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
    CREATE INDEX idx_post_task_jobs_status_created
    ON post_task_jobs (status, created_at ASC)
    "#,
    r#"
    CREATE INDEX idx_post_task_jobs_task_created
    ON post_task_jobs (task_id, created_at ASC)
    "#,
    "CREATE UNIQUE INDEX idx_threads_session_unique ON threads (session_id)",
    "CREATE INDEX idx_artifact_records_content ON artifact_records (checksum, size_bytes, blob_deleted_at)",
    r#"
    CREATE INDEX idx_runtime_events_model_history_session_sequence
    ON runtime_events (session_id, sequence_no)
    WHERE event_type IN (
        'TaskCreated', 'TurnQueued', 'TurnUpdated', 'TurnCancelled',
        'AssistantMessage', 'ToolCompleted', 'TaskCompleted',
        'TaskAborted', 'TaskInterrupted', 'TaskUncertain',
        'CandidateReady', 'VerificationReady', 'CompactionCompleted'
    )
    "#,
    "CREATE INDEX idx_context_snapshots_session_created ON context_snapshots (session_id, created_at DESC)",
];

fn schema_checksum() -> String {
    let mut digest = Sha256::new();
    digest.update(CURRENT_VERSION.to_le_bytes());
    digest.update(SCHEMA_NAME.as_bytes());
    digest.update(EVENT_CONTRACT.as_bytes());
    for statement in CURRENT_SCHEMA {
        digest.update([0_u8]);
        digest.update(statement.as_bytes());
    }
    format!("sha256:{:x}", digest.finalize())
}

/// 仅接受当前基线的唯一账本记录；旧 checksum 和历史版本不再豁免。
pub(crate) fn validate_applied(applied: &[(i64, String, String)]) -> Result<(), String> {
    if let [(version, name, checksum)] = applied
        && *version == CURRENT_VERSION
        && name == SCHEMA_NAME
        && *checksum == schema_checksum()
    {
        return Ok(());
    }
    Err(format!(
        "data_unsupported: expected schema {CURRENT_VERSION} with its exact checksum; old or unknown formats are not migrated; use a new GOLUTRA_AGENT_HOME"
    ))
}

pub(crate) async fn run(pool: &SqlitePool) -> Result<(), String> {
    let mut connection = pool.acquire().await.map_err(|error| error.to_string())?;
    begin_immediate_with_retry(&mut connection).await?;

    let result = initialize_current(&mut connection).await;
    match result {
        Ok(()) => {
            sqlx::query("COMMIT")
                .execute(&mut *connection)
                .await
                .map_err(|error| error.to_string())?;
            Ok(())
        }
        Err(error) => {
            let _ = sqlx::query("ROLLBACK").execute(&mut *connection).await;
            Err(error)
        }
    }
}

async fn begin_immediate_with_retry(connection: &mut SqliteConnection) -> Result<(), String> {
    for attempt in 0..=MIGRATION_LOCK_RETRIES {
        match sqlx::query("BEGIN IMMEDIATE")
            .execute(&mut *connection)
            .await
        {
            Ok(_) => return Ok(()),
            Err(error) if is_sqlite_busy(&error) && attempt < MIGRATION_LOCK_RETRIES => {
                sleep(MIGRATION_RETRY_DELAY).await;
            }
            Err(error) => return Err(error.to_string()),
        }
    }
    unreachable!("migration lock retry loop always returns")
}

fn is_sqlite_busy(error: &sqlx::Error) -> bool {
    matches!(
        error,
        sqlx::Error::Database(database_error)
            if database_error
                .code()
                .is_some_and(|code| code == "5" || code == "6")
    )
}

async fn initialize_current(connection: &mut SqliteConnection) -> Result<(), String> {
    let objects: i64 =
        sqlx::query_scalar("SELECT count(*) FROM sqlite_master WHERE name NOT LIKE 'sqlite_%'")
            .fetch_one(&mut *connection)
            .await
            .map_err(|error| error.to_string())?;
    if objects != 0 {
        // 缺账本的旧库不是空库；必须在任何 DDL/业务写入之前拒绝。
        let exists: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='schema_migrations'",
        )
        .fetch_one(&mut *connection)
        .await
        .map_err(|error| error.to_string())?;
        if exists == 0 {
            return Err(
                "data_unsupported: unversioned database; use a new GOLUTRA_AGENT_HOME".to_owned(),
            );
        }
        let applied =
            sqlx::query("SELECT version, name, checksum FROM schema_migrations ORDER BY version")
                .fetch_all(&mut *connection)
                .await
                .map_err(|error| error.to_string())?
                .into_iter()
                .map(|row| {
                    Ok((
                        row.try_get("version")?,
                        row.try_get("name")?,
                        row.try_get("checksum")?,
                    ))
                })
                .collect::<Result<Vec<(i64, String, String)>, sqlx::Error>>()
                .map_err(|error| error.to_string())?;
        return validate_applied(&applied);
    }
    for statement in CURRENT_SCHEMA {
        sqlx::query(statement)
            .execute(&mut *connection)
            .await
            .map_err(|error| error.to_string())?;
    }
    sqlx::query("CREATE TABLE schema_migrations (
        version INTEGER PRIMARY KEY, name TEXT NOT NULL, checksum TEXT NOT NULL, applied_at TEXT NOT NULL
    )").execute(&mut *connection).await.map_err(|error| error.to_string())?;
    sqlx::query("INSERT INTO schema_migrations VALUES (?, ?, ?, ?)")
        .bind(CURRENT_VERSION)
        .bind(SCHEMA_NAME)
        .bind(schema_checksum())
        .bind(chrono::Utc::now().to_rfc3339())
        .execute(&mut *connection)
        .await
        .map_err(|error| error.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;

    async fn empty_pool() -> SqlitePool {
        SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn fresh_database_has_one_current_baseline_and_reopens_unchanged() {
        let pool = empty_pool().await;
        run(&pool).await.unwrap();
        let before: Vec<(i64, String, String, String)> =
            sqlx::query_as("SELECT * FROM schema_migrations")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(before.len(), 1);
        assert_eq!(before[0].0, CURRENT_VERSION);
        assert_eq!(before[0].2, schema_checksum());
        run(&pool).await.unwrap();
        let after: Vec<(i64, String, String, String)> =
            sqlx::query_as("SELECT * FROM schema_migrations")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(before, after);
        for index in [
            "idx_runtime_events_model_history_session_sequence",
            "idx_context_snapshots_session_created",
            "idx_threads_session_unique",
            "idx_artifact_records_content",
        ] {
            let exists: i64 = sqlx::query_scalar("SELECT count(*) FROM sqlite_master WHERE name=?")
                .bind(index)
                .fetch_one(&pool)
                .await
                .unwrap();
            assert_eq!(exists, 1, "{index}");
        }
    }

    #[tokio::test]
    async fn old_future_and_legacy_checksum_are_rejected_without_refresh() {
        for (version, checksum) in [
            (1, "sha256:golutra-agent-v1-base-20260808"),
            (2, "sha256:golutra-agent-v2-legacy-columns-20260808"),
            (6, "old"),
            (8, "future"),
            (CURRENT_VERSION, "wrong"),
        ] {
            let pool = empty_pool().await;
            run(&pool).await.unwrap();
            sqlx::query("UPDATE schema_migrations SET version=?, checksum=?")
                .bind(version)
                .bind(checksum)
                .execute(&pool)
                .await
                .unwrap();
            assert!(run(&pool).await.unwrap_err().contains("data_unsupported"));
            let row: (i64, String) =
                sqlx::query_as("SELECT version, checksum FROM schema_migrations")
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            assert_eq!(row, (version, checksum.to_owned()));
        }
    }

    #[tokio::test]
    async fn unversioned_and_empty_ledger_databases_are_not_initialized() {
        for ledger in [false, true] {
            let pool = empty_pool().await;
            sqlx::query("CREATE TABLE old_data (value TEXT)")
                .execute(&pool)
                .await
                .unwrap();
            sqlx::query("INSERT INTO old_data VALUES ('preserve me')")
                .execute(&pool)
                .await
                .unwrap();
            if ledger {
                sqlx::query(
                    "CREATE TABLE schema_migrations (version INTEGER, name TEXT, checksum TEXT)",
                )
                .execute(&pool)
                .await
                .unwrap();
            }
            assert!(run(&pool).await.unwrap_err().contains("data_unsupported"));
            let data: String = sqlx::query_scalar("SELECT value FROM old_data")
                .fetch_one(&pool)
                .await
                .unwrap();
            assert_eq!(data, "preserve me");
            let tables: i64 =
                sqlx::query_scalar("SELECT count(*) FROM sqlite_master WHERE type='table'")
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            assert_eq!(tables, if ledger { 2 } else { 1 });
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_openers_observe_one_complete_baseline() {
        let directory = tempfile::tempdir().unwrap();
        let mut tasks = tokio::task::JoinSet::new();
        for _ in 0..4 {
            let url = format!(
                "sqlite:{}",
                directory.path().join("runtime.sqlite").display()
            );
            tasks.spawn(async move { crate::RuntimeStore::connect(&url).await });
        }
        while let Some(result) = tasks.join_next().await {
            result.unwrap().unwrap();
        }
    }
}
