//! 真实进程的共享使用、独占初始化与退出释放；未来版本夹具不冒充已发布二进制。
use fs2::FileExt;
use golutra_agent_store::{DataCompatibility, RuntimeStore, inspect_data_compatibility};
use sqlx::sqlite::SqlitePoolOptions;
use std::{
    fs::OpenOptions,
    io::{BufRead, BufReader, Read, Write},
    process::{Child, Command, Stdio},
};

struct Worker(Child);
impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn worker(root: &std::path::Path, mode: &str) -> Worker {
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "usage_worker", "--nocapture"])
        .env("GOLUTRA_AGENT_DATA_TEST_ROOT", root)
        .env("GOLUTRA_AGENT_DATA_TEST_MODE", mode)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let mut reader = BufReader::new(child.stdout.take().unwrap());
    loop {
        let mut line = String::new();
        assert_ne!(
            reader.read_line(&mut line).unwrap(),
            0,
            "worker exited before ready"
        );
        if line.trim() == "USAGE_READY" {
            break;
        }
    }
    Worker(child)
}

#[tokio::test]
async fn usage_worker() {
    let Some(root) = std::env::var_os("GOLUTRA_AGENT_DATA_TEST_ROOT") else {
        return;
    };
    let root = std::path::PathBuf::from(root);
    let mode = std::env::var("GOLUTRA_AGENT_DATA_TEST_MODE").unwrap();
    let _store;
    let _lease;
    if mode == "store" {
        _store = Some(
            RuntimeStore::connect(&format!("sqlite:{}", root.join("runtime.sqlite").display()))
                .await
                .unwrap(),
        );
        _lease = None;
    } else {
        _store = None;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(root.join("runtime.sqlite.usage.lock"))
            .unwrap();
        FileExt::lock_shared(&file).unwrap();
        _lease = Some(file);
    }
    println!("USAGE_READY");
    std::io::stdout().flush().unwrap();
    let _ = std::io::stdin().read_exact(&mut [0]);
}

#[tokio::test]
async fn compatible_processes_share_until_last_clone_and_crash_releases_lock() {
    let root = tempfile::tempdir().unwrap();
    let url = format!("sqlite:{}", root.path().join("runtime.sqlite").display());
    let first = RuntimeStore::connect(&url).await.unwrap();
    let clone = first.clone();
    let child = worker(root.path(), "store");
    let contender = OpenOptions::new()
        .read(true)
        .write(true)
        .open(root.path().join("runtime.sqlite.usage.lock"))
        .unwrap();
    assert!(FileExt::try_lock_exclusive(&contender).is_err());
    assert_eq!(first.max_sequence_no().await.unwrap(), 0);
    drop(first);
    drop(child);
    assert!(
        FileExt::try_lock_exclusive(&contender).is_err(),
        "clone must retain lease"
    );
    drop(clone);
    // 并行测试 fork/exec 时可短暂继承关闭前的描述符；允许系统完成释放，
    // 但真正泄漏的共享锁仍须在有界期限内失败。
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        match FileExt::try_lock_exclusive(&contender) {
            Ok(()) => break,
            Err(error)
                if error.raw_os_error() == fs2::lock_contended_error().raw_os_error()
                    && std::time::Instant::now() < deadline =>
            {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            Err(error) => panic!("usage lock was not released after the last clone: {error}"),
        }
    }
    FileExt::unlock(&contender).unwrap();
    RuntimeStore::connect(&url).await.unwrap();
}

#[tokio::test]
async fn old_and_future_schema_are_rejected_even_without_active_users() {
    for version in [1, 4, 5, 6, 999] {
        let root = tempfile::tempdir().unwrap();
        let url = format!("sqlite:{}", root.path().join("runtime.sqlite").display());
        drop(RuntimeStore::connect(&url).await.unwrap());
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect(&url)
            .await
            .unwrap();
        sqlx::query("UPDATE schema_migrations SET version=?")
            .bind(version)
            .execute(&pool)
            .await
            .unwrap();
        let before: Vec<(i64, String, String, String)> =
            sqlx::query_as("SELECT * FROM schema_migrations")
                .fetch_all(&pool)
                .await
                .unwrap();
        let holder = worker(root.path(), "lease");
        assert_eq!(
            inspect_data_compatibility(&pool).await.unwrap().status,
            DataCompatibility::Unsupported
        );
        assert!(
            RuntimeStore::connect(&url)
                .await
                .unwrap_err()
                .to_string()
                .contains("data_unsupported")
        );
        drop(holder);
        // 释放旧实例不能使不支持的数据突然变成可升级。
        assert!(
            RuntimeStore::connect(&url)
                .await
                .unwrap_err()
                .to_string()
                .contains("data_unsupported")
        );
        let after: Vec<(i64, String, String, String)> =
            sqlx::query_as("SELECT * FROM schema_migrations")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(before, after);
        pool.close().await;
    }
}

#[tokio::test]
async fn fresh_initialization_requires_exclusive_lease_and_crash_releases_it() {
    let root = tempfile::tempdir().unwrap();
    let url = format!("sqlite:{}", root.path().join("runtime.sqlite").display());
    let holder = worker(root.path(), "lease");
    assert!(
        RuntimeStore::connect(&url)
            .await
            .unwrap_err()
            .to_string()
            .contains("data_in_use")
    );
    drop(holder);
    let store = RuntimeStore::connect(&url).await.unwrap();
    assert_eq!(
        store.data_compatibility().await.unwrap().status,
        DataCompatibility::Supported
    );
}
