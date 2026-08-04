use std::{
    fs,
    process::Stdio,
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};

use golutra_core::{EvidenceId, PolicyId};
use golutra_policy::WorkspacePolicy;
use tempfile::tempdir;
use tokio::process::Command;

use super::*;

#[test]
fn shell_timeout_contract_honors_long_foreground_requests() {
    let requested = 120_000_u64;

    assert_eq!(effective_shell_timeout(requested), requested);
    assert_eq!(
        effective_shell_timeout(u64::MAX),
        MAX_BACKGROUND_PROCESS_TIMEOUT_MS
    );
}

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
            "apply_patch",
            "ask_user",
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
    let search = registry.contract("rg_search").expect("rg contract");
    assert_eq!(search.side_effect_type, SideEffectType::None);
    assert_eq!(search.retry_policy, "retry_allowed");
}

#[test]
fn shell_contract_explains_how_to_submit_compound_commands() {
    let registry = ToolRegistry::p0_default();
    let contract = registry.contract("shell").expect("shell contract");
    let description = contract
        .input_schema
        .pointer("/properties/command/description")
        .and_then(Value::as_str)
        .expect("shell command description");

    assert!(description.contains("bash -lc"));
    assert!(description.contains("Unquoted operators"));
    assert!(description.contains("Python heredoc"));
    let timeout = contract.input_schema["properties"]["timeout_ms"]["description"]
        .as_str()
        .expect("timeout description");
    let background = contract.input_schema["properties"]["background"]["description"]
        .as_str()
        .expect("background description");
    let yield_time = contract.input_schema["properties"]["yield_time_ms"]["description"]
        .as_str()
        .expect("yield time description");
    assert!(timeout.contains("absolute process lifetime"));
    assert!(background.contains("runtime-scoped"));
    assert!(background.contains("do not use background=true"));
    assert!(background.contains("platform-appropriate lifecycle mechanism"));
    assert!(background.contains("verify availability before returning"));
    assert!(yield_time.contains("initial wait"));
    assert!(yield_time.contains("does not extend"));
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
        "background": true,
        "yield-time_ms": 1_000,
        "api_key": "plain-secret-value",
    }));

    assert_eq!(projected["path"], "src/runtime.rs");
    assert_eq!(projected["pattern"], "RuntimeHost");
    assert_eq!(projected["query"], "runtime host");
    assert_eq!(projected["symbol"], "RuntimeHost::run");
    assert_eq!(projected["timeout_ms"], 5_000);
    assert_eq!(projected["background"], true);
    assert_eq!(projected["yield-time_ms"], 1_000);
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

#[test]
fn model_visible_tool_result_excludes_governance_and_artifact_metadata() {
    let envelope = ToolResultEnvelope {
        tool_call_id: ToolCallId::new(),
        tool_name: "shell".to_owned(),
        status: ToolResultStatus::Ok,
        summary: "command completed".to_owned(),
        structured_facts: json!({
            "command": "cargo test",
            "exit_code": 0,
            "output": "ok 1 test",
        }),
        model_visible_excerpt: Some("ok 1 test".to_owned()),
        raw_artifact_ref: Some(ArtifactId::new()),
        evidence_refs: vec![EvidenceId::new()],
        risk: "high-risk-internal-value".to_owned(),
        verification_hint: Some("internal governance hint".to_owned()),
    };

    let serialized = model_visible_tool_result(&envelope);
    let projection: Value = serde_json::from_str(&serialized).expect("projection JSON");

    assert_eq!(projection["status"], "ok");
    assert_eq!(projection["structured_facts"]["exit_code"], 0);
    assert!(projection.get("raw_artifact_ref").is_none());
    assert!(projection.get("evidence_refs").is_none());
    assert!(projection.get("risk").is_none());
    assert!(projection.get("verification_hint").is_none());
    assert!(!serialized.contains("high-risk-internal-value"));
    assert!(!serialized.contains("internal governance hint"));
    assert!(serialized.len() <= MAX_MODEL_TOOL_RESULT_BYTES);
}

#[test]
fn model_visible_tool_result_bounds_large_facts_and_output() {
    let envelope = ToolResultEnvelope {
        tool_call_id: ToolCallId::new(),
        tool_name: "external_mcp".to_owned(),
        status: ToolResultStatus::Ok,
        summary: "summary".to_owned(),
        structured_facts: json!({
            "items": (0..256)
                .map(|index| Value::String(format!("item-{index}-{}", "x".repeat(512))))
                .collect::<Vec<_>>(),
        }),
        model_visible_excerpt: Some("output".repeat(16 * 1024)),
        raw_artifact_ref: None,
        evidence_refs: Vec::new(),
        risk: "external".to_owned(),
        verification_hint: None,
    };

    let serialized = model_visible_tool_result(&envelope);
    assert!(serialized.len() <= MAX_MODEL_TOOL_RESULT_BYTES);
    assert!(serialized.contains("_golutra_truncated") || serialized.contains("omitted"));
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
async fn unrestricted_runtime_writes_outside_workspace_without_disabling_validation() {
    let workspace = tempdir().expect("workspace");
    let outside = tempdir().expect("outside");
    let target = outside.path().join("secrets.7z");
    let runtime = ToolRuntime::new(WorkspacePolicy::new(workspace.path()).expect("policy"))
        .with_unrestricted_access(true);

    assert!(!runtime.sandbox_os_enforced());
    let write_request = request(
        "write_file",
        json!({"path": target, "content": "unrestricted"}),
    );
    assert_eq!(
        runtime
            .evaluate(&write_request)
            .expect("unrestricted policy")
            .decision,
        PolicyDecision::Allow
    );
    let report = runtime
        .execute(write_request, CancellationToken::new())
        .await
        .expect("outside write");
    assert_eq!(report.envelope.status, ToolResultStatus::Ok);
    assert_eq!(
        fs::read_to_string(&target).expect("outside file"),
        "unrestricted"
    );

    let malformed = request("write_file", json!({"path": target}));
    assert!(matches!(
        runtime.evaluate(&malformed),
        Err(ToolError::InvalidArguments(_))
    ));

    let pipeline = request(
        "shell",
        json!({"command": "pip install sample-package 2>&1 | tail -60"}),
    );
    assert_eq!(
        runtime
            .evaluate(&pipeline)
            .expect("unrestricted shell policy")
            .decision,
        PolicyDecision::Allow
    );
    assert!(matches!(
        runtime.execute(pipeline, CancellationToken::new()).await,
        Err(ToolError::InvalidArguments(message))
            if message.contains("explicit bash -lc wrapper")
    ));
}

#[tokio::test]
async fn tool_runtime_invokes_a_governed_call_through_one_public_seam() {
    let workspace = tempdir().expect("workspace");
    let runtime = ToolRuntime::new(WorkspacePolicy::new(workspace.path()).expect("policy"));
    let request = request(
        "write_file",
        json!({"path": "result.txt", "content": "done"}),
    );
    let policy = runtime.evaluate(&request).expect("policy");

    let report = runtime
        .invoke(
            ToolInvocation::new(request, policy, false),
            CancellationToken::new(),
            None,
        )
        .await
        .expect("tool invocation");

    assert_eq!(report.envelope.status, ToolResultStatus::Ok);
    assert_eq!(
        fs::read_to_string(workspace.path().join("result.txt")).expect("result"),
        "done"
    );
}

#[tokio::test]
async fn apply_patch_changes_multiple_files_through_one_atomic_tool_call() {
    let workspace = tempdir().expect("workspace");
    fs::write(workspace.path().join("one.txt"), "old\n").expect("fixture");
    let executor = executor(workspace.path());
    let patch = concat!(
        "diff --git a/one.txt b/one.txt\n",
        "--- a/one.txt\n",
        "+++ b/one.txt\n",
        "@@ -1 +1 @@\n",
        "-old\n",
        "+new\n",
        "diff --git a/two.txt b/two.txt\n",
        "new file mode 100644\n",
        "--- /dev/null\n",
        "+++ b/two.txt\n",
        "@@ -0,0 +1 @@\n",
        "+second\n",
    );

    let report = executor
        .execute(
            request("apply_patch", json!({"patch": patch})),
            CancellationToken::new(),
        )
        .await
        .expect("patch execution");

    assert_eq!(report.envelope.status, ToolResultStatus::Ok);
    assert_eq!(
        fs::read_to_string(workspace.path().join("one.txt")).expect("one"),
        "new\n"
    );
    assert_eq!(
        fs::read_to_string(workspace.path().join("two.txt")).expect("two"),
        "second\n"
    );
    assert_eq!(report.changed_files.len(), 2);
    assert_eq!(report.before_images.len(), 2);
    assert_eq!(report.after_images.len(), 2);
}

#[cfg(unix)]
#[tokio::test]
async fn unrestricted_apply_patch_can_modify_an_outside_path() {
    let workspace = tempdir().expect("workspace");
    let outside = tempfile::tempdir_in(workspace.path().parent().expect("workspace parent"))
        .expect("outside");
    let relative_target = format!(
        "../{}/external.txt",
        outside
            .path()
            .file_name()
            .expect("outside directory name")
            .to_string_lossy()
    );
    let patch = format!(
        concat!(
            "diff --git a/{0} b/{0}\n",
            "new file mode 100644\n",
            "--- /dev/null\n",
            "+++ b/{0}\n",
            "@@ -0,0 +1 @@\n",
            "+outside\n",
        ),
        relative_target
    );
    let executor =
        ToolRuntime::new(WorkspacePolicy::new(workspace.path()).expect("workspace policy"))
            .with_unrestricted_access(true);

    let report = executor
        .execute(
            request("apply_patch", json!({"patch": patch})),
            CancellationToken::new(),
        )
        .await
        .expect("outside patch execution");

    assert_eq!(report.envelope.status, ToolResultStatus::Ok);
    assert_eq!(
        fs::read_to_string(outside.path().join("external.txt")).expect("outside file"),
        "outside\n"
    );
    assert_eq!(
        report.changed_files,
        vec![
            outside
                .path()
                .join("external.txt")
                .canonicalize()
                .expect("canonical outside file")
        ]
    );
}

#[test]
fn apply_patch_rejects_truncated_path_discovery() {
    let error = parse_git_numstat_paths("1\t0\tfirst.txt\0", true)
        .expect_err("truncated path output must not produce a partial checkpoint");

    assert!(matches!(
        error,
        ToolError::InvalidArguments(message) if message.contains("checkpoint safely")
    ));
}

#[cfg(unix)]
#[tokio::test]
async fn apply_patch_records_a_symlink_target_without_reading_its_destination() {
    let workspace = tempdir().expect("workspace");
    let outside = tempdir().expect("outside");
    let outside_file = outside.path().join("secret.txt");
    fs::write(&outside_file, "outside-secret-content").expect("outside fixture");
    let link_target = outside_file.to_string_lossy();
    let patch = format!(
        concat!(
            "diff --git a/link.txt b/link.txt\n",
            "new file mode 120000\n",
            "--- /dev/null\n",
            "+++ b/link.txt\n",
            "@@ -0,0 +1 @@\n",
            "+{}\n",
            "\\ No newline at end of file\n",
        ),
        link_target
    );

    let report = executor(workspace.path())
        .execute(
            request("apply_patch", json!({"patch": patch})),
            CancellationToken::new(),
        )
        .await
        .expect("patch execution");

    assert_eq!(report.envelope.status, ToolResultStatus::Ok);
    assert_eq!(
        fs::read_link(workspace.path().join("link.txt")).expect("symlink"),
        outside_file
    );
    let after = report.after_images.first().expect("post image");
    assert_eq!(
        after.content.as_deref(),
        Some(link_target.as_bytes()),
        "the post image must contain the link target, not destination bytes"
    );
    assert_ne!(
        after.content.as_deref(),
        Some(b"outside-secret-content".as_slice())
    );
}

#[tokio::test]
async fn apply_patch_rejects_the_whole_patch_when_one_file_conflicts() {
    let workspace = tempdir().expect("workspace");
    fs::write(workspace.path().join("one.txt"), "old\n").expect("fixture");
    fs::write(workspace.path().join("two.txt"), "actual\n").expect("fixture");
    let executor = executor(workspace.path());
    let patch = concat!(
        "diff --git a/one.txt b/one.txt\n",
        "--- a/one.txt\n",
        "+++ b/one.txt\n",
        "@@ -1 +1 @@\n",
        "-old\n",
        "+new\n",
        "diff --git a/two.txt b/two.txt\n",
        "--- a/two.txt\n",
        "+++ b/two.txt\n",
        "@@ -1 +1 @@\n",
        "-expected\n",
        "+changed\n",
    );

    let report = executor
        .execute(
            request("apply_patch", json!({"patch": patch})),
            CancellationToken::new(),
        )
        .await
        .expect("patch execution");

    assert_eq!(report.envelope.status, ToolResultStatus::Error);
    assert!(report.envelope.summary.contains("atomically"));
    assert_eq!(
        fs::read_to_string(workspace.path().join("one.txt")).expect("one"),
        "old\n"
    );
    assert_eq!(
        fs::read_to_string(workspace.path().join("two.txt")).expect("two"),
        "actual\n"
    );
    assert_eq!(report.before_images.len(), 2);
    assert_eq!(report.after_images.len(), 2);
    assert!(report.changed_files.is_empty());
    assert_eq!(
        report.envelope.structured_facts["workspace_changes_known"],
        true
    );
    assert_eq!(
        report.envelope.structured_facts["workspace_change_count"],
        0
    );
}

#[cfg(unix)]
#[tokio::test]
async fn apply_patch_rejects_a_symlink_parent_changed_after_checkpoint() {
    use std::os::unix::fs::symlink;

    let workspace = tempdir().expect("workspace");
    let first = workspace.path().join("first");
    let second = workspace.path().join("second");
    fs::create_dir(&first).expect("first");
    fs::create_dir(&second).expect("second");
    let link = workspace.path().join("target");
    symlink(&first, &link).expect("first symlink");
    let executor = executor(workspace.path());
    let patch = concat!(
        "diff --git a/target/output.txt b/target/output.txt\n",
        "new file mode 100644\n",
        "--- /dev/null\n",
        "+++ b/target/output.txt\n",
        "@@ -0,0 +1 @@\n",
        "+blocked\n",
    );
    let request = request("apply_patch", json!({"patch": patch}));
    let policy = executor.evaluate(&request).expect("policy");
    let before_images = executor
        .prepare_side_effect(&request)
        .await
        .expect("checkpoint");
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
    assert_eq!(
        report.envelope.summary,
        "patch target paths changed after checkpoint"
    );
    assert!(!first.join("output.txt").exists());
    assert!(!second.join("output.txt").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn apply_patch_rechecks_a_symlink_parent_before_an_outside_write() {
    use std::os::unix::fs::symlink;

    let workspace = tempdir().expect("workspace");
    let outside = tempdir().expect("outside");
    let inside = workspace.path().join("inside");
    fs::create_dir(&inside).expect("inside");
    let link = workspace.path().join("target");
    symlink(&inside, &link).expect("inside symlink");
    let executor = executor(workspace.path());
    let patch = concat!(
        "diff --git a/target/output.txt b/target/output.txt\n",
        "new file mode 100644\n",
        "--- /dev/null\n",
        "+++ b/target/output.txt\n",
        "@@ -0,0 +1 @@\n",
        "+blocked\n",
    );
    let request = request("apply_patch", json!({"patch": patch}));
    let policy = executor.evaluate(&request).expect("policy");
    let before_images = executor
        .prepare_side_effect(&request)
        .await
        .expect("checkpoint");
    fs::remove_file(&link).expect("remove symlink");
    symlink(outside.path(), &link).expect("outside symlink");

    let result = executor
        .execute_with_policy_and_before_images(
            request,
            policy,
            false,
            CancellationToken::new(),
            before_images,
        )
        .await;

    assert!(matches!(result, Err(ToolError::Execution(_))));
    assert!(!outside.path().join("output.txt").exists());
}

#[tokio::test]
async fn container_workspace_paths_map_to_the_active_workspace() {
    let workspace = tempdir().expect("workspace");
    let executor = executor(workspace.path());

    let report = executor
        .execute(
            request(
                "write_file",
                json!({"path": "/app/mapped.txt", "content": "mapped"}),
            ),
            CancellationToken::new(),
        )
        .await
        .expect("write execution");

    assert_eq!(report.envelope.status, ToolResultStatus::Ok);
    assert_eq!(
        fs::read_to_string(workspace.path().join("mapped.txt")).expect("mapped"),
        "mapped"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn required_content_comparison_rejects_workspace_symlink_escape() {
    use std::os::unix::fs::symlink;

    let workspace = tempdir().expect("workspace");
    let outside = tempdir().expect("outside");
    fs::write(outside.path().join("secret.txt"), "secret").expect("outside file");
    symlink(
        outside.path().join("secret.txt"),
        workspace.path().join("linked.txt"),
    )
    .expect("outside symlink");
    let executor = executor(workspace.path());

    let comparison = executor
        .compare_workspace_file_content("linked.txt", b"secret")
        .await;

    assert!(matches!(comparison, Err(ToolError::Execution(_))));
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
async fn shell_facts_record_the_executor_network_capability() {
    let workspace = tempdir().expect("workspace");
    let request = || request("shell", json!({"command": "echo ok"}));

    let isolated = executor(workspace.path());
    let isolated_report = execute_approved(&isolated, request(), CancellationToken::new()).await;
    assert_eq!(
        isolated_report.envelope.structured_facts["network_access"],
        false
    );

    let network_enabled = executor(workspace.path()).with_network_access(true);
    let enabled_report =
        execute_approved(&network_enabled, request(), CancellationToken::new()).await;
    assert_eq!(
        enabled_report.envelope.structured_facts["network_access"],
        true
    );
}

#[tokio::test]
async fn shell_parser_preserves_quoted_arguments() {
    let workspace = tempdir().expect("workspace");
    let executor = executor(workspace.path());

    let report = execute_approved(
        &executor,
        request("shell", json!({"command": "printf '%s' 'hello; world'"})),
        CancellationToken::new(),
    )
    .await;

    assert_eq!(report.envelope.status, ToolResultStatus::Ok);
    assert_eq!(
        report.envelope.model_visible_excerpt.as_deref(),
        Some("hello; world")
    );
}

#[test]
fn shell_parser_rejects_unclosed_quotes() {
    assert!(matches!(
        CommandLine::parse("printf 'unterminated"),
        Err(ToolError::InvalidArguments(_))
    ));
}

#[test]
fn shell_parser_rejects_unwrapped_compound_commands() {
    for command in [
        "pip install sample-package 2>&1 | tail -60",
        "printf ok > result.txt",
        "true && echo done",
    ] {
        assert!(
            matches!(
                CommandLine::parse(command),
                Err(ToolError::InvalidArguments(message))
                    if message.contains("explicit bash -lc wrapper")
            ),
            "{command}"
        );
    }
}

#[test]
fn shell_parser_preserves_multiline_explicit_wrapper_scripts() {
    let command = CommandLine::parse("bash -lc 'python - <<'PY'\nprint('ok')\nPY'")
        .expect("explicit wrapper parser");

    assert_eq!(command.program, "bash");
    assert_eq!(command.args[0], "-lc");
    assert!(command.args[1].contains("python - <<'PY'"));
    assert!(command.args[1].contains("print('ok')"));
    assert_eq!(command.stdin, None);
}

#[test]
fn shell_parser_extracts_quoted_python_heredoc_stdin() {
    let command = CommandLine::parse("python - <<'PY'\nprint('direct stdin')\nPY")
        .expect("direct Python heredoc");

    assert_eq!(command.program, "python");
    assert_eq!(command.args, ["-"]);
    assert_eq!(
        command.stdin.as_deref(),
        Some(b"print('direct stdin')\n".as_slice())
    );
}

#[tokio::test]
async fn foreground_python_heredoc_executes_its_body_without_a_shell() {
    let workspace = tempdir().expect("workspace");
    let executor = executor(workspace.path());
    let report = execute_approved(
        &executor,
        request(
            "shell",
            json!({"command": "python3 - <<'PY'\nfrom pathlib import Path\nPath('direct.txt').write_text('executed')\nprint('done')\nPY"}),
        ),
        CancellationToken::new(),
    )
    .await;

    assert_eq!(report.envelope.status, ToolResultStatus::Ok);
    assert_eq!(
        fs::read_to_string(workspace.path().join("direct.txt")).expect("direct output"),
        "executed"
    );
    assert_eq!(
        report.envelope.model_visible_excerpt.as_deref(),
        Some("done\n")
    );
}

#[tokio::test]
async fn background_python_heredoc_is_rejected_before_launch() {
    let workspace = tempdir().expect("workspace");
    let executor = executor(workspace.path());
    let error = executor
        .invoke(
            ToolInvocation::new(
                request(
                    "shell",
                    json!({
                        "command": "python - <<'PY'\nprint('no background')\nPY",
                        "background": true
                    }),
                ),
                PolicyEvaluation {
                    policy_ref: PolicyId::new(),
                    subject: "tool".to_owned(),
                    action: "shell".to_owned(),
                    resource: "python heredoc".to_owned(),
                    decision: PolicyDecision::Allow,
                    block_disposition: None,
                    reason: "test approval".to_owned(),
                    evidence_refs: Vec::new(),
                },
                true,
            ),
            CancellationToken::new(),
            None,
        )
        .await
        .expect_err("background heredoc must be rejected");

    assert!(error.to_string().contains("foreground commands"));
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
    assert!(
        final_sample
            .output_excerpt
            .as_deref()
            .is_some_and(|excerpt| excerpt.contains("three"))
    );
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
        .invoke(
            ToolInvocation::new(request, policy, true).with_preparation(preparation),
            CancellationToken::new(),
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
    assert_eq!(
        start.envelope.structured_facts["process_lifetime_scope"],
        "runtime"
    );
    assert_eq!(
        start.envelope.structured_facts["survives_runtime_exit"],
        false
    );
    assert!(start.envelope.summary.contains("post-runtime consumers"));
    assert!(start.envelope.summary.contains("detached process"));
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
    assert_eq!(
        terminal.envelope.structured_facts["process_lifetime_scope"],
        "runtime"
    );
    assert_eq!(
        terminal.envelope.structured_facts["survives_runtime_exit"],
        false
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
    fs::write(
        workspace.path().join("descendants.sh"),
        "printf 'ready\\n'\nsleep 5 &\nwait\n",
    )
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
                "yield_time_ms": 1_000,
            }),
        ),
        CancellationToken::new(),
    )
    .await;
    assert!(
        running.envelope.structured_facts["output_cursor"]
            .as_u64()
            .is_some_and(|cursor| cursor > 0)
    );
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
    assert_eq!(terminated.envelope.status, ToolResultStatus::Ok);

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
async fn process_timeout_interrupts_a_blocked_stdin_write() {
    let workspace = tempdir().expect("workspace");
    let input = vec![b'x'; 16 * 1024 * 1024];
    let args = ["-c".to_owned(), "sleep 5".to_owned()];
    let sandbox = golutra_sandbox::SystemSandbox::detect();

    let output = tokio::time::timeout(
        Duration::from_secs(1),
        run_process_with_progress(
            ProcessExecutionRequest {
                program: "sh",
                args: &args,
                cwd: workspace.path(),
                workspace_root: workspace.path(),
                timeout_ms: 20,
                cancellation: CancellationToken::new(),
                sandbox: &sandbox,
                workspace_access: golutra_sandbox::WorkspaceAccess::ReadWrite,
                allow_network: false,
                stdin: Some(&input),
                isolated_home: false,
            },
            None,
        ),
    )
    .await
    .expect("blocked stdin write respects the process timeout")
    .expect("process result");

    assert!(output.timed_out);
}

#[cfg(unix)]
#[tokio::test]
async fn process_stdin_failure_preserves_the_child_exit_and_output() {
    let workspace = tempdir().expect("workspace");
    let input = vec![b'x'; 16 * 1024 * 1024];
    let args = [
        "-c".to_owned(),
        concat!(
            "exec 0<&-; ",
            "printf 'stdout-before-exit\\n'; ",
            "sleep 0.05; ",
            "printf 'stderr-before-exit\\n' >&2; ",
            "exit 2"
        )
        .to_owned(),
    ];
    let sandbox = golutra_sandbox::SystemSandbox::process_only();

    let output = tokio::time::timeout(
        Duration::from_secs(2),
        run_process_with_progress(
            ProcessExecutionRequest {
                program: "sh",
                args: &args,
                cwd: workspace.path(),
                workspace_root: workspace.path(),
                timeout_ms: 1_000,
                cancellation: CancellationToken::new(),
                sandbox: &sandbox,
                workspace_access: golutra_sandbox::WorkspaceAccess::ReadWrite,
                allow_network: false,
                stdin: Some(&input),
                isolated_home: false,
            },
            None,
        ),
    )
    .await
    .expect("child cleanup remains bounded")
    .expect("process result survives stdin failure");

    assert_eq!(output.exit_code, Some(2));
    assert!(output.stdin_error.is_some());
    assert!(output.raw_output.contains("stdout-before-exit"));
    assert!(output.raw_output.contains("stderr-before-exit"));
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
        provider_tool_call_id: None,
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
        tool_call_id: ToolCallId::new(),
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
async fn caller_declared_verifier_records_workspace_mutations() {
    let workspace = tempdir().expect("workspace");
    fs::write(workspace.path().join("tracked.txt"), "before").expect("tracked fixture");
    let tool_call_id = ToolCallId::new();
    let report = executor(workspace.path())
        .with_sandbox(SystemSandbox::process_only())
        .execute_verifier(
            VerifierExecutionRequest {
                tool_call_id,
                session_id: SessionId::new(),
                turn_id: Some(TurnId::new()),
                program: "sh".to_owned(),
                args: vec![
                    "-c".to_owned(),
                    "printf after > tracked.txt; printf created > created.txt".to_owned(),
                ],
                cwd: PathBuf::from("."),
                timeout_ms: 5_000,
                expected_exit_code: 0,
                max_output_bytes: 1024,
            },
            CancellationToken::new(),
        )
        .await
        .expect("verifier runs");

    assert_eq!(report.envelope.tool_call_id, tool_call_id);
    assert_eq!(report.envelope.status, ToolResultStatus::Error);
    assert_eq!(
        report.envelope.structured_facts["workspace_mutation_detected"],
        true
    );
    assert_eq!(
        report.envelope.structured_facts["workspace_changes_known"],
        true
    );
    assert_eq!(
        report.envelope.structured_facts["workspace_change_count"],
        2
    );
    let changed_names = report
        .changed_files
        .iter()
        .filter_map(|path| path.file_name().and_then(|name| name.to_str()))
        .collect::<BTreeSet<_>>();
    assert_eq!(
        changed_names,
        BTreeSet::from(["created.txt", "tracked.txt"])
    );
    assert!(report.before_images.iter().any(|image| {
        image.path.ends_with("tracked.txt") && image.content.as_deref() == Some(b"before")
    }));
    assert!(report.after_images.iter().any(|image| {
        image.path.ends_with("tracked.txt") && image.content.as_deref() == Some(b"after")
    }));
    assert!(
        report
            .before_images
            .iter()
            .any(|image| { image.path.ends_with("created.txt") && image.content.is_none() })
    );
}

#[tokio::test]
async fn caller_declared_verifier_rejects_cwd_outside_workspace() {
    let workspace = tempdir().expect("workspace");
    let outside = tempdir().expect("outside");
    let error = executor(workspace.path())
        .execute_verifier(
            VerifierExecutionRequest {
                tool_call_id: ToolCallId::new(),
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
