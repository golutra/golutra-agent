use std::{
    fs,
    process::Stdio,
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};

use golutra_policy::WorkspacePolicy;
use tempfile::tempdir;
use tokio::process::Command;

use super::*;

#[derive(Debug)]
struct FakeExternalBackend {
    calls: AtomicUsize,
    delay: Duration,
    output: ExternalToolOutput,
}

impl FakeExternalBackend {
    fn successful(delay: Duration) -> Self {
        Self {
            calls: AtomicUsize::new(0),
            delay,
            output: ExternalToolOutput {
                summary: "external response".to_owned(),
                content: "token=plain-secret-value\nexternal output".to_owned(),
                structured_facts: json!({"provider": "fixture", "api_key": "secret"}),
                is_error: false,
            },
        }
    }
}

#[async_trait]
impl ExternalToolBackend for FakeExternalBackend {
    fn contracts(&self) -> Vec<ToolContract> {
        vec![contract(
            "mcp__fixture__echo",
            SideEffectType::ExternalSystem,
        )]
    }

    async fn call(
        &self,
        _request: &ToolRequest,
        cancellation: CancellationToken,
    ) -> Result<ExternalToolOutput, ToolError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        tokio::select! {
            () = cancellation.cancelled() => Err(ToolError::Execution(
                "external backend cancelled".to_owned(),
            )),
            () = tokio::time::sleep(self.delay) => Ok(self.output.clone()),
        }
    }
}

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
            "find_references",
            "list_dir",
            "process_poll",
            "process_reconnect",
            "process_terminate",
            "process_write",
            "read_file",
            "rg_search",
            "shell",
            "symbol_search",
            "write_file"
        ]
    );
}

#[tokio::test]
async fn external_tools_require_approval_and_redact_output() {
    let workspace = tempdir().expect("workspace");
    let backend = Arc::new(FakeExternalBackend::successful(Duration::ZERO));
    let executor = executor(workspace.path())
        .with_external_backend(backend.clone())
        .expect("external backend registers");
    let tool_request = request("mcp__fixture__echo", json!({}));

    let policy = executor.evaluate(&tool_request).expect("policy");
    assert_eq!(policy.decision, PolicyDecision::Ask);
    assert_eq!(policy.resource, "external-tool:mcp__fixture__echo");

    let blocked = executor
        .execute(tool_request.clone(), CancellationToken::new())
        .await
        .expect("unapproved call returns report");
    assert_eq!(blocked.envelope.status, ToolResultStatus::Blocked);
    assert_eq!(backend.calls.load(Ordering::SeqCst), 0);

    let approved = executor
        .execute_with_policy(tool_request, policy, true, CancellationToken::new())
        .await
        .expect("approved call runs");
    assert_eq!(approved.envelope.status, ToolResultStatus::Ok);
    assert_eq!(approved.envelope.risk, "external_mcp_tool");
    assert_eq!(
        approved.envelope.structured_facts["workspace_changes_known"],
        false
    );
    assert_eq!(backend.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        approved.envelope.structured_facts["api_key"],
        "<redacted-secret>"
    );
    assert!(
        !String::from_utf8_lossy(&approved.artifact_contents[0].bytes)
            .contains("plain-secret-value")
    );
}

#[tokio::test]
async fn external_tool_timeout_returns_a_terminal_envelope() {
    let workspace = tempdir().expect("workspace");
    let backend = Arc::new(FakeExternalBackend::successful(Duration::from_millis(
        EXTERNAL_TOOL_TIMEOUT_MS + 100,
    )));
    let executor = executor(workspace.path())
        .with_external_backend(backend)
        .expect("external backend registers");
    let tool_request = request("mcp__fixture__echo", json!({}));
    let policy = executor.evaluate(&tool_request).expect("policy");

    let report = executor
        .execute_with_policy(tool_request, policy, true, CancellationToken::new())
        .await
        .expect("timeout returns report");

    assert_eq!(report.envelope.status, ToolResultStatus::Timeout);
    assert_eq!(report.envelope.risk, "external_mcp_tool");
}

#[tokio::test]
async fn code_intelligence_tools_find_symbols_and_references() {
    let workspace = tempdir().expect("workspace");
    fs::write(
        workspace.path().join("lib.rs"),
        "pub struct RuntimeHost; fn attach(host: RuntimeHost) { let _ = host; }",
    )
    .expect("source");
    let executor = BasicToolExecutor::new(WorkspacePolicy::new(workspace.path()).expect("policy"));

    let symbols = executor
        .execute(
            request("symbol_search", json!({"query": "RuntimeHost"})),
            CancellationToken::new(),
        )
        .await
        .expect("symbol search");
    let references = executor
        .execute(
            request("find_references", json!({"symbol": "RuntimeHost"})),
            CancellationToken::new(),
        )
        .await
        .expect("reference search");

    assert_eq!(symbols.envelope.status, ToolResultStatus::Ok);
    assert_eq!(symbols.envelope.structured_facts["matches"], json!(1));
    assert_eq!(references.envelope.status, ToolResultStatus::Ok);
    assert!(references.envelope.structured_facts["references"] == json!(1));
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

#[test]
fn tool_started_arguments_preserve_invocation_fields_and_redact_secrets() {
    let projected = redact_tool_arguments(&json!({
        "path": "src/runtime.rs",
        "command": "printf API_KEY=plain-secret-value",
        "pattern": "RuntimeHost",
        "query": "runtime host",
        "symbol": "RuntimeHost::run",
        "timeout_ms": 5_000,
        "api_key": "plain-secret-value",
    }));

    assert_eq!(projected["path"], "src/runtime.rs");
    assert_eq!(projected["pattern"], "RuntimeHost");
    assert_eq!(projected["query"], "runtime host");
    assert_eq!(projected["symbol"], "RuntimeHost::run");
    assert_eq!(projected["timeout_ms"], 5_000);
    assert_eq!(projected["api_key"], "<redacted-secret>");
    let serialized = serde_json::to_string(&projected).expect("serialize projected arguments");
    assert!(!serialized.contains("plain-secret-value"));
}

#[test]
fn tool_started_arguments_summarize_payloads_and_enforce_a_hard_limit() {
    let projected = redact_tool_arguments(&json!({
        "path": "src/runtime.rs",
        "content": "x".repeat(MAX_TOOL_ARGUMENT_DISPLAY_BYTES * 8),
        "search": "old".repeat(MAX_TOOL_ARGUMENT_DISPLAY_BYTES),
        "replace": "new".repeat(MAX_TOOL_ARGUMENT_DISPLAY_BYTES),
        "metadata": (0..256)
            .map(|index| {
                (
                    format!("key-{index}"),
                    Value::String("value".repeat(512)),
                )
            })
            .collect::<serde_json::Map<String, Value>>(),
    }));

    assert_eq!(projected["path"], "src/runtime.rs");
    assert!(
        projected["content"]
            .as_str()
            .is_some_and(|value| value.contains("omitted"))
    );
    assert!(
        serde_json::to_vec(&projected)
            .expect("serialize projected arguments")
            .len()
            <= MAX_TOOL_ARGUMENT_DISPLAY_BYTES
    );
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

#[tokio::test]
async fn shell_progress_and_terminal_metrics_share_one_tool_call() {
    let workspace = tempdir().expect("workspace");
    fs::write(
        workspace.path().join("progress.sh"),
        "printf one\nsleep 0.06\nprintf two >&2\nsleep 0.06\nprintf three\n",
    )
    .expect("progress script writes");
    let executor = executor(workspace.path());
    let request = request("shell", json!({"command": "sh progress.sh"}));
    let tool_call_id = request.tool_call_id;
    let policy = executor.evaluate(&request).expect("policy evaluates");
    let mut progress = Vec::new();

    let report = executor
        .execute_with_policy_and_before_images_with_progress(
            request,
            policy,
            true,
            CancellationToken::new(),
            Vec::new(),
            Some(&mut |event| progress.push(event)),
        )
        .await
        .expect("shell executes");

    assert_eq!(report.envelope.tool_call_id, tool_call_id);
    assert_eq!(report.metrics.exit_code, Some(0));
    assert_eq!(report.metrics.output_bytes, 11);
    assert_eq!(report.metrics.output_lines, 2);
    assert!(!report.metrics.output_truncated);
    assert_eq!(
        progress.first().map(|event| event.phase),
        Some(ToolProgressPhase::Started)
    );
    assert_eq!(
        progress.last().map(|event| event.phase),
        Some(ToolProgressPhase::Completed)
    );
    assert!(
        progress
            .iter()
            .all(|event| event.tool_call_id == tool_call_id)
    );
    let output_samples = progress
        .iter()
        .filter(|event| event.phase == ToolProgressPhase::Output)
        .collect::<Vec<_>>();
    assert!(output_samples.len() >= 2);
    assert!(output_samples.windows(2).all(|samples| {
        samples[0].output_bytes <= samples[1].output_bytes
            && samples[0].output_lines <= samples[1].output_lines
    }));
    let final_sample = output_samples.last().expect("terminal output sample");
    assert_eq!(final_sample.output_bytes, report.metrics.output_bytes);
    assert_eq!(final_sample.output_lines, report.metrics.output_lines);
}

#[tokio::test]
async fn shell_reports_workspace_file_side_effects_from_a_bounded_scan() {
    let workspace = tempdir().expect("workspace");
    let executor = executor(workspace.path());

    let report = execute_approved(
        &executor,
        request("shell", json!({"command": "touch created.txt"})),
        CancellationToken::new(),
    )
    .await;

    assert_eq!(report.envelope.status, ToolResultStatus::Ok);
    assert_eq!(
        report.changed_files,
        vec![
            workspace
                .path()
                .canonicalize()
                .expect("canonical workspace")
                .join("created.txt")
        ]
    );
    assert!(
        report
            .before_images
            .iter()
            .any(|image| image.path.ends_with("created.txt") && image.content.is_none())
    );
    assert_eq!(
        report.envelope.structured_facts["workspace_changes_known"],
        true
    );
    assert!(report.after_images.iter().any(|image| {
        image.path.ends_with("created.txt") && image.content.as_deref() == Some(b"".as_slice())
    }));
}

#[tokio::test]
async fn shell_reuses_the_persisted_preparation_as_its_diff_baseline() {
    let workspace = tempdir().expect("workspace");
    fs::write(workspace.path().join("tracked.txt"), "before").expect("tracked fixture");
    fs::write(
        workspace.path().join("mutate.sh"),
        "printf after > tracked.txt\n",
    )
    .expect("mutation script");
    let executor = executor(workspace.path());
    let request = request("shell", json!({"command": "sh mutate.sh"}));
    let policy = executor.evaluate(&request).expect("policy evaluates");
    let preparation = executor
        .prepare_side_effect_snapshot(&request)
        .await
        .expect("preparation snapshot");

    fs::write(workspace.path().join("tracked.txt"), "between").expect("concurrent mutation");
    let report = executor
        .execute_with_policy_and_preparation_with_progress(
            request,
            policy,
            true,
            CancellationToken::new(),
            preparation,
            None,
        )
        .await
        .expect("shell executes");

    let before = report
        .before_images
        .iter()
        .find(|image| image.path.ends_with("tracked.txt"))
        .expect("tracked before-image");
    let after = report
        .after_images
        .iter()
        .find(|image| image.path.ends_with("tracked.txt"))
        .expect("tracked after-image");
    assert_eq!(before.content.as_deref(), Some(b"before".as_slice()));
    assert_eq!(after.content.as_deref(), Some(b"after".as_slice()));
}

#[tokio::test]
async fn background_process_supports_cursor_reconnect_stdin_and_terminal_diff() {
    let workspace = tempdir().expect("workspace");
    fs::write(
        workspace.path().join("interactive.sh"),
        concat!(
            "printf 'first\\n'\n",
            "sleep 0.05\n",
            "read value\n",
            "printf 'second:%s\\n' \"$value\"\n",
            "printf changed > background.txt\n",
        ),
    )
    .expect("interactive script");
    let executor = executor(workspace.path());
    let session_id = SessionId::new();
    let start = execute_approved(
        &executor,
        request_for_session(
            session_id,
            "shell",
            json!({
                "command": "sh interactive.sh",
                "background": true,
                "yield_time_ms": 1_000,
            }),
        ),
        CancellationToken::new(),
    )
    .await;
    assert_eq!(start.envelope.structured_facts["process_state"], "running");
    let process_id = start.envelope.structured_facts["process_id"]
        .as_str()
        .expect("process id")
        .to_owned();
    let first_cursor = start.envelope.structured_facts["output_cursor"]
        .as_u64()
        .expect("output cursor");
    assert!(first_cursor > 0);
    assert!(artifact_text(&start).contains("first"));

    let reconnect = executor
        .execute(
            request_for_session(
                session_id,
                "process_reconnect",
                json!({"process_id": process_id, "cursor": first_cursor}),
            ),
            CancellationToken::new(),
        )
        .await
        .expect("reconnect");
    assert!(artifact_text(&reconnect).is_empty());

    let isolated = executor
        .execute(
            request_for_session(
                SessionId::new(),
                "process_poll",
                json!({"process_id": process_id, "cursor": first_cursor, "wait_ms": 0}),
            ),
            CancellationToken::new(),
        )
        .await;
    assert!(matches!(isolated, Err(ToolError::Execution(_))));

    let write_request = request_for_session(
        session_id,
        "process_write",
        json!({
            "process_id": process_id,
            "input": "hello-from-stdin\n",
            "cursor": first_cursor,
            "wait_ms": 1_000,
        }),
    );
    let write_policy = executor.evaluate(&write_request).expect("write policy");
    assert!(!write_policy.resource.contains("hello-from-stdin"));
    let write = executor
        .execute_with_policy(write_request, write_policy, false, CancellationToken::new())
        .await
        .expect("write stdin");
    assert!(!artifact_text(&write).contains("first"));
    assert!(artifact_text(&write).contains("second:hello-from-stdin"));

    let mut cursor = write.envelope.structured_facts["output_cursor"]
        .as_u64()
        .expect("write cursor");
    let terminal = loop {
        let report = executor
            .execute(
                request_for_session(
                    session_id,
                    "process_poll",
                    json!({"process_id": process_id, "cursor": cursor, "wait_ms": 1_000}),
                ),
                CancellationToken::new(),
            )
            .await
            .expect("terminal poll");
        cursor = report.envelope.structured_facts["output_cursor"]
            .as_u64()
            .expect("terminal cursor");
        if report.envelope.structured_facts["process_state"] != "running" {
            break report;
        }
    };
    assert_eq!(
        terminal.envelope.structured_facts["process_state"],
        "exited"
    );
    assert_eq!(
        terminal.envelope.structured_facts["workspace_changes_known"],
        true
    );
    assert!(
        terminal
            .changed_files
            .iter()
            .any(|path| path.ends_with("background.txt"))
    );
}

#[cfg(unix)]
#[tokio::test]
async fn background_process_terminate_timeout_and_cancellation_are_terminal() {
    let workspace = tempdir().expect("workspace");
    fs::write(workspace.path().join("descendants.sh"), "sleep 5 &\nwait\n")
        .expect("descendant script");
    let executor = executor(workspace.path());
    let session_id = SessionId::new();
    let running = execute_approved(
        &executor,
        request_for_session(
            session_id,
            "shell",
            json!({
                "command": "sh descendants.sh",
                "background": true,
                "yield_time_ms": 0,
            }),
        ),
        CancellationToken::new(),
    )
    .await;
    let process_id = running.envelope.structured_facts["process_id"]
        .as_str()
        .expect("process id");
    let terminated = tokio::time::timeout(
        Duration::from_secs(2),
        executor.execute(
            request_for_session(
                session_id,
                "process_terminate",
                json!({"process_id": process_id}),
            ),
            CancellationToken::new(),
        ),
    )
    .await
    .expect("process group terminates")
    .expect("terminate report");
    assert_eq!(
        terminated.envelope.structured_facts["process_state"],
        "terminated"
    );
    assert_eq!(terminated.envelope.status, ToolResultStatus::Cancelled);

    let timed_out = execute_approved(
        &executor,
        request_for_session(
            SessionId::new(),
            "shell",
            json!({
                "command": "sleep 5",
                "background": true,
                "timeout_ms": 20,
                "yield_time_ms": 1_000,
            }),
        ),
        CancellationToken::new(),
    )
    .await;
    assert_eq!(timed_out.envelope.status, ToolResultStatus::Timeout);
    assert_eq!(
        timed_out.envelope.structured_facts["process_state"],
        "timed_out"
    );

    let cancellation = CancellationToken::new();
    let cancel_from_task = cancellation.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(20)).await;
        cancel_from_task.cancel();
    });
    let cancelled = execute_approved(
        &executor,
        request_for_session(
            SessionId::new(),
            "shell",
            json!({
                "command": "sleep 5",
                "background": true,
                "yield_time_ms": 1_000,
            }),
        ),
        cancellation,
    )
    .await;
    assert_eq!(cancelled.envelope.status, ToolResultStatus::Cancelled);
    assert_eq!(
        cancelled.envelope.structured_facts["process_state"],
        "cancelled"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn background_process_output_journal_is_bounded_and_reports_cursor_loss() {
    let workspace = tempdir().expect("workspace");
    let executor = executor(workspace.path());
    let session_id = SessionId::new();
    let start = execute_approved(
        &executor,
        request_for_session(
            session_id,
            "shell",
            json!({
                "command": "yes output",
                "background": true,
                "timeout_ms": 100,
                "yield_time_ms": 0,
            }),
        ),
        CancellationToken::new(),
    )
    .await;
    let process_id = start.envelope.structured_facts["process_id"]
        .as_str()
        .expect("process id");
    tokio::time::sleep(Duration::from_millis(250)).await;
    let report = executor
        .execute(
            request_for_session(
                session_id,
                "process_reconnect",
                json!({"process_id": process_id, "cursor": 0}),
            ),
            CancellationToken::new(),
        )
        .await
        .expect("reconnect after output flood");

    assert_eq!(report.envelope.structured_facts["output_truncated"], true);
    assert_eq!(report.envelope.structured_facts["output_lost"], true);
    assert!(report.metrics.output_bytes > 2 * 1024 * 1024);
    assert!(artifact_text(&report).starts_with("[earlier process output omitted]"));
    assert!(artifact_text(&report).len() <= 2 * 1024 * 1024 + 64);
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
            &golutra_sandbox::SystemSandbox::detect(),
            golutra_sandbox::WorkspaceAccess::ReadWrite,
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
            &golutra_sandbox::SystemSandbox::detect(),
            golutra_sandbox::WorkspaceAccess::ReadWrite,
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
    request_for_session(SessionId::new(), tool_name, arguments)
}

fn request_for_session(session_id: SessionId, tool_name: &str, arguments: Value) -> ToolRequest {
    ToolRequest {
        tool_call_id: ToolCallId::new(),
        session_id,
        turn_id: Some(TurnId::new()),
        tool_name: tool_name.to_owned(),
        arguments,
    }
}

fn artifact_text(report: &ToolExecutionReport) -> String {
    String::from_utf8_lossy(&report.artifact_contents[0].bytes).into_owned()
}

#[tokio::test]
async fn caller_declared_verifier_records_pass_and_failure_as_evidence() {
    let workspace = tempdir().expect("workspace");
    let executor = executor(workspace.path());
    let request = |expected_exit_code| VerifierExecutionRequest {
        session_id: SessionId::new(),
        turn_id: Some(TurnId::new()),
        program: "sh".to_owned(),
        args: vec!["-c".to_owned(), "printf verifier-output; exit 3".to_owned()],
        cwd: PathBuf::from("."),
        timeout_ms: 5_000,
        expected_exit_code,
        max_output_bytes: 1024,
    };

    let passed = executor
        .execute_verifier(request(3), CancellationToken::new())
        .await
        .expect("verifier runs");
    let failed = executor
        .execute_verifier(request(0), CancellationToken::new())
        .await
        .expect("verifier runs");

    assert_eq!(passed.envelope.status, ToolResultStatus::Ok);
    assert_eq!(failed.envelope.status, ToolResultStatus::Error);
    assert_eq!(passed.metrics.exit_code, Some(3));
    assert_eq!(artifact_text(&passed), "verifier-output");
    assert!(!passed.envelope.evidence_refs.is_empty());
}

#[tokio::test]
async fn caller_declared_verifier_rejects_cwd_outside_workspace() {
    let workspace = tempdir().expect("workspace");
    let outside = tempdir().expect("outside");
    let error = executor(workspace.path())
        .execute_verifier(
            VerifierExecutionRequest {
                session_id: SessionId::new(),
                turn_id: Some(TurnId::new()),
                program: "true".to_owned(),
                args: Vec::new(),
                cwd: outside.path().to_path_buf(),
                timeout_ms: 1_000,
                expected_exit_code: 0,
                max_output_bytes: 1024,
            },
            CancellationToken::new(),
        )
        .await
        .expect_err("outside cwd rejected");

    assert!(error.to_string().contains("inside the workspace"));
}
