//! 当前数据格式检查与进程使用期锁；仅空库可初始化，历史格式拒绝打开。

use crate::{StoreError, StoreResult, migrations};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sqlx::{Row, SqlitePool};
use std::{
    fs::{File, OpenOptions},
    path::Path,
    sync::Arc,
    time::Duration,
};

// 给同时冷启动的初始化留出约五秒；不能无限等待另一场用户任务结束。
const USAGE_LOCK_RETRIES: usize = 100;
const USAGE_LOCK_RETRY_DELAY: Duration = Duration::from_millis(50);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DataCompatibility {
    Supported,
    Uninitialized,
    Unsupported,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DataCompatibilityReport {
    pub contract_version: u32,
    pub binary_version: String,
    pub schema_version: i64,
    pub supported_schema_version: i64,
    pub status: DataCompatibility,
    pub reason: Option<String>,
}

/// 只读检查账本与 checksum，不创建表、不迁移、不根据软件 semver 猜兼容性。
pub async fn inspect_data_compatibility(pool: &SqlitePool) -> StoreResult<DataCompatibilityReport> {
    let exists: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='schema_migrations'",
    )
    .fetch_one(pool)
    .await?;
    let applied = if exists == 0 {
        Vec::new()
    } else {
        sqlx::query("SELECT version, name, checksum FROM schema_migrations ORDER BY version")
            .fetch_all(pool)
            .await?
            .into_iter()
            .map(|row| {
                Ok((
                    row.try_get("version")?,
                    row.try_get("name")?,
                    row.try_get("checksum")?,
                ))
            })
            .collect::<Result<Vec<(i64, String, String)>, sqlx::Error>>()?
    };
    let schema_version = applied.last().map_or(0, |row| row.0);
    let objects: i64 =
        sqlx::query_scalar("SELECT count(*) FROM sqlite_master WHERE name NOT LIKE 'sqlite_%'")
            .fetch_one(pool)
            .await?;
    let reason = if objects == 0 {
        None
    } else {
        migrations::validate_applied(&applied).err()
    };
    let status = if objects == 0 {
        DataCompatibility::Uninitialized
    } else if reason.is_some() {
        DataCompatibility::Unsupported
    } else {
        DataCompatibility::Supported
    };
    Ok(DataCompatibilityReport {
        contract_version: 1,
        binary_version: env!("CARGO_PKG_VERSION").to_owned(),
        schema_version,
        supported_schema_version: migrations::CURRENT_VERSION,
        status,
        reason,
    })
}

/// 锁绑定 canonical DB 路径；Arc 的最后一个 store clone 释放后才允许独占。
pub(crate) fn open_usage_lock(database: &Path) -> StoreResult<File> {
    let database = if database.exists() {
        database.canonicalize()
    } else {
        database
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or(Path::new("."))
            .canonicalize()
            .map(|parent| parent.join(database.file_name().unwrap_or_default()))
    }
    .map_err(|error| StoreError::Migration(error.to_string()))?;
    let mut lock_name = database.as_os_str().to_os_string();
    lock_name.push(".usage.lock");
    let lock_path = std::path::PathBuf::from(lock_name);
    if std::fs::symlink_metadata(&lock_path).is_ok_and(|metadata| metadata.file_type().is_symlink())
    {
        return Err(StoreError::Migration(
            "usage lock must not be a symbolic link".to_owned(),
        ));
    }
    let mut options = OpenOptions::new();
    options.create(true).truncate(false).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
        .open(lock_path)
        .map_err(|error| StoreError::Migration(error.to_string()))
}

async fn lock_shared(file: &File) -> StoreResult<()> {
    for attempt in 0..=USAGE_LOCK_RETRIES {
        match FileExt::try_lock_shared(file) {
            Ok(()) => return Ok(()),
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock
                    && attempt < USAGE_LOCK_RETRIES =>
            {
                tokio::time::sleep(USAGE_LOCK_RETRY_DELAY).await
            }
            Err(error) => {
                return Err(StoreError::Migration(format!(
                    "data_in_use: schema initialization holds usage lock: {error}"
                )));
            }
        }
    }
    unreachable!()
}

/// 旧/未知格式在业务写入前拒绝；仅全空库可在独占锁下初始化。
pub(crate) async fn prepare_shared_store(pool: &SqlitePool, file: File) -> StoreResult<Arc<File>> {
    lock_shared(&file).await?;
    let report = inspect_data_compatibility(pool).await?;
    match report.status {
        DataCompatibility::Unsupported => {
            return Err(StoreError::Migration(format!(
                "data_unsupported: {}",
                report.reason.unwrap_or_default()
            )));
        }
        DataCompatibility::Supported => return Ok(Arc::new(file)),
        DataCompatibility::Uninitialized => {}
    }
    FileExt::unlock(&file).map_err(|error| StoreError::Migration(error.to_string()))?;
    // 不用阻塞式升级锁：另一个进程可能持有整场任务的共享锁。
    // 同时冷启动者短暂争用时重试；已打开的 store 不释放锁时有界失败。
    for attempt in 0..=USAGE_LOCK_RETRIES {
        match FileExt::try_lock_exclusive(&file) {
            Ok(()) => break,
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock
                    && attempt < USAGE_LOCK_RETRIES =>
            {
                tokio::time::sleep(USAGE_LOCK_RETRY_DELAY).await
            }
            Err(error) => {
                return Err(StoreError::Migration(format!(
                    "data_in_use: close Agent instances before schema initialization: {error}"
                )));
            }
        }
        // 另一冷启动者可能已完成初始化并持有共享锁；无需再次要求独占。
        match FileExt::try_lock_shared(&file) {
            Ok(()) => {
                let current = inspect_data_compatibility(pool).await?;
                if current.status == DataCompatibility::Supported {
                    return Ok(Arc::new(file));
                }
                if current.status == DataCompatibility::Unsupported {
                    return Err(StoreError::Migration(format!(
                        "data_unsupported: {}",
                        current.reason.unwrap_or_default()
                    )));
                }
                FileExt::unlock(&file).map_err(|error| StoreError::Migration(error.to_string()))?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) => return Err(StoreError::Migration(error.to_string())),
        }
    }
    migrations::run(pool).await.map_err(StoreError::Migration)?;
    // Windows 无原子降级；释放后重新取得共享锁并复查，避免间隙中的格式变化被漏过。
    FileExt::unlock(&file).map_err(|error| StoreError::Migration(error.to_string()))?;
    lock_shared(&file).await?;
    require_supported(pool).await?;
    Ok(Arc::new(file))
}

pub(crate) async fn require_supported(pool: &SqlitePool) -> StoreResult<()> {
    let report = inspect_data_compatibility(pool).await?;
    if report.status != DataCompatibility::Supported {
        return Err(StoreError::Migration(format!(
            "data_unsupported: schema changed while open: {report:?}"
        )));
    }
    Ok(())
}
