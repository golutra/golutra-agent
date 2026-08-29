use std::{
    fs,
    path::PathBuf,
    process::Stdio,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    time::Duration,
};

use golutra_core::{EvidenceId, PolicyId};
use golutra_policy::WorkspacePolicy;
use tempfile::tempdir;
use tokio::process::Command;

use super::*;
use crate::builtin::contract;

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
    contract_side_effect: SideEffectType,
    capabilities: Option<ToolCapabilities>,
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
            contract_side_effect: SideEffectType::ExternalSystem,
            capabilities: None,
        }
    }

    fn with_contract_capabilities(
        mut self,
        side_effect_type: SideEffectType,
        capabilities: ToolCapabilities,
    ) -> Self {
        self.contract_side_effect = side_effect_type;
        self.capabilities = Some(capabilities);
        self
    }
}

#[async_trait]
impl ExternalToolBackend for FakeExternalBackend {
    fn contracts(&self) -> Vec<ToolContract> {
        vec![contract("mcp__fixture__echo", self.contract_side_effect)]
    }

    fn capabilities(&self) -> HashMap<String, ToolCapabilities> {
        self.capabilities
            .clone()
            .map(|capabilities| HashMap::from([("mcp__fixture__echo".to_owned(), capabilities)]))
            .unwrap_or_default()
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

#[test]
fn external_tool_capabilities_are_explicit_and_validated() {
    let workspace = tempdir().expect("workspace");
    let default_runtime = executor(workspace.path())
        .with_external_backend(Arc::new(FakeExternalBackend::successful(Duration::ZERO)))
        .expect("external backend registers");
    assert_eq!(
        default_runtime
            .registry()
            .capabilities("mcp__fixture__echo"),
        Some(&ToolCapabilities::default())
    );

    let opted_in = executor(workspace.path())
        .with_external_backend(Arc::new(
            FakeExternalBackend::successful(Duration::ZERO).with_contract_capabilities(
                SideEffectType::None,
                ToolCapabilities {
                    available_in_coding_profile: true,
                    parallel_read_safe: true,
                    coding_profile_hidden_arguments: Vec::new(),
                },
            ),
        ))
        .expect("pure external read registers");
    assert!(
        opted_in
            .registry()
            .capabilities("mcp__fixture__echo")
            .is_some_and(|capabilities| capabilities.parallel_read_safe)
    );

    let unsafe_backend = Arc::new(
        FakeExternalBackend::successful(Duration::ZERO).with_contract_capabilities(
            SideEffectType::ExternalSystem,
            ToolCapabilities {
                available_in_coding_profile: true,
                parallel_read_safe: true,
                coding_profile_hidden_arguments: Vec::new(),
            },
        ),
    );
    let error = executor(workspace.path())
        .with_external_backend(unsafe_backend)
        .expect_err("side-effecting tools cannot opt into parallel reads");
    assert!(error.to_string().contains("admits side effects"));

    let removed = opted_in.without_tool("mcp__fixture__echo");
    assert!(removed.registry().contract("mcp__fixture__echo").is_none());
    assert!(
        removed
            .registry()
            .capabilities("mcp__fixture__echo")
            .is_none()
    );
}

#[test]
fn replay_contracts_register_without_a_live_external_backend() {
    let workspace = tempdir().expect("workspace");
    let mut external = contract("mcp__fixture__recorded", SideEffectType::None);
    external.input_schema["properties"]["query"] = json!({"type": "string"});
    external.input_schema["required"] = json!(["query"]);
    let runtime = executor(workspace.path())
        .with_replay_contracts([external.clone()])
        .expect("recorded contract registers");

    assert_eq!(
        runtime.registry().contract(&external.tool_name),
        Some(&external)
    );
    assert_eq!(
        runtime.registry().capabilities(&external.tool_name),
        Some(&ToolCapabilities {
            available_in_coding_profile: false,
            parallel_read_safe: false,
            coding_profile_hidden_arguments: Vec::new(),
        })
    );
}

#[derive(Debug)]
struct FakeTaskDelegationBackend {
    calls: AtomicUsize,
    mutation: Option<PathBuf>,
}

#[derive(Debug)]
struct CancellationAwareDelegationBackend {
    cancelled: Arc<AtomicBool>,
}

#[async_trait]
impl TaskDelegationBackend for CancellationAwareDelegationBackend {
    async fn delegate(
        &self,
        _request: &ToolRequest,
        cancellation: CancellationToken,
    ) -> Result<TaskDelegationOutput, ToolError> {
        cancellation.cancelled().await;
        self.cancelled.store(true, Ordering::SeqCst);
        Ok(TaskDelegationOutput {
            status: ToolResultStatus::Cancelled,
            summary: "delegated task cancelled".to_owned(),
            content: String::new(),
            structured_facts: json!({"cancelled": true}),
        })
    }
}

#[async_trait]
impl TaskDelegationBackend for FakeTaskDelegationBackend {
    async fn delegate(
        &self,
        request: &ToolRequest,
        cancellation: CancellationToken,
    ) -> Result<TaskDelegationOutput, ToolError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if cancellation.is_cancelled() {
            return Ok(TaskDelegationOutput {
                status: ToolResultStatus::Cancelled,
                summary: "delegated task cancelled".to_owned(),
                content: String::new(),
                structured_facts: json!({"child_status": "cancelled"}),
            });
        }
        if let Some(path) = &self.mutation {
            tokio::fs::write(path, "child change")
                .await
                .map_err(|error| ToolError::Execution(error.to_string()))?;
        }
        Ok(TaskDelegationOutput {
            status: ToolResultStatus::Ok,
            summary: "delegated task completed".to_owned(),
            content: "child response".to_owned(),
            structured_facts: json!({
                "task": request.arguments["task"],
                "effective_model": request.arguments.get("model"),
                "effective_reasoning_effort": request.arguments.get("reasoning_effort"),
                "child_status": "completed",
            }),
        })
    }
}

#[tokio::test]
async fn registry_contains_p0_tools() {
    let registry = ToolRegistry::p0_default();
    let names = registry
        .provider_contracts()
        .into_iter()
        .map(|contract| contract.tool_name.as_str())
        .collect::<Vec<_>>();

    assert_eq!(
        names,
        vec![
            "apply_patch",
            "edit_file",
            "read_file",
            "shell",
            "shell_session",
            "subagent",
            "web_search",
            "write_file"
        ]
    );
    assert!(registry.contract("list_dir").is_some());
    assert!(registry.provider_contracts().iter().all(|contract| {
        !matches!(
            contract.tool_name.as_str(),
            "list_dir" | "rg_search" | "process_poll" | "ask_user"
        )
    }));
}

#[tokio::test]
async fn delegated_task_is_registered_only_with_a_backend_and_tracks_workspace_changes() {
    let workspace = tempdir().expect("workspace");
    let changed = workspace.path().join("delegated.txt");
    let backend = Arc::new(FakeTaskDelegationBackend {
        calls: AtomicUsize::new(0),
        mutation: Some(changed.clone()),
    });
    let executor = executor(workspace.path())
        .with_task_delegation_backend(backend.clone())
        .expect("delegation backend registers");
    let request = request(
        "subagent",
        json!({
            "task": "inspect and update the delegated fixture",
            "model": "test-model",
            "reasoning_effort": "high",
        }),
    );
    let contract = executor
        .registry()
        .contract("subagent")
        .expect("delegation contract");
    assert!(
        executor
            .registry()
            .capabilities("subagent")
            .is_some_and(|capabilities| capabilities.available_in_coding_profile)
    );
    assert_eq!(contract.side_effect_type, SideEffectType::Process);
    assert_eq!(
        contract.input_schema["properties"]["reasoning_effort"]["enum"],
        json!(["low", "medium", "high", "xhigh"])
    );
    let policy = executor.evaluate(&request).expect("delegation policy");
    assert_eq!(policy.decision, PolicyDecision::Allow);
    assert!(!policy.resource.contains("inspect and update"));
    let preparation = executor
        .prepare_side_effect_snapshot(&request)
        .await
        .expect("delegation preparation");
    let report = executor
        .invoke(
            ToolInvocation::new(request, policy, false).with_preparation(preparation),
            CancellationToken::new(),
            None,
        )
        .await
        .expect("delegated task executes");

    assert_eq!(report.envelope.status, ToolResultStatus::Ok);
    assert_eq!(report.envelope.risk, "delegated_agent");
    assert_eq!(
        report.envelope.structured_facts["child_status"],
        "completed"
    );
    assert_eq!(
        report.envelope.structured_facts["workspace_changes_known"],
        true
    );
    assert_eq!(backend.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        fs::read_to_string(changed).expect("delegated output"),
        "child change"
    );
    assert!(
        report
            .changed_files
            .iter()
            .any(|path| path.ends_with("delegated.txt"))
    );
    assert!(artifact_text(&report).contains("child response"));
}

#[tokio::test]
async fn delegated_task_rejects_invalid_effort_and_can_be_removed_for_children() {
    let workspace = tempdir().expect("workspace");
    let backend = Arc::new(FakeTaskDelegationBackend {
        calls: AtomicUsize::new(0),
        mutation: None,
    });
    let executor = executor(workspace.path())
        .with_task_delegation_backend(backend.clone())
        .expect("delegation backend registers");
    let invalid = request(
        "subagent",
        json!({"task": "inspect", "reasoning_effort": "extreme"}),
    );
    assert!(matches!(
        executor.evaluate(&invalid),
        Err(ToolError::InvalidArguments(_))
    ));
    assert_eq!(backend.calls.load(Ordering::SeqCst), 0);

    let child = executor.without_tool("subagent");
    assert!(child.registry().contract("subagent").is_none());
    assert!(matches!(
        child.evaluate(&request("subagent", json!({"task": "inspect"}))),
        Err(ToolError::UnknownTool(_))
    ));
}

#[tokio::test]
async fn delegated_task_receives_the_enclosing_runtime_deadline() {
    let workspace = tempdir().expect("workspace");
    let cancelled = Arc::new(AtomicBool::new(false));
    let executor = executor(workspace.path())
        .with_task_delegation_backend(Arc::new(CancellationAwareDelegationBackend {
            cancelled: cancelled.clone(),
        }))
        .expect("delegation backend registers");
    let request = request("subagent", json!({"task": "wait for cancellation"}));
    let policy = executor.evaluate(&request).expect("delegation policy");
    let deadline = tokio::time::Instant::now() + Duration::from_millis(25);
    let report = tokio::time::timeout(
        Duration::from_secs(1),
        executor.invoke(
            ToolInvocation::new(request, policy, false).with_deadline(deadline),
            CancellationToken::new(),
            None,
        ),
    )
    .await
    .expect("deadline cancellation should settle")
    .expect("delegation report");

    assert_eq!(report.envelope.status, ToolResultStatus::Timeout);
    assert_eq!(report.envelope.structured_facts["timed_out"], true);
    assert_eq!(
        report.envelope.structured_facts["deadline_stage"],
        "execution"
    );
    assert!(cancelled.load(Ordering::SeqCst));
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
    let workdir = contract.input_schema["properties"]["workdir"]["description"]
        .as_str()
        .expect("workdir description");
    assert!(timeout.contains("absolute process lifetime"));
    assert!(background.contains("runtime-scoped"));
    assert!(background.contains("stops when the runtime ends"));
    assert!(background.contains("shell_session"));
    let yield_time_lower = yield_time.to_ascii_lowercase();
    assert!(yield_time_lower.contains("initial wait"));
    assert!(yield_time_lower.contains("does not extend"));
    assert!(workdir.contains("working directory"));
    assert!(workdir.contains("workspace-relative"));
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
async fn enclosing_deadline_bounds_external_tools_and_reports_timeout() {
    let workspace = tempdir().expect("workspace");
    let backend = Arc::new(FakeExternalBackend::successful(Duration::from_secs(1)));
    let executor = executor(workspace.path())
        .with_external_backend(backend)
        .expect("external backend registers");
    let request = request("mcp__fixture__echo", json!({}));
    let policy = executor.evaluate(&request).expect("policy");
    let deadline = tokio::time::Instant::now() + Duration::from_millis(20);

    let report = tokio::time::timeout(
        Duration::from_secs(1),
        executor.invoke(
            ToolInvocation::new(request, policy, true).with_deadline(deadline),
            CancellationToken::new(),
            None,
        ),
    )
    .await
    .expect("enclosing deadline settles the invocation")
    .expect("deadline returns a report");

    assert_eq!(report.envelope.status, ToolResultStatus::Timeout);
    assert_eq!(report.envelope.structured_facts["timed_out"], true);
    assert_eq!(
        report.envelope.structured_facts["deadline_stage"],
        "execution"
    );
    assert_eq!(report.envelope.risk, "external_mcp_tool");
}

#[tokio::test]
async fn expired_deadline_prevents_file_side_effects() {
    let workspace = tempdir().expect("workspace");
    let target = workspace.path().join("deadline.txt");
    let executor = executor(workspace.path());
    let request = request(
        "write_file",
        json!({"path": "deadline.txt", "content": "must not be written"}),
    );
    let policy = executor.evaluate(&request).expect("policy");
    let expired = tokio::time::Instant::now() - Duration::from_millis(1);
    let mut progress = Vec::new();
    let mut collect_progress = |event| progress.push(event);

    let report = executor
        .invoke(
            ToolInvocation::new(request, policy, false).with_deadline(expired),
            CancellationToken::new(),
            Some(&mut collect_progress),
        )
        .await
        .expect("expired deadline returns a report");
    assert_eq!(report.envelope.status, ToolResultStatus::Timeout);
    assert_eq!(
        report.envelope.structured_facts["deadline_stage"],
        "side-effect preparation"
    );
    assert!(!target.exists(), "expired invocations cannot mutate files");
    assert_eq!(
        progress
            .iter()
            .filter(|event| event.phase == ToolProgressPhase::Completed)
            .count(),
        1
    );
}

#[tokio::test]
async fn deadline_cleanup_preserves_a_completed_tool_report() {
    let workspace = tempdir().expect("workspace");
    let executor = executor(workspace.path());
    let request = request(
        "write_file",
        json!({"path": "deadline.txt", "content": "written"}),
    );
    let policy = executor.evaluate(&request).expect("policy");
    let mut operation = Box::pin(async {
        tokio::time::sleep(Duration::from_millis(5)).await;
        executor
            .invoke(
                ToolInvocation::new(request, policy, false),
                CancellationToken::new(),
                None,
            )
            .await
    });
    let cancellation = CancellationToken::new();
    let operation_cancellation = CancellationToken::new();

    let outcome = await_tool_operation(
        &mut operation,
        &cancellation,
        &operation_cancellation,
        Some(tokio::time::Instant::now() + Duration::from_millis(1)),
    )
    .await;
    let ToolOperationOutcome::TimedOut(Some(Ok(report))) = outcome else {
        panic!("the completed report must survive deadline cleanup");
    };
    let report = mark_report_deadline_exceeded(report);

    assert_eq!(report.envelope.status, ToolResultStatus::Timeout);
    assert_eq!(
        report.envelope.structured_facts["completed_during_deadline_cleanup"],
        true
    );
    assert_eq!(
        report.envelope.structured_facts["workspace_changes_known"],
        true
    );
    assert_eq!(report.changed_files.len(), 1);
    assert!(report.changed_files[0].ends_with("deadline.txt"));
    assert_eq!(
        fs::read_to_string(workspace.path().join("deadline.txt")).expect("written file"),
        "written"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn file_tools_reject_special_files_without_blocking() {
    let workspace = tempdir().expect("workspace");
    let fifo = workspace.path().join("input.pipe");
    let status = std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .expect("launch mkfifo");
    assert!(status.success());
    let executor = executor(workspace.path());

    let error = tokio::time::timeout(
        Duration::from_secs(1),
        executor.execute(
            request("read_file", json!({"path": "input.pipe"})),
            CancellationToken::new(),
        ),
    )
    .await
    .expect("special-file rejection is bounded")
    .expect_err("FIFO is not a regular file");

    assert!(error.to_string().contains("not a regular file"));
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

fn projection_envelope(
    tool_name: &str,
    status: ToolResultStatus,
    summary: &str,
    structured_facts: Value,
    model_visible_excerpt: Option<&str>,
) -> ToolResultEnvelope {
    ToolResultEnvelope {
        tool_call_id: ToolCallId::new(),
        tool_name: tool_name.to_owned(),
        status,
        summary: summary.to_owned(),
        structured_facts,
        model_visible_excerpt: model_visible_excerpt.map(ToOwned::to_owned),
        raw_artifact_ref: Some(ArtifactId::new()),
        evidence_refs: vec![EvidenceId::new()],
        risk: "internal-only-risk".to_owned(),
        verification_hint: Some("internal-only-hint".to_owned()),
    }
}

#[test]
fn model_visible_tool_result_projects_provider_tools_without_duplicate_payloads() {
    let read: Value = serde_json::from_str(&model_visible_tool_result(&projection_envelope(
        "read_file",
        ToolResultStatus::Ok,
        "file read",
        json!({
            "path": "src/lib.rs",
            "bytes": 12,
            "continuation": {"next_cursor": 12},
            "content": "duplicate-content",
            "content_digest": "drop-me",
        }),
        Some("file content"),
    )))
    .expect("read projection");
    assert_eq!(read["structured_facts"]["path"], "src/lib.rs");
    assert_eq!(read["structured_facts"]["continuation"]["next_cursor"], 12);
    assert!(read["structured_facts"].get("content").is_none());
    assert!(read["structured_facts"].get("content_digest").is_none());
    assert_eq!(read["model_visible_excerpt"], "file content");
    assert!(read.get("summary").is_none());

    let mutation: Value = serde_json::from_str(&model_visible_tool_result(&projection_envelope(
        "apply_patch",
        ToolResultStatus::Ok,
        "patch applied",
        json!({
            "changed_files": ["src/lib.rs"],
            "changed_file_count": 1,
            "patch_digest": "drop-me",
            "summary": "drop-me-too",
        }),
        Some("the full patch output is durable"),
    )))
    .expect("mutation projection");
    assert_eq!(mutation["structured_facts"]["changed_file_count"], 1);
    assert!(mutation["structured_facts"].get("patch_digest").is_none());
    assert_eq!(mutation["summary"], "patch applied");
    assert!(mutation.get("model_visible_excerpt").is_none());

    let shell: Value = serde_json::from_str(&model_visible_tool_result(&projection_envelope(
        "shell_session",
        ToolResultStatus::Ok,
        "background process is running",
        json!({
            "process_id": "proc-1",
            "process_state": "running",
            "output_cursor": 42,
            "exit_code": null,
            "command": "drop-command",
        }),
        Some("new output"),
    )))
    .expect("shell projection");
    assert_eq!(shell["structured_facts"]["process_id"], "proc-1");
    assert_eq!(shell["structured_facts"]["output_cursor"], 42);
    assert!(shell["structured_facts"].get("command").is_none());
    assert_eq!(shell["model_visible_excerpt"], "new output");
    assert!(shell.get("summary").is_none());

    let search: Value = serde_json::from_str(&model_visible_tool_result(&projection_envelope(
        "web_search",
        ToolResultStatus::Ok,
        "web search returned 1 results",
        json!({
            "query": "rust async",
            "results": [{
                "title": "Rust",
                "url": "https://example.com",
                "snippet": "useful",
                "summary": "drop-duplicate",
            }],
            "source": "drop-source",
        }),
        Some("web search returned 1 results"),
    )))
    .expect("search projection");
    assert_eq!(search["structured_facts"]["query"], "rust async");
    assert_eq!(search["structured_facts"]["results"][0]["title"], "Rust");
    assert!(
        search["structured_facts"]["results"][0]
            .get("summary")
            .is_none()
    );
    assert!(search["structured_facts"].get("source").is_none());
    assert!(search.get("summary").is_none());
    assert!(search.get("model_visible_excerpt").is_none());

    let child: Value = serde_json::from_str(&model_visible_tool_result(&projection_envelope(
        "subagent",
        ToolResultStatus::Ok,
        "child completed",
        json!({
            "task": "drop-task",
            "effective_model": "drop-model",
            "child_status": "completed",
            "workspace_change_count": 0,
        }),
        Some("child facts and content"),
    )))
    .expect("subagent projection");
    assert_eq!(child["structured_facts"]["child_status"], "completed");
    assert_eq!(child["structured_facts"]["workspace_change_count"], 0);
    assert!(child["structured_facts"].get("task").is_none());
    assert!(child["structured_facts"].get("effective_model").is_none());
    assert_eq!(child["summary"], "child completed");
    assert_eq!(child["model_visible_excerpt"], "child facts and content");
}

#[test]
fn model_visible_tool_result_keeps_failure_facts_and_size_bound() {
    let envelope = projection_envelope(
        "shell",
        ToolResultStatus::Timeout,
        "process timed out while waiting for output",
        json!({
            "timed_out": true,
            "exit_code": null,
            "reason": "deadline exceeded",
            "output": "x".repeat(64 * 1024),
        }),
        Some(&"partial output ".repeat(8 * 1024)),
    );
    let serialized = model_visible_tool_result(&envelope);
    let projection: Value = serde_json::from_str(&serialized).expect("failure projection");
    assert_eq!(projection["status"], "timeout");
    assert_eq!(projection["structured_facts"]["timed_out"], true);
    assert_eq!(
        projection["structured_facts"]["reason"],
        "deadline exceeded"
    );
    assert_eq!(
        projection["summary"],
        "process timed out while waiting for output"
    );
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

#[test]
fn model_visible_tool_result_respects_a_tighter_budget_without_losing_status() {
    let envelope = projection_envelope(
        "read_file",
        ToolResultStatus::Ok,
        "file read",
        json!({
            "path": "src/lib.rs",
            "bytes": 128,
            "continuation": {"next_cursor": 128},
            "has_more": true,
        }),
        Some(&"line ".repeat(4 * 1024)),
    );

    let serialized = model_visible_tool_result_with_limit(&envelope, 1_024);
    let projection: Value = serde_json::from_str(&serialized).expect("bounded projection");
    assert!(serialized.len() <= 1_024);
    assert_eq!(projection["status"], "ok");
    assert_eq!(projection["structured_facts"]["path"], "src/lib.rs");
    assert_eq!(
        projection["structured_facts"]["continuation"]["next_cursor"],
        128
    );
}

#[test]
fn token_budget_projection_keeps_read_continuation_facts() {
    let envelope = projection_envelope(
        "read_file",
        ToolResultStatus::Ok,
        "file read",
        json!({
            "path": "src/lib.rs",
            "continuation": {"next_offset": 256, "next_cursor": 9},
            "has_more": true,
            "content": "line ".repeat(16 * 1024),
        }),
        Some(&"line ".repeat(16 * 1024)),
    );

    let serialized = model_visible_tool_result_with_token_budget(&envelope, 256);
    let projection: Value = serde_json::from_str(&serialized).expect("token-bounded projection");
    assert!(serialized.len() <= 1_024);
    assert_eq!(projection["status"], "ok");
    assert_eq!(projection["structured_facts"]["path"], "src/lib.rs");
    assert_eq!(
        projection["structured_facts"]["continuation"]["next_offset"],
        256
    );
}

#[test]
fn tight_projection_prioritizes_error_and_continuation_over_large_optional_facts() {
    let mut facts = serde_json::Map::new();
    facts.insert("path".to_owned(), Value::String("资料/说明.txt".to_owned()));
    facts.insert(
        "continuation".to_owned(),
        json!({"next_offset": 512, "next_cursor": 17, "has_more": true}),
    );
    facts.insert(
        "error".to_owned(),
        Value::String("读取失败：文件暂时不可用".repeat(256)),
    );
    facts.insert("reason".to_owned(), Value::String("retryable".to_owned()));
    for index in 0..64 {
        facts.insert(
            format!("optional_{index}"),
            Value::String("大段无关输出".repeat(512)),
        );
    }
    let envelope = projection_envelope(
        "read_file",
        ToolResultStatus::Error,
        "读取失败，请使用 continuation 继续",
        Value::Object(facts),
        Some(&"无关输出".repeat(4 * 1024)),
    );

    let serialized = model_visible_tool_result_with_limit(&envelope, 1_024);
    assert!(serialized.len() <= 1_024);
    let projection: Value = serde_json::from_str(&serialized).expect("valid projection");
    assert_eq!(projection["tool_name"], "read_file");
    assert_eq!(projection["status"], "error");
    assert_eq!(
        projection["structured_facts"]["continuation"]["next_offset"],
        512
    );
    assert!(
        projection["structured_facts"]["error"]
            .as_str()
            .is_some_and(|error| error.starts_with("读取失败：文件暂时不可用"))
    );
}

#[test]
fn tight_mutation_projection_always_keeps_success_summary() {
    let mut facts = serde_json::Map::new();
    facts.insert(
        "changed_files".to_owned(),
        Value::Array(
            (0..128)
                .map(|index| Value::String(format!("src/{index}-{}.rs", "x".repeat(256))))
                .collect(),
        ),
    );
    facts.insert("changed_file_count".to_owned(), json!(128));
    for index in 0..64 {
        facts.insert(
            format!("optional_{index}"),
            Value::String("无关字段".repeat(512)),
        );
    }
    let envelope = projection_envelope(
        "apply_patch",
        ToolResultStatus::Ok,
        "已原子应用 128 个文件变更",
        Value::Object(facts),
        Some(&"完整 patch 输出".repeat(4 * 1024)),
    );

    let serialized = model_visible_tool_result_with_limit(&envelope, 1_024);
    assert!(serialized.len() <= 1_024);
    let projection: Value = serde_json::from_str(&serialized).expect("valid mutation projection");
    assert_eq!(projection["status"], "ok");
    assert_eq!(projection["summary"], "已原子应用 128 个文件变更");
    assert_eq!(projection["structured_facts"]["changed_file_count"], 128);
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
async fn read_file_returns_bounded_windows_with_a_stable_continuation_offset() {
    let workspace = tempdir().expect("workspace");
    let content = (1..=205)
        .map(|line| format!("line-{line}\n"))
        .collect::<String>();
    fs::write(workspace.path().join("large.txt"), &content).expect("fixture");
    let executor = executor(workspace.path());

    let first = executor
        .execute(
            request("read_file", json!({"path": "large.txt"})),
            CancellationToken::new(),
        )
        .await
        .expect("first window");
    assert_eq!(first.envelope.structured_facts["offset"], 1);
    assert_eq!(first.envelope.structured_facts["limit"], 200);
    assert_eq!(first.envelope.structured_facts["total_lines"], 205);
    assert_eq!(first.envelope.structured_facts["has_more"], true);
    assert_eq!(first.envelope.structured_facts["truncated"], true);
    assert_eq!(
        first.envelope.structured_facts["continuation"]["next_offset"],
        201
    );
    assert!(artifact_text(&first).contains("line-200"));
    assert!(!artifact_text(&first).contains("line-201"));

    let second = executor
        .execute(
            request(
                "read_file",
                json!({"path": "large.txt", "offset": 201, "limit": 20}),
            ),
            CancellationToken::new(),
        )
        .await
        .expect("continuation window");
    assert_eq!(second.envelope.structured_facts["offset"], 201);
    assert_eq!(second.envelope.structured_facts["lines"], 5);
    assert_eq!(second.envelope.structured_facts["has_more"], false);
    assert_eq!(second.envelope.structured_facts["truncated"], false);
    assert_eq!(second.envelope.structured_facts["eof"], true);
    assert!(artifact_text(&second).contains("line-205"));
}

#[tokio::test]
async fn read_file_rejects_zero_based_offsets() {
    let workspace = tempdir().expect("workspace");
    fs::write(workspace.path().join("src.txt"), "content").expect("fixture");
    let executor = executor(workspace.path());

    let result = executor
        .execute(
            request("read_file", json!({"path": "src.txt", "offset": 0})),
            CancellationToken::new(),
        )
        .await;
    assert!(matches!(result, Err(ToolError::InvalidArguments(_))));
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
    assert_eq!(report.envelope.structured_facts["bytes"], 3);
    assert!(
        report.envelope.structured_facts["content_digest"]
            .as_str()
            .is_some_and(|digest| digest.starts_with("sha256:"))
    );
    assert_eq!(
        report.envelope.model_visible_excerpt.as_deref(),
        Some("file written")
    );
    assert!(
        !report
            .envelope
            .model_visible_excerpt
            .as_deref()
            .is_some_and(|excerpt| excerpt.contains("new"))
    );
}

#[tokio::test]
async fn mutation_results_keep_content_in_artifacts_but_not_model_excerpt() {
    let workspace = tempdir().expect("workspace");
    fs::write(workspace.path().join("src.txt"), "before").expect("fixture");
    let executor = executor(workspace.path());

    let report = executor
        .execute(
            request(
                "edit_file",
                json!({
                    "path": "src.txt",
                    "edits": [{"old_text": "before", "new_text": "after"}]
                }),
            ),
            CancellationToken::new(),
        )
        .await
        .expect("edit execution");

    assert_eq!(
        report.envelope.model_visible_excerpt.as_deref(),
        Some("file edited")
    );
    assert_eq!(report.artifact_contents.len(), 1);
    assert!(String::from_utf8_lossy(&report.artifact_contents[0].bytes).contains("after"));
    assert!(
        !report
            .envelope
            .model_visible_excerpt
            .as_deref()
            .is_some_and(|excerpt| excerpt.contains("after"))
    );
}

#[tokio::test]
async fn edit_file_applies_multiple_non_overlapping_replacements_atomically() {
    let workspace = tempdir().expect("workspace");
    let path = workspace.path().join("src.txt");
    fs::write(&path, "alpha beta gamma").expect("fixture");
    let executor = executor(workspace.path());

    let report = executor
        .execute(
            request(
                "edit_file",
                json!({
                    "path": "src.txt",
                    "edits": [
                        {"old_text": "gamma", "new_text": "GAMMA"},
                        {"old_text": "alpha", "new_text": "ALPHA"}
                    ]
                }),
            ),
            CancellationToken::new(),
        )
        .await
        .expect("multi-edit execution");

    assert_eq!(report.envelope.status, ToolResultStatus::Ok);
    assert_eq!(report.envelope.structured_facts["edit_count"], 2);
    assert_eq!(report.envelope.structured_facts["replacements"], 2);
    assert_eq!(
        fs::read_to_string(path).expect("edited file"),
        "ALPHA beta GAMMA"
    );
}

#[tokio::test]
async fn edit_file_rejects_overlapping_edits_without_partial_writes() {
    let workspace = tempdir().expect("workspace");
    let path = workspace.path().join("src.txt");
    fs::write(&path, "alpha beta").expect("fixture");
    let executor = executor(workspace.path());

    let report = executor
        .execute(
            request(
                "edit_file",
                json!({
                    "path": "src.txt",
                    "edits": [
                        {"old_text": "alpha", "new_text": "ALPHA"},
                        {"old_text": "pha beta", "new_text": "changed"}
                    ]
                }),
            ),
            CancellationToken::new(),
        )
        .await
        .expect("overlap report");

    assert_eq!(report.envelope.status, ToolResultStatus::Error);
    assert_eq!(report.envelope.structured_facts["overlap"], true);
    assert_eq!(
        fs::read_to_string(path).expect("unchanged file"),
        "alpha beta"
    );
}

#[tokio::test]
async fn edit_file_rejects_non_unique_whitespace_targets_without_partial_writes() {
    let workspace = tempdir().expect("workspace");
    let path = workspace.path().join("src.txt");
    fs::write(&path, "left  right  tail").expect("fixture");
    let executor = executor(workspace.path());

    let report = executor
        .execute(
            request(
                "edit_file",
                json!({
                    "path": "src.txt",
                    "edits": [{"old_text": "  ", "new_text": "\t"}]
                }),
            ),
            CancellationToken::new(),
        )
        .await
        .expect("whitespace target report");

    assert_eq!(report.envelope.status, ToolResultStatus::Error);
    assert_eq!(report.envelope.structured_facts["search_found"], true);
    assert_eq!(
        fs::read_to_string(path).expect("unchanged file"),
        "left  right  tail"
    );
}

#[tokio::test]
async fn edit_file_requires_the_canonical_edits_array() {
    let workspace = tempdir().expect("workspace");
    fs::write(workspace.path().join("src.txt"), "before").expect("fixture");
    let executor = executor(workspace.path());

    let result = executor
        .execute(
            request(
                "edit_file",
                json!({"path": "src.txt", "search": "before", "replace": "after"}),
            ),
            CancellationToken::new(),
        )
        .await;
    assert!(matches!(result, Err(ToolError::InvalidArguments(_))));
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
        json!({"command": "printf 'one\\ntwo\\n' | tail -1"}),
    );
    assert_eq!(
        runtime
            .evaluate(&pipeline)
            .expect("unrestricted shell policy")
            .decision,
        PolicyDecision::Allow
    );
    let report = runtime
        .execute(pipeline, CancellationToken::new())
        .await
        .expect("unrestricted compound shell command");
    assert_eq!(report.envelope.status, ToolResultStatus::Ok);
    assert_eq!(
        report.envelope.model_visible_excerpt.as_deref(),
        Some("two\n")
    );
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

#[tokio::test]
async fn apply_patch_accepts_model_begin_patch_format_atomically() {
    let workspace = tempdir().expect("workspace");
    fs::write(workspace.path().join("one.txt"), "old\nkeep\n").expect("fixture");
    let executor = executor(workspace.path());
    let patch = concat!(
        "*** Begin Patch\n",
        "*** Update File: one.txt\n",
        "@@\n",
        " old\n",
        "-keep\n",
        "+changed\n",
        "*** Add File: two.txt\n",
        "+second\n",
        "*** End Patch\n",
    );

    let report = executor
        .execute(
            request("apply_patch", json!({"patch": patch})),
            CancellationToken::new(),
        )
        .await
        .expect("model patch execution");

    assert_eq!(report.envelope.status, ToolResultStatus::Ok);
    assert_eq!(
        fs::read_to_string(workspace.path().join("one.txt")).expect("one"),
        "old\nchanged\n"
    );
    assert_eq!(
        fs::read_to_string(workspace.path().join("two.txt")).expect("two"),
        "second\n"
    );
    assert_eq!(report.changed_files.len(), 2);
}

#[tokio::test]
async fn apply_patch_rejects_ambiguous_model_context_without_partial_writes() {
    let workspace = tempdir().expect("workspace");
    fs::write(workspace.path().join("one.txt"), "same\nother\nsame\n").expect("fixture");
    let executor = executor(workspace.path());
    let patch = concat!(
        "*** Begin Patch\n",
        "*** Update File: one.txt\n",
        "@@\n",
        "-same\n",
        "+changed\n",
        "*** End Patch\n",
    );

    let error = executor
        .execute(
            request("apply_patch", json!({"patch": patch})),
            CancellationToken::new(),
        )
        .await
        .expect_err("ambiguous model context must be rejected");

    assert!(error.to_string().contains("ambiguous"));
    assert_eq!(
        fs::read_to_string(workspace.path().join("one.txt")).expect("unchanged"),
        "same\nother\nsame\n"
    );
}

#[tokio::test]
async fn apply_patch_supports_model_add_and_delete_entries() {
    let workspace = tempdir().expect("workspace");
    fs::write(workspace.path().join("remove.txt"), "remove me\n").expect("fixture");
    let executor = executor(workspace.path());
    let patch = concat!(
        "*** Begin Patch\n",
        "*** Add File: empty.txt\n",
        "*** Delete File: remove.txt\n",
        "*** End Patch\n",
    );

    let report = executor
        .execute(
            request("apply_patch", json!({"patch": patch})),
            CancellationToken::new(),
        )
        .await
        .expect("add/delete patch execution");

    assert_eq!(report.envelope.status, ToolResultStatus::Ok);
    assert!(workspace.path().join("empty.txt").is_file());
    assert!(!workspace.path().join("remove.txt").exists());
    assert_eq!(report.changed_files.len(), 2);
}

#[tokio::test]
async fn apply_patch_preserves_model_whitespace_and_outer_padding() {
    let workspace = tempdir().expect("workspace");
    let executor = executor(workspace.path());
    let patch = concat!(
        "\n  *** Begin Patch\n",
        "*** Add File:whitespace.txt\n",
        "+  trailing spaces  \n",
        "*** End Patch   \n\n",
    );

    executor
        .execute(
            request("apply_patch", json!({"patch": patch})),
            CancellationToken::new(),
        )
        .await
        .expect("padded model patch execution");

    assert_eq!(
        fs::read_to_string(workspace.path().join("whitespace.txt")).expect("file"),
        "  trailing spaces  \n"
    );
}

#[tokio::test]
async fn apply_patch_rejects_model_add_when_target_exists() {
    let workspace = tempdir().expect("workspace");
    let path = workspace.path().join("existing.txt");
    fs::write(&path, "original\n").expect("fixture");
    let executor = executor(workspace.path());
    let patch = concat!(
        "*** Begin Patch\n",
        "*** Add File: existing.txt\n",
        "+replacement\n",
        "*** End Patch\n",
    );

    let error = executor
        .execute(
            request("apply_patch", json!({"patch": patch})),
            CancellationToken::new(),
        )
        .await
        .expect_err("existing add target must be rejected");

    assert!(error.to_string().contains("already exists"));
    assert_eq!(
        fs::read_to_string(path).expect("unchanged file"),
        "original\n"
    );
}

#[tokio::test]
async fn apply_patch_rejects_model_move_when_destination_exists() {
    let workspace = tempdir().expect("workspace");
    fs::write(workspace.path().join("source.txt"), "source\n").expect("source fixture");
    fs::write(workspace.path().join("destination.txt"), "destination\n")
        .expect("destination fixture");
    let executor = executor(workspace.path());
    let patch = concat!(
        "*** Begin Patch\n",
        "*** Update File: source.txt\n",
        "*** Move to: destination.txt\n",
        "@@\n",
        "-source\n",
        "+changed\n",
        "*** End Patch\n",
    );

    let error = executor
        .execute(
            request("apply_patch", json!({"patch": patch})),
            CancellationToken::new(),
        )
        .await
        .expect_err("existing move destination must be rejected");

    assert!(error.to_string().contains("destination already exists"));
    assert!(workspace.path().join("source.txt").is_file());
    assert!(workspace.path().join("destination.txt").is_file());
}

#[test]
fn model_patch_rejects_lexical_path_alias_collisions() {
    let patches = [
        concat!(
            "*** Begin Patch\n",
            "*** Update File: a.txt\n",
            "@@\n",
            "-old\n",
            "+new\n",
            "*** Add File: ./a.txt\n",
            "+other\n",
            "*** End Patch\n",
        ),
        concat!(
            "*** Begin Patch\n",
            "*** Update File: dir/../a.txt\n",
            "@@\n",
            "-old\n",
            "+new\n",
            "*** Add File: a.txt\n",
            "+other\n",
            "*** End Patch\n",
        ),
        concat!(
            "*** Begin Patch\n",
            "*** Update File: a.txt\n",
            "*** Move to: ./a.txt\n",
            "@@\n",
            "-old\n",
            "+new\n",
            "*** End Patch\n",
        ),
    ];

    for patch in patches {
        let error = super::model_patch::parse(patch).expect_err("path aliases must conflict");
        assert!(
            error.contains("more than once"),
            "unexpected alias error: {error}"
        );
    }
}

#[test]
fn model_patch_render_rejects_lexical_move_self_collision() {
    let patch = super::model_patch::ModelPatch {
        files: vec![super::model_patch::ModelPatchFile {
            path: PathBuf::from("a.txt"),
            move_path: Some(PathBuf::from("./a.txt")),
            kind: super::model_patch::ModelPatchFileKind::Update(vec![
                super::model_patch::ModelPatchHunk {
                    header: "@@".to_owned(),
                    context: None,
                    lines: vec!["-old".to_owned(), "+new".to_owned()],
                    end_of_file: false,
                    new_no_newline: false,
                },
            ]),
        }],
    };
    let originals = std::collections::BTreeMap::from([(PathBuf::from("a.txt"), b"old\n".to_vec())]);

    let error = super::model_patch::render(&patch, &originals)
        .expect_err("lexical move aliases must be rejected by the renderer");
    assert!(
        error.contains("move destination must differ"),
        "unexpected error: {error}"
    );
}

#[test]
fn model_patch_rejects_hunks_without_a_change() {
    let patch = concat!(
        "*** Begin Patch\n",
        "*** Update File: a.txt\n",
        "@@\n",
        " context only\n",
        "*** End Patch\n",
    );
    let error = super::model_patch::parse(patch).expect_err("context-only hunk must be rejected");
    assert!(error.contains("does not change"));
}

#[tokio::test]
async fn apply_patch_allows_an_explicit_empty_add_file() {
    let workspace = tempdir().expect("workspace");
    let executor = executor(workspace.path());
    let patch = concat!(
        "*** Begin Patch\n",
        "*** Add File: empty.txt\n",
        "*** End Patch\n",
    );

    let report = executor
        .execute(
            request("apply_patch", json!({"patch": patch})),
            CancellationToken::new(),
        )
        .await
        .expect("empty add execution");
    assert_eq!(report.envelope.status, ToolResultStatus::Ok);
    assert_eq!(
        fs::read(workspace.path().join("empty.txt")).expect("empty file"),
        b""
    );
}

#[tokio::test]
async fn apply_patch_treats_an_unprefixed_blank_hunk_line_as_context() {
    let workspace = tempdir().expect("workspace");
    fs::write(workspace.path().join("blank.txt"), "before\n\nafter\n").expect("fixture");
    let executor = executor(workspace.path());
    let patch = concat!(
        "*** Begin Patch\n",
        "*** Update File: blank.txt\n",
        "@@\n",
        " before\n",
        "\n",
        "-after\n",
        "+changed\n",
        "*** End Patch\n",
    );

    executor
        .execute(
            request("apply_patch", json!({"patch": patch})),
            CancellationToken::new(),
        )
        .await
        .expect("blank context patch execution");
    assert_eq!(
        fs::read_to_string(workspace.path().join("blank.txt")).expect("patched file"),
        "before\n\nchanged\n"
    );
}

#[tokio::test]
async fn apply_patch_alias_collision_leaves_workspace_unchanged() {
    let workspace = tempdir().expect("workspace");
    let path = workspace.path().join("a.txt");
    fs::write(&path, "old\n").expect("fixture");
    let executor = executor(workspace.path());
    let patch = concat!(
        "*** Begin Patch\n",
        "*** Update File: a.txt\n",
        "@@\n",
        "-old\n",
        "+changed\n",
        "*** Add File: dir/../a.txt\n",
        "+conflict\n",
        "*** End Patch\n",
    );

    let error = executor
        .execute(
            request("apply_patch", json!({"patch": patch})),
            CancellationToken::new(),
        )
        .await
        .expect_err("alias collision must be rejected before writing");
    assert!(error.to_string().contains("more than once"));
    assert_eq!(fs::read_to_string(path).expect("unchanged file"), "old\n");
}

#[tokio::test]
async fn apply_patch_supports_model_move_and_context_anchor() {
    let workspace = tempdir().expect("workspace");
    fs::create_dir_all(workspace.path().join("src")).expect("source directory");
    fs::create_dir_all(workspace.path().join("dst")).expect("destination directory");
    fs::write(
        workspace.path().join("src/module.txt"),
        "section a\nvalue\nsection b\nvalue\n",
    )
    .expect("source fixture");
    let executor = executor(workspace.path());
    let patch = concat!(
        "*** Begin Patch\n",
        "*** Update File: src/module.txt\n",
        "*** Move to: dst/module.txt\n",
        "@@ section b\n",
        "-value\n",
        "+changed\n",
        "*** End Patch\n",
    );

    let report = executor
        .execute(
            request("apply_patch", json!({"patch": patch})),
            CancellationToken::new(),
        )
        .await
        .expect("move patch execution");

    assert_eq!(report.envelope.status, ToolResultStatus::Ok);
    assert!(!workspace.path().join("src/module.txt").exists());
    assert_eq!(
        fs::read_to_string(workspace.path().join("dst/module.txt")).expect("destination"),
        "section a\nvalue\nsection b\nchanged\n"
    );
    assert_eq!(report.changed_files.len(), 2);
}

#[tokio::test]
async fn apply_patch_uses_end_of_file_for_append_hunks() {
    let workspace = tempdir().expect("workspace");
    fs::write(workspace.path().join("tail.txt"), "first\nlast\n").expect("fixture");
    let executor = executor(workspace.path());
    let patch = concat!(
        "*** Begin Patch\n",
        "*** Update File: tail.txt\n",
        "@@\n",
        "+appended\n",
        "*** End of File\n",
        "*** End Patch\n",
    );

    let report = executor
        .execute(
            request("apply_patch", json!({"patch": patch})),
            CancellationToken::new(),
        )
        .await
        .expect("EOF patch execution");

    assert_eq!(report.envelope.status, ToolResultStatus::Ok);
    assert_eq!(
        fs::read_to_string(workspace.path().join("tail.txt")).expect("tail"),
        "first\nlast\nappended\n"
    );
}

#[tokio::test]
async fn apply_patch_add_eof_marker_preserves_trailing_newline_semantics() {
    let workspace = tempdir().expect("workspace");
    let executor = executor(workspace.path());
    let patch = concat!(
        "*** Begin Patch\n",
        "*** Add File: with-eof-marker.txt\n",
        "+line\n",
        "*** End of File\n",
        "*** Add File: without-newline.txt\n",
        "+line\n",
        "\\ No newline at end of file\n",
        "*** End Patch\n",
    );

    let report = executor
        .execute(
            request("apply_patch", json!({"patch": patch})),
            CancellationToken::new(),
        )
        .await
        .expect("add patch execution");
    assert_eq!(report.envelope.status, ToolResultStatus::Ok);
    assert_eq!(
        fs::read_to_string(workspace.path().join("with-eof-marker.txt")).expect("eof file"),
        "line\n"
    );
    assert_eq!(
        fs::read_to_string(workspace.path().join("without-newline.txt")).expect("no-newline file"),
        "line"
    );
}

#[tokio::test]
async fn apply_patch_rejects_checkpoints_over_the_total_retention_limit() {
    let workspace = tempdir().expect("workspace");
    let file_bytes = MAX_WORKSPACE_SNAPSHOT_CONTENT_BYTES / 3 + 1;
    for name in ["one.bin", "two.bin", "three.bin"] {
        fs::write(workspace.path().join(name), vec![b'x'; file_bytes]).expect("large fixture");
    }
    let executor = executor(workspace.path());
    let patch = concat!(
        "diff --git a/one.bin b/one.bin\n",
        "--- a/one.bin\n",
        "+++ b/one.bin\n",
        "@@ -1 +1 @@\n",
        "-x\n",
        "+y\n",
        "diff --git a/three.bin b/three.bin\n",
        "--- a/three.bin\n",
        "+++ b/three.bin\n",
        "@@ -1 +1 @@\n",
        "-x\n",
        "+y\n",
        "diff --git a/two.bin b/two.bin\n",
        "--- a/two.bin\n",
        "+++ b/two.bin\n",
        "@@ -1 +1 @@\n",
        "-x\n",
        "+y\n",
    );

    let result = executor
        .prepare_side_effect_snapshot(&request("apply_patch", json!({"patch": patch})))
        .await;

    assert!(matches!(
        result,
        Err(ToolError::InvalidArguments(message))
            if message.contains("patch checkpoint exceeds")
    ));
    for name in ["one.bin", "two.bin", "three.bin"] {
        let bytes = fs::read(workspace.path().join(name)).expect("unchanged fixture");
        assert_eq!(bytes.len(), file_bytes);
        assert!(bytes.iter().all(|byte| *byte == b'x'));
    }
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
        json!({
            "path": "src.txt",
            "edits": [{"old_text": "before", "new_text": "agent"}]
        }),
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
        json!({
            "path": "src.txt",
            "edits": [{"old_text": "later", "new_text": "agent"}]
        }),
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

#[test]
fn shell_request_read_only_fast_path_is_conservative() {
    assert!(shell_request_is_strictly_read_only(
        &json!({"command": "cat README.md"})
    ));
    assert!(shell_request_is_strictly_read_only(
        &json!({"argv": ["rg", "token", "src"]})
    ));
    for arguments in [
        json!({"command": "bash -lc 'cat README.md'"}),
        json!({"command": "printf ok | tr o O"}),
        json!({"command": "cat README.md", "background": true}),
        json!({"command": "find . -exec cat {} +"}),
        json!({"command": "sort -o result.txt"}),
    ] {
        assert!(!shell_request_is_strictly_read_only(&arguments));
    }
}

#[tokio::test]
async fn strict_read_only_shell_skips_workspace_snapshot_and_reports_zero_changes() {
    let workspace = tempdir().expect("workspace");
    fs::write(workspace.path().join("README.md"), "read-only\n").expect("fixture");
    let executor = executor(workspace.path());
    let request = request("shell", json!({"command": "cat README.md"}));
    let policy = executor.evaluate(&request).expect("policy evaluates");
    let preparation = executor
        .prepare_side_effect_snapshot(&request)
        .await
        .expect("read-only preparation");

    assert!(preparation.before_images.is_empty());
    assert!(preparation.complete);
    assert!(!preparation.tracks_workspace_changes());

    let report = executor
        .invoke(
            ToolInvocation::new(request, policy, true).with_preparation(preparation),
            CancellationToken::new(),
            None,
        )
        .await
        .expect("read-only shell executes");
    assert_eq!(report.envelope.status, ToolResultStatus::Ok);
    assert_eq!(
        report.envelope.structured_facts["workspace_changes_known"],
        true
    );
    assert_eq!(
        report.envelope.structured_facts["workspace_change_count"],
        0
    );
    assert!(report.before_images.is_empty());
    assert!(report.after_images.is_empty());
    assert!(report.changed_files.is_empty());
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

#[cfg(unix)]
#[test]
fn shell_parser_implicitly_wraps_compound_commands_only_for_unrestricted_execution() {
    let command = CommandLine::parse_for_execution("printf ok | tr o O", true)
        .expect("unrestricted compound command");

    assert_eq!(command.program, "bash");
    assert_eq!(command.args, ["-lc", "printf ok | tr o O"]);
    assert_eq!(command.stdin, None);
    assert!(matches!(
        CommandLine::parse_for_execution("printf ok | tr o O", false),
        Err(ToolError::InvalidArguments(message))
            if message.contains("explicit bash -lc wrapper")
    ));
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
async fn shell_accepts_direct_argv_and_a_redundant_command_prefix() {
    let workspace = tempdir().expect("workspace");
    let executor = executor(workspace.path());
    let report = execute_approved(
        &executor,
        request(
            "shell",
            json!({
                "command": "printf",
                "argv": ["printf", "%s", "argv-ok"]
            }),
        ),
        CancellationToken::new(),
    )
    .await;

    assert_eq!(report.envelope.status, ToolResultStatus::Ok);
    assert_eq!(
        report.envelope.model_visible_excerpt.as_deref(),
        Some("argv-ok")
    );
}

#[tokio::test]
async fn shell_rejects_conflicting_command_and_argv_without_launching() {
    let workspace = tempdir().expect("workspace");
    let executor = executor(workspace.path());
    let error = executor
        .execute(
            request(
                "shell",
                json!({
                    "command": "printf",
                    "argv": ["echo", "should-not-run"]
                }),
            ),
            CancellationToken::new(),
        )
        .await
        .expect_err("conflicting command and argv must be rejected");

    assert!(error.to_string().contains("different commands"));
}

#[tokio::test]
async fn shell_direct_argv_preserves_newlines_without_shell_interpretation() {
    let workspace = tempdir().expect("workspace");
    let report = execute_approved(
        &executor(workspace.path()),
        request(
            "shell",
            json!({"argv": ["printf", "%s", "line-one\nline-two"]}),
        ),
        CancellationToken::new(),
    )
    .await;

    assert_eq!(report.envelope.status, ToolResultStatus::Ok);
    assert_eq!(
        report.envelope.model_visible_excerpt.as_deref(),
        Some("line-one\nline-two")
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
    assert_eq!(start.envelope.structured_facts["terminal"], false);
    assert_eq!(
        start.envelope.structured_facts["wait_strategy"],
        "event_driven_cursor"
    );
    let authoritative_pid = start.envelope.structured_facts["authoritative_pid"]
        .as_u64()
        .expect("authoritative pid");
    assert!(authoritative_pid > 0);
    assert!(start.envelope.summary.contains("post-runtime consumers"));
    assert!(start.envelope.summary.contains("detached process"));
    let process_id = start.envelope.structured_facts["process_id"]
        .as_str()
        .expect("process id")
        .to_owned();
    let first_cursor = start.envelope.structured_facts["output_cursor"]
        .as_u64()
        .expect("output cursor");
    assert_eq!(
        start.envelope.structured_facts["next_action"]["tool"],
        "shell_session"
    );
    assert_eq!(
        start.envelope.structured_facts["next_action"]["cursor"],
        first_cursor
    );
    assert_eq!(
        start.envelope.structured_facts["next_action"]["authoritative_pid"],
        authoritative_pid
    );
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
    assert_eq!(terminal.envelope.structured_facts["terminal"], true);
    let terminal_event_id = terminal.envelope.structured_facts["terminal_event_id"]
        .as_u64()
        .expect("terminal event id");
    assert!(terminal_event_id > 0);
    assert_eq!(
        terminal.envelope.structured_facts["next_action"]["kind"],
        "terminal"
    );
    assert_eq!(
        terminal.envelope.structured_facts["next_action"]["terminal_event_id"],
        terminal_event_id
    );
    let reconnected = executor
        .execute(
            request_for_session(
                session_id,
                "shell_session",
                json!({
                    "action": "wait",
                    "process_id": process_id,
                    "authoritative_pid": authoritative_pid,
                    "cursor": cursor,
                    "wait_ms": 0,
                }),
            ),
            CancellationToken::new(),
        )
        .await
        .expect("reconnect terminal process");
    assert_eq!(
        reconnected.envelope.structured_facts["terminal_event_id"],
        terminal_event_id
    );
    assert!(
        terminal
            .changed_files
            .iter()
            .any(|path| path.ends_with("background.txt"))
    );
}

#[tokio::test]
async fn shell_session_unifies_event_wait_write_and_terminate() {
    let workspace = tempdir().expect("workspace");
    fs::write(
        workspace.path().join("session.sh"),
        "printf 'ready\\n'\nread value\nprintf 'received:%s\\n' \"$value\"\n",
    )
    .expect("session script");
    let executor = executor(workspace.path());
    let session_id = SessionId::new();
    let start = execute_approved(
        &executor,
        request_for_session(
            session_id,
            "shell",
            json!({
                "command": "sh session.sh",
                "background": true,
                "yield_time_ms": 1_000,
            }),
        ),
        CancellationToken::new(),
    )
    .await;
    let process_id = start.envelope.structured_facts["process_id"]
        .as_str()
        .expect("process id")
        .to_owned();
    let authoritative_pid = start.envelope.structured_facts["authoritative_pid"]
        .as_u64()
        .expect("authoritative pid");
    let cursor = start.envelope.structured_facts["output_cursor"]
        .as_u64()
        .expect("cursor");
    let wrong_pid = authoritative_pid.saturating_add(1);
    let rejected = executor
        .execute(
            request_for_session(
                session_id,
                "shell_session",
                json!({
                    "action": "wait",
                    "process_id": process_id,
                    "authoritative_pid": wrong_pid,
                    "cursor": cursor,
                    "wait_ms": 0,
                }),
            ),
            CancellationToken::new(),
        )
        .await;
    assert!(
        matches!(rejected, Err(ToolError::Execution(message)) if message.contains("authoritative PID mismatch"))
    );
    let missing_pid = executor
        .execute(
            request_for_session(
                session_id,
                "shell_session",
                json!({
                    "action": "wait",
                    "process_id": process_id,
                    "cursor": cursor,
                    "wait_ms": 0,
                }),
            ),
            CancellationToken::new(),
        )
        .await;
    assert!(matches!(missing_pid, Err(ToolError::InvalidArguments(_))));
    let waited = executor
        .execute(
            request_for_session(
                session_id,
                "shell_session",
                json!({
                    "action": "wait",
                    "process_id": process_id,
                    "authoritative_pid": authoritative_pid,
                    "cursor": cursor,
                    "wait_ms": 0,
                }),
            ),
            CancellationToken::new(),
        )
        .await
        .expect("session wait");
    assert_eq!(waited.envelope.structured_facts["process_state"], "running");
    let next_cursor = waited.envelope.structured_facts["output_cursor"]
        .as_u64()
        .expect("next cursor");
    let written = executor
        .execute(
            request_for_session(
                session_id,
                "shell_session",
                json!({
                    "action": "write",
                    "process_id": process_id,
                    "authoritative_pid": authoritative_pid,
                    "cursor": next_cursor,
                    "input": "done\n",
                    "wait_ms": 1_000,
                }),
            ),
            CancellationToken::new(),
        )
        .await
        .expect("session write");
    assert!(artifact_text(&written).contains("received:done"));
    let terminal = executor
        .execute(
            request_for_session(
                session_id,
                "shell_session",
                json!({
                    "action": "wait",
                    "process_id": process_id,
                    "authoritative_pid": authoritative_pid,
                    "cursor": written.envelope.structured_facts["output_cursor"],
                    "wait_ms": 1_000,
                }),
            ),
            CancellationToken::new(),
        )
        .await
        .expect("session terminal wait");
    assert_eq!(
        terminal.envelope.structured_facts["process_state"],
        "exited"
    );
}

#[tokio::test]
async fn shell_workdir_changes_cwd_without_changing_workspace_tracking_root() {
    let workspace = tempdir().expect("workspace");
    let nested = workspace.path().join("nested");
    fs::create_dir(&nested).expect("nested directory");
    fs::write(
        nested.join("foreground.sh"),
        "pwd\nprintf foreground > ../foreground.txt\n",
    )
    .expect("foreground script");
    let executor = executor(workspace.path());

    let report = execute_approved(
        &executor,
        request(
            "shell",
            json!({"command": "sh foreground.sh", "workdir": "nested"}),
        ),
        CancellationToken::new(),
    )
    .await;

    assert_eq!(report.envelope.status, ToolResultStatus::Ok);
    assert!(
        artifact_text(&report).contains(
            &nested
                .canonicalize()
                .expect("canonical nested")
                .display()
                .to_string()
        )
    );
    assert_eq!(
        fs::read_to_string(workspace.path().join("foreground.txt")).expect("foreground output"),
        "foreground"
    );
    assert!(
        report
            .changed_files
            .iter()
            .any(|path| path.ends_with("foreground.txt"))
    );

    let outside = tempdir().expect("outside");
    let blocked = request(
        "shell",
        json!({
            "command": "sh foreground.sh",
            "workdir": outside.path().display().to_string(),
        }),
    );
    let policy = executor.evaluate(&blocked).expect("workdir policy");
    assert_eq!(policy.decision, PolicyDecision::Block);
    assert!(policy.reason.contains("outside workspace"));
}

#[tokio::test]
async fn process_list_is_session_scoped_and_reports_background_status() {
    let workspace = tempdir().expect("workspace");
    let nested = workspace.path().join("nested");
    fs::create_dir(&nested).expect("nested directory");
    fs::write(
        nested.join("background.sh"),
        concat!(
            "printf 'ready\\n'\n",
            "printf background > ../background.txt\n",
            "read value\n",
            "printf 'done:%s\\n' \"$value\"\n",
        ),
    )
    .expect("background script");
    let executor = executor(workspace.path());
    let session_id = SessionId::new();
    let start = execute_approved(
        &executor,
        request_for_session(
            session_id,
            "shell",
            json!({
                "command": "sh background.sh API_KEY=plain-secret-value",
                "workdir": "nested",
                "background": true,
                "yield_time_ms": 1_000,
            }),
        ),
        CancellationToken::new(),
    )
    .await;
    let process_id = start.envelope.structured_facts["process_id"]
        .as_str()
        .expect("process id")
        .to_owned();
    let cursor = start.envelope.structured_facts["output_cursor"]
        .as_u64()
        .expect("output cursor");

    let listed = executor
        .execute(
            request_for_session(session_id, "process_list", json!({})),
            CancellationToken::new(),
        )
        .await
        .expect("list current session");
    assert_eq!(listed.envelope.status, ToolResultStatus::Ok);
    assert_eq!(listed.envelope.structured_facts["process_count"], 1);
    assert_eq!(listed.envelope.structured_facts["running_count"], 1);
    assert_eq!(listed.metrics.item_count, Some(1));
    let process = &listed.envelope.structured_facts["processes"][0];
    assert_eq!(process["process_id"], process_id);
    assert_eq!(
        process["command"],
        "sh background.sh API_KEY=<redacted-secret>"
    );
    assert!(!artifact_text(&listed).contains("plain-secret-value"));
    assert_eq!(process["process_state"], "running");
    assert_eq!(process["output_cursor"], cursor);
    assert!(
        process["output_bytes"]
            .as_u64()
            .is_some_and(|bytes| bytes > 0)
    );
    assert!(!artifact_text(&listed).contains("ready"));

    let isolated = executor
        .execute(
            request_for_session(SessionId::new(), "process_list", json!({})),
            CancellationToken::new(),
        )
        .await
        .expect("list other session");
    assert_eq!(isolated.envelope.structured_facts["process_count"], 0);

    let mut terminal = executor
        .execute(
            request_for_session(
                session_id,
                "process_write",
                json!({
                    "process_id": process_id,
                    "input": "continue\n",
                    "cursor": cursor,
                    "wait_ms": 1_000,
                }),
            ),
            CancellationToken::new(),
        )
        .await
        .expect("finish background process");
    let mut terminal_cursor = terminal.envelope.structured_facts["output_cursor"]
        .as_u64()
        .expect("terminal cursor");
    while terminal.envelope.structured_facts["process_state"] == "running" {
        terminal = executor
            .execute(
                request_for_session(
                    session_id,
                    "process_poll",
                    json!({
                        "process_id": process_id,
                        "cursor": terminal_cursor,
                        "wait_ms": 1_000,
                    }),
                ),
                CancellationToken::new(),
            )
            .await
            .expect("poll background process to terminal state");
        terminal_cursor = terminal.envelope.structured_facts["output_cursor"]
            .as_u64()
            .expect("terminal cursor");
    }
    assert_eq!(
        terminal.envelope.structured_facts["process_state"],
        "exited"
    );
    assert!(
        terminal
            .changed_files
            .iter()
            .any(|path| path.ends_with("background.txt"))
    );

    let completed = executor
        .execute(
            request_for_session(session_id, "process_list", json!({})),
            CancellationToken::new(),
        )
        .await
        .expect("list completed process");
    let process = &completed.envelope.structured_facts["processes"][0];
    assert_eq!(process["process_state"], "exited");
    assert_eq!(process["exit_code"], 0);
}

#[cfg(unix)]
#[tokio::test]
async fn process_supervisor_can_terminate_all_running_session_processes() {
    let workspace = tempdir().expect("workspace");
    let supervisor = ProcessSupervisor::new();
    let executor = executor(workspace.path()).with_process_supervisor(supervisor.clone());
    let session_id = SessionId::new();
    let started = execute_approved(
        &executor,
        request_for_session(
            session_id,
            "shell",
            json!({
                "command": "sleep 30",
                "background": true,
                "yield_time_ms": 1,
            }),
        ),
        CancellationToken::new(),
    )
    .await;
    assert_eq!(
        started.envelope.structured_facts["process_state"],
        "running"
    );

    let terminated = supervisor
        .terminate_session(session_id)
        .await
        .expect("terminate session processes");
    assert_eq!(terminated, 1);

    let listed = executor
        .execute(
            request_for_session(session_id, "process_list", json!({})),
            CancellationToken::new(),
        )
        .await
        .expect("list terminated process");
    assert_eq!(listed.envelope.structured_facts["process_count"], 1);
    assert_eq!(
        listed.envelope.structured_facts["processes"][0]["process_state"],
        "terminated"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn process_supervisor_clears_pid_after_reaching_terminal_state() {
    let workspace = tempdir().expect("workspace");
    let supervisor = ProcessSupervisor::new();
    let sandbox = SystemSandbox::process_only();
    let session_id = SessionId::new();
    let args = vec!["-c".to_owned(), "printf ready\\n; exit 0".to_owned()];
    let workspace_before = workspace_scan::capture(workspace.path()).await;
    let started = supervisor
        .start(ProcessStartRequest {
            process_id: "clear-pid".to_owned(),
            session_id,
            program: "/bin/sh",
            args: &args,
            command_display: "clear pid fixture".to_owned(),
            cwd: workspace.path(),
            workspace_root: workspace.path(),
            timeout_ms: 5_000,
            wait_ms: 1_000,
            cancellation: CancellationToken::new(),
            sandbox: &sandbox,
            workspace_access: WorkspaceAccess::ReadWrite,
            allow_network: false,
            workspace_before,
        })
        .await
        .expect("process starts");
    let terminal = if started.state.is_terminal() {
        started
    } else {
        tokio::time::timeout(
            Duration::from_secs(2),
            supervisor.poll(session_id, "clear-pid", started.output_cursor, 2_000),
        )
        .await
        .expect("wait for terminal remains bounded")
        .expect("process reaches terminal")
    };
    assert!(terminal.state.is_terminal());
    assert_eq!(
        supervisor
            .retained_pid_for_test(session_id, "clear-pid")
            .await,
        None,
        "terminal processes must release their OS PID handle"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn process_supervisor_terminal_terminate_does_not_signal_stale_pid() {
    let workspace = tempdir().expect("workspace");
    let supervisor = ProcessSupervisor::new();
    let sandbox = SystemSandbox::process_only();
    let session_id = SessionId::new();
    let args = vec!["-c".to_owned(), "printf ready\\n; exit 0".to_owned()];
    let workspace_before = workspace_scan::capture(workspace.path()).await;
    let started = supervisor
        .start(ProcessStartRequest {
            process_id: "stale-pid".to_owned(),
            session_id,
            program: "/bin/sh",
            args: &args,
            command_display: "stale pid fixture".to_owned(),
            cwd: workspace.path(),
            workspace_root: workspace.path(),
            timeout_ms: 5_000,
            wait_ms: 1_000,
            cancellation: CancellationToken::new(),
            sandbox: &sandbox,
            workspace_access: WorkspaceAccess::ReadWrite,
            allow_network: false,
            workspace_before,
        })
        .await
        .expect("process starts");
    let terminal = if started.state.is_terminal() {
        started
    } else {
        tokio::time::timeout(
            Duration::from_secs(2),
            supervisor.poll(session_id, "stale-pid", started.output_cursor, 2_000),
        )
        .await
        .expect("wait for terminal remains bounded")
        .expect("process reaches terminal")
    };
    assert!(terminal.state.is_terminal());
    assert_eq!(
        supervisor
            .retained_pid_for_test(session_id, "stale-pid")
            .await,
        None
    );

    // Bait: a live unrelated process. If terminate() still signals the injected
    // PID after terminal publication, this child will die.
    let mut bait = std::process::Command::new("/bin/sleep")
        .arg("30")
        .spawn()
        .expect("spawn bait process");
    let bait_pid = bait.id();
    assert!(
        supervisor
            .inject_pid_for_test(session_id, "stale-pid", bait_pid)
            .await,
        "terminal entry accepts injected stale pid for the regression"
    );

    let after_terminate = supervisor
        .terminate(session_id, "stale-pid", terminal.output_cursor)
        .await
        .expect("terminate on terminal process remains idempotent");
    assert!(after_terminate.state.is_terminal());

    let bait_status = std::process::Command::new("ps")
        .args(["-o", "stat=", "-p", &bait_pid.to_string()])
        .output()
        .expect("inspect bait status");
    let bait_is_running = bait_status.status.success()
        && String::from_utf8_lossy(&bait_status.stdout)
            .lines()
            .any(|line| !line.trim().is_empty() && !line.trim_start().starts_with('Z'));
    let _ = bait.kill();
    let _ = bait.wait();
    assert!(
        bait_is_running,
        "terminate after terminal must not signal a stale/recycled PID"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn process_supervisor_reaped_window_does_not_signal_an_injected_pid() {
    let workspace = tempdir().expect("workspace");
    let supervisor = ProcessSupervisor::new();
    let sandbox = SystemSandbox::process_only();
    let session_id = SessionId::new();
    let process_id = "reaped-window";
    let (entered, release) = supervisor.install_reap_window_hook_for_test(process_id);
    let args = vec!["-c".to_owned(), "exit 0".to_owned()];
    let workspace_before = workspace_scan::capture(workspace.path()).await;
    let started = supervisor
        .start(ProcessStartRequest {
            process_id: process_id.to_owned(),
            session_id,
            program: "/bin/sh",
            args: &args,
            command_display: "reaped window fixture".to_owned(),
            cwd: workspace.path(),
            workspace_root: workspace.path(),
            timeout_ms: 5_000,
            wait_ms: 0,
            cancellation: CancellationToken::new(),
            sandbox: &sandbox,
            workspace_access: WorkspaceAccess::ReadWrite,
            allow_network: false,
            workspace_before,
        })
        .await
        .expect("process starts");
    tokio::time::timeout(Duration::from_secs(2), entered.notified())
        .await
        .expect("reaped window is reached");

    let mut bait = std::process::Command::new("/bin/sleep")
        .arg("30")
        .spawn()
        .expect("spawn bait process");
    let bait_pid = bait.id();
    assert!(
        supervisor
            .inject_reaped_pid_for_test(session_id, process_id, bait_pid)
            .await,
        "the fixture must expose a reaped lifecycle before terminal publication"
    );
    supervisor
        .request_termination_for_test(session_id, process_id)
        .await
        .expect("request termination");

    let bait_status = std::process::Command::new("ps")
        .args(["-o", "stat=", "-p", &bait_pid.to_string()])
        .output()
        .expect("inspect bait status");
    let bait_is_running = bait_status.status.success()
        && String::from_utf8_lossy(&bait_status.stdout)
            .lines()
            .any(|line| !line.trim().is_empty() && !line.trim_start().starts_with('Z'));
    assert!(
        bait_is_running,
        "termination after child reap must not signal an injected PID"
    );

    release.notify_one();
    let terminal = tokio::time::timeout(
        Duration::from_secs(2),
        supervisor.poll(session_id, process_id, started.output_cursor, 2_000),
    )
    .await
    .expect("terminal bookkeeping remains bounded")
    .expect("terminal snapshot");
    assert_eq!(terminal.state, ProcessState::Terminated);
    assert!(terminal.terminal_event_id.is_some());
    assert_eq!(
        supervisor
            .retained_pid_for_test(session_id, process_id)
            .await,
        None
    );

    let _ = bait.kill();
    let _ = bait.wait();
}

#[cfg(unix)]
#[tokio::test]
async fn process_supervisor_caps_retained_terminal_process_memory() {
    let workspace = tempdir().expect("workspace");
    let supervisor = ProcessSupervisor::new();
    let sandbox = SystemSandbox::process_only();
    let session_id = SessionId::new();
    let limit = max_terminal_processes();
    for index in 0..(limit + 4) {
        let process_id = format!("terminal-cap-{index}");
        let args = vec!["-c".to_owned(), format!("printf '{index}\\n'; exit 0")];
        let workspace_before = workspace_scan::capture(workspace.path()).await;
        let process_id_for_poll = process_id.clone();
        let started = supervisor
            .start(ProcessStartRequest {
                process_id,
                session_id,
                program: "/bin/sh",
                args: &args,
                command_display: format!("terminal cap fixture {index}"),
                cwd: workspace.path(),
                workspace_root: workspace.path(),
                timeout_ms: 5_000,
                wait_ms: 1_000,
                cancellation: CancellationToken::new(),
                sandbox: &sandbox,
                workspace_access: WorkspaceAccess::ReadWrite,
                allow_network: false,
                workspace_before,
            })
            .await
            .expect("terminal process starts");
        let terminal = if started.state.is_terminal() {
            started
        } else {
            tokio::time::timeout(
                Duration::from_secs(2),
                supervisor.poll(
                    session_id,
                    &process_id_for_poll,
                    started.output_cursor,
                    2_000,
                ),
            )
            .await
            .expect("wait for terminal remains bounded")
            .expect("process reaches terminal")
        };
        assert!(terminal.state.is_terminal());
    }
    // list() prunes before reading, so the post-insert overflow from the final
    // start is trimmed before we assert the retention cap.
    let listed = supervisor.list(session_id).await;
    let retained = supervisor.retained_process_count_for_test().await;
    assert!(
        retained <= limit,
        "retained terminal processes {retained} exceeded cap {limit}"
    );
    assert_eq!(listed.len(), retained);
}

#[cfg(unix)]
#[tokio::test]
async fn process_supervisor_terminate_session_eagerly_kills_descendants() {
    let workspace = tempdir().expect("workspace");
    let supervisor = ProcessSupervisor::new();
    let sandbox = SystemSandbox::process_only();
    let session_id = SessionId::new();
    let args = vec![
        "-c".to_owned(),
        "sleep 30 & child=$!; printf 'child=%s\\n' \"$child\"; wait".to_owned(),
    ];
    let workspace_before = workspace_scan::capture(workspace.path()).await;
    let started = tokio::time::timeout(
        Duration::from_secs(2),
        supervisor.start(ProcessStartRequest {
            process_id: "terminate-session-descendant".to_owned(),
            session_id,
            program: "/bin/sh",
            args: &args,
            command_display: "terminate session descendant fixture".to_owned(),
            cwd: workspace.path(),
            workspace_root: workspace.path(),
            timeout_ms: 30_000,
            wait_ms: 1_000,
            cancellation: CancellationToken::new(),
            sandbox: &sandbox,
            workspace_access: WorkspaceAccess::ReadWrite,
            allow_network: false,
            workspace_before,
        }),
    )
    .await
    .expect("process start remains bounded")
    .expect("process starts");
    assert_eq!(started.state, ProcessState::Running);
    let child_pid = started
        .output
        .lines()
        .find_map(|line| line.strip_prefix("child=")?.trim().parse::<i32>().ok())
        .expect("descendant pid is reported");

    let terminated = tokio::time::timeout(
        Duration::from_secs(2),
        supervisor.terminate_session(session_id),
    )
    .await
    .expect("terminate_session remains bounded")
    .expect("session processes terminate");
    assert_eq!(terminated, 1);

    let terminal = supervisor
        .reconnect(session_id, "terminate-session-descendant", 0)
        .await
        .expect("terminal process remains queryable");
    assert!(terminal.state.is_terminal());
    let descendant_status = std::process::Command::new("ps")
        .args(["-o", "stat=", "-p", &child_pid.to_string()])
        .output()
        .expect("inspect descendant status");
    let descendant_is_running = descendant_status.status.success()
        && String::from_utf8_lossy(&descendant_status.stdout)
            .lines()
            .any(|line| !line.trim().is_empty() && !line.trim_start().starts_with('Z'));
    assert!(
        !descendant_is_running,
        "managed descendant survived terminate_session"
    );
    assert_eq!(
        supervisor
            .retained_pid_for_test(session_id, "terminate-session-descendant")
            .await,
        None
    );
}

#[cfg(unix)]
#[tokio::test]
async fn process_supervisor_shutdown_waits_for_terminal_bookkeeping_and_descendants() {
    let workspace = tempdir().expect("workspace");
    let supervisor = ProcessSupervisor::new();
    let sandbox = SystemSandbox::process_only();
    let session_id = SessionId::new();
    let args = vec![
        "-c".to_owned(),
        "sleep 30 & child=$!; printf 'child=%s\\n' \"$child\"; wait".to_owned(),
    ];
    let workspace_before = workspace_scan::capture(workspace.path()).await;
    let started = tokio::time::timeout(
        Duration::from_secs(2),
        supervisor.start(ProcessStartRequest {
            process_id: "shutdown-descendant".to_owned(),
            session_id,
            program: "/bin/sh",
            args: &args,
            command_display: "shutdown descendant fixture".to_owned(),
            cwd: workspace.path(),
            workspace_root: workspace.path(),
            timeout_ms: 30_000,
            wait_ms: 1_000,
            cancellation: CancellationToken::new(),
            sandbox: &sandbox,
            workspace_access: WorkspaceAccess::ReadWrite,
            allow_network: false,
            workspace_before,
        }),
    )
    .await
    .expect("process start remains bounded")
    .expect("process starts");
    assert_eq!(started.state, ProcessState::Running);
    let child_pid = started
        .output
        .lines()
        .find_map(|line| line.strip_prefix("child=")?.trim().parse::<i32>().ok())
        .expect("descendant pid is reported");

    tokio::time::timeout(Duration::from_secs(2), supervisor.shutdown_and_wait())
        .await
        .expect("shutdown remains bounded")
        .expect("managed process shutdown");

    let terminal = supervisor
        .reconnect(session_id, "shutdown-descendant", 0)
        .await
        .expect("terminal process remains queryable");
    assert!(terminal.state.is_terminal());
    assert_eq!(
        terminal.state,
        ProcessState::Cancelled,
        "supervisor shutdown must preserve its cancellation reason"
    );
    let descendant_status = std::process::Command::new("ps")
        .args(["-o", "stat=", "-p", &child_pid.to_string()])
        .output()
        .expect("inspect descendant status");
    let descendant_is_running = descendant_status.status.success()
        && String::from_utf8_lossy(&descendant_status.stdout)
            .lines()
            .any(|line| !line.trim().is_empty() && !line.trim_start().starts_with('Z'));
    assert!(
        !descendant_is_running,
        "managed descendant survived shutdown"
    );
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn process_supervisor_starts_each_process_id_at_most_once() {
    const STARTERS: usize = 8;

    let workspace = tempdir().expect("workspace");
    let workspace_path = workspace.path().to_path_buf();
    let supervisor = ProcessSupervisor::new();
    let session_id = SessionId::new();
    let barrier = Arc::new(tokio::sync::Barrier::new(STARTERS));
    let mut starts = Vec::with_capacity(STARTERS);

    for _ in 0..STARTERS {
        let barrier = Arc::clone(&barrier);
        let supervisor = supervisor.clone();
        let workspace_path = workspace_path.clone();
        starts.push(tokio::spawn(async move {
            let sandbox = SystemSandbox::process_only();
            let args = vec![
                "-c".to_owned(),
                "printf x >> launch-count; sleep 30".to_owned(),
            ];
            let workspace_before = workspace_scan::capture(&workspace_path).await;
            barrier.wait().await;
            supervisor
                .start(ProcessStartRequest {
                    process_id: "shared-process-id".to_owned(),
                    session_id,
                    program: "/bin/sh",
                    args: &args,
                    command_display: "shared process fixture".to_owned(),
                    cwd: &workspace_path,
                    workspace_root: &workspace_path,
                    timeout_ms: 30_000,
                    wait_ms: 0,
                    cancellation: CancellationToken::new(),
                    sandbox: &sandbox,
                    workspace_access: WorkspaceAccess::ReadWrite,
                    allow_network: false,
                    workspace_before,
                })
                .await
        }));
    }

    for start in starts {
        let snapshot = start.await.expect("start task").expect("start process");
        assert_eq!(snapshot.process_id, "shared-process-id");
    }
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if tokio::fs::read_to_string(workspace_path.join("launch-count"))
                .await
                .is_ok_and(|value| !value.is_empty())
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("process writes launch marker");

    assert_eq!(
        tokio::fs::read_to_string(workspace_path.join("launch-count"))
            .await
            .expect("launch marker"),
        "x"
    );
    assert_eq!(supervisor.list(session_id).await.len(), 1);
    assert_eq!(
        supervisor
            .terminate_session(session_id)
            .await
            .expect("terminate process"),
        1
    );
}

#[cfg(unix)]
#[allow(clippy::too_many_arguments)]
async fn start_process_identity_fixture(
    supervisor: &ProcessSupervisor,
    process_id: &str,
    session_id: SessionId,
    program: &str,
    args: &[String],
    cwd: &Path,
    workspace_root: &Path,
    timeout_ms: u64,
    sandbox: &SystemSandbox,
    workspace_access: WorkspaceAccess,
    allow_network: bool,
) -> Result<ProcessSnapshot, ToolError> {
    let workspace_before = workspace_scan::capture(workspace_root).await;
    supervisor
        .start(ProcessStartRequest {
            process_id: process_id.to_owned(),
            session_id,
            program,
            args,
            command_display: "process identity fixture".to_owned(),
            cwd,
            workspace_root,
            timeout_ms,
            wait_ms: 0,
            cancellation: CancellationToken::new(),
            sandbox,
            workspace_access,
            allow_network,
            workspace_before,
        })
        .await
}

#[cfg(unix)]
#[tokio::test]
async fn process_supervisor_reuses_only_equivalent_start_requests() {
    let root = tempdir().expect("root");
    let workspace = root.path().join("workspace");
    let cwd = workspace.join("cwd");
    let alternate_cwd = workspace.join("alternate-cwd");
    fs::create_dir_all(&cwd).expect("cwd");
    fs::create_dir_all(&alternate_cwd).expect("alternate cwd");
    let workspace_alias = root.path().join("workspace-alias");
    std::os::unix::fs::symlink(&workspace, &workspace_alias).expect("workspace alias");

    let supervisor = ProcessSupervisor::new();
    let sandbox = SystemSandbox::process_only();
    let session_id = SessionId::new();
    let process_id = "request-identity";
    let args = vec!["-c".to_owned(), "sleep 30".to_owned()];
    let alternate_args = vec!["-c".to_owned(), "sleep 29".to_owned()];

    let started = start_process_identity_fixture(
        &supervisor,
        process_id,
        session_id,
        "/bin/sh",
        &args,
        &cwd,
        &workspace,
        30_000,
        &sandbox,
        WorkspaceAccess::ReadWrite,
        false,
    )
    .await
    .expect("start process");
    assert_eq!(started.process_id, process_id);

    let reused = start_process_identity_fixture(
        &supervisor,
        process_id,
        session_id,
        "/bin/sh",
        &args,
        &workspace_alias.join("cwd"),
        &workspace_alias,
        30_000,
        &sandbox,
        WorkspaceAccess::ReadWrite,
        false,
    )
    .await
    .expect("reuse canonical-equivalent process request");
    assert_eq!(reused.process_id, process_id);

    struct ConflictCase<'a> {
        label: &'static str,
        program: &'a str,
        args: &'a [String],
        cwd: &'a Path,
        workspace_root: &'a Path,
        timeout_ms: u64,
        workspace_access: WorkspaceAccess,
        allow_network: bool,
    }

    let cases = [
        ConflictCase {
            label: "program",
            program: "/bin/echo",
            args: &args,
            cwd: &cwd,
            workspace_root: &workspace,
            timeout_ms: 30_000,
            workspace_access: WorkspaceAccess::ReadWrite,
            allow_network: false,
        },
        ConflictCase {
            label: "arguments",
            program: "/bin/sh",
            args: &alternate_args,
            cwd: &cwd,
            workspace_root: &workspace,
            timeout_ms: 30_000,
            workspace_access: WorkspaceAccess::ReadWrite,
            allow_network: false,
        },
        ConflictCase {
            label: "working directory",
            program: "/bin/sh",
            args: &args,
            cwd: &alternate_cwd,
            workspace_root: &workspace,
            timeout_ms: 30_000,
            workspace_access: WorkspaceAccess::ReadWrite,
            allow_network: false,
        },
        ConflictCase {
            label: "workspace root",
            program: "/bin/sh",
            args: &args,
            cwd: &cwd,
            workspace_root: root.path(),
            timeout_ms: 30_000,
            workspace_access: WorkspaceAccess::ReadWrite,
            allow_network: false,
        },
        ConflictCase {
            label: "timeout",
            program: "/bin/sh",
            args: &args,
            cwd: &cwd,
            workspace_root: &workspace,
            timeout_ms: 29_000,
            workspace_access: WorkspaceAccess::ReadWrite,
            allow_network: false,
        },
        ConflictCase {
            label: "workspace access",
            program: "/bin/sh",
            args: &args,
            cwd: &cwd,
            workspace_root: &workspace,
            timeout_ms: 30_000,
            workspace_access: WorkspaceAccess::ReadOnly,
            allow_network: false,
        },
        ConflictCase {
            label: "network access",
            program: "/bin/sh",
            args: &args,
            cwd: &cwd,
            workspace_root: &workspace,
            timeout_ms: 30_000,
            workspace_access: WorkspaceAccess::ReadWrite,
            allow_network: true,
        },
    ];

    for case in cases {
        let error = start_process_identity_fixture(
            &supervisor,
            process_id,
            session_id,
            case.program,
            case.args,
            case.cwd,
            case.workspace_root,
            case.timeout_ms,
            &sandbox,
            case.workspace_access,
            case.allow_network,
        )
        .await
        .expect_err(case.label);
        let ToolError::Execution(message) = error else {
            panic!("{} returned the wrong error: {error}", case.label);
        };
        assert_eq!(
            message,
            format!(
                "process `{process_id}` idempotency conflict: start request does not match the existing process"
            ),
            "{}",
            case.label
        );
    }

    assert_eq!(supervisor.list(session_id).await.len(), 1);
    assert_eq!(
        supervisor
            .terminate_session(session_id)
            .await
            .expect("terminate process"),
        1
    );
}

#[cfg(unix)]
#[tokio::test]
async fn process_supervisor_bounds_reader_drain_when_a_descendant_keeps_pipes_open() {
    let workspace = tempdir().expect("workspace");
    let supervisor = ProcessSupervisor::new();
    let sandbox = SystemSandbox::process_only();
    let session_id = SessionId::new();
    let args = vec![
        "-c".to_owned(),
        "sleep 30 & child=$!; printf 'child=%s\\n' \"$child\"".to_owned(),
    ];
    let workspace_before = workspace_scan::capture(workspace.path()).await;

    let snapshot = tokio::time::timeout(
        Duration::from_secs(2),
        supervisor.start(ProcessStartRequest {
            process_id: "inherited-pipes".to_owned(),
            session_id,
            program: "/bin/sh",
            args: &args,
            command_display: "inherited pipe fixture".to_owned(),
            cwd: workspace.path(),
            workspace_root: workspace.path(),
            timeout_ms: 30_000,
            wait_ms: 1_500,
            cancellation: CancellationToken::new(),
            sandbox: &sandbox,
            workspace_access: WorkspaceAccess::ReadWrite,
            allow_network: false,
            workspace_before,
        }),
    )
    .await
    .expect("reader drain remains bounded")
    .expect("process starts");

    let terminal = supervisor
        .poll(session_id, "inherited-pipes", snapshot.output_cursor, 1_500)
        .await
        .expect("wait for natural process exit");
    assert_eq!(terminal.state, ProcessState::Exited);
    assert_eq!(terminal.exit_code, Some(0));
    let child_pid = format!("{}{}", snapshot.output, terminal.output)
        .lines()
        .find_map(|line| line.strip_prefix("child=")?.trim().parse::<i32>().ok())
        .expect("descendant pid is reported");
    let descendant_status = std::process::Command::new("ps")
        .args(["-o", "stat=", "-p", &child_pid.to_string()])
        .output()
        .expect("inspect descendant status");
    let descendant_is_running = descendant_status.status.success()
        && String::from_utf8_lossy(&descendant_status.stdout)
            .lines()
            .any(|line| !line.trim().is_empty() && !line.trim_start().starts_with('Z'));
    assert!(
        !descendant_is_running,
        "descendant holding inherited pipes survived natural process exit"
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
        Duration::from_secs(7),
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
                "timeout_ms": 15_000,
                "yield_time_ms": 0,
            }),
        ),
        CancellationToken::new(),
    )
    .await;
    let process_id = start.envelope.structured_facts["process_id"]
        .as_str()
        .expect("process id");
    let mut cursor = start.envelope.structured_facts["output_cursor"]
        .as_u64()
        .expect("start cursor");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let report = executor
            .execute(
                request_for_session(
                    session_id,
                    "process_poll",
                    json!({"process_id": process_id, "cursor": cursor, "wait_ms": 250}),
                ),
                CancellationToken::new(),
            )
            .await
            .expect("poll output flood");
        cursor = report.envelope.structured_facts["output_cursor"]
            .as_u64()
            .expect("output cursor");
        if report.envelope.structured_facts["output_truncated"] == true {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "output journal did not reach its retention bound"
        );
    }
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

    tokio::time::timeout(
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
    .expect("output flood process termination remains responsive")
    .expect("output flood termination report");

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
            json!({
                "path": "src.txt",
                "edits": [{"old_text": "", "new_text": "replacement"}]
            }),
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
