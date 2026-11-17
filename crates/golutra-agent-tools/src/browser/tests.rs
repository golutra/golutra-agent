use super::*;
#[cfg(unix)]
#[path = "ego_regressions.rs"]
mod ego_regressions;
use golutra_agent_core::{SessionId, ToolCallId};
#[cfg(unix)]
use tempfile::TempDir;

fn request(session_id: SessionId, tool: &str, arguments: Value) -> ToolRequest {
    ToolRequest {
        tool_call_id: ToolCallId::new(),
        provider_tool_call_id: None,
        session_id,
        turn_id: None,
        tool_name: tool.to_owned(),
        arguments,
    }
}

#[test]
fn configuration_is_optional_and_requires_explicit_absolute_launcher() {
    let root = tempfile::tempdir().unwrap();
    assert!(EgoBrowserConfig::load(root.path()).unwrap().is_none());
    let relative = EgoBrowserConfig {
        executable: "ego-browser".into(),
        sdk_path: None,
    };
    assert!(
        EgoBrowserBackend::new(relative, root.path(), &root.path().join("state"), true).is_err()
    );
    std::fs::write(
        root.path().join("browser.json"),
        r#"{"executable":"/absolute/browser","unknown":true}"#,
    )
    .unwrap();
    assert!(EgoBrowserConfig::load(root.path()).is_err());
}

#[test]
fn leases_are_exclusive_and_partitioned_by_session() {
    let root = tempfile::tempdir().unwrap();
    let call = request(SessionId::new(), "browser_run", json!({"code":"return 1"}));
    let lease = StateLease::acquire(root.path(), &call).unwrap();
    assert!(StateLease::acquire(root.path(), &call).is_err());
    let other = request(SessionId::new(), "browser_run", json!({"code":"return 2"}));
    assert!(StateLease::acquire(root.path(), &other).is_ok());
    drop(lease);
    assert!(StateLease::acquire(root.path(), &call).is_ok());
}

#[tokio::test]
async fn chrome_cancelled_before_dispatch_has_no_journal_or_fallback() {
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("state");
    let backend = chrome::ChromeBrowserBackend::new(
        connection::ChromeConfig {
            backend: connection::ChromeKind::Chrome,
            connection_dir: root.path().join("missing-bridge"),
        },
        root.path(),
        &state,
        true,
    )
    .unwrap();
    let cancel = CancellationToken::new();
    cancel.cancel();
    let result = backend
        .call(
            &request(
                SessionId::new(),
                "browser_run",
                json!({"action":"click","selector":"#submit"}),
            ),
            cancel,
        )
        .await
        .unwrap();
    assert!(result.is_error);
    assert_eq!(result.structured_facts["code"], "chrome_cancelled");
    assert!(result.structured_facts.get("recommendation").is_none());
    assert!(!state.exists());
}

#[tokio::test]
async fn duplicate_launch_and_restricted_launch_do_not_start_an_application() {
    let root = tempfile::tempdir().unwrap();
    let call = request(SessionId::new(), "browser_launch", json!({}));
    let lease = StateLease::acquire(root.path(), &call).unwrap();
    private_directory(
        &lease
            .directory
            .join(format!("launch-{}", call.tool_call_id)),
    )
    .unwrap();
    drop(lease);
    let duplicate = launch_once(
        launch::BrowserKind::Chrome,
        root.path(),
        &call,
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(duplicate.structured_facts["code"], "browser_duplicate_call");
    let restricted = chrome::ChromeBrowserBackend::new(
        connection::ChromeConfig {
            backend: connection::ChromeKind::Chrome,
            connection_dir: root.path().join("bridge"),
        },
        root.path(),
        &root.path().join("state"),
        false,
    )
    .unwrap();
    let result = restricted
        .call(&call, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(
        result.structured_facts["code"],
        "browser_requires_unrestricted"
    );
}

#[cfg(unix)]
struct Fixture {
    root: TempDir,
    config: EgoBrowserConfig,
    session: SessionId,
}

#[cfg(unix)]
impl Fixture {
    fn new() -> Self {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let fixture = root.path().join("ego.mjs");
        std::fs::write(&fixture, include_str!("fixtures/ego.mjs")).unwrap();
        let node = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
            .map(|directory| directory.join("node"))
            .find(|file| file.is_file())
            .expect("Node.js is required for Ego bridge tests");
        let executable = root.path().join("ego fixture");
        let shell = format!(
            "#!/bin/sh\nexec {} {} \"$@\"\n",
            shlex::try_quote(node.to_str().unwrap()).unwrap(),
            shlex::try_quote(fixture.to_str().unwrap()).unwrap()
        );
        std::fs::write(&executable, shell).unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        Self {
            config: EgoBrowserConfig {
                executable,
                sdk_path: None,
            },
            root,
            session: SessionId::new(),
        }
    }

    fn backend(&self) -> EgoBrowserBackend {
        EgoBrowserBackend::new(
            self.config.clone(),
            self.root.path(),
            &self.root.path().join("state"),
            true,
        )
        .unwrap()
    }

    async fn call(&self, name: &str, arguments: Value) -> ExternalToolOutput {
        self.backend()
            .call(
                &request(self.session, name, arguments),
                CancellationToken::new(),
            )
            .await
            .unwrap()
    }
}

#[cfg(unix)]
#[tokio::test]
async fn unbound_status_does_not_include_unrelated_unnamed_user_spaces() {
    let fixture = Fixture::new();
    let database = fixture.root.path().join("spaces.json");
    let user_space = json!({"next":2,"spaces":[{"id":1,"name":"","ownership":"user","recentTabTitles":["private-unrelated-title"]}]});
    std::fs::write(&database, user_space.to_string()).unwrap();
    let output = fixture.call("browser_status", json!({})).await;
    assert!(!output.is_error);
    assert_eq!(output.structured_facts["browser"]["spaces"], json!([]));
    assert!(!output.content.contains("private-unrelated-title"));
    assert_eq!(
        serde_json::from_slice::<Value>(&std::fs::read(database).unwrap()).unwrap(),
        user_space
    );
}

#[cfg(unix)]
#[tokio::test]
async fn launcher_zero_exit_without_execution_receipt_is_not_success() {
    let fixture = Fixture::new();
    // 原生 Ego 首次设置未完成时会返回 0，但不执行输入脚本，不能仅依赖退出码。
    std::fs::write(
        fixture.root.path().join("ego.mjs"),
        "console.log('Please complete the onboarding process first.');",
    )
    .unwrap();
    let result = fixture.call("browser_status", json!({})).await;
    assert!(result.is_error);
    assert_eq!(result.structured_facts["exit_code"], 0);
    assert_eq!(result.structured_facts["phase"], "idle");
    assert_eq!(result.structured_facts["outcome_unknown"], false);
    assert_eq!(result.structured_facts["automatic_retry"], false);
    assert!(result.content.contains("onboarding"));
    assert!(result.images.is_empty());
}

#[cfg(unix)]
#[tokio::test]
async fn browser_resumes_across_backend_recreation_and_fork_gets_own_space() {
    let fixture = Fixture::new();
    let first = fixture.call("browser_run", json!({"code":"await page.goto('https://example.test/中文'); console.log(await page.snapshot());"})).await;
    assert!(!first.is_error, "{first:?}");
    assert!(first.content.contains("中文"));
    let second = fixture
        .call(
            "browser_run",
            json!({"code":"console.log(await page.snapshot());"}),
        )
        .await;
    assert!(!second.is_error, "{second:?}");
    assert_eq!(
        first.structured_facts["space_id"],
        second.structured_facts["space_id"]
    );
    assert!(second.content.contains("example.test"));
    let child = fixture
        .backend()
        .call(
            &request(
                SessionId::new(),
                "browser_run",
                json!({"code":"console.log(await page.snapshot());"}),
            ),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_ne!(
        child.structured_facts["space_id"],
        first.structured_facts["space_id"]
    );
    assert!(!child.content.contains("example.test"));
}

#[cfg(unix)]
#[tokio::test]
async fn browser_handoff_requires_explicit_resume_and_finish_ends_binding() {
    let fixture = Fixture::new();
    let first = fixture
        .call("browser_run", json!({"code":"return 'ready'"}))
        .await;
    assert!(!first.is_error, "{first:?}");
    let handed = fixture.call("browser_handoff", json!({})).await;
    assert!(!handed.is_error, "{handed:?}");
    assert_eq!(handed.structured_facts["phase"], "waiting_for_user");
    let stopped = fixture
        .call("browser_run", json!({"code":"throw Error('must not run')"}))
        .await;
    assert!(stopped.is_error);
    assert_eq!(stopped.structured_facts["code"], "user_in_control");
    let resumed = fixture
        .call(
            "browser_run",
            json!({"code":"return 'resumed'", "resume_after_user":true}),
        )
        .await;
    assert!(!resumed.is_error, "{resumed:?}");
    assert_eq!(
        resumed.structured_facts["space_id"],
        first.structured_facts["space_id"]
    );
    assert!(
        !fixture
            .call("browser_finish", json!({"keep":[]}))
            .await
            .is_error
    );
    assert!(
        fixture
            .call("browser_finish", json!({"keep":[]}))
            .await
            .is_error
    );
    let next = fixture
        .call("browser_run", json!({"code":"return 'next goal'"}))
        .await;
    assert_ne!(
        next.structured_facts["space_id"],
        first.structured_facts["space_id"]
    );
}

#[cfg(unix)]
#[tokio::test]
#[ignore = "explicit process lifecycle stress; protocol fixture, not an Ego real-browser test"]
async fn browser_lifecycle_stress_preserves_every_receipt() {
    let evidence = tempfile::Builder::new()
        .prefix("golutra-ego-protocol-stress-")
        .tempdir()
        .unwrap()
        .keep();
    println!("protocol stress evidence: {}", evidence.display());
    let fixture = Fixture::new();
    for cycle in 0..30 {
        let steps = [
            ("browser_run", json!({"code":"return 'ready'"}), false),
            ("browser_handoff", json!({}), false),
            (
                "browser_run",
                json!({"code":"throw Error('must not execute')"}),
                true,
            ),
            (
                "browser_run",
                json!({"code":"return 'resumed'", "resume_after_user":true}),
                false,
            ),
            ("browser_finish", json!({"keep":[]}), false),
        ];
        for (index, (name, args, expected_error)) in steps.into_iter().enumerate() {
            let output = fixture.call(name, args).await;
            std::fs::write(evidence.join(format!("{cycle:02}-{index}.json")),
                serde_json::to_vec_pretty(&json!({"summary":output.summary,"content":output.content,"facts":output.structured_facts,"is_error":output.is_error})).unwrap()).unwrap();
            assert_eq!(
                output.is_error, expected_error,
                "cycle {cycle} step {index}: {output:?}"
            );
        }
    }
}

#[cfg(unix)]
#[tokio::test]
async fn browser_timeout_preserves_unknown_outcome_and_existing_page() {
    let fixture = Fixture::new();
    // 先确认网页状态已持久化；并发负载下不能假设 Node 在 400ms 内完成启动。
    let ready = fixture
        .call(
            "browser_run",
            json!({"code":"await page.goto('https://example.test/one-side-effect');"}),
        )
        .await;
    assert!(!ready.is_error, "{ready:?}");
    let code = "await new Promise(resolve => setTimeout(resolve, 10000)); await page.goto('https://example.test/must-not-run');";
    let first = fixture
        .call("browser_run", json!({"code":code,"timeout_ms":400}))
        .await;
    assert!(first.is_error, "{first:?}");
    assert_eq!(first.structured_facts["timed_out"], true);
    assert_eq!(first.structured_facts["outcome_unknown"], true);
    let blocked = fixture
        .call(
            "browser_run",
            json!({"code":"await page.goto('https://example.test/must-not-retry');"}),
        )
        .await;
    assert_eq!(blocked.structured_facts["code"], "browser_outcome_unknown");
    let second = fixture
        .call("browser_run", json!({"observation":"snapshot"}))
        .await;
    assert!(!second.is_error, "{second:?}");
    assert_eq!(second.structured_facts["previous_phase"], "outcome_unknown");
    assert!(second.content.contains("one-side-effect"));
    assert_eq!(
        first.structured_facts["space_id"],
        second.structured_facts["space_id"]
    );
}

#[cfg(unix)]
#[tokio::test]
async fn browser_images_are_real_bytes_and_guide_is_pageable() {
    let fixture = Fixture::new();
    let first = fixture
        .call(
            "browser_run",
            json!({"code":"await browser.screenshot(page);"}),
        )
        .await;
    assert!(!first.is_error, "{first:?}");
    assert_eq!(first.images.len(), 1);
    assert!(first.images[0].bytes.starts_with(b"\x89PNG"));
    let guide = fixture.call("browser_help", json!({})).await;
    assert!(!guide.is_error);
    assert!(guide.content.starts_with("Golutra integration:"));
    assert!(guide.content.chars().count() > 6000);
    assert_eq!(guide.structured_facts["next_offset"], 6000);
    let next = fixture
        .call(
            "browser_help",
            json!({"offset":6000,"guide_id":guide.structured_facts["guide_id"]}),
        )
        .await;
    assert_eq!(next.structured_facts["offset"], 6000);
    assert!(!next.content.is_empty());
}

#[cfg(unix)]
#[tokio::test]
async fn browser_runtime_persists_images_as_binary_artifacts() {
    use golutra_agent_policy::WorkspacePolicy;
    use std::sync::Arc;
    let fixture = Fixture::new();
    let runtime = crate::ToolRuntime::new(WorkspacePolicy::new(fixture.root.path()).unwrap())
        .with_unrestricted_access(true)
        .with_external_backend(Arc::new(fixture.backend()))
        .unwrap();
    let report = runtime
        .execute(
            request(
                fixture.session,
                "browser_run",
                json!({"code":"await browser.screenshot(page); console.log('image captured');"}),
            ),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(
        report.envelope.status,
        golutra_agent_core::ToolResultStatus::Ok,
        "{:?}",
        report.envelope
    );
    let artifact = report
        .artifacts
        .iter()
        .find(|artifact| artifact.artifact_type == "tool_image:image/png")
        .unwrap();
    let content = report
        .artifact_contents
        .iter()
        .find(|content| content.artifact_id == artifact.artifact_id)
        .unwrap();
    assert!(content.bytes.starts_with(b"\x89PNG"));
    assert_eq!(artifact.checksum, checksum(&content.bytes));
    assert!(
        report
            .evidence
            .iter()
            .any(|evidence| evidence.artifact_refs.contains(&artifact.artifact_id))
    );
}

#[test]
fn browser_image_receipts_cannot_read_unrelated_host_files() {
    let root = tempfile::tempdir().unwrap();
    let artifacts = root.path().join("artifacts");
    private_directory(&artifacts).unwrap();
    let secret = root.path().join("secret.png");
    std::fs::write(&secret, b"private").unwrap();
    assert!(read_images(&json!({"images":[secret]}), &artifacts).is_err());
    assert!(!workspace_key(root.path()).contains(':'));
}

#[cfg(unix)]
#[tokio::test]
async fn browser_does_not_repeat_duplicate_side_effects() {
    let fixture = Fixture::new();
    let backend = fixture.backend();
    let first_call = request(
        fixture.session,
        "browser_run",
        json!({"code":"await page.goto('https://example.test/first');"}),
    );
    assert!(
        !backend
            .call(&first_call, CancellationToken::new())
            .await
            .unwrap()
            .is_error
    );
    assert!(
        !fixture
            .call(
                "browser_run",
                json!({"code":"await page.goto('https://example.test/second');"})
            )
            .await
            .is_error
    );
    let duplicate = backend
        .call(&first_call, CancellationToken::new())
        .await
        .unwrap();
    assert!(duplicate.is_error);
    assert_eq!(duplicate.structured_facts["code"], "browser_duplicate_call");
    let observation = fixture
        .call(
            "browser_run",
            json!({"code":"console.log(await page.snapshot());"}),
        )
        .await;
    assert!(observation.content.contains("/second"));
}

#[cfg(unix)]
#[tokio::test]
async fn browser_cancellation_releases_lease_and_preserves_page() {
    let fixture = Fixture::new();
    let ready = fixture.root.path().join("ready");
    let code = format!(
        "await page.goto('https://example.test/before-cancel'); const fs = await import('node:fs'); fs.writeFileSync({}, 'ready'); await new Promise(resolve => setTimeout(resolve, 10000));",
        serde_json::to_string(&ready).unwrap()
    );
    let backend = fixture.backend();
    let call = request(fixture.session, "browser_run", json!({"code":code}));
    let cancellation = CancellationToken::new();
    let cancel_when_ready = async {
        tokio::time::timeout(Duration::from_secs(5), async {
            let mut interval = tokio::time::interval(Duration::from_millis(10));
            while !ready.is_file() {
                interval.tick().await;
            }
        })
        .await
        .expect("fixture must reach browser side effect");
        cancellation.cancel();
    };
    let (output, ()) = tokio::join!(backend.call(&call, cancellation.clone()), cancel_when_ready);
    let output = output.unwrap();
    assert_eq!(output.structured_facts["cancelled"], true);
    assert_eq!(output.structured_facts["outcome_unknown"], true);
    let resumed = fixture
        .call("browser_run", json!({"observation":"snapshot"}))
        .await;
    assert!(!resumed.is_error);
    assert!(resumed.content.contains("before-cancel"));
}

#[cfg(unix)]
#[tokio::test]
async fn ego_finish_defaults_to_native_retention_and_new_goal_does_not_reclaim_results() {
    let fixture = Fixture::new();
    let first=fixture.call("browser_run",json!({"code":"await page.goto('https://example.test/result'); await (await task.newPage()).goto('https://example.test/second');"})).await;
    assert!(!first.is_error);
    let finished = fixture.call("browser_finish", json!({})).await;
    assert!(!finished.is_error, "{finished:?}");
    assert_eq!(finished.structured_facts["phase"], "finished");
    assert_eq!(
        finished.structured_facts["browser"]["retention_policy"],
        "all"
    );
    assert_eq!(
        finished.structured_facts["browser"]["completion"]["keptManagedLabels"],
        json!(["p1", "p2"])
    );
    let next = fixture
        .call(
            "browser_run",
            json!({"code":"return 'new task'","new_goal":true}),
        )
        .await;
    assert!(!next.is_error);
    assert_ne!(
        first.structured_facts["space_id"],
        next.structured_facts["space_id"]
    );
    let released = fixture
        .call("browser_finish", json!({"release":true}))
        .await;
    assert_eq!(
        released.structured_facts["browser"]["completion"]["closedSpace"],
        true
    );
    let db: Value =
        serde_json::from_slice(&std::fs::read(fixture.root.path().join("spaces.json")).unwrap())
            .unwrap();
    assert_eq!(db["spaces"].as_array().unwrap().len(), 1);
    assert_eq!(db["spaces"][0]["id"], first.structured_facts["space_id"]);
}

#[cfg(unix)]
#[tokio::test]
async fn ego_finish_validates_before_dispatch_and_requires_native_receipt() {
    let fixture = Fixture::new();
    for args in [
        json!({"keep":null}),
        json!({"keep":[""]}),
        json!({"keep":"none"}),
        json!({"keep":["p1"],"release":true}),
    ] {
        assert!(
            fixture
                .backend()
                .call(
                    &request(fixture.session, "browser_finish", args),
                    CancellationToken::new()
                )
                .await
                .is_err()
        );
    }
    assert!(!fixture.root.path().join("spaces.json").exists());
    assert!(
        !fixture
            .call("browser_run", json!({"code":"return 1"}))
            .await
            .is_error
    );
    std::fs::write(fixture.root.path().join("missing-finish-receipt"), "").unwrap();
    let output = fixture.call("browser_finish", json!({})).await;
    assert!(output.is_error);
    assert_eq!(
        output.structured_facts["browser"]["code"],
        "EGO_FINISH_RECEIPT_INVALID"
    );
    assert_eq!(output.structured_facts["outcome_unknown"], true);
    let retry = fixture
        .call("browser_finish", json!({"release":true}))
        .await;
    assert_eq!(retry.structured_facts["code"], "browser_outcome_unknown");
    let ended = fixture
        .call("browser_run", json!({"observation":"snapshot"}))
        .await;
    assert_eq!(ended.structured_facts["phase"], "ended");
    let blocked = fixture
        .call(
            "browser_run",
            json!({"code":"throw Error('must not run')","resume_after_user":true}),
        )
        .await;
    assert_eq!(blocked.structured_facts["code"], "browser_task_ended");
}

#[cfg(unix)]
#[tokio::test]
async fn ego_selected_retention_reports_closed_and_kept_pages() {
    let fixture = Fixture::new();
    assert!(
        !fixture
            .call(
                "browser_run",
                json!({"code":"await (await task.newPage()).goto('https://example.test/result');"})
            )
            .await
            .is_error
    );
    let result = fixture.call("browser_finish", json!({"keep":["p2"]})).await;
    assert!(!result.is_error);
    assert_eq!(
        result.structured_facts["browser"]["completion"]["keptManagedLabels"],
        json!(["p2"])
    );
    assert_eq!(
        result.structured_facts["browser"]["completion"]["closedManagedLabels"],
        json!(["p1"])
    );
    assert!(result.content.contains("keptManagedLabels"));
}

#[cfg(unix)]
#[tokio::test]
async fn ego_observation_cannot_execute_arbitrary_code_or_create_a_task() {
    let fixture = Fixture::new();
    let observed = fixture
        .call("browser_run", json!({"observation":"snapshot"}))
        .await;
    assert_eq!(observed.structured_facts["code"], "no_active_browser_task");
    assert!(!fixture.root.path().join("spaces.json").exists());
    let mixed = request(
        fixture.session,
        "browser_run",
        json!({"observation":"snapshot","code":"throw Error('must not run')"}),
    );
    assert!(
        fixture
            .backend()
            .call(&mixed, CancellationToken::new())
            .await
            .is_err()
    );
    let cancel = CancellationToken::new();
    cancel.cancel();
    let result = fixture
        .backend()
        .call(
            &request(fixture.session, "browser_run", json!({"code":"return 1"})),
            cancel,
        )
        .await
        .unwrap();
    assert_eq!(result.structured_facts["code"], "ego_cancelled");
    assert!(!fixture.root.path().join("spaces.json").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn browser_cannot_launch_in_restricted_mode_or_recreate_deleted_space() {
    let fixture = Fixture::new();
    let restricted = EgoBrowserBackend::new(
        fixture.config.clone(),
        fixture.root.path(),
        &fixture.root.path().join("state"),
        false,
    )
    .unwrap();
    let blocked = restricted
        .call(
            &request(fixture.session, "browser_run", json!({"code":"return 1"})),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(blocked.is_error);
    assert!(!fixture.root.path().join("spaces.json").exists());
    assert!(
        !fixture
            .call("browser_run", json!({"code":"return 1"}))
            .await
            .is_error
    );
    std::fs::write(
        fixture.root.path().join("spaces.json"),
        r#"{"next":2,"spaces":[]}"#,
    )
    .unwrap();
    let missing = fixture
        .call("browser_run", json!({"code":"return 2"}))
        .await;
    assert!(missing.is_error);
    assert_eq!(
        missing.structured_facts["browser"]["code"],
        "EGO_SPACE_MISSING"
    );
}
