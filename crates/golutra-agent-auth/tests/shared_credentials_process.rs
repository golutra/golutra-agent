use std::{fs, process::Command};

use golutra_agent_auth::{
    CredentialRef, CredentialSource, DefaultSecretStore, SecretKind, SecretStore,
};
use secrecy::{ExposeSecret, SecretString};

fn reference(id: &str) -> CredentialRef {
    CredentialRef::with_id(id, CredentialSource::Disk, SecretKind::ApiKey).expect("reference")
}

// 子进程只操作父测试提供的隔离目录；验证真实 OS 文件锁而非同进程 mutex。
#[test]
fn credential_writer_child() {
    let Some(directory) = std::env::var_os("GOLUTRA_AGENT_TEST_CREDENTIAL_HOME") else {
        return;
    };
    let store = DefaultSecretStore::new(directory).expect("store");
    let prefix = std::env::var("GOLUTRA_AGENT_TEST_CREDENTIAL_PREFIX").expect("prefix");
    for index in 0..20 {
        let id = format!("cred_{prefix}_{index}");
        store
            .set(
                &reference(&id),
                &SecretString::from(format!("fixture-{id}")),
            )
            .expect("set");
    }
}

#[test]
fn desktop_and_npm_processes_preserve_shared_credentials() {
    let directory = tempfile::tempdir().expect("directory");
    let mut children = ["desktop", "npm"].map(|prefix| {
        Command::new(std::env::current_exe().expect("test binary"))
            .args(["--exact", "credential_writer_child"])
            .env("GOLUTRA_AGENT_TEST_CREDENTIAL_HOME", directory.path())
            .env("GOLUTRA_AGENT_TEST_CREDENTIAL_PREFIX", prefix)
            .spawn()
            .expect("child")
    });
    for child in &mut children {
        assert!(child.wait().expect("wait").success());
    }
    let store = DefaultSecretStore::new(directory.path()).expect("reader");
    for prefix in ["desktop", "npm"] {
        for index in 0..20 {
            let id = format!("cred_{prefix}_{index}");
            assert_eq!(
                store
                    .get(&reference(&id))
                    .expect("read")
                    .expect("secret")
                    .expose_secret(),
                &format!("fixture-{id}")
            );
        }
    }
}

#[test]
fn future_credential_format_rejects_reads_and_mutations_without_overwrite() {
    let directory = tempfile::tempdir().expect("directory");
    let store = DefaultSecretStore::new(directory.path()).expect("store");
    let path = store.credentials_path();
    let original = br#"{"version":999,"credentials":{},"future":"preserve"}"#;
    fs::write(&path, original).expect("future credentials");
    let key = reference("cred_future");
    assert!(store.get(&key).is_err());
    assert!(
        store
            .set(&key, &SecretString::from("fixture".to_owned()))
            .is_err()
    );
    assert!(store.delete(&key).is_err());
    assert_eq!(fs::read(&path).expect("unchanged"), original);
}
