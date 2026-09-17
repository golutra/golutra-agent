use super::*;

#[test]
fn shell_cli_environment_and_process_waiting_in_isolated_runtime() {
    let home = tempdir().unwrap();
    for (environment_policy, github) in [
        ("{}", "fixture-github"),
        (r#"{"exclude":["GITHUB_TOKEN"]}"#, ""),
    ] {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "tests::shell_integration_tests::shell_cli_child",
                "--nocapture",
            ])
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", home.path())
            .env("GOLUTRA_AGENT_SHELL_ENVIRONMENT_POLICY", environment_policy)
            .env("GOLUTRA_COMMAND_SCOPE_TOKEN", "fixture-scope")
            .env("GOLUTRA_RUNTIME_PROFILE", "dev")
            .env("GOLUTRA_COMMAND_IPC_ADDR", "fixture-ipc")
            .env("GITHUB_TOKEN", "fixture-github")
            .env("OPENAI_API_KEY", "fixture-openai")
            .env("GOLUTRA_AGENT_TRANSPORT_TOKEN", "fixture-internal")
            .env("GOLUTRA_AGENT_PROVIDER_API_KEY", "fixture-internal")
            .env("GOLUTRA_AGENT_PROVIDER_CUSTOM_HEADERS", "fixture-internal")
            .env(
                "GOLUTRA_AGENT_CUSTOM_PROVIDER_API_KEY_TEST",
                "fixture-internal",
            )
            .env("GOLUTRA_AGENT_SHELL_TEST_CHILD", "1")
            .env("FIXTURE_EXPECT_GITHUB", github)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[tokio::test]
async fn shell_cli_child() {
    if std::env::var("GOLUTRA_AGENT_SHELL_TEST_CHILD").as_deref() != Ok("1") {
        return;
    }
    let workspace = tempdir().unwrap();
    fs::write(workspace.path().join("cli.sh"), concat!(
        "set -eu\n",
        "test \"$GOLUTRA_COMMAND_SCOPE_TOKEN\" = fixture-scope\n",
        "test \"$GOLUTRA_RUNTIME_PROFILE\" = dev\n",
        "test \"$GOLUTRA_COMMAND_IPC_ADDR\" = fixture-ipc\n",
        "test \"${GITHUB_TOKEN-}\" = \"$FIXTURE_EXPECT_GITHUB\"\n",
        "test \"$OPENAI_API_KEY\" = fixture-openai\n",
        "test -z \"${GOLUTRA_AGENT_TRANSPORT_TOKEN+x}${GOLUTRA_AGENT_PROVIDER_API_KEY+x}${GOLUTRA_AGENT_PROVIDER_CUSTOM_HEADERS+x}${GOLUTRA_AGENT_CUSTOM_PROVIDER_API_KEY_TEST+x}\"\n",
        "if test \"${1-}\" = wait; then\n",
        "  printf x >> launches\n",
        "  read -r value\n",
        "  test \"$value\" = finish\n",
        "fi\n",
        "printf '%s\\n' '{\"ok\":true,\"result\":{\"status\":\"error\",\"message\":\"fixture business failure\",\"data\":{\"requestId\":\"external-fixture\"}}}'\n",
    )).unwrap();
    let executor = executor(workspace.path()).with_sandbox(SystemSandbox::process_only());
    let foreground = execute_approved(
        &executor,
        request("shell", json!({"argv":["/bin/sh","cli.sh"]})),
        CancellationToken::new(),
    )
    .await;
    assert_business_failure_is_process_success(&foreground);

    let session = SessionId::new();
    let start = execute_approved(
        &executor,
        request_for_session(
            session,
            "shell",
            json!({"argv":["/bin/sh","cli.sh","wait"],"background":true}),
        ),
        CancellationToken::new(),
    )
    .await;
    assert_eq!(start.envelope.structured_facts["terminal"], false);
    let process = start.envelope.structured_facts["process_id"].clone();
    let pid = start.envelope.structured_facts["authoritative_pid"].clone();
    let mut cursor = start.envelope.structured_facts["output_cursor"].clone();
    for _ in 0..2 {
        let waiting = executor.execute(request_for_session(session, "shell_session", json!({"action":"wait","process_id":process,"authoritative_pid":pid,"cursor":cursor,"wait_ms":10,"wait_for_terminal":true})), CancellationToken::new()).await.unwrap();
        assert_eq!(waiting.envelope.structured_facts["terminal"], false);
        assert_eq!(waiting.envelope.structured_facts["process_id"], process);
        cursor = waiting.envelope.structured_facts["output_cursor"].clone();
    }
    let wrong_id = executor.execute(request_for_session(session, "shell_session", json!({"action":"wait","process_id":"external-fixture","authoritative_pid":pid,"wait_ms":1})), CancellationToken::new()).await;
    assert!(wrong_id.is_err());

    let write_request = request_for_session(
        session,
        "shell_session",
        json!({"action":"write","process_id":process,"authoritative_pid":pid,"cursor":cursor,"input":"finish\n","wait_ms":1}),
    );
    let policy = executor.evaluate(&write_request).unwrap();
    executor
        .execute_with_policy(write_request, policy, true, CancellationToken::new())
        .await
        .unwrap();
    let finished = executor.execute(request_for_session(session, "shell_session", json!({"action":"wait","process_id":process,"authoritative_pid":pid,"cursor":0,"wait_ms":5000,"wait_for_terminal":true})), CancellationToken::new()).await.unwrap();
    assert_eq!(finished.envelope.structured_facts["terminal"], true);
    assert_business_failure_is_process_success(&finished);
    assert_eq!(
        fs::read_to_string(workspace.path().join("launches")).unwrap(),
        "x"
    );
    let next = finished.envelope.structured_facts["output_cursor"].clone();
    let reread = executor.execute(request_for_session(session, "shell_session", json!({"action":"wait","process_id":process,"authoritative_pid":pid,"cursor":next,"wait_ms":1})), CancellationToken::new()).await.unwrap();
    assert!(artifact_text(&reread).is_empty());
    assert_eq!(reread.envelope.structured_facts["exit_code"], 0);
}

fn assert_business_failure_is_process_success(report: &ToolExecutionReport) {
    assert_eq!(report.envelope.status, ToolResultStatus::Ok);
    assert_eq!(report.envelope.structured_facts["exit_code"], 0);
    let output: Value = serde_json::from_str(&artifact_text(report)).unwrap();
    assert_eq!(output["ok"], true);
    assert_eq!(output["result"]["status"], "error");
    assert!(
        report
            .envelope
            .model_visible_excerpt
            .as_deref()
            .unwrap()
            .contains("fixture business failure")
    );
}
