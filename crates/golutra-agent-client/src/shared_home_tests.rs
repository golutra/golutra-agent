//! 桌面与 CLI 的目录身份和跨进程会话 ownership 回归。
use super::*;
use golutra_agent_core::ActorKind;
use std::io::{BufRead, BufReader, Read, Write};

async fn storage_identity(transport: &EmbeddedTransport) -> Value {
    transport
        .query(RuntimeQuery {
            query_id: golutra_agent_core::QueryId::new(),
            session_id: transport.default_session_id(),
            task_id: None,
            kind: RuntimeQueryKind::StorageStatus,
            requester: ActorKind::Sdk,
            cursor: None,
            timestamp: chrono::Utc::now(),
        })
        .await
        .unwrap()["identity"]
        .clone()
}

#[tokio::test]
async fn shared_home_identity_uses_resolved_paths_without_sensitive_settings() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("共享 home");
    let cwd = root.path().join("项目 workspace");
    std::fs::create_dir_all(&cwd).unwrap();
    let transport = EmbeddedTransport::from_home_and_cwd(&home, &cwd)
        .await
        .unwrap();
    let paths = RuntimePaths::from_home_and_cwd(&home, cwd.join(".")).unwrap();
    let config = ProviderConfigPaths::from_home(home.join(".")).unwrap();
    assert_eq!(paths.home, config.home);
    let identity = storage_identity(&transport).await;
    assert_eq!(identity["config_home"], json!(config.home));
    assert_eq!(identity["runtime_db"], json!(paths.runtime_db));
    assert_eq!(identity["workspace"], json!(paths.cwd));
    assert_eq!(identity["persistence_mode"], "durable");
    assert!(!identity.to_string().contains("credentials"));
    let memory = EmbeddedTransport::in_memory().await.unwrap();
    assert_eq!(
        storage_identity(&memory).await["persistence_mode"],
        "memory"
    );
    #[cfg(unix)]
    {
        let alias = root.path().join("home-link");
        std::os::unix::fs::symlink(&paths.home, &alias).unwrap();
        assert!(RuntimePaths::from_home_and_cwd(&alias, &cwd).is_err());
        assert!(ProviderConfigPaths::from_home(&alias).is_err());
    }
}

#[tokio::test]
async fn shared_session_worker() {
    let Some(root) = std::env::var_os("GOLUTRA_AGENT_SESSION_TEST_ROOT") else {
        return;
    };
    let root = PathBuf::from(root);
    let owner = EmbeddedTransport::from_home_and_cwd(root.join("home"), &root)
        .await
        .unwrap();
    let SessionLeaseAttempt::Acquired(_lease) = owner
        .host
        .try_acquire_session_lease(owner.default_session_id())
        .unwrap()
    else {
        panic!("lease busy");
    };
    println!("SESSION_READY {}", owner.default_session_id());
    std::io::stdout().flush().unwrap();
    let _ = std::io::stdin().read_exact(&mut [0]);
}

#[tokio::test]
async fn shared_history_does_not_take_over_live_session_and_crash_releases_lease() {
    use std::process::{Child, Command, Stdio};
    struct Worker(Child);
    impl Drop for Worker {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let root = tempfile::tempdir().unwrap();
    let mut child = Worker(
        Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "shared_home_tests::shared_session_worker",
                "--nocapture",
            ])
            .env("GOLUTRA_AGENT_SESSION_TEST_ROOT", root.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    );
    let mut output = BufReader::new(child.0.stdout.take().unwrap());
    let session_id = loop {
        let mut line = String::new();
        assert_ne!(output.read_line(&mut line).unwrap(), 0);
        if let Some(id) = line.trim().strip_prefix("SESSION_READY ") {
            break id.parse::<SessionId>().unwrap();
        }
    };
    let contender = EmbeddedTransport::from_home_and_cwd(root.path().join("home"), root.path())
        .await
        .unwrap();
    contender
        .host
        .storage
        .store
        .list_session_states()
        .await
        .unwrap();
    assert!(matches!(
        contender
            .host
            .try_acquire_session_lease(session_id)
            .unwrap(),
        SessionLeaseAttempt::Busy
    ));
    drop(child);
    assert!(matches!(
        contender
            .host
            .try_acquire_session_lease(session_id)
            .unwrap(),
        SessionLeaseAttempt::Acquired(_)
    ));
}
