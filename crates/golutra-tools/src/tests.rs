use std::{fs, process::Stdio, time::Duration};

use golutra_policy::WorkspacePolicy;
use tempfile::tempdir;
use tokio::process::Command;

use super::*;

#[tokio::test]
async fn registry_contains_p0_tools() {
    let registry = ToolRegistry::p0_default();
    let names = registry
        .contracts()
        .into_iter()
        .map(|contract| contract.tool_name.as_str())
        .collect::<Vec<_>>();

    assert_eq!(
        names,
        vec![
            "edit_file",
            "list_dir",
            "read_file",
            "rg_search",
            "shell",
            "write_file"
        ]
    );
}

#[test]
fn redaction_covers_json_assignments_headers_and_prefixed_tokens() {
    let raw = concat!(
        "{\"api_key\":\"plain-secret-value\"}\n",
        "Authorization: Bearer plain-secret-value\n",
        "TOKEN=plain-secret-value\n",
        "response sk-1234567890abcdef\n",
    );

    let (redacted, status) = redact_sensitive_text(raw);

    assert_eq!(status, RedactionStatus::Redacted);
    assert_eq!(redacted.matches("<redacted-secret>").count(), 4);
    assert!(!redacted.contains("plain-secret-value"));
    assert!(!redacted.contains("sk-1234567890abcdef"));
}

#[tokio::test]
async fn shell_policy_and_structured_facts_do_not_persist_secret_arguments() {
    let workspace = tempdir().expect("workspace");
    let executor = executor(workspace.path());
    let request = request(
        "shell",
        json!({"command": "printf '%s' API_KEY=plain-secret-value"}),
    );
    let policy = executor.evaluate(&request).expect("policy");
    assert!(!policy.resource.contains("plain-secret-value"));

    let report = executor
        .execute_with_policy(request, policy, true, CancellationToken::new())
        .await
        .expect("shell report");

    assert!(
        !report
            .envelope
            .structured_facts
            .to_string()
            .contains("plain-secret-value")
    );
    assert!(
        !report.artifact_contents[0]
            .bytes
            .windows("plain-secret-value".len())
            .any(|window| window == b"plain-secret-value")
    );
}

#[tokio::test]
async fn read_file_returns_envelope_artifact_and_evidence() {
    let workspace = tempdir().expect("workspace");
    fs::write(workspace.path().join("README.md"), "hello").expect("fixture");
    let executor = executor(workspace.path());

    let report = executor
        .execute(
            request("read_file", json!({"path": "README.md"})),
            CancellationToken::new(),
        )
        .await
        .expect("tool runs");

    assert_eq!(report.envelope.status, ToolResultStatus::Ok);
    assert_eq!(report.artifacts.len(), 1);
    assert_eq!(report.evidence.len(), 1);
}

#[tokio::test]
async fn write_file_records_changed_file() {
    let workspace = tempdir().expect("workspace");
    let executor = executor(workspace.path());

    let report = executor
        .execute(
            request("write_file", json!({"path": "src.txt", "content": "new"})),
            CancellationToken::new(),
        )
        .await
        .expect("tool runs");

    assert_eq!(report.envelope.status, ToolResultStatus::Ok);
    assert_eq!(report.changed_files.len(), 1);
    assert_eq!(
        fs::read_to_string(workspace.path().join("src.txt")).unwrap(),
        "new"
    );
}

#[tokio::test]
async fn edit_file_refuses_to_overwrite_changes_made_after_before_image() {
    let workspace = tempdir().expect("workspace");
    let path = workspace.path().join("src.txt");
    fs::write(&path, "before").expect("before");
    let executor = executor(workspace.path());
    let request = request(
        "edit_file",
        json!({"path": "src.txt", "search": "before", "replace": "agent"}),
    );
    let policy = executor.evaluate(&request).expect("policy");
    let before_images = executor
        .prepare_side_effect(&request)
        .await
        .expect("before image");
    fs::write(&path, "external change").expect("external update");

    let report = executor
        .execute_with_policy_and_before_images(
            request,
            policy,
            false,
            CancellationToken::new(),
            before_images,
        )
        .await
        .expect("conflict report");

    assert_eq!(report.envelope.status, ToolResultStatus::Error);
    assert_eq!(
        report.envelope.summary,
        "edit target changed after checkpoint"
    );
    assert_eq!(fs::read_to_string(path).unwrap(), "external change");
}

#[tokio::test]
async fn edit_file_checkpoints_even_when_the_initial_search_does_not_match() {
    let workspace = tempdir().expect("workspace");
    let path = workspace.path().join("src.txt");
    fs::write(&path, "initial").expect("initial");
    let executor = executor(workspace.path());
    let request = request(
        "edit_file",
        json!({"path": "src.txt", "search": "later", "replace": "agent"}),
    );
    let policy = executor.evaluate(&request).expect("policy");
    let before_images = executor
        .prepare_side_effect(&request)
        .await
        .expect("before image");
    assert_eq!(before_images.len(), 1);
    fs::write(&path, "later").expect("external update");

    let report = executor
        .execute_with_policy_and_before_images(
            request,
            policy,
            false,
            CancellationToken::new(),
            before_images,
        )
        .await
        .expect("conflict report");

    assert_eq!(report.envelope.status, ToolResultStatus::Error);
    assert_eq!(fs::read_to_string(path).unwrap(), "later");
}

#[tokio::test]
async fn blocks_workspace_escape() {
    let workspace = tempdir().expect("workspace");
    let outside = tempdir().expect("outside");
    let outside_file = outside.path().join("secret.txt");
    fs::write(&outside_file, "secret").expect("fixture");
    let executor = executor(workspace.path());

    let report = executor
        .execute(
            request("read_file", json!({"path": outside_file.to_string_lossy()})),
            CancellationToken::new(),
        )
        .await
        .expect("tool runs");

    assert_eq!(report.envelope.status, ToolResultStatus::Blocked);
}

#[cfg(unix)]
#[tokio::test]
async fn rechecks_a_path_after_policy_evaluation_before_writing() {
    use std::os::unix::fs::symlink;

    let workspace = tempdir().expect("workspace");
    let outside = tempdir().expect("outside");
    fs::create_dir(workspace.path().join("inside")).expect("inside");
    let link = workspace.path().join("target");
    symlink(workspace.path().join("inside"), &link).expect("inside symlink");
    let executor = executor(workspace.path());
    let request = request(
        "write_file",
        json!({"path": "target/output.txt", "content": "blocked"}),
    );
    let policy = executor.evaluate(&request).expect("initial policy");
    fs::remove_file(&link).expect("remove symlink");
    symlink(outside.path(), &link).expect("outside symlink");

    let result = executor
        .execute_with_policy(request, policy, false, CancellationToken::new())
        .await;

    assert!(matches!(result, Err(ToolError::Execution(_))));
    assert!(!outside.path().join("output.txt").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn refuses_a_workspace_symlink_target_changed_after_checkpoint() {
    use std::os::unix::fs::symlink;

    let workspace = tempdir().expect("workspace");
    let first = workspace.path().join("first");
    let second = workspace.path().join("second");
    fs::create_dir(&first).expect("first");
    fs::create_dir(&second).expect("second");
    let link = workspace.path().join("target");
    symlink(&first, &link).expect("first symlink");
    let executor = executor(workspace.path());
    let request = request(
        "write_file",
        json!({"path": "target/output.txt", "content": "blocked"}),
    );
    let policy = executor.evaluate(&request).expect("initial policy");
    let before_images = executor
        .prepare_side_effect(&request)
        .await
        .expect("before image");
    fs::remove_file(&link).expect("remove symlink");
    symlink(&second, &link).expect("second symlink");

    let report = executor
        .execute_with_policy_and_before_images(
            request,
            policy,
            false,
            CancellationToken::new(),
            before_images,
        )
        .await
        .expect("conflict report");

    assert_eq!(report.envelope.status, ToolResultStatus::Error);
    assert!(!first.join("output.txt").exists());
    assert!(!second.join("output.txt").exists());
}

#[tokio::test]
async fn rg_pattern_starting_with_dash_is_not_treated_as_an_option() {
    if Command::new("rg")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .is_err()
    {
        return;
    }
    let workspace = tempdir().expect("workspace");
    fs::write(workspace.path().join("input.txt"), "--pre=sh\n").expect("input");
    let executor = executor(workspace.path());

    let report = executor
        .execute(
            request("rg_search", json!({"pattern": "--pre=sh", "path": "."})),
            CancellationToken::new(),
        )
        .await
        .expect("rg search");

    assert_eq!(report.envelope.status, ToolResultStatus::Ok);
    assert!(
        report
            .envelope
            .model_visible_excerpt
            .as_deref()
            .is_some_and(|output| output.contains("--pre=sh"))
    );
}

#[tokio::test]
async fn rejects_files_larger_than_the_tool_content_limit() {
    let workspace = tempdir().expect("workspace");
    let path = workspace.path().join("large.bin");
    let file = fs::File::create(&path).expect("large file");
    file.set_len(MAX_FILE_CONTENT_BYTES + 1)
        .expect("sparse file");
    let executor = executor(workspace.path());

    let result = executor
        .execute(
            request("read_file", json!({"path": "large.bin"})),
            CancellationToken::new(),
        )
        .await;

    assert!(matches!(result, Err(ToolError::Execution(_))));
}

#[tokio::test]
async fn shell_rejects_metacharacters_before_execution() {
    let workspace = tempdir().expect("workspace");
    let executor = executor(workspace.path());

    let report = executor
        .execute(
            request("shell", json!({"command": "echo ok; cat .env"})),
            CancellationToken::new(),
        )
        .await
        .expect("tool runs");

    assert_eq!(report.envelope.status, ToolResultStatus::Blocked);
}

#[tokio::test]
async fn shell_runs_simple_command_without_shell_interpreter() {
    let workspace = tempdir().expect("workspace");
    let executor = executor(workspace.path());

    let report = execute_approved(
        &executor,
        request("shell", json!({"command": "echo ok"})),
        CancellationToken::new(),
    )
    .await;

    assert_eq!(report.envelope.status, ToolResultStatus::Ok);
    assert_eq!(
        report.envelope.model_visible_excerpt.as_deref(),
        Some("ok\n")
    );
}

#[tokio::test]
async fn shell_parser_preserves_quoted_arguments() {
    let workspace = tempdir().expect("workspace");
    let executor = executor(workspace.path());

    let report = execute_approved(
        &executor,
        request("shell", json!({"command": "printf '%s' 'hello world'"})),
        CancellationToken::new(),
    )
    .await;

    assert_eq!(report.envelope.status, ToolResultStatus::Ok);
    assert_eq!(
        report.envelope.model_visible_excerpt.as_deref(),
        Some("hello world")
    );
}

#[test]
fn shell_parser_rejects_unclosed_quotes() {
    assert!(matches!(
        CommandLine::parse("printf 'unterminated"),
        Err(ToolError::InvalidArguments(_))
    ));
}

#[tokio::test]
async fn shell_timeout_and_cancellation_have_distinct_statuses() {
    let workspace = tempdir().expect("workspace");
    let executor = executor(workspace.path());
    let timed_out = execute_approved(
        &executor,
        request("shell", json!({"command": "sleep 1", "timeout_ms": 20})),
        CancellationToken::new(),
    )
    .await;
    assert_eq!(timed_out.envelope.status, ToolResultStatus::Timeout);

    let cancellation = CancellationToken::new();
    let cancel_from_task = cancellation.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(20)).await;
        cancel_from_task.cancel();
    });
    let cancelled = execute_approved(
        &executor,
        request("shell", json!({"command": "sleep 1"})),
        cancellation,
    )
    .await;
    assert_eq!(cancelled.envelope.status, ToolResultStatus::Cancelled);
}

#[cfg(unix)]
#[tokio::test]
async fn process_cancellation_terminates_descendants_and_drains_pipes() {
    let workspace = tempdir().expect("workspace");
    let cancellation = CancellationToken::new();
    let cancel_from_task = cancellation.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(20)).await;
        cancel_from_task.cancel();
    });

    let output = tokio::time::timeout(
        Duration::from_secs(1),
        run_process(
            "sh",
            &["-c".to_owned(), "sleep 5 & wait".to_owned()],
            workspace.path(),
            DEFAULT_TIMEOUT_MS,
            cancellation,
        ),
    )
    .await
    .expect("process tree terminates promptly")
    .expect("process result");

    assert!(output.cancelled);
}

#[cfg(unix)]
#[tokio::test]
async fn process_output_is_bounded_while_pipe_is_drained() {
    let workspace = tempdir().expect("workspace");
    let output = tokio::time::timeout(
        Duration::from_secs(1),
        run_process(
            "sh",
            &["-c".to_owned(), "yes output".to_owned()],
            workspace.path(),
            20,
            CancellationToken::new(),
        ),
    )
    .await
    .expect("unbounded output process terminates promptly")
    .expect("process result");

    assert!(output.timed_out);
    assert!(output.raw_output.len() <= MAX_PIPE_OUTPUT_BYTES * 2 + 128);
}

#[tokio::test]
async fn pipe_reader_marks_output_that_exceeds_its_bound() {
    let input = vec![b'x'; MAX_PIPE_OUTPUT_BYTES + 1];
    let output = join_pipe_reader(spawn_pipe_reader(std::io::Cursor::new(input)))
        .await
        .expect("pipe reader result");

    assert!(output.len() <= MAX_PIPE_OUTPUT_BYTES + 32);
    assert!(output.contains("[process output truncated]"));
}

#[tokio::test]
async fn rejects_arguments_that_do_not_match_tool_contract() {
    let workspace = tempdir().expect("workspace");
    let executor = executor(workspace.path());

    let error = executor
        .execute(
            request("write_file", json!({"path": "src.txt", "unexpected": true})),
            CancellationToken::new(),
        )
        .await
        .expect_err("invalid arguments are rejected");

    assert!(matches!(error, ToolError::InvalidArguments(_)));

    let error = executor
        .execute(
            request(
                "shell",
                json!({"command": "x".repeat(MAX_SHELL_COMMAND_CHARS + 1)}),
            ),
            CancellationToken::new(),
        )
        .await
        .expect_err("oversized shell command is rejected");
    assert!(matches!(error, ToolError::InvalidArguments(_)));
}

#[tokio::test]
async fn required_paths_patterns_and_edit_search_must_not_be_empty() {
    let workspace = tempdir().expect("workspace");
    fs::write(workspace.path().join("src.txt"), "content").expect("fixture");
    let executor = executor(workspace.path());

    for invalid in [
        request("read_file", json!({"path": ""})),
        request("write_file", json!({"path": "", "content": ""})),
        request(
            "edit_file",
            json!({"path": "src.txt", "search": "", "replace": "replacement"}),
        ),
        request("rg_search", json!({"pattern": ""})),
    ] {
        assert!(matches!(
            executor.evaluate(&invalid),
            Err(ToolError::InvalidArguments(_))
        ));
    }

    let empty_file = request("write_file", json!({"path": "empty.txt", "content": ""}));
    assert!(executor.evaluate(&empty_file).is_ok());
}

#[test]
fn validation_errors_mask_argument_values() {
    let workspace = tempdir().expect("workspace");
    let executor = executor(workspace.path());
    let secret = "plain-secret-value".repeat(MAX_PATH_ARGUMENT_CHARS);

    let error = executor
        .evaluate(&request("read_file", json!({"path": secret})))
        .expect_err("oversized path is rejected")
        .to_string();

    assert!(!error.contains("plain-secret-value"));
    assert!(error.contains("<redacted-value>"));
}

#[test]
fn structured_facts_are_recursively_redacted() {
    let request = request("read_file", json!({"path": "README.md"}));
    let policy = execution_policy(&request, PolicyDecision::Allow, "test");
    let report = success_report(
        request,
        "token=plain-secret-value",
        json!({
            "nested": {
                "api_key": "plain-secret-value",
                "token_usage": {"total_tokens": 42},
                "messages": ["response sk-1234567890abcdef"]
            }
        }),
        String::new(),
        Vec::new(),
        policy,
    );
    let serialized = serde_json::to_string(&report.envelope).expect("envelope");

    assert!(!serialized.contains("plain-secret-value"));
    assert!(!serialized.contains("sk-1234567890abcdef"));
    assert_eq!(
        report.envelope.structured_facts["nested"]["api_key"],
        "<redacted-secret>"
    );
    assert_eq!(
        report.envelope.structured_facts["nested"]["token_usage"]["total_tokens"],
        42
    );
}

#[tokio::test]
async fn ask_policy_requires_explicit_approval() {
    let workspace = tempdir().expect("workspace");
    let executor = executor(workspace.path());
    let tool_request = request("shell", json!({"command": "echo ok"}));

    let report = executor
        .execute(tool_request, CancellationToken::new())
        .await
        .expect("tool evaluates");

    assert_eq!(report.envelope.status, ToolResultStatus::Blocked);
    assert_eq!(report.policy_evaluation.decision, PolicyDecision::Ask);
}

async fn execute_approved(
    executor: &BasicToolExecutor,
    request: ToolRequest,
    cancellation: CancellationToken,
) -> ToolExecutionReport {
    let policy = executor.evaluate(&request).expect("policy evaluates");
    assert_eq!(policy.decision, PolicyDecision::Ask);
    executor
        .execute_with_policy(request, policy, true, cancellation)
        .await
        .expect("approved tool runs")
}

fn executor(path: &Path) -> BasicToolExecutor {
    BasicToolExecutor::new(WorkspacePolicy::new(path).expect("policy"))
}

fn request(tool_name: &str, arguments: Value) -> ToolRequest {
    ToolRequest {
        tool_call_id: ToolCallId::new(),
        session_id: SessionId::new(),
        turn_id: Some(TurnId::new()),
        tool_name: tool_name.to_owned(),
        arguments,
    }
}
