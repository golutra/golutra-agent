use super::*;
use golutra_agent_core::{SessionId, ToolCallId};

fn request(session_id: SessionId, tool: &str, arguments: Value) -> ToolRequest {
    ToolRequest {
        tool_call_id: ToolCallId::new(),
        provider_tool_call_id: None,
        session_id,
        turn_id: None,
        tool_name: tool.into(),
        arguments,
    }
}

fn fixture(root: &Path) -> BrowserRouter {
    let executable = root.join("opencli.mjs");
    std::fs::write(&executable, include_str!("../opencli/fixture.mjs")).unwrap();
    BrowserConfig::Chrome(OpenCliConfig {
        executable: Some(executable),
        ..Default::default()
    })
    .save(root)
    .unwrap();
    let mut router = BrowserRouter::new(root, root, &root.join("state"), true).unwrap();
    router.discover = || BrowserConfig::Auto(Default::default());
    router
}

async fn call(
    router: &BrowserRouter,
    session: SessionId,
    tool: &str,
    args: Value,
) -> ExternalToolOutput {
    router
        .call(&request(session, tool, args), CancellationToken::new())
        .await
        .unwrap()
}

#[tokio::test]
async fn selection_is_agent_writable_session_scoped_and_cannot_enable_disabled_tools() {
    let root = tempfile::tempdir().unwrap();
    let router = fixture(root.path());
    let saved = std::fs::read(root.path().join("browser.json")).unwrap();
    let session = SessionId::new();
    let selection = call(
        &router,
        session,
        "browser_config",
        json!({"browser_backend":"auto"}),
    )
    .await;
    assert_eq!(selection.structured_facts["browser_backend"], "auto");
    assert!(selection.structured_facts["resolved_backend"].is_null());
    let reconstructed = fixture(root.path());
    let status = call(&reconstructed, session, "browser_status", json!({})).await;
    assert_eq!(status.structured_facts["browser_backend"], "auto");
    assert_eq!(status.structured_facts["selection_scope"], "session");
    assert_eq!(
        call(&router, SessionId::new(), "browser_config", json!({}))
            .await
            .structured_facts["browser_backend"],
        "chrome"
    );
    assert_eq!(
        saved,
        std::fs::read(root.path().join("browser.json")).unwrap()
    );
    for args in [
        json!({"browser_backend":"safari"}),
        json!({"browser_backend":"chrome", "executable":"/tmp/arbitrary"}),
    ] {
        assert!(
            router
                .call(
                    &request(session, "browser_config", args),
                    CancellationToken::new()
                )
                .await
                .is_err()
        );
    }
    BrowserConfig::Disabled(connection::DisabledConfig {
        backend: connection::DisabledKind::Disabled,
    })
    .save(root.path())
    .unwrap();
    let stopped = call(
        &router,
        session,
        "browser_config",
        json!({"browser_backend":"ego"}),
    )
    .await;
    assert!(stopped.is_error);
    assert_eq!(stopped.structured_facts["browser_backend"], "disabled");
    assert_eq!(
        call(
            &router,
            session,
            "browser_run",
            json!({"action":"open","url":"https://example.test"})
        )
        .await
        .structured_facts["code"],
        "browser_disabled"
    );
    assert!(!root.path().join("calls.jsonl").exists());
}

#[cfg(unix)]
fn save_ego(root: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let fixture = root.join("ego.mjs");
    std::fs::write(&fixture, include_str!("../fixtures/ego.mjs")).unwrap();
    let node = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|p| p.join("node"))
        .find(|p| p.is_file())
        .expect("Node required");
    let executable = root.join("ego-browser");
    std::fs::write(
        &executable,
        format!(
            "#!/bin/sh\nexec {} {} \"$@\"\n",
            shlex::try_quote(node.to_str().unwrap()).unwrap(),
            shlex::try_quote(fixture.to_str().unwrap()).unwrap()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
    BrowserConfig::Ego(EgoBrowserConfig {
        executable,
        sdk_path: None,
    })
    .save(root)
    .unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn pinned_task_survives_preference_changes_and_switches_after_release_without_replay() {
    let root = tempfile::tempdir().unwrap();
    let router = fixture(root.path());
    let session = SessionId::new();
    let opening = request(
        session,
        "browser_run",
        json!({"action":"open","url":"https://example.test"}),
    );
    assert!(
        !router
            .call(&opening, CancellationToken::new())
            .await
            .unwrap()
            .is_error
    );
    save_ego(root.path());
    let changed = call(
        &router,
        session,
        "browser_config",
        json!({"browser_backend":"ego"}),
    )
    .await;
    assert_eq!(changed.structured_facts["browser_backend"], "ego");
    assert_eq!(changed.structured_facts["resolved_backend"], "chrome");
    assert_eq!(changed.structured_facts["next_backend"], "ego");
    assert_eq!(changed.structured_facts["switch_pending"], true);
    let router =
        BrowserRouter::new(root.path(), root.path(), &root.path().join("state"), true).unwrap();
    let snapshot = call(
        &router,
        session,
        "browser_run",
        json!({"action":"snapshot"}),
    )
    .await;
    assert!(!snapshot.is_error, "{snapshot:?}");
    assert_eq!(snapshot.structured_facts["resolved_backend"], "chrome");
    assert!(
        !call(&router, session, "browser_finish", json!({"release":true}))
            .await
            .is_error
    );
    assert_eq!(
        call(&router, session, "browser_config", json!({}))
            .await
            .structured_facts["resolved_backend"],
        "ego"
    );
    let replay = ToolRequest {
        arguments: json!({"code":"console.log('must not replay')"}),
        ..opening
    };
    assert_eq!(
        router
            .call(&replay, CancellationToken::new())
            .await
            .unwrap()
            .structured_facts["code"],
        "browser_duplicate_call"
    );
    let ego = call(
        &router,
        session,
        "browser_run",
        json!({"code":"console.log('native Ego selected');"}),
    )
    .await;
    assert!(!ego.is_error, "{ego:?}");
    assert_eq!(ego.structured_facts["resolved_backend"], "ego");
    assert!(ego.content.contains("native Ego selected"));
    assert!(
        !call(&router, session, "browser_finish", json!({"release":true}))
            .await
            .is_error
    );
}

#[tokio::test]
async fn active_call_blocks_concurrent_selection_and_unknown_outcome_does_not_fallback() {
    let root = tempfile::tempdir().unwrap();
    let router = std::sync::Arc::new(fixture(root.path()));
    let session = SessionId::new();
    assert!(
        !call(
            &router,
            session,
            "browser_run",
            json!({"action":"open","url":"https://example.test"})
        )
        .await
        .is_error
    );
    let cancel = CancellationToken::new();
    let task = tokio::spawn({
        let router = router.clone();
        let cancel = cancel.clone();
        async move {
            router
                .call(
                    &request(
                        session,
                        "browser_run",
                        json!({"action":"evaluate","expression":"hang"}),
                    ),
                    cancel,
                )
                .await
                .unwrap()
        }
    });
    tokio::time::timeout(Duration::from_secs(10), async {
        while !root.path().join("started").exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let blocked = router
        .call(
            &request(session, "browser_config", json!({"browser_backend":"auto"})),
            CancellationToken::new(),
        )
        .await;
    assert!(blocked.unwrap_err().to_string().contains("browser_busy"));
    cancel.cancel();
    let unknown = task.await.unwrap();
    assert_eq!(unknown.structured_facts["outcome_unknown"], true);
    let changed = call(
        &router,
        session,
        "browser_config",
        json!({"browser_backend":"auto"}),
    )
    .await;
    assert_eq!(changed.structured_facts["resolved_backend"], "chrome");
    assert_eq!(changed.structured_facts["switch_pending"], true);
    let denied = call(
        &router,
        session,
        "browser_run",
        json!({"action":"click","selector":"#button"}),
    )
    .await;
    assert!(denied.is_error);
    assert_eq!(denied.structured_facts["code"], "browser_outcome_unknown");
    assert_eq!(denied.structured_facts["resolved_backend"], "chrome");
}

#[tokio::test]
async fn legacy_binding_is_preserved_and_backend_specific_arguments_are_not_ignored() {
    let root = tempfile::tempdir().unwrap();
    let router = fixture(root.path());
    let session = SessionId::new();
    let BrowserConfig::Chrome(config) = BrowserConfig::load(root.path()).unwrap().unwrap() else {
        panic!()
    };
    let legacy =
        OpenCliBrowserBackend::new(config, root.path(), &root.path().join("state"), true).unwrap();
    assert!(
        !legacy
            .call(
                &request(
                    session,
                    "browser_run",
                    json!({"action":"open","url":"https://example.test"})
                ),
                CancellationToken::new()
            )
            .await
            .unwrap()
            .is_error
    );
    BrowserConfig::Auto(Default::default())
        .save(root.path())
        .unwrap();
    let status = call(&router, session, "browser_config", json!({})).await;
    assert_eq!(status.structured_facts["browser_backend"], "auto");
    assert_eq!(status.structured_facts["resolved_backend"], "chrome");
    let before = std::fs::read(root.path().join("calls.jsonl")).unwrap();
    assert!(
        call(
            &router,
            session,
            "browser_run",
            json!({"action":"click","selector":"#ok","code":"unexpected()"})
        )
        .await
        .is_error
    );
    assert_eq!(
        before,
        std::fs::read(root.path().join("calls.jsonl")).unwrap()
    );
    assert!(
        !call(&router, session, "browser_finish", json!({"release":true}))
            .await
            .is_error
    );
}

#[test]
fn stable_contracts_allow_both_backends_and_background_control() {
    let root = tempfile::tempdir().unwrap();
    let router = fixture(root.path());
    let mut run = router
        .contracts()
        .into_iter()
        .find(|c| c.tool_name == "browser_run")
        .unwrap();
    crate::background::enable_browser_background(&mut run);
    assert!(validate(&run, &json!({"action":"snapshot","background":true})).is_ok());
    assert!(
        validate(
            &run,
            &json!({"code":"console.log(await page.snapshot());","background":true})
        )
        .is_ok()
    );
    for contract in router.contracts() {
        assert!(jsonschema::validator_for(&contract.input_schema).is_ok());
    }
}
