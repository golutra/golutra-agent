//! 用真实进程竞争验证 revision 不是仅在 GUI 或单进程 mutex 中检查。
use golutra_agent_config::{
    ConfigError, ProviderConfigPaths, patch_runtime_settings, read_runtime_settings,
};
use std::process::{Command, Stdio};

#[test]
fn settings_writer_process() {
    let Some(root) = std::env::var_os("GOLUTRA_AGENT_SETTINGS_TEST_ROOT") else {
        return;
    };
    let paths = ProviderConfigPaths::from_home(root).unwrap();
    let expected = std::env::var("GOLUTRA_AGENT_SETTINGS_TEST_REVISION").unwrap();
    let model = std::env::var("GOLUTRA_AGENT_SETTINGS_TEST_MODEL").unwrap();
    let patch = serde_json::json!({"model": model})
        .as_object()
        .unwrap()
        .clone();
    match patch_runtime_settings(&paths, &expected, patch) {
        Ok(_) => println!("PATCH_APPLIED"),
        Err(ConfigError::VersionConflict { .. }) => println!("PATCH_CONFLICT"),
        Err(error) => panic!("{error}"),
    }
}

#[test]
fn concurrent_stale_settings_have_exactly_one_winner() {
    let root = tempfile::tempdir().unwrap();
    let paths = ProviderConfigPaths::from_home(root.path()).unwrap();
    let revision = read_runtime_settings(&paths).unwrap().revision;
    let children: Vec<_> = ["desktop", "npm"]
        .into_iter()
        .map(|model| {
            Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "settings_writer_process", "--nocapture"])
                .env("GOLUTRA_AGENT_SETTINGS_TEST_ROOT", root.path())
                .env("GOLUTRA_AGENT_SETTINGS_TEST_REVISION", &revision)
                .env("GOLUTRA_AGENT_SETTINGS_TEST_MODEL", model)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect();
    let results: Vec<_> = children
        .into_iter()
        .map(|child| {
            let result = child.wait_with_output().unwrap();
            assert!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
            String::from_utf8(result.stdout).unwrap()
        })
        .collect();
    assert_eq!(
        results
            .iter()
            .filter(|out| out.contains("PATCH_APPLIED"))
            .count(),
        1
    );
    assert_eq!(
        results
            .iter()
            .filter(|out| out.contains("PATCH_CONFLICT"))
            .count(),
        1
    );
    assert_ne!(read_runtime_settings(&paths).unwrap().revision, revision);
}

#[tokio::test]
async fn provider_revision_is_checked_inside_verified_transaction() {
    use golutra_agent_config::{ProviderSettings, update_provider_settings_at_revision_verified};
    let root = tempfile::tempdir().unwrap();
    let paths = ProviderConfigPaths::from_home(root.path()).unwrap();
    let settings = ProviderSettings::default();
    let stale_revision = settings.revision().unwrap();
    let mut changed = settings;
    changed.upsert_profile(golutra_agent_config::ProviderProfile::mock(), true);
    changed.save(&paths.user_config).unwrap();
    let before = std::fs::read(&paths.user_config).unwrap();
    let error =
        update_provider_settings_at_revision_verified(&paths, root.path(), &stale_revision, |_| {
            panic!("stale mutation must not run")
        })
        .await
        .unwrap_err();
    assert!(error.to_string().contains("config_version_conflict"));
    assert_eq!(std::fs::read(&paths.user_config).unwrap(), before);
}
