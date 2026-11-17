use super::*;
use golutra_agent_core::{SessionId, ToolCallId};

fn request(session: SessionId, tool: &str, args: Value) -> ToolRequest {
    ToolRequest {
        tool_call_id: ToolCallId::new(),
        provider_tool_call_id: None,
        session_id: session,
        turn_id: None,
        tool_name: tool.into(),
        arguments: args,
    }
}

fn fixture(root: &Path) -> OpenCliBrowserBackend {
    let executable = root.join("opencli.mjs");
    std::fs::write(&executable, include_str!("fixture.mjs")).unwrap();
    OpenCliBrowserBackend::new(
        OpenCliConfig {
            executable: Some(executable),
            ..Default::default()
        },
        root,
        &root.join("state"),
        true,
    )
    .unwrap()
}

async fn call(
    backend: &OpenCliBrowserBackend,
    session: SessionId,
    tool: &str,
    args: Value,
) -> ExternalToolOutput {
    backend
        .call(&request(session, tool, args), CancellationToken::new())
        .await
        .unwrap()
}

#[tokio::test]
async fn opencli_process_lifecycle_pins_profile_isolates_sessions_and_preserves_unknown_outcomes() {
    let root = tempfile::tempdir().unwrap();
    let backend = fixture(root.path());
    let session = SessionId::new();
    assert!(
        !call(&backend, session, "browser_status", json!({}))
            .await
            .is_error
    );
    assert!(
        call(
            &backend,
            session,
            "browser_run",
            json!({"action":"snapshot"})
        )
        .await
        .is_error
    );
    let opening = request(
        session,
        "browser_run",
        json!({"action":"open","url":"https://example.com"}),
    );
    assert!(
        !backend
            .call(&opening, CancellationToken::new())
            .await
            .unwrap()
            .is_error
    );
    let before = std::fs::read(root.path().join("calls.jsonl")).unwrap();
    assert!(
        backend
            .call(&opening, CancellationToken::new())
            .await
            .unwrap()
            .is_error
    );
    assert_eq!(
        before,
        std::fs::read(root.path().join("calls.jsonl")).unwrap()
    );
    assert!(
        !call(&backend, session, "browser_handoff", json!({}))
            .await
            .is_error
    );
    assert!(
        call(
            &backend,
            session,
            "browser_run",
            json!({"action":"fill","selector":"#x","text":"hello"})
        )
        .await
        .is_error
    );
    assert!(
        !call(
            &backend,
            session,
            "browser_run",
            json!({"action":"snapshot","resume_after_user":true})
        )
        .await
        .is_error
    );
    assert!(
        call(
            &backend,
            session,
            "browser_run",
            json!({"action":"evaluate","expression":"fail"})
        )
        .await
        .is_error
    );
    let frames = call(&backend, session, "browser_run", json!({"action":"frames"})).await;
    assert!(!frames.is_error);
    assert_eq!(frames.structured_facts["phase"], "outcome_unknown");
    assert!(
        call(
            &backend,
            session,
            "browser_run",
            json!({"action":"click","selector":"#save"})
        )
        .await
        .is_error
    );
    assert!(
        !call(
            &backend,
            session,
            "browser_run",
            json!({"action":"snapshot"})
        )
        .await
        .is_error
    );
    let shot = call(
        &backend,
        session,
        "browser_run",
        json!({"action":"screenshot"}),
    )
    .await;
    assert_eq!(shot.images.len(), 1);
    assert!(!shot.content.contains("iVBOR"));
    assert!(
        !call(&backend, session, "browser_finish", json!({"release":true}))
            .await
            .is_error
    );
    assert!(
        !call(
            &backend,
            session,
            "browser_run",
            json!({"action":"open","url":"https://example.com"})
        )
        .await
        .is_error
    );
    assert!(
        !call(
            &backend,
            SessionId::new(),
            "browser_run",
            json!({"action":"open","url":"https://example.com"})
        )
        .await
        .is_error
    );
    let calls = std::fs::read_to_string(root.path().join("calls.jsonl")).unwrap();
    let operations: Vec<Value> = calls
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .filter(|v: &Value| v.as_array().unwrap().contains(&json!("browser")))
        .collect();
    let mut sessions = std::collections::HashSet::new();
    for args in operations {
        let args = args.as_array().unwrap();
        assert_eq!(args[0], "--profile");
        assert_eq!(args[1], "profile-one");
        sessions.insert(args[4].as_str().unwrap().to_owned());
    }
    assert_eq!(sessions.len(), 3);
}

#[tokio::test]
async fn cancellation_reaps_cli_and_requires_observation_before_more_writes() {
    let root = tempfile::tempdir().unwrap();
    let backend = fixture(root.path());
    let session = SessionId::new();
    assert!(
        !call(
            &backend,
            session,
            "browser_run",
            json!({"action":"open","url":"https://example.com"})
        )
        .await
        .is_error
    );
    let token = CancellationToken::new();
    let cancel = token.clone();
    let marker = root.path().join("started");
    let cancelling = async move {
        tokio::time::timeout(Duration::from_secs(5), async {
            while !marker.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        cancel.cancel();
    };
    let request = request(
        session,
        "browser_run",
        json!({"action":"evaluate","expression":"hang"}),
    );
    let (result, ()) = tokio::join!(backend.call(&request, token), cancelling);
    assert!(result.unwrap().is_error);
    assert!(
        call(
            &backend,
            session,
            "browser_run",
            json!({"action":"click","selector":"#x"})
        )
        .await
        .is_error
    );
    assert!(
        !call(
            &backend,
            session,
            "browser_run",
            json!({"action":"snapshot"})
        )
        .await
        .is_error
    );
}

#[test]
fn arguments_keep_model_text_out_of_options_and_reject_unsupported_actions() {
    assert_eq!(
        arguments::arguments(&json!({"action":"click","selector":"a","nth":0})).unwrap(),
        vec!["click", "--nth", "0", "--", "a"]
    );
    for args in [
        json!({"action":"click","selector":"a","nth":-1}),
        json!({"action":"click","selector":"a","nth":1.5}),
        json!({"action":"evaluate","expression":"1","nth":0}),
    ] {
        assert!(arguments::arguments(&args).is_err());
    }
    assert_eq!(
        arguments::arguments(
            &json!({"action":"fill","selector":"#name","text":"--profile private"})
        )
        .unwrap(),
        vec!["fill", "--", "#name", "--profile private"]
    );
    assert!(arguments::arguments(&json!({"action":"open","url":"file:///etc/hosts"})).is_err());
    assert!(
        arguments::arguments(&json!({"action":"click","selector":"#x","frame_index":1})).is_err()
    );
    assert!(arguments::arguments(&json!({"action":"chrome","text":"tabs.remove"})).is_err());
    assert!(
        arguments::arguments(&json!({"action":"upload","selector":"input","files":["relative"]}))
            .is_err()
    );
}

#[tokio::test]
async fn finish_keeps_original_page_pauses_actions_and_releases_only_explicitly() {
    let root = tempfile::tempdir().unwrap();
    let backend = fixture(root.path());
    let session = SessionId::new();
    assert!(
        !call(
            &backend,
            session,
            "browser_run",
            json!({"action":"open","url":"https://example.com"})
        )
        .await
        .is_error
    );
    let finish = call(&backend, session, "browser_finish", json!({})).await;
    assert!(!finish.is_error);
    assert_eq!(
        finish.structured_facts["browser"]["data"]["page_retained"],
        true
    );
    assert_eq!(
        finish.structured_facts["browser"]["data"]["lease_released"],
        false
    );
    assert_eq!(finish.structured_facts["phase"], "waiting_for_user");
    assert!(
        !std::fs::read_to_string(root.path().join("calls.jsonl"))
            .unwrap()
            .contains("unbind")
    );
    assert!(
        call(
            &backend,
            session,
            "browser_run",
            json!({"action":"click","selector":"#save"})
        )
        .await
        .is_error
    );
    assert!(
        !call(
            &backend,
            session,
            "browser_run",
            json!({"action":"evaluate","expression":"1","resume_after_user":true})
        )
        .await
        .is_error
    );
    let released = call(&backend, session, "browser_finish", json!({"release":true})).await;
    assert_eq!(released.structured_facts["phase"], "finished");
    assert!(
        std::fs::read_to_string(root.path().join("calls.jsonl"))
            .unwrap()
            .contains("unbind")
    );
}

#[tokio::test]
async fn target_rejection_allows_correction_but_same_code_from_evaluate_stays_unknown() {
    let root = tempfile::tempdir().unwrap();
    let backend = fixture(root.path());
    let session = SessionId::new();
    assert!(
        !call(
            &backend,
            session,
            "browser_run",
            json!({"action":"open","url":"https://example.com"})
        )
        .await
        .is_error
    );
    let failed = call(
        &backend,
        session,
        "browser_run",
        json!({"action":"click","selector":"ambiguous"}),
    )
    .await;
    assert!(failed.is_error);
    assert_eq!(failed.structured_facts["action_dispatched"], false);
    assert_eq!(failed.structured_facts["phase"], "ready");
    assert!(
        !call(
            &backend,
            session,
            "browser_run",
            json!({"action":"evaluate","expression":"inspect"})
        )
        .await
        .is_error
    );
    assert!(
        !call(
            &backend,
            session,
            "browser_run",
            json!({"action":"click","selector":"ambiguous","nth":0})
        )
        .await
        .is_error
    );
    let failed = call(
        &backend,
        session,
        "browser_run",
        json!({"action":"evaluate","expression":"ambiguous"}),
    )
    .await;
    assert_eq!(failed.structured_facts["phase"], "outcome_unknown");
    assert!(
        call(
            &backend,
            session,
            "browser_run",
            json!({"action":"click","selector":"#save"})
        )
        .await
        .is_error
    );
}

#[test]
fn legacy_chrome_configuration_migrates_without_changing_ego_or_disabled() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("browser.json"),
        json!({"backend":"chrome","connection_dir":root.path().join("bridge")}).to_string(),
    )
    .unwrap();
    let config = super::super::connection::BrowserConfig::load(root.path())
        .unwrap()
        .unwrap();
    assert!(matches!(
        config,
        super::super::connection::BrowserConfig::Chrome(_)
    ));
    config.save(root.path()).unwrap();
    assert!(
        !std::fs::read_to_string(root.path().join("browser.json"))
            .unwrap()
            .contains("connection_dir")
    );
}

#[tokio::test]
async fn restricted_and_cancelled_calls_do_not_start_opencli() {
    let root = tempfile::tempdir().unwrap();
    let request = ToolRequest {
        tool_call_id: golutra_agent_core::ToolCallId::new(),
        provider_tool_call_id: None,
        session_id: golutra_agent_core::SessionId::new(),
        turn_id: None,
        tool_name: "browser_run".into(),
        arguments: json!({"action":"open","url":"https://example.com"}),
    };
    let backend = OpenCliBrowserBackend::new(
        OpenCliConfig::default(),
        root.path(),
        &root.path().join("state"),
        false,
    )
    .unwrap();
    assert!(
        backend
            .call(&request, CancellationToken::new())
            .await
            .unwrap()
            .is_error
    );
    let backend = OpenCliBrowserBackend::new(
        OpenCliConfig::default(),
        root.path(),
        &root.path().join("state"),
        true,
    )
    .unwrap();
    let token = CancellationToken::new();
    token.cancel();
    assert!(backend.call(&request, token).await.unwrap().is_error);
    assert!(!root.path().join("state").exists());
}
