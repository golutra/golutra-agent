//! Transactional, versioned SQLite schema migrations.
//!
//! A migration is applied once, recorded with a checksum, and committed before
//! another store connection can observe the new version. Legacy databases that
//! predate the migration ledger are upgraded by the same ordered runner.

#[cfg(test)]
use std::{collections::HashSet, path::Path};

use golutra_core::ArtifactRecord;
use sha2::{Digest, Sha256};
#[cfg(test)]
use sqlx::sqlite::{SqliteJournalMode, SqlitePoolOptions};
use sqlx::{Row, SqliteConnection, SqlitePool};
use tokio::time::sleep;

use crate::artifact_expiration;

const CURRENT_VERSION: i64 = 5;
const MIGRATION_LOCK_RETRIES: usize = 40;
const MIGRATION_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(25);

const MIGRATION_1: &[&str] = &[
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
        status TEXT NOT NULL DEFAULT 'completed',
        created_at TEXT NOT NULL,
        updated_at TEXT
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

const MIGRATION_1_NAME: &str = "base_schema";
const MIGRATION_2_NAME: &str = "legacy_columns_and_indexes";
const MIGRATION_3_NAME: &str = "model_history_index";
const MIGRATION_4_NAME: &str = "model_history_facts_index";
const MIGRATION_5_NAME: &str = "context_snapshot_session_index";
const LEGACY_MIGRATION_1_CHECKSUM: &str = "sha256:golutra-v1-base-20260808";
const LEGACY_MIGRATION_2_CHECKSUM: &str = "sha256:golutra-v2-legacy-columns-20260808";
const MIGRATION_2_COLUMNS: &[(&str, &str, &str)] = &[
    ("threads", "forked_from_turn_id", "TEXT"),
    ("threads", "forked_from_sequence_no", "INTEGER"),
    ("threads", "rebound_from_workspace_root", "TEXT"),
    ("threads", "rollout_path", "TEXT"),
    ("threads", "removed", "INTEGER NOT NULL DEFAULT 0"),
    (
        "command_acks",
        "status",
        "TEXT NOT NULL DEFAULT 'completed'",
    ),
    ("command_acks", "updated_at", "TEXT"),
    ("artifact_records", "created_at", "TEXT"),
    ("artifact_records", "retention_policy", "TEXT"),
    ("artifact_records", "size_bytes", "INTEGER"),
    ("artifact_records", "expires_at", "TEXT"),
    ("artifact_records", "blob_deleted_at", "TEXT"),
];
const CREATE_THREAD_DEDUPLICATION_SQL: &str =
    "CREATE TEMP TABLE IF NOT EXISTS golutra_thread_deduplication (
         duplicate_thread_id TEXT PRIMARY KEY,
         survivor_thread_id TEXT NOT NULL
     )";
const CLEAR_THREAD_DEDUPLICATION_SQL: &str = "DELETE FROM golutra_thread_deduplication";
const POPULATE_THREAD_DEDUPLICATION_SQL: &str = "INSERT INTO golutra_thread_deduplication (
         duplicate_thread_id,
         survivor_thread_id
     )
     SELECT duplicate.thread_id, survivor.thread_id
     FROM threads AS duplicate
     JOIN threads AS survivor ON survivor.session_id = duplicate.session_id
     WHERE duplicate.thread_id <> survivor.thread_id
       AND NOT EXISTS (
           SELECT 1 FROM threads AS newer
           WHERE newer.session_id = survivor.session_id
             AND (
                 newer.recency_at > survivor.recency_at
                 OR (
                     newer.recency_at = survivor.recency_at
                     AND newer.updated_at > survivor.updated_at
                 )
                 OR (
                     newer.recency_at = survivor.recency_at
                     AND newer.updated_at = survivor.updated_at
                     AND newer.thread_id > survivor.thread_id
                 )
             )
       )";
const REPARENT_DEDUPLICATED_THREADS_SQL: &str = "UPDATE threads
     SET parent_thread_id = CASE
         WHEN thread_id = (
             SELECT survivor_thread_id
             FROM golutra_thread_deduplication
             WHERE duplicate_thread_id = threads.parent_thread_id
         ) THEN NULL
         ELSE (
             SELECT survivor_thread_id
             FROM golutra_thread_deduplication
             WHERE duplicate_thread_id = threads.parent_thread_id
         )
     END
     WHERE EXISTS (
         SELECT 1
         FROM golutra_thread_deduplication
         WHERE duplicate_thread_id = threads.parent_thread_id
     )";
const DELETE_DEDUPLICATED_THREADS_SQL: &str = "DELETE FROM threads
     WHERE thread_id IN (
         SELECT duplicate_thread_id FROM golutra_thread_deduplication
     )";
const DROP_THREAD_DEDUPLICATION_SQL: &str = "DROP TABLE golutra_thread_deduplication";
const DROP_LEGACY_THREAD_INDEX_SQL: &str = "DROP INDEX IF EXISTS idx_threads_session";
const CREATE_THREAD_SESSION_INDEX_SQL: &str =
    "CREATE UNIQUE INDEX IF NOT EXISTS idx_threads_session_unique ON threads (session_id)";
const CREATE_ARTIFACT_CONTENT_INDEX_SQL: &str =
    "CREATE INDEX IF NOT EXISTS idx_artifact_records_content
     ON artifact_records (checksum, size_bytes, blob_deleted_at)";
const SELECT_ARTIFACTS_TO_BACKFILL_SQL: &str =
    "SELECT artifact_id, artifact_json FROM artifact_records
     WHERE created_at IS NULL OR retention_policy IS NULL OR size_bytes IS NULL";
const UPDATE_ARTIFACT_METADATA_SQL: &str = "UPDATE artifact_records
     SET created_at = ?, retention_policy = ?, size_bytes = ?, expires_at = ?
     WHERE artifact_id = ?";
const ARTIFACT_BACKFILL_FORMAT_VERSION: &str = "artifact-record-json:v1";
const MIGRATION_2_SQL: &[&str] = &[
    CREATE_THREAD_DEDUPLICATION_SQL,
    CLEAR_THREAD_DEDUPLICATION_SQL,
    POPULATE_THREAD_DEDUPLICATION_SQL,
    REPARENT_DEDUPLICATED_THREADS_SQL,
    DELETE_DEDUPLICATED_THREADS_SQL,
    DROP_THREAD_DEDUPLICATION_SQL,
    DROP_LEGACY_THREAD_INDEX_SQL,
    CREATE_THREAD_SESSION_INDEX_SQL,
    CREATE_ARTIFACT_CONTENT_INDEX_SQL,
    SELECT_ARTIFACTS_TO_BACKFILL_SQL,
    UPDATE_ARTIFACT_METADATA_SQL,
];

// 查询模型历史时固定带 session、sequence 和 event_type 条件；这个部分索引
// 让增量上下文刷新只触碰可投影的事件，避免在长会话 telemetry 上做全索引扫描。
const MIGRATION_3: &[&str] = &[r#"
    CREATE INDEX IF NOT EXISTS idx_runtime_events_model_history_session_sequence
    ON runtime_events (session_id, sequence_no)
    WHERE event_type IN (
        'TaskCreated', 'TurnQueued', 'TurnUpdated', 'TurnCancelled',
        'AssistantMessage', 'CompactionCompleted'
    )
    "#];

// 工具完成和任务终态也是模型恢复所需的历史事实。单独的版本迁移替换
// 旧部分索引，确保已有数据库不会只更新 checksum 却继续使用旧索引。
const MIGRATION_4: &[&str] = &[
    "DROP INDEX IF EXISTS idx_runtime_events_model_history_session_sequence",
    r#"
    CREATE INDEX IF NOT EXISTS idx_runtime_events_model_history_session_sequence
    ON runtime_events (session_id, sequence_no)
    WHERE event_type IN (
        'TaskCreated', 'TurnQueued', 'TurnUpdated', 'TurnCancelled',
        'AssistantMessage', 'ToolCompleted', 'TaskCompleted',
        'TaskAborted', 'TaskInterrupted', 'TaskUncertain',
        'CandidateReady', 'VerificationReady', 'CompactionCompleted'
    )
    "#,
];

// 为已经完成基础迁移的数据库补齐 session 最近快照索引，避免 resume 在长会话上退化为全表扫描。
const MIGRATION_5: &[&str] = &[r#"
    CREATE INDEX IF NOT EXISTS idx_context_snapshots_session_created
    ON context_snapshots (session_id, created_at DESC)
    "#];

// This checksum was produced by the first versioned migration runner before its procedural
// signature was derived directly from the SQL and column definitions above.
const PREVIOUS_MIGRATION_2_SIGNATURE: &[&str] = &[
    "threads.forked_from_turn_id TEXT",
    "threads.forked_from_sequence_no INTEGER",
    "threads.rebound_from_workspace_root TEXT",
    "threads.rollout_path TEXT",
    "threads.removed INTEGER NOT NULL DEFAULT 0",
    "command_acks.status TEXT NOT NULL DEFAULT 'completed'",
    "command_acks.updated_at TEXT",
    "artifact_records.created_at TEXT",
    "artifact_records.retention_policy TEXT",
    "artifact_records.size_bytes INTEGER",
    "artifact_records.expires_at TEXT",
    "artifact_records.blob_deleted_at TEXT",
    "DROP INDEX idx_threads_session",
    "CREATE UNIQUE INDEX idx_threads_session_unique ON threads(session_id)",
    "CREATE INDEX idx_artifact_records_content ON artifact_records(checksum,size_bytes,blob_deleted_at)",
    "deduplicate legacy threads and backfill artifact metadata from artifact_json",
];

fn migration_checksum(version: i64) -> String {
    let mut digest = Sha256::new();
    digest.update(version.to_le_bytes());
    match version {
        1 => {
            digest.update(MIGRATION_1_NAME.as_bytes());
            for statement in MIGRATION_1 {
                digest.update([0_u8]);
                digest.update(statement.as_bytes());
            }
        }
        2 => {
            digest.update(MIGRATION_2_NAME.as_bytes());
            for (table, column, declaration) in MIGRATION_2_COLUMNS {
                for part in [*table, *column, *declaration] {
                    digest.update([0_u8]);
                    digest.update(part.as_bytes());
                }
            }
            for operation in MIGRATION_2_SQL {
                digest.update([0_u8]);
                digest.update(operation.as_bytes());
            }
            digest.update([0_u8]);
            digest.update(ARTIFACT_BACKFILL_FORMAT_VERSION.as_bytes());
        }
        3 => {
            digest.update(MIGRATION_3_NAME.as_bytes());
            for statement in MIGRATION_3 {
                digest.update([0_u8]);
                digest.update(statement.as_bytes());
            }
        }
        4 => {
            digest.update(MIGRATION_4_NAME.as_bytes());
            for statement in MIGRATION_4 {
                digest.update([0_u8]);
                digest.update(statement.as_bytes());
            }
        }
        5 => {
            digest.update(MIGRATION_5_NAME.as_bytes());
            for statement in MIGRATION_5 {
                digest.update([0_u8]);
                digest.update(statement.as_bytes());
            }
        }
        _ => return format!("sha256:unsupported-migration-{version}"),
    }
    let digest = digest.finalize();
    let encoded = digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("sha256:{encoded}")
}

fn migration_checksum_matches(version: i64, checksum: &str) -> bool {
    checksum == migration_checksum(version)
        || (version == 2 && checksum == previous_migration_2_checksum())
        || matches!(
            (version, checksum),
            (1, LEGACY_MIGRATION_1_CHECKSUM) | (2, LEGACY_MIGRATION_2_CHECKSUM)
        )
}

fn previous_migration_2_checksum() -> String {
    let mut digest = Sha256::new();
    digest.update(2_i64.to_le_bytes());
    digest.update(MIGRATION_2_NAME.as_bytes());
    for operation in PREVIOUS_MIGRATION_2_SIGNATURE {
        digest.update([0_u8]);
        digest.update(operation.as_bytes());
    }
    let digest = digest.finalize();
    let encoded = digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("sha256:{encoded}")
}

pub(crate) async fn run(pool: &SqlitePool) -> Result<(), String> {
    let mut connection = pool.acquire().await.map_err(|error| error.to_string())?;
    begin_immediate_with_retry(&mut connection).await?;

    let result = apply_pending(&mut connection).await;
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

async fn apply_pending(connection: &mut SqliteConnection) -> Result<(), String> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS schema_migrations (
            version INTEGER PRIMARY KEY,
            name TEXT NOT NULL,
            checksum TEXT NOT NULL,
            applied_at TEXT NOT NULL
        )",
    )
    .execute(&mut *connection)
    .await
    .map_err(|error| error.to_string())?;

    let rows =
        sqlx::query("SELECT version, name, checksum FROM schema_migrations ORDER BY version")
            .fetch_all(&mut *connection)
            .await
            .map_err(|error| error.to_string())?;
    let applied = rows
        .into_iter()
        .map(|row| {
            Ok::<_, sqlx::Error>((
                row.try_get::<i64, _>("version")?,
                row.try_get::<String, _>("name")?,
                row.try_get::<String, _>("checksum")?,
            ))
        })
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?;
    validate_applied(&applied)?;
    refresh_legacy_checksums(connection, &applied).await?;
    let current = applied.last().map_or(0, |entry| entry.0);
    if current > CURRENT_VERSION {
        return Err(format!(
            "database schema version {current} is newer than supported version {CURRENT_VERSION}"
        ));
    }

    if current < 1 {
        for statement in MIGRATION_1 {
            sqlx::query(statement)
                .execute(&mut *connection)
                .await
                .map_err(|error| error.to_string())?;
        }
        record_migration(connection, 1, MIGRATION_1_NAME, &migration_checksum(1)).await?;
    }
    if current < 2 {
        migration_2(connection).await?;
        record_migration(connection, 2, MIGRATION_2_NAME, &migration_checksum(2)).await?;
    }
    if current < 3 {
        for statement in MIGRATION_3 {
            sqlx::query(statement)
                .execute(&mut *connection)
                .await
                .map_err(|error| error.to_string())?;
        }
        record_migration(connection, 3, MIGRATION_3_NAME, &migration_checksum(3)).await?;
    }
    if current < 4 {
        for statement in MIGRATION_4 {
            sqlx::query(statement)
                .execute(&mut *connection)
                .await
                .map_err(|error| error.to_string())?;
        }
        record_migration(connection, 4, MIGRATION_4_NAME, &migration_checksum(4)).await?;
    }
    if current < 5 {
        for statement in MIGRATION_5 {
            sqlx::query(statement)
                .execute(&mut *connection)
                .await
                .map_err(|error| error.to_string())?;
        }
        record_migration(connection, 5, MIGRATION_5_NAME, &migration_checksum(5)).await?;
    }
    Ok(())
}

async fn refresh_legacy_checksums(
    connection: &mut SqliteConnection,
    applied: &[(i64, String, String)],
) -> Result<(), String> {
    for (version, _, checksum) in applied {
        if checksum.as_str() == migration_checksum(*version) {
            continue;
        }
        sqlx::query("UPDATE schema_migrations SET checksum = ? WHERE version = ?")
            .bind(migration_checksum(*version))
            .bind(*version)
            .execute(&mut *connection)
            .await
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn validate_applied(applied: &[(i64, String, String)]) -> Result<(), String> {
    for (index, (version, name, checksum)) in applied.iter().enumerate() {
        let expected_version = i64::try_from(index + 1).unwrap_or(i64::MAX);
        if *version != expected_version {
            return Err(format!(
                "schema migration history has a gap at version {version}"
            ));
        }
        let expected_name = match *version {
            1 => MIGRATION_1_NAME,
            2 => MIGRATION_2_NAME,
            3 => MIGRATION_3_NAME,
            4 => MIGRATION_4_NAME,
            5 => MIGRATION_5_NAME,
            _ => return Err(format!("schema migration version {version} is unsupported")),
        };
        if name != expected_name || !migration_checksum_matches(*version, checksum) {
            return Err(format!(
                "schema migration {version} checksum/name does not match the compiled migration"
            ));
        }
    }
    Ok(())
}

async fn record_migration(
    connection: &mut SqliteConnection,
    version: i64,
    name: &str,
    checksum: &str,
) -> Result<(), String> {
    sqlx::query(
        "INSERT INTO schema_migrations (version, name, checksum, applied_at)
         VALUES (?, ?, ?, ?)",
    )
    .bind(version)
    .bind(name)
    .bind(checksum)
    .bind(chrono::Utc::now().to_rfc3339())
    .execute(&mut *connection)
    .await
    .map_err(|error| error.to_string())?;
    Ok(())
}

async fn migration_2(connection: &mut SqliteConnection) -> Result<(), String> {
    for (table, name, declaration) in MIGRATION_2_COLUMNS {
        add_column_if_missing(connection, table, name, declaration).await?;
    }

    deduplicate_legacy_threads(connection).await?;
    sqlx::query(DROP_LEGACY_THREAD_INDEX_SQL)
        .execute(&mut *connection)
        .await
        .map_err(|error| error.to_string())?;
    sqlx::query(CREATE_THREAD_SESSION_INDEX_SQL)
        .execute(&mut *connection)
        .await
        .map_err(|error| error.to_string())?;
    sqlx::query(CREATE_ARTIFACT_CONTENT_INDEX_SQL)
        .execute(&mut *connection)
        .await
        .map_err(|error| error.to_string())?;

    let rows = sqlx::query(SELECT_ARTIFACTS_TO_BACKFILL_SQL)
        .fetch_all(&mut *connection)
        .await
        .map_err(|error| error.to_string())?;
    for row in rows {
        let artifact_id: String = row
            .try_get("artifact_id")
            .map_err(|error| error.to_string())?;
        let artifact_json: String = row
            .try_get("artifact_json")
            .map_err(|error| error.to_string())?;
        let artifact: ArtifactRecord =
            serde_json::from_str(&artifact_json).map_err(|error| error.to_string())?;
        sqlx::query(UPDATE_ARTIFACT_METADATA_SQL)
            .bind(artifact.created_at.to_rfc3339())
            .bind(&artifact.retention_policy)
            .bind(i64::try_from(artifact.size_bytes).unwrap_or(i64::MAX))
            .bind(artifact_expiration(&artifact).map(|value| value.to_rfc3339()))
            .bind(artifact_id)
            .execute(&mut *connection)
            .await
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

async fn deduplicate_legacy_threads(connection: &mut SqliteConnection) -> Result<(), String> {
    sqlx::query(CREATE_THREAD_DEDUPLICATION_SQL)
        .execute(&mut *connection)
        .await
        .map_err(|error| error.to_string())?;
    sqlx::query(CLEAR_THREAD_DEDUPLICATION_SQL)
        .execute(&mut *connection)
        .await
        .map_err(|error| error.to_string())?;
    sqlx::query(POPULATE_THREAD_DEDUPLICATION_SQL)
        .execute(&mut *connection)
        .await
        .map_err(|error| error.to_string())?;
    sqlx::query(REPARENT_DEDUPLICATED_THREADS_SQL)
        .execute(&mut *connection)
        .await
        .map_err(|error| error.to_string())?;
    sqlx::query(DELETE_DEDUPLICATED_THREADS_SQL)
        .execute(&mut *connection)
        .await
        .map_err(|error| error.to_string())?;
    sqlx::query(DROP_THREAD_DEDUPLICATION_SQL)
        .execute(&mut *connection)
        .await
        .map_err(|error| error.to_string())?;
    Ok(())
}

async fn add_column_if_missing(
    connection: &mut SqliteConnection,
    table: &str,
    name: &str,
    declaration: &str,
) -> Result<(), String> {
    let rows = sqlx::query(&format!("PRAGMA table_info({table})"))
        .fetch_all(&mut *connection)
        .await
        .map_err(|error| error.to_string())?;
    let exists = rows.iter().any(|row| {
        row.try_get::<String, _>("name")
            .is_ok_and(|existing| existing == name)
    });
    if !exists {
        sqlx::query(&format!(
            "ALTER TABLE {table} ADD COLUMN {name} {declaration}"
        ))
        .execute(&mut *connection)
        .await
        .map_err(|error| error.to_string())?;
    }
    Ok(())
}

#[cfg(test)]
pub(crate) async fn open_legacy_fixture(path: &Path) -> Result<(), String> {
    let options = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Delete);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .map_err(|error| error.to_string())?;
    sqlx::query(
        "CREATE TABLE threads (
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
        )",
    )
    .execute(&pool)
    .await
    .map_err(|error| error.to_string())?;
    drop(pool);
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use sqlx::{Row, sqlite::SqliteConnectOptions};
    use tempfile::tempdir;

    use super::*;

    #[tokio::test]
    async fn fresh_database_records_ordered_migrations_and_checksums() {
        let directory = tempdir().expect("directory");
        let path = directory.path().join("runtime.sqlite");
        let options = SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))
            .expect("options")
            .create_if_missing(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .expect("pool");
        run(&pool).await.expect("migrations");
        let rows = sqlx::query("SELECT version, checksum FROM schema_migrations ORDER BY version")
            .fetch_all(&pool)
            .await
            .expect("history");
        assert_eq!(
            rows.len(),
            usize::try_from(CURRENT_VERSION).expect("version")
        );
        assert_eq!(rows[0].try_get::<i64, _>("version").expect("version"), 1);
        assert_eq!(rows[1].try_get::<i64, _>("version").expect("version"), 2);
        assert_eq!(rows[2].try_get::<i64, _>("version").expect("version"), 3);
        assert_eq!(
            rows[0].try_get::<String, _>("checksum").expect("checksum"),
            migration_checksum(1)
        );
        assert_eq!(
            rows[1].try_get::<String, _>("checksum").expect("checksum"),
            migration_checksum(2)
        );
        assert_eq!(
            rows[2].try_get::<String, _>("checksum").expect("checksum"),
            migration_checksum(3)
        );
        assert_eq!(
            rows[3].try_get::<String, _>("checksum").expect("checksum"),
            migration_checksum(4)
        );
        assert_eq!(
            rows[4].try_get::<String, _>("checksum").expect("checksum"),
            migration_checksum(5)
        );
    }

    #[tokio::test]
    async fn previous_versioned_checksum_is_accepted_and_refreshed() {
        let directory = tempdir().expect("directory");
        let path = directory.path().join("previous-checksum.sqlite");
        let options = SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))
            .expect("options")
            .create_if_missing(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .expect("pool");
        run(&pool).await.expect("initial migrations");
        let previous_checksum = previous_migration_2_checksum();
        assert_ne!(previous_checksum, migration_checksum(2));
        sqlx::query("UPDATE schema_migrations SET checksum = ? WHERE version = 2")
            .bind(previous_checksum)
            .execute(&pool)
            .await
            .expect("previous checksum");

        run(&pool).await.expect("compatible reopen");
        let checksum: String =
            sqlx::query_scalar("SELECT checksum FROM schema_migrations WHERE version = 2")
                .fetch_one(&pool)
                .await
                .expect("refreshed checksum");
        assert_eq!(checksum, migration_checksum(2));
    }

    #[tokio::test]
    async fn legacy_threads_are_upgraded_inside_the_versioned_runner() {
        let directory = tempdir().expect("directory");
        let path = directory.path().join("legacy.sqlite");
        open_legacy_fixture(&path).await.expect("legacy fixture");
        let options = SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))
            .expect("options")
            .create_if_missing(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .expect("pool");
        run(&pool).await.expect("migration");
        let columns = sqlx::query("PRAGMA table_info(threads)")
            .fetch_all(&pool)
            .await
            .expect("columns")
            .into_iter()
            .map(|row| row.try_get::<String, _>("name").expect("name"))
            .collect::<HashSet<_>>();
        assert!(columns.contains("removed"));
        assert!(columns.contains("rollout_path"));
    }

    #[tokio::test]
    async fn model_history_partial_index_is_present() {
        let directory = tempdir().expect("directory");
        let path = directory.path().join("history-index.sqlite");
        let options = SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))
            .expect("options")
            .create_if_missing(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .expect("pool");
        run(&pool).await.expect("migrations");
        let sql: Option<String> = sqlx::query_scalar(
            "SELECT sql FROM sqlite_master WHERE type = 'index' AND name = 'idx_runtime_events_model_history_session_sequence'",
        )
        .fetch_optional(&pool)
        .await
        .expect("index query");
        assert!(sql.is_some_and(|sql| sql.contains("event_type IN")));
    }

    #[tokio::test]
    async fn context_snapshot_index_is_added_when_upgrading_from_version_four() {
        let directory = tempdir().expect("directory");
        let path = directory.path().join("context-index-upgrade.sqlite");
        let options = SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))
            .expect("options")
            .create_if_missing(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .expect("pool");
        run(&pool).await.expect("initial migrations");
        sqlx::query("DROP INDEX idx_context_snapshots_session_created")
            .execute(&pool)
            .await
            .expect("drop index");
        sqlx::query("DELETE FROM schema_migrations WHERE version = 5")
            .execute(&pool)
            .await
            .expect("rewind migration ledger");

        run(&pool).await.expect("version four upgrade");
        let sql: Option<String> = sqlx::query_scalar(
            "SELECT sql FROM sqlite_master WHERE type = 'index' AND name = 'idx_context_snapshots_session_created'",
        )
        .fetch_optional(&pool)
        .await
        .expect("index query");
        assert!(sql.is_some_and(|sql| sql.contains("context_snapshots")));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_openers_observe_one_complete_migration_history() {
        let directory = tempdir().expect("directory");
        let path = directory.path().join("concurrent.sqlite");
        let urls = (0..4)
            .map(|_| format!("sqlite://{}", path.display()))
            .collect::<Vec<_>>();
        let tasks = urls
            .into_iter()
            .map(|url| async move {
                let store = crate::RuntimeStore::connect(&url).await;
                store.map(|_| ())
            })
            .collect::<Vec<_>>();
        let mut join_set = tokio::task::JoinSet::new();
        for task in tasks {
            join_set.spawn(task);
        }
        let mut results = Vec::new();
        while let Some(result) = join_set.join_next().await {
            results.push(result.expect("migration opener task"));
        }
        assert!(results.iter().all(Result::is_ok), "{results:?}");

        let options = SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))
            .expect("options")
            .create_if_missing(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .expect("pool");
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM schema_migrations")
            .fetch_one(&pool)
            .await
            .expect("count");
        assert_eq!(count, CURRENT_VERSION);
    }
}
