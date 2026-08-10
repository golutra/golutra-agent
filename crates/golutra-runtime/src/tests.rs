use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

use golutra_context::{ContextBudgetPolicy, ContextBuilder, ContextContributor, estimate_tokens};
use golutra_core::{
    Actor, ActorKind, BudgetOverflowAction, BusyPolicy, PolicyBlockDisposition, TaskStatus,
    ToolCallId, WorkspaceId,
};
use golutra_governor::GovernorLimits;
use golutra_llm::{
    LlmProvider, MockProvider, ProviderError, ProviderFinishReason, ProviderMessage,
    ProviderRequest, ProviderResponse, ProviderStreamEvent, ProviderToolCall, ProviderUsage,
    UsageSource,
};
use golutra_policy::WorkspacePolicy;
use golutra_protocol::{ExternalVerificationSpec, RuntimeEventType};
use golutra_sandbox::SystemSandbox;
use golutra_tools::BasicToolExecutor;
use serde_json::json;
use tempfile::tempdir;

use super::*;

fn objective_test_report(tool_name: &str, command: Option<&str>) -> ToolExecutionReport {
    ToolExecutionReport {
        envelope: golutra_core::ToolResultEnvelope {
            tool_call_id: ToolCallId::new(),
            tool_name: tool_name.to_owned(),
            status: ToolResultStatus::Ok,
            summary: format!("{tool_name} completed"),
            structured_facts: command.map_or_else(
                || json!({}),
                |command| {
                    json!({
                        "command": command,
                        "exit_code": 0,
                        "timed_out": false,
                        "cancelled": false,
                    })
                },
            ),
            model_visible_excerpt: None,
            raw_artifact_ref: None,
            evidence_refs: Vec::new(),
            risk: "p0_local_tool".to_owned(),
            verification_hint: None,
        },
        policy_evaluation: golutra_core::PolicyEvaluation {
            policy_ref: golutra_core::PolicyId::new(),
            subject: "tool".to_owned(),
            action: tool_name.to_owned(),
            resource: command.unwrap_or(tool_name).to_owned(),
            decision: PolicyDecision::Allow,
            block_disposition: None,
            reason: "test".to_owned(),
            evidence_refs: Vec::new(),
        },
        artifacts: Vec::new(),
        evidence: Vec::new(),
        artifact_contents: Vec::new(),
        metrics: Default::default(),
        changed_files: Vec::new(),
        before_images: Vec::new(),
        after_images: Vec::new(),
    }
}

fn objective_test_report_with_output(command: &str, output: &str) -> ToolExecutionReport {
    let mut report = objective_test_report("shell", Some(command));
    report
        .artifact_contents
        .push(golutra_tools::ArtifactContent {
            artifact_id: golutra_core::ArtifactId::new(),
            bytes: output.as_bytes().to_vec(),
        });
    report
}

#[test]
fn agent_run_preserves_legacy_touched_code_contract() {
    let run = AgentRun::new(AgentTaskRequest {
        session_id: SessionId::new(),
        task_id: TaskId::new(),
        turn_id: TurnId::new(),
        objective: "change the implementation".to_owned(),
        completion_criteria: vec!["tests pass".to_owned()],
        output_schema: None,
        touched_code: true,
        contributors: Vec::new(),
        tools: Vec::new(),
    });

    assert_eq!(
        run.task_contract.workspace_change,
        WorkspaceChangeRequirement::Required
    );
    assert!(run.task_contract.require_objective_validation);
    assert_eq!(run.task_contract.max_correction_rounds, 1);
}

#[test]
fn shell_timeout_is_clamped_to_the_remaining_governor_budget() {
    let mut request = golutra_tools::ToolRequest {
        tool_call_id: ToolCallId::new(),
        provider_tool_call_id: None,
        session_id: SessionId::new(),
        turn_id: None,
        tool_name: "shell".to_owned(),
        arguments: json!({"command": "sleep 10", "timeout_ms": 30_000}),
    };

    clamp_shell_timeout_to_budget(&mut request, 125);
    assert_eq!(request.arguments["timeout_ms"], 125);

    request.arguments = json!({"command": "sleep 10"});
    clamp_shell_timeout_to_budget(&mut request, 30_000);
    assert_eq!(request.arguments["timeout_ms"], 5_000);

    request.arguments = json!({"command": "sleep 10", "background": true});
    clamp_shell_timeout_to_budget(&mut request, 30_000);
    assert_eq!(request.arguments["timeout_ms"], 30_000);

    request.arguments = json!({"command": "sleep 10", "background": true});
    clamp_shell_timeout_to_budget(&mut request, 2 * 60 * 60 * 1_000);
    assert_eq!(request.arguments["timeout_ms"], 60 * 60 * 1_000);

    request.arguments["timeout_ms"] = json!("invalid");
    clamp_shell_timeout_to_budget(&mut request, 40);
    assert_eq!(request.arguments["timeout_ms"], "invalid");
}

#[test]
fn shell_execution_budget_preserves_the_first_deadline_advisory_window() {
    assert_eq!(shell_execution_budget(600_000, 0, false), 480_000);
    assert_eq!(shell_execution_budget(600_000, 243_000, false), 237_000);
    assert_eq!(shell_execution_budget(600_000, 500_000, false), 50_000);
    assert_eq!(shell_execution_budget(600_000, 500_000, true), 70_000);
    assert_eq!(shell_execution_budget(600_000, 580_000, true), 10_000);
}

#[test]
fn runtime_observation_sink_accepts_a_function_adapter() {
    let mut observed = Vec::new();
    let mut sink = |observation| observed.push(observation);

    RuntimeObservationSink::emit(
        &mut sink,
        RuntimeObservation::ToolStarted {
            tool_call_id: ToolCallId::new(),
            provider_tool_call_id: None,
            tool_name: "read_file".to_owned(),
            display_arguments: json!({"path": "README.md"}),
            recovery_policy: ToolRecoveryPolicy::for_side_effect(SideEffectType::None),
        },
    );

    assert!(matches!(
        observed.as_slice(),
        [RuntimeObservation::ToolStarted { tool_name, .. }] if tool_name == "read_file"
    ));
}

#[test]
fn successful_tool_result_resets_only_the_consecutive_failure_count() {
    let mut total = 7;
    let mut consecutive = 3;

    update_tool_failure_counts(ToolResultStatus::Ok, &mut total, &mut consecutive);
    assert_eq!(total, 7);
    assert_eq!(consecutive, 0);

    update_tool_failure_counts(ToolResultStatus::Error, &mut total, &mut consecutive);
    assert_eq!(total, 8);
    assert_eq!(consecutive, 1);
}

#[test]
fn duplicate_failures_in_one_provider_round_count_as_one_retry() {
    let failed = HashSet::from(["shell:{\"command\":\"git\"}".to_owned()]);
    let mut signature = None;
    let mut count = 0;

    update_repeated_failure_streak(&failed, &mut signature, &mut count);
    assert_eq!(count, 1);
    update_repeated_failure_streak(&failed, &mut signature, &mut count);
    assert_eq!(count, 2);

    update_repeated_failure_streak(&HashSet::new(), &mut signature, &mut count);
    assert_eq!(signature, None);
    assert_eq!(count, 0);
}

#[test]
fn progress_fingerprint_only_groups_the_same_inspection_of_a_file() {
    let first = semantic_tool_action_fingerprint(
        "shell",
        &json!({"command": "bash -lc 'sed -n \"1,120p\" src/runtime.rs'"}),
    );
    let repeated = semantic_tool_action_fingerprint(
        "shell",
        &json!({"command": "bash -lc 'sed -n \"1,120p\" src/runtime.rs'"}),
    );
    let next_slice = semantic_tool_action_fingerprint(
        "shell",
        &json!({"command": "bash -lc 'sed -n \"121,240p\" src/runtime.rs'"}),
    );
    let other = semantic_tool_action_fingerprint(
        "shell",
        &json!({"command": "bash -lc 'sed -n \"1,120p\" src/policy.rs'"}),
    );
    let first_experiment = semantic_tool_action_fingerprint(
        "shell",
        &json!({"command": "bash -lc 'cat > /tmp/probe.c <<EOF\nint x;\nEOF\ngcc /tmp/probe.c'"}),
    );
    let revised_experiment = semantic_tool_action_fingerprint(
        "shell",
        &json!({"command": "bash -lc 'cat > /tmp/probe.c <<EOF\nint main(void) { return 0; }\nEOF\ngcc /tmp/probe.c'"}),
    );

    assert_eq!(first, repeated);
    assert_ne!(first, next_slice);
    assert_ne!(first, other);
    assert_ne!(first_experiment, revised_experiment);
}

#[test]
fn directory_delivery_paths_accept_changed_descendants_only_for_directories() {
    let changed = HashSet::from(["agent.py".to_owned(), "trained_model/policy.pt".to_owned()]);

    assert!(delivery_path_was_changed("agent.py", false, &changed));
    assert!(delivery_path_was_changed("trained_model", true, &changed));
    assert!(!delivery_path_was_changed("trained_model", false, &changed));
    assert!(!delivery_path_was_changed("trained", true, &changed));
}

#[test]
fn successful_objective_validation_is_material_progress() {
    let report = objective_test_report("shell", Some("test -f result.txt"));

    assert!(objective_validation_report(&report).is_some_and(|outcome| outcome.passed));
}

#[test]
fn prepared_validation_metadata_contains_only_observed_command_facts() {
    let secret = "sk-runtime-validation-secret-1234567890";
    let command = format!(
        "python - <<'PY'\nfrom pathlib import Path\ntoken = \"{secret}\"\nassert Path('result.txt').read_text() == 'expected'\nPY"
    );
    let request = ToolRequest {
        tool_call_id: ToolCallId::new(),
        provider_tool_call_id: None,
        session_id: SessionId::new(),
        turn_id: Some(TurnId::new()),
        tool_name: "shell".to_owned(),
        arguments: json!({"command": command}),
    };
    let metadata =
        prepare_objective_validation_metadata(&request).expect("prepared validation metadata");
    assert!(!metadata.to_string().contains(secret));
    assert_eq!(
        metadata.as_object().map(|value| value.len()),
        Some(2),
        "objective text must not add inferred validation requirements"
    );

    let mut report = objective_test_report("shell", Some("<redacted command is not parseable>"));
    attach_prepared_objective_validation(&mut report, Some(metadata));

    let outcome = objective_validation_report(&report).expect("prepared validation outcome");
    assert!(outcome.passed);
    assert_eq!(outcome.kind, ObjectiveValidationKind::Diagnostic);
}

#[test]
fn python_validation_requires_observed_runtime_state() {
    for command in [
        r#"python3 -c "accuracy = 0.7; assert accuracy >= 0.62""#,
        r#"python3 -c "actual = {'status': 'ready'}; assert actual['status'] == 'ready'""#,
        r#"python3 -c "from pathlib import Path; actual = Path('result.txt').read_text(); actual = 'expected'; assert actual == 'expected'""#,
        r#"python3 -c "import json; actual = json.loads('{\"status\": \"ready\"}'); assert actual['status'] == 'ready'""#,
        r#"python3 -c "import hashlib; actual = hashlib.sha256(b'constant').hexdigest(); assert len(actual) == 64""#,
        r#"python3 -c "import re; assert re.fullmatch('ready', 'ready')""#,
        r#"python3 -c "assert actual == expected""#,
        r#"python3 -c "if actual != expected: raise RuntimeError('mismatch')""#,
    ] {
        assert!(!is_objective_validation_command(command), "{command}");
    }

    for command in [
        r#"python3 -c "from pathlib import Path; actual = Path('result.txt').read_text(); assert actual == 'expected'""#,
        r#"python3 -c "from pathlib import Path; score = float(Path('score.txt').read_text()); assert score >= 0.62""#,
        r#"python3 -c "from pathlib import Path; actual = Path('result.txt').read_text(); actual += '\n'; assert actual.endswith('\n')""#,
        r#"python3 -c "from pathlib import Path; assert Path('result.txt').exists()""#,
        r#"python3 -c "import json; from pathlib import Path; actual = json.loads(Path('result.json').read_text()); assert actual['status'] == 'ready'""#,
        r#"python3 -c "import requests; response = requests.get('https://example.com/status'); assert response.status_code == 200""#,
        r#"python3 -c "artifact = load_artifact('result.bin'); assert artifact.valid""#,
    ] {
        assert!(is_objective_validation_command(command), "{command}");
    }
}

#[test]
fn inspection_validation_is_limited_to_pure_analysis_contracts() {
    let workspace = tempdir().expect("workspace");
    let path = workspace.path().join("input.txt");
    fs::write(&path, "input").expect("input");
    let report = ToolExecutionReport {
        envelope: golutra_core::ToolResultEnvelope {
            tool_call_id: ToolCallId::new(),
            tool_name: "read_file".to_owned(),
            status: ToolResultStatus::Ok,
            summary: "file read".to_owned(),
            structured_facts: json!({"path": path.clone()}),
            model_visible_excerpt: None,
            raw_artifact_ref: None,
            evidence_refs: Vec::new(),
            risk: "p0_local_tool".to_owned(),
            verification_hint: None,
        },
        policy_evaluation: golutra_core::PolicyEvaluation {
            policy_ref: golutra_core::PolicyId::new(),
            subject: "tool".to_owned(),
            action: "read_file".to_owned(),
            resource: path.display().to_string(),
            decision: PolicyDecision::Allow,
            block_disposition: None,
            reason: "test".to_owned(),
            evidence_refs: Vec::new(),
        },
        artifacts: Vec::new(),
        evidence: Vec::new(),
        artifact_contents: Vec::new(),
        metrics: Default::default(),
        changed_files: Vec::new(),
        before_images: Vec::new(),
        after_images: Vec::new(),
    };
    let objective = "inspect input.txt";

    assert!(
        explicitly_requested_inspection_validation(
            &report,
            objective,
            &[],
            &TaskContract::default(),
            workspace.path(),
        )
        .is_some()
    );
    for contract in [
        TaskContract {
            workspace_change: WorkspaceChangeRequirement::Required,
            ..TaskContract::default()
        },
        TaskContract {
            require_objective_validation: true,
            ..TaskContract::default()
        },
        TaskContract {
            verification: VerificationRequirement::Required,
            ..TaskContract::default()
        },
        TaskContract {
            verification: VerificationRequirement::Independent,
            ..TaskContract::default()
        },
    ] {
        assert!(
            explicitly_requested_inspection_validation(
                &report,
                objective,
                &[],
                &contract,
                workspace.path(),
            )
            .is_none()
        );
    }
}

#[test]
fn old_tool_results_are_compacted_without_breaking_tool_message_identity() {
    let large_result = serde_json::to_string(&json!({
        "tool_name": "shell",
        "status": "error",
        "summary": "dependency installation failed",
        "model_visible_excerpt": "package output ".repeat(4_000),
    }))
    .expect("tool result");
    let mut messages = (0..3)
        .map(|index| ProviderMessage {
            role: ProviderRole::Tool,
            content: large_result.clone(),
            tool_call_id: Some(format!("call-{index}")),
            tool_name: Some("shell".to_owned()),
            tool_calls: Vec::new(),
            metadata: Default::default(),
        })
        .collect::<Vec<_>>();
    let mut sources = (0..3)
        .map(|index| ContextMessageSource {
            contributor: "tool_result_excerpt".to_owned(),
            source_refs: vec![format!("tool-call:{index}")],
            origin: "tool_result".to_owned(),
            visibility: ModelInputVisibility::ModelVisible,
        })
        .collect::<Vec<_>>();

    let compacted = compact_tool_result_history(&mut messages, &mut sources);

    assert_eq!(compacted, 2);
    assert!(messages[0].content.contains("history_state"));
    assert!(messages[1].content.contains("history_state"));
    assert_eq!(messages[2].content, large_result);
    assert_eq!(messages[0].tool_call_id.as_deref(), Some("call-0"));
    assert_eq!(sources[0].origin, "tool_result_compaction");
}

#[test]
fn semantic_failure_families_survive_unrelated_successes() {
    let apt = semantic_failure_family(
        "shell",
        &json!({"command": "bash -lc 'apt-get install -y python3-pip'"}),
    );
    let apt_variant = semantic_failure_family(
        "shell",
        &json!({"command": "DEBIAN_FRONTEND=noninteractive apt-get install python3-pip"}),
    );
    let diagnostic =
        semantic_failure_family("shell", &json!({"command": "python3 -m pip --version"}));
    assert_eq!(apt, "dependency_install:apt:python3-pip");
    assert_eq!(apt, apt_variant);
    assert_ne!(apt, diagnostic);

    let mut ledger = FailureFamilyLedger::default();
    ledger.observe(&apt, ToolResultStatus::Timeout);
    ledger.observe(&diagnostic, ToolResultStatus::Ok);
    ledger.observe(&apt_variant, ToolResultStatus::Error);

    assert_eq!(ledger.failures(&apt), 2);
    assert_eq!(ledger.failures(&diagnostic), 0);
}

#[derive(Debug, Clone)]
enum FallbackTestProvider {
    Failing(Box<golutra_core::ProviderContract>),
    Endless(Box<golutra_core::ProviderContract>),
    Success(Box<MockProvider>),
}

#[derive(Debug, Clone)]
struct SixRoundProvider {
    calls: Arc<AtomicUsize>,
    contract: golutra_core::ProviderContract,
}

#[derive(Debug, Clone)]
struct SupportThenDeliveryProvider {
    calls: Arc<AtomicUsize>,
    contract: golutra_core::ProviderContract,
}

#[derive(Debug, Clone)]
struct ValidationGateProvider {
    calls: Arc<AtomicUsize>,
    saw_nudge: Arc<AtomicBool>,
    contract: golutra_core::ProviderContract,
}

#[derive(Debug, Clone)]
struct DuplicateFailureRecoveryProvider {
    calls: Arc<AtomicUsize>,
    saw_duplicate_results: Arc<AtomicBool>,
    contract: golutra_core::ProviderContract,
}

#[derive(Debug, Clone)]
struct ToolResultProjectionProvider {
    calls: Arc<AtomicUsize>,
    saw_operational_facts: Arc<AtomicBool>,
    saw_governance_metadata: Arc<AtomicBool>,
    contract: golutra_core::ProviderContract,
}

#[derive(Debug, Clone)]
struct StructuredQuestionProvider {
    calls: Arc<AtomicUsize>,
    saw_answer: Arc<AtomicBool>,
    contract: golutra_core::ProviderContract,
}

#[derive(Debug, Clone)]
struct ProgressAdvisoryProvider {
    calls: Arc<AtomicUsize>,
    saw_advisory: Arc<AtomicBool>,
    contract: golutra_core::ProviderContract,
}

#[derive(Debug, Clone)]
struct CorrectionStallProvider {
    calls: Arc<AtomicUsize>,
    saw_advisory: Arc<AtomicBool>,
    contract: golutra_core::ProviderContract,
}

#[derive(Debug, Clone)]
struct AssistantOnlyCorrectionProvider {
    calls: Arc<AtomicUsize>,
    contract: golutra_core::ProviderContract,
}

#[derive(Debug, Clone)]
struct RequiredReadProvider {
    calls: Arc<AtomicUsize>,
    contract: golutra_core::ProviderContract,
}

#[derive(Debug, Clone)]
struct EndlessProgressProvider {
    contract: golutra_core::ProviderContract,
}

#[derive(Debug, Clone)]
struct SequencedTextProvider {
    calls: Arc<AtomicUsize>,
    delay: Duration,
    block_from_call: Option<usize>,
    contract: golutra_core::ProviderContract,
}

#[derive(Debug, Clone)]
struct QueuedWriteCorrectionProvider {
    calls: Arc<AtomicUsize>,
    contract: golutra_core::ProviderContract,
}

#[async_trait]
impl LlmProvider for QueuedWriteCorrectionProvider {
    async fn complete(&self, _request: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        let (message, tool_calls, finish_reason) = if call == 1 {
            (
                None,
                vec![ProviderToolCall {
                    tool_call_id: "queued-write".to_owned(),
                    tool_name: "write_file".to_owned(),
                    arguments: json!({"path": "result.py", "content": "value = 1\n"}),
                }],
                ProviderFinishReason::ToolCalls,
            )
        } else {
            (
                Some(ProviderMessage {
                    role: ProviderRole::Assistant,
                    content: format!("response {call}"),
                    tool_call_id: None,
                    tool_name: None,
                    tool_calls: Vec::new(),
                    metadata: Default::default(),
                }),
                Vec::new(),
                ProviderFinishReason::Stop,
            )
        };
        Ok(ProviderResponse {
            response_id: golutra_core::ProviderResponseId::new(),
            message,
            tool_calls,
            usage: ProviderUsage {
                input_tokens: Some(10),
                output_tokens: Some(5),
                reasoning_tokens: None,
                cached_input_tokens: None,
                total_tokens: Some(15),
                usage_source: UsageSource::Estimated,
                raw: json!({"round": call}),
            },
            finish_reason,
            raw_metadata: json!({"round": call}),
        })
    }

    fn contract(&self) -> golutra_core::ProviderContract {
        self.contract.clone()
    }
}

#[async_trait]
impl LlmProvider for SequencedTextProvider {
    async fn complete(&self, _request: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        if self.block_from_call.is_some_and(|limit| call >= limit) {
            loop {
                tokio::time::sleep(Duration::from_secs(60)).await;
            }
        }
        tokio::time::sleep(self.delay).await;
        Ok(ProviderResponse {
            response_id: golutra_core::ProviderResponseId::new(),
            message: Some(ProviderMessage {
                role: ProviderRole::Assistant,
                content: format!("response {call}"),
                tool_call_id: None,
                tool_name: None,
                tool_calls: Vec::new(),
                metadata: Default::default(),
            }),
            tool_calls: Vec::new(),
            usage: ProviderUsage {
                input_tokens: Some(10),
                output_tokens: Some(5),
                reasoning_tokens: None,
                cached_input_tokens: None,
                total_tokens: Some(15),
                usage_source: UsageSource::Estimated,
                raw: json!({"round": call}),
            },
            finish_reason: ProviderFinishReason::Stop,
            raw_metadata: json!({"round": call}),
        })
    }

    fn contract(&self) -> golutra_core::ProviderContract {
        self.contract.clone()
    }
}

#[async_trait]
impl LlmProvider for EndlessProgressProvider {
    async fn complete(&self, _request: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        loop {
            tokio::time::sleep(Duration::from_secs(60)).await;
        }
    }

    async fn complete_stream(
        &self,
        _request: ProviderRequest,
        on_event: &mut (dyn FnMut(ProviderStreamEvent) + Send),
    ) -> Result<ProviderResponse, ProviderError> {
        loop {
            tokio::time::sleep(Duration::from_millis(5)).await;
            on_event(ProviderStreamEvent::ReasoningDelta {
                text: ".".to_owned(),
            });
        }
    }

    fn contract(&self) -> golutra_core::ProviderContract {
        self.contract.clone()
    }
}

#[async_trait]
impl LlmProvider for RequiredReadProvider {
    async fn complete(&self, _request: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        let (message, tool_calls, finish_reason) = if call == 0 {
            (
                None,
                vec![
                    ProviderToolCall {
                        tool_call_id: "required-read".to_owned(),
                        tool_name: "read_file".to_owned(),
                        arguments: json!({"path": "required.bin"}),
                    },
                    ProviderToolCall {
                        tool_call_id: "unrelated-read".to_owned(),
                        tool_name: "read_file".to_owned(),
                        arguments: json!({"path": "available.txt"}),
                    },
                ],
                ProviderFinishReason::ToolCalls,
            )
        } else {
            (
                Some(ProviderMessage {
                    role: ProviderRole::Assistant,
                    content: "inspection finished".to_owned(),
                    tool_call_id: None,
                    tool_name: None,
                    tool_calls: Vec::new(),
                    metadata: Default::default(),
                }),
                Vec::new(),
                ProviderFinishReason::Stop,
            )
        };
        Ok(ProviderResponse {
            response_id: golutra_core::ProviderResponseId::new(),
            message,
            tool_calls,
            usage: ProviderUsage {
                input_tokens: Some(10),
                output_tokens: Some(5),
                reasoning_tokens: None,
                cached_input_tokens: None,
                total_tokens: Some(15),
                usage_source: UsageSource::Estimated,
                raw: json!({"round": call}),
            },
            finish_reason,
            raw_metadata: json!({"round": call}),
        })
    }

    fn contract(&self) -> golutra_core::ProviderContract {
        self.contract.clone()
    }
}

#[async_trait]
impl LlmProvider for AssistantOnlyCorrectionProvider {
    async fn complete(&self, _request: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        let (message, tool_calls, finish_reason) = if call == 0 {
            (
                None,
                vec![ProviderToolCall {
                    tool_call_id: "initial-write".to_owned(),
                    tool_name: "write_file".to_owned(),
                    arguments: json!({"path": "result.py", "content": "value = 1\n"}),
                }],
                ProviderFinishReason::ToolCalls,
            )
        } else {
            (
                Some(ProviderMessage {
                    role: ProviderRole::Assistant,
                    content: format!("candidate response {call}"),
                    tool_call_id: None,
                    tool_name: None,
                    tool_calls: Vec::new(),
                    metadata: Default::default(),
                }),
                Vec::new(),
                ProviderFinishReason::Stop,
            )
        };
        Ok(ProviderResponse {
            response_id: golutra_core::ProviderResponseId::new(),
            message,
            tool_calls,
            usage: ProviderUsage {
                input_tokens: Some(10),
                output_tokens: Some(5),
                reasoning_tokens: None,
                cached_input_tokens: None,
                total_tokens: Some(15),
                usage_source: UsageSource::Estimated,
                raw: json!({"round": call}),
            },
            finish_reason,
            raw_metadata: json!({"round": call}),
        })
    }

    fn contract(&self) -> golutra_core::ProviderContract {
        self.contract.clone()
    }
}

#[async_trait]
impl LlmProvider for CorrectionStallProvider {
    async fn complete(&self, request: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        if request.messages.iter().any(|message| {
            message.role == ProviderRole::User
                && message.content.contains("Runtime progress advisory")
                && message.content.contains("verification correction")
        }) {
            self.saw_advisory.store(true, Ordering::SeqCst);
        }
        let (message, tool_calls, finish_reason) = match call {
            0 => (
                None,
                vec![ProviderToolCall {
                    tool_call_id: "initial-write".to_owned(),
                    tool_name: "write_file".to_owned(),
                    arguments: json!({"path": "result.py", "content": "value = 1\n"}),
                }],
                ProviderFinishReason::ToolCalls,
            ),
            1 => (
                Some(ProviderMessage {
                    role: ProviderRole::Assistant,
                    content: "implementation complete".to_owned(),
                    tool_call_id: None,
                    tool_name: None,
                    tool_calls: Vec::new(),
                    metadata: Default::default(),
                }),
                Vec::new(),
                ProviderFinishReason::Stop,
            ),
            _ => {
                let probe = call.saturating_sub(2);
                (
                    None,
                    vec![ProviderToolCall {
                        tool_call_id: format!("correction-read-{probe}"),
                        tool_name: "read_file".to_owned(),
                        arguments: json!({"path": format!("probe-{probe}.txt")}),
                    }],
                    ProviderFinishReason::ToolCalls,
                )
            }
        };
        Ok(ProviderResponse {
            response_id: golutra_core::ProviderResponseId::new(),
            message,
            tool_calls,
            usage: ProviderUsage {
                input_tokens: Some(10),
                output_tokens: Some(5),
                reasoning_tokens: None,
                cached_input_tokens: None,
                total_tokens: Some(15),
                usage_source: UsageSource::Estimated,
                raw: json!({"round": call}),
            },
            finish_reason,
            raw_metadata: json!({"round": call}),
        })
    }

    fn contract(&self) -> golutra_core::ProviderContract {
        self.contract.clone()
    }
}

#[async_trait]
impl LlmProvider for ProgressAdvisoryProvider {
    async fn complete(&self, request: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        if call == 3 {
            self.saw_advisory.store(
                request.messages.iter().any(|message| {
                    message.role == ProviderRole::User
                        && message.content.contains("Runtime progress advisory")
                }),
                Ordering::SeqCst,
            );
        }
        let (message, tool_calls, finish_reason) = if call < 3 {
            (
                None,
                vec![ProviderToolCall {
                    tool_call_id: format!("repeated-read-{call}"),
                    tool_name: "read_file".to_owned(),
                    arguments: json!({"path": "input.txt"}),
                }],
                ProviderFinishReason::ToolCalls,
            )
        } else {
            (
                Some(ProviderMessage {
                    role: ProviderRole::Assistant,
                    content: "inspection complete".to_owned(),
                    tool_call_id: None,
                    tool_name: None,
                    tool_calls: Vec::new(),
                    metadata: Default::default(),
                }),
                Vec::new(),
                ProviderFinishReason::Stop,
            )
        };
        Ok(ProviderResponse {
            response_id: golutra_core::ProviderResponseId::new(),
            message,
            tool_calls,
            usage: ProviderUsage {
                input_tokens: Some(10),
                output_tokens: Some(5),
                reasoning_tokens: None,
                cached_input_tokens: None,
                total_tokens: Some(15),
                usage_source: UsageSource::Estimated,
                raw: json!({"round": call}),
            },
            finish_reason,
            raw_metadata: json!({"round": call}),
        })
    }

    fn contract(&self) -> golutra_core::ProviderContract {
        self.contract.clone()
    }
}

#[async_trait]
impl LlmProvider for ToolResultProjectionProvider {
    async fn complete(&self, request: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        if call == 1
            && let Some(tool_message) = request
                .messages
                .iter()
                .rev()
                .find(|message| message.role == ProviderRole::Tool)
        {
            let projection = serde_json::from_str::<serde_json::Value>(&tool_message.content)
                .expect("model-visible tool result is JSON");
            self.saw_operational_facts.store(
                projection["structured_facts"]["bytes"] == 2
                    && projection["model_visible_excerpt"] == "ok",
                Ordering::SeqCst,
            );
            self.saw_governance_metadata.store(
                projection.get("raw_artifact_ref").is_some()
                    || projection.get("evidence_refs").is_some()
                    || projection.get("risk").is_some()
                    || projection.get("verification_hint").is_some(),
                Ordering::SeqCst,
            );
        }
        let (message, tool_calls, finish_reason) = if call == 0 {
            (
                None,
                vec![ProviderToolCall {
                    tool_call_id: "projection-read".to_owned(),
                    tool_name: "read_file".to_owned(),
                    arguments: json!({"path": "input.txt"}),
                }],
                ProviderFinishReason::ToolCalls,
            )
        } else {
            (
                Some(ProviderMessage {
                    role: ProviderRole::Assistant,
                    content: "tool result received".to_owned(),
                    tool_call_id: None,
                    tool_name: None,
                    tool_calls: Vec::new(),
                    metadata: Default::default(),
                }),
                Vec::new(),
                ProviderFinishReason::Stop,
            )
        };
        Ok(ProviderResponse {
            response_id: golutra_core::ProviderResponseId::new(),
            message,
            tool_calls,
            usage: ProviderUsage {
                input_tokens: Some(10),
                output_tokens: Some(5),
                reasoning_tokens: None,
                cached_input_tokens: None,
                total_tokens: Some(15),
                usage_source: UsageSource::Estimated,
                raw: json!({"round": call}),
            },
            finish_reason,
            raw_metadata: json!({"round": call}),
        })
    }

    fn contract(&self) -> golutra_core::ProviderContract {
        self.contract.clone()
    }
}

#[async_trait]
impl LlmProvider for StructuredQuestionProvider {
    async fn complete(&self, request: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        let (message, tool_calls, finish_reason) = if call == 0 {
            (
                None,
                vec![ProviderToolCall {
                    tool_call_id: "question-1".to_owned(),
                    tool_name: "ask_user".to_owned(),
                    arguments: json!({
                        "questions": [{
                            "id": "format",
                            "header": "Output",
                            "question": "Which format?",
                            "mode": "single",
                            "options": [
                                {"id": "json", "label": "JSON"},
                                {"id": "text", "label": "Text"}
                            ]
                        }]
                    }),
                }],
                ProviderFinishReason::ToolCalls,
            )
        } else {
            self.saw_answer.store(
                request.messages.iter().rev().any(|message| {
                    message.role == ProviderRole::Tool
                        && message.tool_name.as_deref() == Some("ask_user")
                        && message.content.contains("json")
                        && message
                            .content
                            .contains("Pretty-print with two-space indentation")
                }),
                Ordering::SeqCst,
            );
            (
                Some(ProviderMessage {
                    role: ProviderRole::Assistant,
                    content: "JSON selected".to_owned(),
                    tool_call_id: None,
                    tool_name: None,
                    tool_calls: Vec::new(),
                    metadata: Default::default(),
                }),
                Vec::new(),
                ProviderFinishReason::Stop,
            )
        };
        Ok(ProviderResponse {
            response_id: golutra_core::ProviderResponseId::new(),
            message,
            tool_calls,
            usage: ProviderUsage {
                input_tokens: Some(10),
                output_tokens: Some(5),
                reasoning_tokens: None,
                cached_input_tokens: None,
                total_tokens: Some(15),
                usage_source: UsageSource::Estimated,
                raw: json!({"round": call}),
            },
            finish_reason,
            raw_metadata: json!({"round": call}),
        })
    }

    fn contract(&self) -> golutra_core::ProviderContract {
        self.contract.clone()
    }
}

#[async_trait]
impl LlmProvider for ValidationGateProvider {
    async fn complete(&self, request: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        let usage = ProviderUsage {
            input_tokens: Some(10),
            output_tokens: Some(5),
            reasoning_tokens: None,
            cached_input_tokens: None,
            total_tokens: Some(15),
            usage_source: UsageSource::Estimated,
            raw: json!({"round": call}),
        };
        let (message, tool_calls, finish_reason) = match call {
            0 => (
                None,
                vec![ProviderToolCall {
                    tool_call_id: "write-result".to_owned(),
                    tool_name: "write_file".to_owned(),
                    arguments: json!({"path": "recovered.txt", "content": "source bytes"}),
                }],
                ProviderFinishReason::ToolCalls,
            ),
            1 => (
                Some(ProviderMessage {
                    role: ProviderRole::Assistant,
                    content: "recovery complete".to_owned(),
                    tool_call_id: None,
                    tool_name: None,
                    tool_calls: Vec::new(),
                    metadata: Default::default(),
                }),
                Vec::new(),
                ProviderFinishReason::Stop,
            ),
            2 => {
                self.saw_nudge.store(
                    request.messages.iter().any(|message| {
                        message.role == ProviderRole::User
                            && message
                                .content
                                .contains("Runtime verification did not pass")
                    }),
                    Ordering::SeqCst,
                );
                (
                    None,
                    vec![ProviderToolCall {
                        tool_call_id: "compare-result".to_owned(),
                        tool_name: "shell".to_owned(),
                        arguments: json!({
                            "command": "python3 -c \"from pathlib import Path; assert Path('source.txt').read_bytes() == Path('recovered.txt').read_bytes()\""
                        }),
                    }],
                    ProviderFinishReason::ToolCalls,
                )
            }
            _ => (
                Some(ProviderMessage {
                    role: ProviderRole::Assistant,
                    content: "recovery verified".to_owned(),
                    tool_call_id: None,
                    tool_name: None,
                    tool_calls: Vec::new(),
                    metadata: Default::default(),
                }),
                Vec::new(),
                ProviderFinishReason::Stop,
            ),
        };
        Ok(ProviderResponse {
            response_id: golutra_core::ProviderResponseId::new(),
            message,
            tool_calls,
            usage,
            finish_reason,
            raw_metadata: json!({"round": call}),
        })
    }

    fn contract(&self) -> golutra_core::ProviderContract {
        self.contract.clone()
    }
}

#[async_trait]
impl LlmProvider for DuplicateFailureRecoveryProvider {
    async fn complete(&self, request: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        let usage = ProviderUsage {
            input_tokens: Some(10),
            output_tokens: Some(5),
            reasoning_tokens: None,
            cached_input_tokens: None,
            total_tokens: Some(15),
            usage_source: UsageSource::Estimated,
            raw: json!({"round": call}),
        };
        let (message, tool_calls, finish_reason) = match call {
            0 => (
                None,
                vec![
                    ProviderToolCall {
                        tool_call_id: "duplicate-shell-a".to_owned(),
                        tool_name: "shell".to_owned(),
                        arguments: json!({"command": "pwd && pwd"}),
                    },
                    ProviderToolCall {
                        tool_call_id: "duplicate-shell-b".to_owned(),
                        tool_name: "shell".to_owned(),
                        arguments: json!({"command": "pwd && pwd"}),
                    },
                ],
                ProviderFinishReason::ToolCalls,
            ),
            1 => {
                let tool_result_ids = request
                    .messages
                    .iter()
                    .filter(|message| message.role == ProviderRole::Tool)
                    .filter_map(|message| message.tool_call_id.as_deref())
                    .collect::<HashSet<_>>();
                self.saw_duplicate_results.store(
                    tool_result_ids == HashSet::from(["duplicate-shell-a", "duplicate-shell-b"]),
                    Ordering::SeqCst,
                );
                (
                    None,
                    vec![ProviderToolCall {
                        tool_call_id: "recovered-write".to_owned(),
                        tool_name: "write_file".to_owned(),
                        arguments: json!({"path": "result.txt", "content": "recovered\n"}),
                    }],
                    ProviderFinishReason::ToolCalls,
                )
            }
            _ => (
                Some(ProviderMessage {
                    role: ProviderRole::Assistant,
                    content: "recovered after duplicate failures".to_owned(),
                    tool_call_id: None,
                    tool_name: None,
                    tool_calls: Vec::new(),
                    metadata: Default::default(),
                }),
                Vec::new(),
                ProviderFinishReason::Stop,
            ),
        };
        Ok(ProviderResponse {
            response_id: golutra_core::ProviderResponseId::new(),
            message,
            tool_calls,
            usage,
            finish_reason,
            raw_metadata: json!({"round": call}),
        })
    }

    fn contract(&self) -> golutra_core::ProviderContract {
        self.contract.clone()
    }
}

#[async_trait]
impl LlmProvider for SupportThenDeliveryProvider {
    async fn complete(&self, _request: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        let usage = ProviderUsage {
            input_tokens: Some(10),
            output_tokens: Some(5),
            reasoning_tokens: None,
            cached_input_tokens: None,
            total_tokens: Some(15),
            usage_source: UsageSource::Estimated,
            raw: json!({"round": call}),
        };
        let (message, tool_calls, finish_reason) = match call {
            0 => (
                None,
                vec![ProviderToolCall {
                    tool_call_id: "read-support".to_owned(),
                    tool_name: "read_file".to_owned(),
                    arguments: json!({"path": "input.txt"}),
                }],
                ProviderFinishReason::ToolCalls,
            ),
            1 => (
                None,
                vec![ProviderToolCall {
                    tool_call_id: "write-support".to_owned(),
                    tool_name: "write_file".to_owned(),
                    arguments: json!({"path": "helper.py", "content": "print('support')"}),
                }],
                ProviderFinishReason::ToolCalls,
            ),
            2 => (
                None,
                vec![ProviderToolCall {
                    tool_call_id: "write-result".to_owned(),
                    tool_name: "write_file".to_owned(),
                    arguments: json!({"path": "results.txt", "content": "done"}),
                }],
                ProviderFinishReason::ToolCalls,
            ),
            _ => (
                Some(ProviderMessage {
                    role: ProviderRole::Assistant,
                    content: "done".to_owned(),
                    tool_call_id: None,
                    tool_name: None,
                    tool_calls: Vec::new(),
                    metadata: Default::default(),
                }),
                Vec::new(),
                ProviderFinishReason::Stop,
            ),
        };
        Ok(ProviderResponse {
            response_id: golutra_core::ProviderResponseId::new(),
            message,
            tool_calls,
            usage,
            finish_reason,
            raw_metadata: json!({"round": call}),
        })
    }

    fn contract(&self) -> golutra_core::ProviderContract {
        self.contract.clone()
    }
}

#[async_trait]
impl LlmProvider for SixRoundProvider {
    async fn complete(&self, _request: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        let usage = ProviderUsage {
            input_tokens: Some(32),
            output_tokens: Some(8),
            reasoning_tokens: None,
            cached_input_tokens: None,
            total_tokens: Some(40),
            usage_source: UsageSource::Estimated,
            raw: json!({"round": call}),
        };
        if call < 6 {
            return Ok(ProviderResponse {
                response_id: golutra_core::ProviderResponseId::new(),
                message: None,
                tool_calls: vec![ProviderToolCall {
                    tool_call_id: format!("round-{call}"),
                    tool_name: "read_file".to_owned(),
                    arguments: json!({"path": format!("round-{call}.txt")}),
                }],
                usage,
                finish_reason: ProviderFinishReason::ToolCalls,
                raw_metadata: json!({"round": call}),
            });
        }
        Ok(ProviderResponse {
            response_id: golutra_core::ProviderResponseId::new(),
            message: Some(ProviderMessage {
                role: ProviderRole::Assistant,
                content: "finished six rounds".to_owned(),
                tool_call_id: None,
                tool_name: None,
                tool_calls: Vec::new(),
                metadata: Default::default(),
            }),
            tool_calls: Vec::new(),
            usage,
            finish_reason: ProviderFinishReason::Stop,
            raw_metadata: json!({"round": call}),
        })
    }

    fn contract(&self) -> golutra_core::ProviderContract {
        self.contract.clone()
    }
}

#[async_trait]
impl LlmProvider for FallbackTestProvider {
    async fn complete(&self, request: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        match self {
            Self::Failing(_) => Err(ProviderError::Failed {
                message: "primary failed".to_owned(),
            }),
            Self::Endless(_) => loop {
                tokio::time::sleep(Duration::from_secs(60)).await;
            },
            Self::Success(provider) => provider.complete(request).await,
        }
    }

    fn contract(&self) -> golutra_core::ProviderContract {
        match self {
            Self::Failing(contract) | Self::Endless(contract) => contract.as_ref().clone(),
            Self::Success(provider) => provider.contract(),
        }
    }
}

#[test]
fn prevents_second_active_task_in_same_session() {
    let mut manager = RuntimeLaneManager::new();
    let session_id = SessionId::new();
    let actor = actor("cli");

    manager
        .start_task(
            WorkspaceId::new(),
            session_id,
            TaskId::new(),
            TurnId::new(),
            actor.clone(),
            1,
        )
        .expect("first task starts");
    let result = manager.start_task(
        WorkspaceId::new(),
        session_id,
        TaskId::new(),
        TurnId::new(),
        actor,
        2,
    );

    assert_eq!(
        result.expect_err("second task rejected"),
        RuntimeLaneError::ActiveTaskExists
    );
}

#[tokio::test]
async fn pending_turn_queue_closes_atomically_when_the_loop_becomes_idle() {
    let (handle, control) = agent_execution_channel(2);
    let first = PendingAgentTurn {
        command_id: CommandId::new(),
        turn_id: TurnId::new(),
        content: "first queued turn".to_owned(),
        task_contract: None,
        output_schema: None,
        external_verifiers: Vec::new(),
        max_elapsed_ms: None,
        defer_external_verification: false,
        external_verifiers_require_os_sandbox: false,
        allow_network: false,
        yolo: false,
        steer: false,
    };

    handle
        .append_turn(first.clone())
        .await
        .expect("first turn queues");
    assert_eq!(control.pending_turns.take_or_close().await, Some(first));
    assert_eq!(control.pending_turns.take_or_close().await, None);
    assert!(matches!(
        handle
            .append_turn(PendingAgentTurn {
                command_id: CommandId::new(),
                turn_id: TurnId::new(),
                content: "late turn".to_owned(),
                task_contract: None,
                output_schema: None,
                external_verifiers: Vec::new(),
                max_elapsed_ms: None,
                defer_external_verification: false,
                external_verifiers_require_os_sandbox: false,
                allow_network: false,
                yolo: false,
                steer: false,
            })
            .await,
        Err(AgentLoopError::PendingTurnQueueClosed)
    ));
}

#[tokio::test]
async fn reserved_pending_turn_is_not_visible_until_its_event_is_durable() {
    let (handle, control) = agent_execution_channel(1);
    let turn = PendingAgentTurn {
        command_id: CommandId::new(),
        turn_id: TurnId::new(),
        content: "durable queued turn".to_owned(),
        task_contract: None,
        output_schema: None,
        external_verifiers: Vec::new(),
        max_elapsed_ms: None,
        defer_external_verification: false,
        external_verifiers_require_os_sandbox: false,
        allow_network: false,
        yolo: false,
        steer: false,
    };
    let reservation = handle
        .reserve_turn(turn.clone())
        .await
        .expect("turn reserves capacity");
    let waiting = tokio::spawn(async move { control.pending_turns.take_or_close().await });
    tokio::task::yield_now().await;
    assert!(!waiting.is_finished());

    reservation.commit();

    assert_eq!(waiting.await.expect("waiter"), Some(turn));
}

#[tokio::test]
async fn pending_turn_update_is_atomic_with_its_durable_event() {
    let (handle, control) = agent_execution_channel(1);
    let original = PendingAgentTurn {
        command_id: CommandId::new(),
        turn_id: TurnId::new(),
        content: "original prompt".to_owned(),
        task_contract: None,
        output_schema: None,
        external_verifiers: Vec::new(),
        max_elapsed_ms: None,
        defer_external_verification: false,
        external_verifiers_require_os_sandbox: false,
        allow_network: false,
        yolo: false,
        steer: false,
    };
    handle
        .append_turn(original.clone())
        .await
        .expect("turn queues");

    let mut replacement = original.clone();
    replacement.content = "edited prompt".to_owned();
    let mutation = handle
        .reserve_turn_update(original.turn_id, replacement.clone())
        .expect("turn update reserves");
    let waiting = tokio::spawn(async move { control.pending_turns.take_or_close().await });
    tokio::task::yield_now().await;
    assert!(!waiting.is_finished());

    mutation.commit();
    assert_eq!(waiting.await.expect("waiter"), Some(replacement));
}

#[tokio::test]
async fn dropped_pending_turn_mutations_restore_the_original_queue_entry() {
    let (handle, control) = agent_execution_channel(1);
    let original = PendingAgentTurn {
        command_id: CommandId::new(),
        turn_id: TurnId::new(),
        content: "keep this prompt".to_owned(),
        task_contract: None,
        output_schema: None,
        external_verifiers: Vec::new(),
        max_elapsed_ms: None,
        defer_external_verification: false,
        external_verifiers_require_os_sandbox: false,
        allow_network: false,
        yolo: false,
        steer: false,
    };
    handle
        .append_turn(original.clone())
        .await
        .expect("turn queues");

    let mut replacement = original.clone();
    replacement.content = "discard this edit".to_owned();
    drop(
        handle
            .reserve_turn_update(original.turn_id, replacement)
            .expect("turn update reserves"),
    );
    drop(
        handle
            .reserve_turn_cancellation(original.turn_id)
            .expect("turn cancellation reserves"),
    );

    assert_eq!(control.pending_turns.take_or_close().await, Some(original));
}

#[tokio::test]
async fn committed_pending_turn_cancellation_removes_the_turn() {
    let (handle, control) = agent_execution_channel(1);
    let turn = PendingAgentTurn {
        command_id: CommandId::new(),
        turn_id: TurnId::new(),
        content: "cancel this prompt".to_owned(),
        task_contract: None,
        output_schema: None,
        external_verifiers: Vec::new(),
        max_elapsed_ms: None,
        defer_external_verification: false,
        external_verifiers_require_os_sandbox: false,
        allow_network: false,
        yolo: false,
        steer: false,
    };
    handle.append_turn(turn.clone()).await.expect("turn queues");
    handle
        .reserve_turn_cancellation(turn.turn_id)
        .expect("turn cancellation reserves")
        .commit();

    assert_eq!(control.pending_turns.take_or_close().await, None);
}

#[test]
fn completed_lane_allows_next_task_in_same_session() {
    let mut manager = RuntimeLaneManager::new();
    let session_id = SessionId::new();
    let actor = actor("cli");

    manager
        .start_task(
            WorkspaceId::new(),
            session_id,
            TaskId::new(),
            TurnId::new(),
            actor.clone(),
            1,
        )
        .expect("first task starts");
    manager
        .finish_task(session_id, TaskStatus::Completed, 2)
        .expect("first task finishes");
    let next = manager
        .start_task(
            WorkspaceId::new(),
            session_id,
            TaskId::new(),
            TurnId::new(),
            actor,
            3,
        )
        .expect("next task starts");

    assert_eq!(next.lane.status, TaskStatus::Running);
}

#[test]
fn rejects_non_active_controller_input() {
    let mut manager = RuntimeLaneManager::new();
    let session_id = SessionId::new();
    manager
        .start_task(
            WorkspaceId::new(),
            session_id,
            TaskId::new(),
            TurnId::new(),
            actor("cli"),
            1,
        )
        .expect("task starts");

    let decision = manager
        .decide_busy_policy(
            session_id,
            CommandId::new(),
            &actor("web"),
            BusyPolicy::Append,
        )
        .expect("decision exists");

    assert_eq!(decision.applied_policy, BusyPolicy::Reject);
    assert!(!decision.safe_to_inject);
}

#[test]
fn takeover_transfers_the_active_controller_and_records_both_actors() {
    let mut manager = RuntimeLaneManager::new();
    let session_id = SessionId::new();
    let original = actor("original");
    let replacement = actor("replacement");
    manager
        .start_task(
            WorkspaceId::new(),
            session_id,
            TaskId::new(),
            TurnId::new(),
            original.clone(),
            1,
        )
        .expect("task");

    let transition = manager
        .takeover(session_id, replacement.clone(), 2)
        .expect("takeover");

    assert_eq!(transition.lane.active_controller, replacement);
    assert_eq!(
        transition.event.event_type,
        RuntimeEventType::ControllerChanged
    );
    assert_eq!(
        transition.event.payload["previous_controller"],
        json!(original)
    );
}

#[test]
fn abort_moves_lane_to_aborting() {
    let mut manager = RuntimeLaneManager::new();
    let session_id = SessionId::new();
    manager
        .start_task(
            WorkspaceId::new(),
            session_id,
            TaskId::new(),
            TurnId::new(),
            actor("cli"),
            1,
        )
        .expect("task starts");

    let transition = manager.abort(session_id, 2).expect("abort works");

    assert_eq!(transition.lane.status, TaskStatus::Aborting);
    assert_eq!(
        transition.event.event_type,
        RuntimeEventType::TaskAbortRequested
    );
    assert!(is_active_status(TaskStatus::Aborting));
}

#[test]
fn terminal_lane_rejects_control_transitions() {
    let mut manager = RuntimeLaneManager::new();
    let session_id = SessionId::new();
    manager
        .start_task(
            WorkspaceId::new(),
            session_id,
            TaskId::new(),
            TurnId::new(),
            actor("cli"),
            1,
        )
        .expect("task starts");
    manager
        .finish_task(session_id, TaskStatus::Completed, 2)
        .expect("task finishes");

    assert_eq!(
        manager.abort(session_id, 3),
        Err(RuntimeLaneError::LaneNotFound)
    );
    assert_eq!(
        manager.pause(session_id, 4),
        Err(RuntimeLaneError::LaneNotFound)
    );
    assert_eq!(
        manager.resume(session_id, 5),
        Err(RuntimeLaneError::LaneNotFound)
    );
    assert_eq!(
        manager.lane(session_id).map(|lane| lane.status),
        Some(TaskStatus::Completed)
    );
}

fn actor(id: &str) -> Actor {
    Actor {
        kind: ActorKind::Cli,
        id: id.to_owned(),
    }
}

#[tokio::test]
async fn agent_loop_provider_error_includes_detail() {
    let workspace = tempdir().expect("workspace");
    let provider =
        FallbackTestProvider::Failing(Box::new(MockProvider::text_response("unused").contract()));
    let executor = BasicToolExecutor::new(WorkspacePolicy::new(workspace.path()).expect("policy"));
    let agent_loop = AgentLoop::new(provider, ContextBuilder::default(), executor);

    let error = agent_loop
        .run(AgentTaskRequest {
            session_id: SessionId::new(),
            task_id: TaskId::new(),
            turn_id: TurnId::new(),
            objective: "你好".to_owned(),
            completion_criteria: vec!["assistant response".to_owned()],
            output_schema: None,
            touched_code: false,
            contributors: Vec::new(),
            tools: Vec::new(),
        })
        .await
        .expect_err("provider error");

    assert!(error.to_string().contains("provider call failed"));
    assert!(error.to_string().contains("primary failed"));
}

#[tokio::test]
async fn agent_harness_starts_and_settles_a_turn_through_one_public_seam() {
    let workspace = tempdir().expect("workspace");
    let provider = MockProvider::text_response("harness completed");
    let executor = BasicToolExecutor::new(WorkspacePolicy::new(workspace.path()).expect("policy"));
    let harness = AgentHarness::new(provider, ContextBuilder::default(), executor);
    let run = AgentRun::new(AgentTaskRequest {
        session_id: SessionId::new(),
        task_id: TaskId::new(),
        turn_id: TurnId::new(),
        objective: "reply once".to_owned(),
        completion_criteria: vec!["assistant response".to_owned()],
        output_schema: None,
        touched_code: false,
        contributors: Vec::new(),
        tools: Vec::new(),
    });

    let turn = harness.start(run, |_| {});
    let outcome = turn.wait().await.expect("harness outcome");

    assert_eq!(outcome.final_message.as_deref(), Some("harness completed"));
    assert_eq!(outcome.loop_decision.action, LoopAction::StopSuccess);
}

#[tokio::test]
async fn deferred_external_verification_does_not_correct_missing_runtime_proof() {
    let workspace = tempdir().expect("workspace");
    let calls = Arc::new(AtomicUsize::new(0));
    let provider = AssistantOnlyCorrectionProvider {
        calls: calls.clone(),
        contract: MockProvider::text_response("unused").contract(),
    };
    let executor = BasicToolExecutor::new(WorkspacePolicy::new(workspace.path()).expect("policy"));
    let harness = AgentHarness::new(provider, ContextBuilder::default(), executor)
        .with_deferred_external_verification(true);
    let run = AgentRun::new(AgentTaskRequest {
        session_id: SessionId::new(),
        task_id: TaskId::new(),
        turn_id: TurnId::new(),
        objective: "change result.py and leave final validation to the evaluator".to_owned(),
        completion_criteria: Vec::new(),
        output_schema: None,
        touched_code: true,
        contributors: Vec::new(),
        tools: vec!["write_file".to_owned()],
    });
    let (_handle, control) = agent_execution_channel(1);
    let mut trace = Vec::new();

    let outcome = harness
        .execute(run, control, |event| trace.push(event))
        .await
        .expect("deferred candidate");

    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_ne!(outcome.verification.result, VerificationResult::Pass);
    assert!(outcome.candidate_ready_for_external_verification);
    assert!(
        !trace
            .iter()
            .any(|event| matches!(event, AgentLoopTraceEvent::CorrectionIssued(_)))
    );
}

#[test]
fn runtime_deadline_advisory_appears_once_the_final_budget_window_begins() {
    assert!(runtime_deadline_advisory(600_000, 479_999).is_none());

    let advisory = runtime_deadline_advisory(600_000, 480_000).expect("deadline advisory");
    assert!(advisory.contains("about 120 seconds remain"));
    assert!(advisory.contains("preserve and verify"));
    assert!(advisory.contains("return a final response"));
}

#[tokio::test]
async fn active_provider_session_reaches_verification_at_the_runtime_deadline() {
    let workspace = tempdir().expect("workspace");
    let provider = EndlessProgressProvider {
        contract: MockProvider::text_response("unused").contract(),
    };
    let executor = BasicToolExecutor::new(WorkspacePolicy::new(workspace.path()).expect("policy"));
    let limits = GovernorLimits {
        max_elapsed_ms: 40,
        ..GovernorLimits::default()
    };
    let agent_loop = AgentLoop::new(provider, ContextBuilder::default(), executor)
        .with_governor(RuntimeGovernor::new(limits));
    let mut trace = Vec::new();

    let outcome = tokio::time::timeout(
        Duration::from_secs(1),
        agent_loop.run_with_trace(
            AgentTaskRequest {
                session_id: SessionId::new(),
                task_id: TaskId::new(),
                turn_id: TurnId::new(),
                objective: "return the best bounded result".to_owned(),
                completion_criteria: Vec::new(),
                output_schema: None,
                touched_code: false,
                contributors: Vec::new(),
                tools: Vec::new(),
            },
            |event| trace.push(event),
        ),
    )
    .await
    .expect("runtime deadline must bound an active provider")
    .expect("runtime deadline must produce an outcome");

    assert_eq!(outcome.loop_decision.action, LoopAction::AskUser);
    assert!(trace.iter().any(|event| matches!(
        event,
        AgentLoopTraceEvent::LoopGuardTriggered {
            trigger: golutra_core::LoopGuardTrigger::RuntimeDeadline,
            ..
        }
    )));
    assert!(trace.iter().any(|event| matches!(
        event,
        AgentLoopTraceEvent::VerificationCompleted { terminal: true, .. }
    )));
    assert!(trace.iter().any(|event| matches!(
        event,
        AgentLoopTraceEvent::ProviderFailed { error, .. }
            if error.contains("runtime wall-clock deadline")
    )));
}

#[tokio::test]
async fn fallback_deadline_failure_is_attributed_to_the_active_provider() {
    let workspace = tempdir().expect("workspace");
    let mut primary_contract = MockProvider::text_response("unused").contract();
    primary_contract.provider_id = "primary".to_owned();
    primary_contract.model_id = "primary-model".to_owned();
    let mut fallback_contract = primary_contract.clone();
    fallback_contract.provider_id = "fallback".to_owned();
    fallback_contract.model_id = "fallback-model".to_owned();
    let provider = FallbackTestProvider::Failing(Box::new(primary_contract));
    let fallback = FallbackTestProvider::Endless(Box::new(fallback_contract));
    let executor = BasicToolExecutor::new(WorkspacePolicy::new(workspace.path()).expect("policy"));
    let limits = GovernorLimits {
        max_elapsed_ms: 40,
        ..GovernorLimits::default()
    };
    let agent_loop = AgentLoop::new(provider, ContextBuilder::default(), executor)
        .with_fallback(fallback)
        .with_governor(RuntimeGovernor::new(limits));
    let mut trace = Vec::new();

    agent_loop
        .run_with_trace(
            AgentTaskRequest {
                session_id: SessionId::new(),
                task_id: TaskId::new(),
                turn_id: TurnId::new(),
                objective: "return the best bounded fallback result".to_owned(),
                completion_criteria: Vec::new(),
                output_schema: None,
                touched_code: false,
                contributors: Vec::new(),
                tools: Vec::new(),
            },
            |event| trace.push(event),
        )
        .await
        .expect("deadline outcome");

    assert!(trace.iter().any(|event| matches!(
        event,
        AgentLoopTraceEvent::ProviderFailed {
            provider_id,
            model_id,
            error,
            ..
        } if provider_id == "fallback"
            && model_id == "fallback-model"
            && error.contains("runtime wall-clock deadline")
    )));
}

#[tokio::test]
async fn fallback_completion_and_usage_are_attributed_to_the_actual_provider() {
    let workspace = tempdir().expect("workspace");
    let mut primary_contract = MockProvider::text_response("unused").contract();
    primary_contract.provider_id = "primary".to_owned();
    primary_contract.model_id = "primary-model".to_owned();
    let provider = FallbackTestProvider::Failing(Box::new(primary_contract));
    let fallback = FallbackTestProvider::Success(Box::new(MockProvider::text_response("fallback")));
    let executor = BasicToolExecutor::new(WorkspacePolicy::new(workspace.path()).expect("policy"));
    let agent_loop =
        AgentLoop::new(provider, ContextBuilder::default(), executor).with_fallback(fallback);
    let mut trace = Vec::new();

    let outcome = agent_loop
        .run_with_trace(
            AgentTaskRequest {
                session_id: SessionId::new(),
                task_id: TaskId::new(),
                turn_id: TurnId::new(),
                objective: "hello".to_owned(),
                completion_criteria: vec!["assistant response".to_owned()],
                output_schema: None,
                touched_code: false,
                contributors: Vec::new(),
                tools: Vec::new(),
            },
            |event| trace.push(event),
        )
        .await
        .expect("fallback outcome");

    assert_eq!(outcome.final_message.as_deref(), Some("fallback"));
    assert!(trace.iter().any(|event| matches!(
        event,
        AgentLoopTraceEvent::ProviderStarted { provider_id, model_id, .. }
            if provider_id == "mock" && model_id == "mock-model"
    )));
    assert!(trace.iter().any(|event| matches!(
        event,
        AgentLoopTraceEvent::ProviderCompleted { provider_id, model_id, .. }
            if provider_id == "mock" && model_id == "mock-model"
    )));
    assert!(trace.iter().any(|event| matches!(
        event,
        AgentLoopTraceEvent::ProviderStreamed {
            provider_id,
            model_id,
            event: ProviderStreamEvent::TextDelta { text },
            ..
        } if provider_id == "mock" && model_id == "mock-model" && text == "fallback"
    )));
    assert!(trace.iter().any(|event| matches!(
        event,
        AgentLoopTraceEvent::TokenUsageRecorded(record)
            if record.provider_id == "mock" && record.model_id == "mock-model"
    )));
}

#[tokio::test]
async fn zero_iteration_budget_disables_the_legacy_fixed_round_cap() {
    let workspace = tempdir().expect("workspace");
    let executor = BasicToolExecutor::new(WorkspacePolicy::new(workspace.path()).expect("policy"));
    let governor = RuntimeGovernor::new(GovernorLimits {
        max_iterations: 0,
        ..GovernorLimits::default()
    });
    let agent_loop = AgentLoop::new(
        MockProvider::text_response("completed without fixed cap"),
        ContextBuilder::default(),
        executor,
    )
    .with_governor(governor);
    let mut trace = Vec::new();

    let outcome = agent_loop
        .run_with_trace(
            AgentTaskRequest {
                session_id: SessionId::new(),
                task_id: TaskId::new(),
                turn_id: TurnId::new(),
                objective: "inspect runtime".to_owned(),
                completion_criteria: vec!["runtime inspected".to_owned()],
                output_schema: None,
                touched_code: false,
                contributors: Vec::new(),
                tools: Vec::new(),
            },
            |event| trace.push(event),
        )
        .await
        .expect("governed outcome");

    assert!(outcome.final_message.is_some());
    assert!(trace.iter().any(|event| matches!(
        event,
        AgentLoopTraceEvent::AssistantMessage { content, .. }
            if content == "completed without fixed cap"
    )));
    assert!(!trace.iter().any(|event| matches!(
        event,
        AgentLoopTraceEvent::GovernorDecided(decision)
            if decision.action == GovernorAction::Block
    )));
}

#[tokio::test]
async fn agent_loop_can_complete_more_than_four_provider_tool_rounds() {
    let workspace = tempdir().expect("workspace");
    for round in 0..6 {
        fs::write(workspace.path().join(format!("round-{round}.txt")), "ok").expect("fixture");
    }
    let calls = Arc::new(AtomicUsize::new(0));
    let provider = SixRoundProvider {
        calls: Arc::clone(&calls),
        contract: MockProvider::text_response("contract").contract(),
    };
    let executor = BasicToolExecutor::new(WorkspacePolicy::new(workspace.path()).expect("policy"));
    let agent_loop = AgentLoop::new(provider, ContextBuilder::default(), executor);
    let mut trace = Vec::new();

    let outcome = agent_loop
        .run_with_trace(
            AgentTaskRequest {
                session_id: SessionId::new(),
                task_id: TaskId::new(),
                turn_id: TurnId::new(),
                objective: "read six files and report completion".to_owned(),
                completion_criteria: vec!["all files read".to_owned()],
                output_schema: None,
                touched_code: false,
                contributors: Vec::new(),
                tools: vec!["read_file".to_owned()],
            },
            |event| trace.push(event),
        )
        .await
        .expect("long loop");

    assert_eq!(calls.load(Ordering::SeqCst), 7);
    assert!(outcome.final_message.is_some());
    assert!(trace.iter().any(|event| matches!(
        event,
        AgentLoopTraceEvent::AssistantMessage { content, .. }
            if content == "finished six rounds"
    )));
    assert_eq!(
        trace
            .iter()
            .filter(|event| matches!(event, AgentLoopTraceEvent::StepStarted(_)))
            .count(),
        7
    );
    assert_eq!(
        trace
            .iter()
            .filter(|event| matches!(event, AgentLoopTraceEvent::StepCheckpointed(_)))
            .count(),
        7
    );
}

#[tokio::test]
async fn initial_context_overflow_returns_a_structured_blocked_outcome() {
    let workspace = tempdir().expect("workspace");
    let executor = BasicToolExecutor::new(WorkspacePolicy::new(workspace.path()).expect("policy"));
    let context_builder = ContextBuilder::new(ContextBudgetPolicy {
        context_window: 64,
        max_output: 16,
        budget_limit: 8,
        action_if_exceeded: BudgetOverflowAction::Block,
    });
    let agent_loop = AgentLoop::new(
        MockProvider::text_response("provider must not run"),
        context_builder,
        executor,
    );
    let mut trace = Vec::new();

    let outcome = agent_loop
        .run_with_trace(
            AgentTaskRequest {
                session_id: SessionId::new(),
                task_id: TaskId::new(),
                turn_id: TurnId::new(),
                objective: "inspect runtime".to_owned(),
                completion_criteria: vec!["runtime inspected".to_owned()],
                output_schema: None,
                touched_code: false,
                contributors: vec![ContextContributor {
                    name: "objective".to_owned(),
                    role: ProviderRole::User,
                    content: "large context ".repeat(20),
                    token_budget_hint: 0,
                    source_refs: Vec::new(),
                }],
                tools: Vec::new(),
            },
            |event| trace.push(event),
        )
        .await
        .expect("context guard outcome");

    assert_eq!(outcome.loop_decision.action, LoopAction::Blocked);
    assert_eq!(outcome.verification.result, VerificationResult::Unknown);
    assert!(outcome.loop_decision.budget_state.compact_recommended);
    assert!(trace.iter().any(|event| matches!(
        event,
        AgentLoopTraceEvent::LoopGuardTriggered {
            trigger: golutra_core::LoopGuardTrigger::ContextOverflow,
            ..
        }
    )));
    assert!(
        !trace
            .iter()
            .any(|event| matches!(event, AgentLoopTraceEvent::ProviderStarted { .. }))
    );
}

#[tokio::test]
async fn accumulated_tool_messages_are_compacted_and_the_turn_continues() {
    let workspace = tempdir().expect("workspace");
    fs::write(workspace.path().join("large.txt"), "x".repeat(4_096)).expect("fixture");
    let executor = BasicToolExecutor::new(WorkspacePolicy::new(workspace.path()).expect("policy"));
    let contributor = ContextContributor {
        name: "objective".to_owned(),
        role: ProviderRole::User,
        content: "read large.txt".to_owned(),
        token_budget_hint: 0,
        source_refs: Vec::new(),
    };
    let tool_tokens = executor
        .registry()
        .contract("read_file")
        .and_then(|contract| serde_json::to_string(contract).ok())
        .map(|contract| estimate_tokens(&contract))
        .expect("read_file contract");
    let initial_tokens = estimate_tokens(&contributor.content).saturating_add(tool_tokens);
    let context_builder = ContextBuilder::new(ContextBudgetPolicy {
        context_window: initial_tokens.saturating_add(1_024),
        max_output: 64,
        budget_limit: initial_tokens.saturating_add(256),
        action_if_exceeded: BudgetOverflowAction::Trim,
    });
    let agent_loop = AgentLoop::new(
        MockProvider::tool_call("read_file", json!({"path": "large.txt"})),
        context_builder,
        executor,
    );
    let mut trace = Vec::new();

    let outcome = agent_loop
        .run_with_trace(
            AgentTaskRequest {
                session_id: SessionId::new(),
                task_id: TaskId::new(),
                turn_id: TurnId::new(),
                objective: "read large.txt".to_owned(),
                completion_criteria: Vec::new(),
                output_schema: None,
                touched_code: false,
                contributors: vec![contributor],
                tools: vec!["read_file".to_owned()],
            },
            |event| trace.push(event),
        )
        .await
        .expect("compacted outcome");

    assert_eq!(outcome.tool_reports.len(), 1);
    assert_eq!(outcome.loop_decision.action, LoopAction::StopSuccess);
    assert_eq!(
        trace
            .iter()
            .filter(|event| matches!(event, AgentLoopTraceEvent::ProviderStarted { .. }))
            .count(),
        2
    );
    assert!(
        trace
            .iter()
            .any(|event| matches!(event, AgentLoopTraceEvent::ContextAutoCompacted(_)))
    );
    assert!(!trace.iter().any(|event| matches!(
        event,
        AgentLoopTraceEvent::LoopGuardTriggered {
            trigger: golutra_core::LoopGuardTrigger::ContextOverflow,
            ..
        }
    )));
}

#[tokio::test]
async fn agent_loop_does_not_treat_a_write_as_objective_validation() {
    let workspace = tempdir().expect("workspace");
    let provider = MockProvider::tool_call(
        "write_file",
        json!({"path": "result.txt", "content": "done"}),
    );
    let executor = BasicToolExecutor::new(WorkspacePolicy::new(workspace.path()).expect("policy"));
    let agent_loop = AgentLoop::new(provider, ContextBuilder::default(), executor);
    let task_id = TaskId::new();
    let turn_id = TurnId::new();
    let session_id = SessionId::new();

    let outcome = agent_loop
        .run(AgentTaskRequest {
            session_id,
            task_id,
            turn_id,
            objective: "write result".to_owned(),
            completion_criteria: vec!["file written".to_owned()],
            output_schema: None,
            touched_code: true,
            contributors: Vec::new(),
            tools: vec!["write_file".to_owned()],
        })
        .await
        .expect("loop runs");

    assert_eq!(outcome.loop_decision.action, LoopAction::StopFailed);
    assert!(
        !outcome.verification.checks.iter().any(|check| {
            check.kind == golutra_core::VerificationCheckKind::ObjectiveValidation
        })
    );
    assert_eq!(
        fs::read_to_string(workspace.path().join("result.txt")).unwrap(),
        "done"
    );
}

#[tokio::test]
async fn workspace_change_is_returned_to_the_model_until_fresh_validation_passes() {
    let workspace = tempdir().expect("workspace");
    fs::write(workspace.path().join("source.txt"), "source bytes").expect("source");
    let calls = Arc::new(AtomicUsize::new(0));
    let saw_nudge = Arc::new(AtomicBool::new(false));
    let provider = ValidationGateProvider {
        calls: calls.clone(),
        saw_nudge: saw_nudge.clone(),
        contract: MockProvider::text_response("unused").contract(),
    };
    let executor = BasicToolExecutor::new(WorkspacePolicy::new(workspace.path()).expect("policy"));
    let agent_loop = AgentLoop::new(provider, ContextBuilder::default(), executor);
    let (handle, control) = agent_execution_channel(4);
    let (trace_tx, mut trace_rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(async move {
        agent_loop
            .run_with_control_and_trace(
                AgentTaskRequest {
                    session_id: SessionId::new(),
                    task_id: TaskId::new(),
                    turn_id: TurnId::new(),
                    objective: "recover source.txt into recovered.txt exactly".to_owned(),
                    completion_criteria: Vec::new(),
                    output_schema: None,
                    touched_code: true,
                    contributors: Vec::new(),
                    tools: vec!["write_file".to_owned(), "shell".to_owned()],
                },
                control,
                move |event| {
                    let _ = trace_tx.send(event);
                },
            )
            .await
    });
    let mut trace = Vec::new();
    let approval = loop {
        let event = trace_rx.recv().await.expect("approval trace");
        if let AgentLoopTraceEvent::ApprovalRequested(approval) = &event {
            trace.push(event.clone());
            break approval.clone();
        }
        trace.push(event);
    };
    handle
        .resolve_approval(ApprovalResolution {
            approval_id: approval.approval_id,
            decision: ApprovalDecision::Approved,
            scope: ApprovalScope::Once,
            resource_prefix: None,
            reason: "approved by test".to_owned(),
        })
        .await
        .expect("approval resolves");
    let outcome = task.await.expect("task joins").expect("loop runs");
    while let Ok(event) = trace_rx.try_recv() {
        trace.push(event);
    }

    assert_eq!(calls.load(Ordering::SeqCst), 4);
    assert!(saw_nudge.load(Ordering::SeqCst));
    assert_eq!(outcome.verification.result, VerificationResult::Pass);
    assert_eq!(outcome.loop_decision.action, LoopAction::StopSuccess);
    assert!(outcome.verification.checks.iter().any(|check| {
        check.kind == golutra_core::VerificationCheckKind::ObjectiveValidation
            && check.command.as_deref()
                == Some(
                    "python3 -c \"from pathlib import Path; assert Path('source.txt').read_bytes() == Path('recovered.txt').read_bytes()\""
                )
            && check.passed
    }));
    assert!(trace.iter().any(|event| matches!(
        event,
        AgentLoopTraceEvent::VerificationCompleted {
            terminal: false,
            ..
        }
    )));
    assert!(
        trace
            .iter()
            .any(|event| matches!(event, AgentLoopTraceEvent::CorrectionIssued(_)))
    );
}

#[tokio::test]
async fn code_change_without_an_objective_validation_fails() {
    let workspace = tempdir().expect("workspace");
    let provider = MockProvider::tool_call(
        "write_file",
        json!({"path": "src/lib.rs", "content": "pub fn answer() -> u8 { 42 }"}),
    );
    fs::create_dir_all(workspace.path().join("src")).expect("source directory");
    let executor = BasicToolExecutor::new(WorkspacePolicy::new(workspace.path()).expect("policy"));
    let agent_loop = AgentLoop::new(provider, ContextBuilder::default(), executor);

    let outcome = agent_loop
        .run(AgentTaskRequest {
            session_id: SessionId::new(),
            task_id: TaskId::new(),
            turn_id: TurnId::new(),
            objective: "write Rust code".to_owned(),
            completion_criteria: vec!["tests pass".to_owned()],
            output_schema: None,
            touched_code: true,
            contributors: Vec::new(),
            tools: vec!["write_file".to_owned()],
        })
        .await
        .expect("loop runs");

    assert_eq!(outcome.verification.result, VerificationResult::Fail);
    assert_eq!(outcome.loop_decision.action, LoopAction::StopFailed);
    assert!(outcome.verification.checks.iter().any(|check| {
        check.kind == golutra_core::VerificationCheckKind::WorkspaceChange && check.passed
    }));
    assert!(!outcome.verification.checks.iter().any(|check| {
        check.kind == golutra_core::VerificationCheckKind::ObjectiveValidation && check.passed
    }));
}

#[tokio::test]
async fn caller_declared_verifier_controls_code_change_completion() {
    for (path, expected_result, expected_action) in [
        (
            "src/lib.rs",
            VerificationResult::Pass,
            LoopAction::StopSuccess,
        ),
        (
            "src/missing.rs",
            VerificationResult::Fail,
            LoopAction::StopFailed,
        ),
    ] {
        let workspace = tempdir().expect("workspace");
        fs::create_dir_all(workspace.path().join("src")).expect("source directory");
        let provider = MockProvider::tool_call(
            "write_file",
            json!({"path": "src/lib.rs", "content": "pub fn answer() -> u8 { 42 }"}),
        );
        let executor =
            BasicToolExecutor::new(WorkspacePolicy::new(workspace.path()).expect("policy"));
        let agent_loop = AgentLoop::new(provider, ContextBuilder::default(), executor)
            .with_external_verifiers(vec![ExternalVerificationSpec {
                program: "test".to_owned(),
                args: vec!["-f".to_owned(), path.to_owned()],
                cwd: ".".to_owned(),
                timeout_ms: 5_000,
                expected_exit_code: 0,
                max_output_bytes: 1024,
            }]);
        let mut trace = Vec::new();

        let outcome = agent_loop
            .run_with_trace(
                AgentTaskRequest {
                    session_id: SessionId::new(),
                    task_id: TaskId::new(),
                    turn_id: TurnId::new(),
                    objective: "write Rust code".to_owned(),
                    completion_criteria: vec!["tests pass".to_owned()],
                    output_schema: None,
                    touched_code: true,
                    contributors: Vec::new(),
                    tools: vec!["write_file".to_owned(), "shell".to_owned()],
                },
                |event| trace.push(event),
            )
            .await
            .expect("loop runs");

        assert_eq!(outcome.verification.result, expected_result);
        assert_eq!(outcome.loop_decision.action, expected_action);
        assert!(outcome.verification.checks.iter().any(|check| {
            check.name == "objective:test:external_verifier"
                && check.passed == (expected_result == VerificationResult::Pass)
                && !check.evidence_refs.is_empty()
        }));
        assert!(outcome.tool_reports.iter().any(|report| {
            report.envelope.tool_name == "external_verifier" && !report.artifact_contents.is_empty()
        }));
        assert!(!trace.iter().any(|event| matches!(
            event,
            AgentLoopTraceEvent::RetryScheduled { reason, .. }
                if reason.contains("without fresh objective validation")
        )));
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RecordedVerifierCheckpoint {
    tool_call_id: ToolCallId,
    tool_name: String,
    tracked_before_image: Option<Vec<u8>>,
    tracked_content_at_checkpoint: Option<Vec<u8>>,
    complete: bool,
}

#[derive(Debug)]
struct RecordingVerifierCheckpointRecorder {
    tracked_path: PathBuf,
    records: Mutex<Vec<RecordedVerifierCheckpoint>>,
}

#[async_trait]
impl BeforeSideEffectRecorder for RecordingVerifierCheckpointRecorder {
    async fn persist_before_side_effect(
        &self,
        request: &golutra_tools::ToolRequest,
        before_images: &[golutra_tools::FileBeforeImage],
        complete: bool,
    ) -> Result<(), AgentLoopError> {
        let tracked_before_image = before_images
            .iter()
            .find(|image| image.path == self.tracked_path)
            .and_then(|image| image.content.clone());
        let record = RecordedVerifierCheckpoint {
            tool_call_id: request.tool_call_id,
            tool_name: request.tool_name.clone(),
            tracked_before_image,
            tracked_content_at_checkpoint: fs::read(&self.tracked_path).ok(),
            complete,
        };
        self.records
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(record);
        Ok(())
    }
}

#[derive(Debug)]
struct FailingCheckpointRecorder;

#[async_trait]
impl BeforeSideEffectRecorder for FailingCheckpointRecorder {
    async fn persist_before_side_effect(
        &self,
        _request: &golutra_tools::ToolRequest,
        _before_images: &[golutra_tools::FileBeforeImage],
        _complete: bool,
    ) -> Result<(), AgentLoopError> {
        Err(AgentLoopError::Checkpoint(
            "forced checkpoint persistence failure".to_owned(),
        ))
    }
}

#[derive(Debug)]
struct RecordingDelegationBackend {
    called: Arc<AtomicBool>,
}

#[async_trait]
impl golutra_tools::TaskDelegationBackend for RecordingDelegationBackend {
    async fn delegate(
        &self,
        _request: &golutra_tools::ToolRequest,
        _cancellation: CancellationToken,
    ) -> Result<golutra_tools::TaskDelegationOutput, golutra_tools::ToolError> {
        self.called.store(true, Ordering::SeqCst);
        Ok(golutra_tools::TaskDelegationOutput {
            status: ToolResultStatus::Ok,
            summary: "delegated task completed".to_owned(),
            content: "child result".to_owned(),
            structured_facts: json!({"child_status": "completed"}),
        })
    }
}

#[tokio::test]
async fn delegation_requires_a_checkpoint_even_when_the_workspace_is_empty() {
    let workspace = tempdir().expect("workspace");
    let called = Arc::new(AtomicBool::new(false));
    let executor = BasicToolExecutor::new(WorkspacePolicy::new(workspace.path()).expect("policy"))
        .with_task_delegation_backend(Arc::new(RecordingDelegationBackend {
            called: called.clone(),
        }))
        .expect("delegation backend");
    let provider = MockProvider::tool_call(
        "delegate_task",
        json!({"task": "return a concise independent result"}),
    );
    let mut agent_loop = AgentLoop::new(provider, ContextBuilder::default(), executor);
    agent_loop.before_side_effect_recorder = Some(Arc::new(FailingCheckpointRecorder));
    let mut trace = Vec::new();

    let outcome = agent_loop
        .run_with_trace(
            AgentTaskRequest {
                session_id: SessionId::new(),
                task_id: TaskId::new(),
                turn_id: TurnId::new(),
                objective: "delegate an independent task".to_owned(),
                completion_criteria: Vec::new(),
                output_schema: None,
                touched_code: false,
                contributors: Vec::new(),
                tools: vec!["delegate_task".to_owned()],
            },
            |event| trace.push(event),
        )
        .await
        .expect("loop completes");

    assert!(!called.load(Ordering::SeqCst));
    let report = outcome
        .tool_reports
        .iter()
        .find(|report| report.envelope.tool_name == "delegate_task")
        .expect("delegation report");
    assert_eq!(report.envelope.status, ToolResultStatus::Error);
    assert!(report.artifact_contents.iter().any(|content| {
        String::from_utf8_lossy(&content.bytes).contains("checkpoint persistence failure")
    }));
    assert!(trace.iter().any(|event| matches!(
        event,
        AgentLoopTraceEvent::ToolCompleted(report)
            if report.envelope.tool_name == "delegate_task"
    )));
}

#[tokio::test]
async fn checkpoint_failure_emits_a_balanced_failed_tool_observation() {
    let workspace = tempdir().expect("workspace");
    let provider = MockProvider::tool_call("shell", json!({"command": "touch should-not-run.txt"}));
    let executor = BasicToolExecutor::new(WorkspacePolicy::new(workspace.path()).expect("policy"))
        .with_sandbox(SystemSandbox::process_only());
    let mut agent_loop = AgentLoop::new(provider, ContextBuilder::default(), executor);
    agent_loop.before_side_effect_recorder = Some(Arc::new(FailingCheckpointRecorder));
    let (handle, control) = agent_execution_channel(2);
    let (trace_tx, mut trace_rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(async move {
        agent_loop
            .run_with_control_and_trace(
                AgentTaskRequest {
                    session_id: SessionId::new(),
                    task_id: TaskId::new(),
                    turn_id: TurnId::new(),
                    objective: "create a file".to_owned(),
                    completion_criteria: vec!["command completes".to_owned()],
                    output_schema: None,
                    touched_code: false,
                    contributors: Vec::new(),
                    tools: vec!["shell".to_owned()],
                },
                control,
                move |event| {
                    let _ = trace_tx.send(event);
                },
            )
            .await
    });
    let mut trace = Vec::new();
    let approval = loop {
        let event = trace_rx.recv().await.expect("approval trace");
        if let AgentLoopTraceEvent::ApprovalRequested(approval) = &event {
            trace.push(event.clone());
            break approval.clone();
        }
        trace.push(event);
    };
    handle
        .resolve_approval(ApprovalResolution {
            approval_id: approval.approval_id,
            decision: ApprovalDecision::Approved,
            scope: ApprovalScope::Once,
            resource_prefix: None,
            reason: "approved by test".to_owned(),
        })
        .await
        .expect("approval resolves");
    let outcome = task.await.expect("task joins").expect("loop completes");
    trace.extend(std::iter::from_fn(|| trace_rx.try_recv().ok()));

    assert!(!workspace.path().join("should-not-run.txt").exists());
    let report = outcome
        .tool_reports
        .iter()
        .find(|report| report.envelope.tool_name == "shell")
        .expect("shell report");
    assert_eq!(report.envelope.status, ToolResultStatus::Error);
    assert!(report.artifact_contents.iter().any(|content| {
        String::from_utf8_lossy(&content.bytes).contains("checkpoint persistence failure")
    }));
    let started = trace.iter().filter_map(|event| match event {
        AgentLoopTraceEvent::ToolStarted { tool_call_id, .. } => Some(*tool_call_id),
        _ => None,
    });
    let completed = trace.iter().filter_map(|event| match event {
        AgentLoopTraceEvent::ToolCompleted(report) => Some(report.envelope.tool_call_id),
        _ => None,
    });
    assert_eq!(started.collect::<Vec<_>>(), completed.collect::<Vec<_>>());
}

#[tokio::test]
async fn caller_declared_verifier_is_checkpointed_and_workspace_mutation_fails() {
    let workspace = tempdir().expect("workspace");
    let tracked_path = workspace.path().join("tracked.txt");
    fs::write(&tracked_path, "before").expect("tracked fixture");
    let tracked_path = fs::canonicalize(tracked_path).expect("canonical tracked path");
    let recorder = Arc::new(RecordingVerifierCheckpointRecorder {
        tracked_path: tracked_path.clone(),
        records: Mutex::new(Vec::new()),
    });
    let executor =
        BasicToolExecutor::new(WorkspacePolicy::new(workspace.path()).expect("workspace policy"))
            .with_sandbox(SystemSandbox::process_only());
    let mut agent_loop = AgentLoop::new(
        MockProvider::text_response("verification was attempted"),
        ContextBuilder::default(),
        executor,
    )
    .with_external_verifiers(vec![ExternalVerificationSpec {
        program: "sh".to_owned(),
        args: vec!["-c".to_owned(), "printf after > tracked.txt".to_owned()],
        cwd: ".".to_owned(),
        timeout_ms: 5_000,
        expected_exit_code: 0,
        max_output_bytes: 1_024,
    }]);
    agent_loop.before_side_effect_recorder = Some(recorder.clone());
    let mut trace = Vec::new();

    let outcome = agent_loop
        .run_with_trace(
            AgentTaskRequest {
                session_id: SessionId::new(),
                task_id: TaskId::new(),
                turn_id: TurnId::new(),
                objective: "validate the tracked output".to_owned(),
                completion_criteria: vec!["the configured verifier passes".to_owned()],
                output_schema: None,
                touched_code: false,
                contributors: Vec::new(),
                tools: Vec::new(),
            },
            |event| trace.push(event),
        )
        .await
        .expect("loop runs");

    let report = outcome
        .tool_reports
        .iter()
        .find(|report| report.envelope.tool_name == "external_verifier")
        .expect("verifier report");
    let records = recorder
        .records
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].tool_name, "external_verifier");
    assert_eq!(records[0].tool_call_id, report.envelope.tool_call_id);
    assert_eq!(
        records[0].tracked_before_image.as_deref(),
        Some(b"before".as_slice())
    );
    assert_eq!(
        records[0].tracked_content_at_checkpoint.as_deref(),
        Some(b"before".as_slice())
    );
    assert!(records[0].complete);
    assert_eq!(fs::read(&tracked_path).expect("mutated file"), b"after");
    assert_eq!(report.changed_files, vec![tracked_path]);
    assert_eq!(report.envelope.status, ToolResultStatus::Error);
    assert_eq!(
        report.envelope.structured_facts["workspace_mutation_detected"],
        true
    );
    assert_ne!(outcome.verification.result, VerificationResult::Pass);
    let started_ids = trace
        .iter()
        .filter_map(|event| match event {
            AgentLoopTraceEvent::ToolStarted {
                tool_call_id,
                tool_name,
                ..
            } if tool_name == "external_verifier" => Some(*tool_call_id),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(started_ids, vec![report.envelope.tool_call_id]);
}

#[tokio::test]
async fn verifier_launch_failure_is_a_balanced_failed_tool_observation() {
    let workspace = tempdir().expect("workspace");
    let executor = BasicToolExecutor::new(WorkspacePolicy::new(workspace.path()).expect("policy"))
        .with_sandbox(SystemSandbox::process_only());
    let agent_loop = AgentLoop::new(
        MockProvider::text_response("verification was attempted"),
        ContextBuilder::default(),
        executor,
    )
    .with_external_verifiers(vec![ExternalVerificationSpec {
        program: "golutra-verifier-that-does-not-exist".to_owned(),
        args: Vec::new(),
        cwd: ".".to_owned(),
        timeout_ms: 1_000,
        expected_exit_code: 0,
        max_output_bytes: 1_024,
    }]);
    let mut trace = Vec::new();

    let outcome = agent_loop
        .run_with_trace(
            AgentTaskRequest {
                session_id: SessionId::new(),
                task_id: TaskId::new(),
                turn_id: TurnId::new(),
                objective: "run the configured verifier".to_owned(),
                completion_criteria: vec!["the verifier passes".to_owned()],
                output_schema: None,
                touched_code: false,
                contributors: Vec::new(),
                tools: Vec::new(),
            },
            |event| trace.push(event),
        )
        .await
        .expect("launch failure becomes a governed outcome");

    assert_eq!(outcome.verification.result, VerificationResult::Fail);
    assert_eq!(outcome.loop_decision.action, LoopAction::StopFailed);
    let started = trace
        .iter()
        .filter_map(|event| match event {
            AgentLoopTraceEvent::ToolStarted {
                tool_call_id,
                tool_name,
                ..
            } if tool_name == "external_verifier" => Some(*tool_call_id),
            _ => None,
        })
        .collect::<Vec<_>>();
    let completed = trace
        .iter()
        .filter_map(|event| match event {
            AgentLoopTraceEvent::ToolCompleted(report)
                if report.envelope.tool_name == "external_verifier" =>
            {
                assert_eq!(report.envelope.status, ToolResultStatus::Error);
                assert!(report.envelope.structured_facts.get("error").is_some());
                Some(report.envelope.tool_call_id)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(!started.is_empty());
    assert_eq!(completed, started);
}

#[tokio::test]
async fn auto_discovered_verifier_does_not_run_without_os_sandboxing() {
    let workspace = tempdir().expect("workspace");
    let marker = workspace.path().join("verifier-ran.txt");
    let executor = BasicToolExecutor::new(WorkspacePolicy::new(workspace.path()).expect("policy"))
        .with_sandbox(SystemSandbox::process_only());
    let agent_loop = AgentLoop::new(
        MockProvider::text_response("verification was attempted"),
        ContextBuilder::default(),
        executor,
    )
    .with_external_verifiers(vec![ExternalVerificationSpec {
        program: "sh".to_owned(),
        args: vec!["-c".to_owned(), "printf ran > verifier-ran.txt".to_owned()],
        cwd: ".".to_owned(),
        timeout_ms: 5_000,
        expected_exit_code: 0,
        max_output_bytes: 1_024,
    }])
    .require_os_sandbox_for_external_verifiers(true);

    let outcome = agent_loop
        .run(AgentTaskRequest {
            session_id: SessionId::new(),
            task_id: TaskId::new(),
            turn_id: TurnId::new(),
            objective: "run repository tests".to_owned(),
            completion_criteria: vec!["tests pass".to_owned()],
            output_schema: None,
            touched_code: false,
            contributors: Vec::new(),
            tools: Vec::new(),
        })
        .await
        .expect("isolation failure becomes an outcome");

    assert!(!marker.exists());
    let report = outcome
        .tool_reports
        .iter()
        .find(|report| report.envelope.tool_name == "external_verifier")
        .expect("verifier report");
    assert_eq!(report.envelope.status, ToolResultStatus::Error);
    assert_eq!(
        report.envelope.structured_facts["sandbox_os_enforced"],
        false
    );
    assert!(
        report
            .artifact_contents
            .iter()
            .any(|content| String::from_utf8_lossy(&content.bytes).contains("OS-enforced sandbox"))
    );
}

#[tokio::test]
async fn verifier_mutation_cannot_satisfy_a_required_delivery() {
    let workspace = tempdir().expect("workspace");
    let executor = BasicToolExecutor::new(WorkspacePolicy::new(workspace.path()).expect("policy"))
        .with_sandbox(SystemSandbox::process_only());
    let agent_loop = AgentLoop::new(
        MockProvider::text_response("the verifier produced the file"),
        ContextBuilder::default(),
        executor,
    )
    .with_external_verifiers(vec![ExternalVerificationSpec {
        program: "sh".to_owned(),
        args: vec!["-c".to_owned(), "printf forged > result.txt".to_owned()],
        cwd: ".".to_owned(),
        timeout_ms: 5_000,
        expected_exit_code: 0,
        max_output_bytes: 1_024,
    }]);
    let (_handle, control) = agent_execution_channel(1);

    let outcome = agent_loop
        .run_with_task_contract_and_observation_sink(
            AgentTaskRequest {
                session_id: SessionId::new(),
                task_id: TaskId::new(),
                turn_id: TurnId::new(),
                objective: "create result.txt".to_owned(),
                completion_criteria: vec!["result.txt is delivered".to_owned()],
                output_schema: None,
                touched_code: true,
                contributors: Vec::new(),
                tools: Vec::new(),
            },
            TaskContract {
                workspace_change: WorkspaceChangeRequirement::Required,
                required_paths: vec!["result.txt".to_owned()],
                require_objective_validation: true,
                verification: golutra_core::VerificationRequirement::Required,
                max_correction_rounds: 0,
                ..TaskContract::default()
            },
            control,
            |_| {},
        )
        .await
        .expect("verifier mutation becomes a failed outcome");

    assert!(workspace.path().join("result.txt").exists());
    assert_eq!(outcome.verification.result, VerificationResult::Fail);
    assert!(outcome.verification.checks.iter().any(|check| {
        check.name == "objective:path:delivery"
            && !check.passed
            && check.message.contains("was not changed")
    }));
    assert!(
        !outcome
            .verification
            .checks
            .iter()
            .any(|check| { check.kind == VerificationCheckKind::WorkspaceChange && check.passed })
    );
}

#[tokio::test]
async fn caller_declared_verifier_can_validate_an_unchanged_existing_delivery() {
    let workspace = tempdir().expect("workspace");
    fs::write(workspace.path().join("results.txt"), "done\n").expect("existing result");
    let provider = MockProvider::text_response("The existing result is valid.");
    let executor = BasicToolExecutor::new(WorkspacePolicy::new(workspace.path()).expect("policy"));
    let outcome = AgentLoop::new(provider, ContextBuilder::default(), executor)
        .with_external_verifiers(vec![ExternalVerificationSpec {
            program: "test".to_owned(),
            args: vec!["-f".to_owned(), "results.txt".to_owned()],
            cwd: ".".to_owned(),
            timeout_ms: 5_000,
            expected_exit_code: 0,
            max_output_bytes: 1024,
        }])
        .run(AgentTaskRequest {
            session_id: SessionId::new(),
            task_id: TaskId::new(),
            turn_id: TurnId::new(),
            objective: "verify the existing results.txt without changing it".to_owned(),
            completion_criteria: vec!["results.txt contains the expected result".to_owned()],
            output_schema: None,
            touched_code: false,
            contributors: Vec::new(),
            tools: Vec::new(),
        })
        .await
        .expect("loop runs");

    assert_eq!(outcome.verification.result, VerificationResult::Pass);
    assert_eq!(outcome.loop_decision.action, LoopAction::StopSuccess);
    assert!(
        !outcome
            .verification
            .checks
            .iter()
            .any(|check| check.name == "objective:path:delivery")
    );
}

#[test]
fn verification_command_classifier_rejects_arbitrary_shell_success() {
    assert!(is_objective_validation_command(
        "cargo test -p golutra-runtime"
    ));
    assert!(is_objective_validation_command("npm run typecheck"));
    assert!(is_objective_validation_command("python -m pytest -q"));
    assert!(is_objective_validation_command(
        "/usr/bin/python3 -m unittest"
    ));
    assert!(is_objective_validation_command(
        "curl -fsS http://127.0.0.1:5000/status"
    ));
    assert!(!is_objective_validation_command(
        "curl http://127.0.0.1:5000/status"
    ));
    assert_eq!(
        objective_validation_command_kind(
            "python3 -c \"from pathlib import Path; actual = Path('result.txt').read_text(); assert actual == 'expected'\""
        ),
        Some(ObjectiveValidationKind::Diagnostic)
    );
    assert_eq!(
        objective_validation_command_kind(
            r#"bash -lc 'python3 - <<"PY"
import json
from pathlib import Path
actual = json.loads(Path("result.json").read_text())
assert actual["status"] == "ready"
PY'"#
        ),
        Some(ObjectiveValidationKind::Diagnostic)
    );
    let direct_heredoc = "python - <<'PY'\nfrom pathlib import Path\nassert Path('result.txt').read_text() == 'expected'\nPY";
    assert_eq!(
        objective_validation_command_kind(direct_heredoc),
        Some(ObjectiveValidationKind::Diagnostic)
    );
    assert_ne!(
        objective_validation_command_identity(direct_heredoc),
        objective_validation_command_identity(
            "python - <<'PY'\nfrom pathlib import Path\nassert Path('result.txt').read_text() == 'different'\nPY"
        )
    );
    let fail_fast_heredoc = r#"bash -lc 'set -e
python3 - <<"PY"
from pathlib import Path
assert Path("result.txt").read_text() == "expected"
PY'"#;
    assert_eq!(
        objective_validation_command_kind(fail_fast_heredoc),
        Some(ObjectiveValidationKind::Diagnostic)
    );
    assert!(!is_objective_validation_command(
        r#"bash -lc 'python3 - <<"PY"
assert True
PY'"#
    ));
    assert!(!is_objective_validation_command(
        r#"bash -lc 'touch changed.txt
python3 - <<"PY"
from pathlib import Path
assert Path("changed.txt").exists()
PY'"#
    ));
    assert!(!is_objective_validation_command(
        "python3 -c \"print('passed')\""
    ));
    assert!(!is_objective_validation_command(
        "python3 -c \"assert True\""
    ));
    assert!(!is_objective_validation_command(
        "python3 -c \"assert (True)\""
    ));
    assert!(!is_objective_validation_command("python3 -c \"assert 1\""));
    assert!(!is_objective_validation_command(
        "python3 -c \"assert 1 == 1\""
    ));
    assert!(!is_objective_validation_command(
        "python3 -c \"assert float('300') >= 300\""
    ));
    assert!(!is_objective_validation_command(
        "python3 -c \"def validate():\\n    assert actual == expected\""
    ));
    assert!(!is_objective_validation_command(
        "python3 -c \"if True: raise RuntimeError('constant')\""
    ));
    assert!(!is_objective_validation_command(
        "python3 -c \"if False: raise RuntimeError('constant')\""
    ));
    assert!(!is_objective_validation_command(
        "python3 -c \"if failed: raise SystemExit(0)\""
    ));
    assert!(is_objective_validation_command(
        "python3 -c \"from pathlib import Path\nactual = Path('result.txt').read_text()\nif actual != 'expected': raise RuntimeError('mismatch')\""
    ));
    assert!(is_objective_validation_command(
        "python3 -c \"import sys\nfrom pathlib import Path\nactual = Path('result.txt').read_text()\nif actual != 'expected':\n    sys.exit(1)\""
    ));
    assert_eq!(
        objective_validation_command_kind("python3 -c \"assert False\""),
        Some(ObjectiveValidationKind::Diagnostic)
    );
    assert!(!is_objective_validation_command(
        "python3 -c \"print('assert actual == expected')\""
    ));
    assert!(!is_objective_validation_command(
        "python3 -O -c \"assert actual == expected\""
    ));
    assert!(is_objective_validation_command(
        "bash -lc 'cargo check && python -m pytest -q'"
    ));
    let chained_heredoc_validation = r#"bash -lc 'grep -q expected result.txt && python3 - <<"PY"
from pathlib import Path
assert Path("result.txt").read_text() == "expected"
PY'"#;
    assert_eq!(
        objective_validation_command_kind(chained_heredoc_validation),
        Some(ObjectiveValidationKind::Diagnostic)
    );
    assert!(!is_objective_validation_command(
        r#"bash -lc 'grep -q expected result.txt || python3 - <<"PY"
from pathlib import Path
assert Path("result.txt").read_text() == "expected"
PY'"#
    ));
    let git_validation = "bash -lc 'set -euo pipefail
branch=$(git branch --show-current)
test \"$branch\" = master
git diff --quiet
git diff --cached --quiet
git merge-base --is-ancestor recovered-move-to-stanford master
git diff --exit-code recovered-move-to-stanford -- _includes/about.md _layouts/default.html
printf \"validation passed\\n\"'";
    assert_eq!(
        objective_validation_command_kind(git_validation),
        Some(ObjectiveValidationKind::Diagnostic)
    );
    let final_git_validation = "bash -lc 'set -euo pipefail
merge_parent=$(git rev-parse HEAD^2)
recovered=$(git rev-parse 268903d)
[ \"$merge_parent\" = \"$recovered\" ]
git diff --quiet 268903d -- _includes/about.md _layouts/default.html
git diff --quiet HEAD --
test -z \"$(git status --porcelain)\"'";
    assert_eq!(
        objective_validation_command_kind(final_git_validation),
        Some(ObjectiveValidationKind::Diagnostic)
    );
    assert_eq!(
        objective_validation_command_kind(
            "bash -lc 'git diff --quiet 268903d HEAD && test -z \"$(git status --porcelain)\"'"
        ),
        Some(ObjectiveValidationKind::Diagnostic)
    );
    let tmux_validation = r##"bash -lc 'set -euo pipefail
[ "$(tmux list-panes -t workflow:0 | wc -l)" -eq 3 ]
tmux list-panes -t workflow:0 -F "#{pane_index}:#{pane_current_command}" | grep -q "^0:python$"
tmux capture-pane -t workflow:0.0 -p | grep -q "Monitoring"
python - <<"PY"
from pathlib import Path
assert Path("/app/project/src/process_data.py").is_file()
PY'"##;
    assert!(shell_command_is_read_only(
        &shlex::split(
            r##"tmux list-panes -t workflow:0 -F "#{pane_index}:#{pane_current_command}""##
        )
        .expect("tmux command")
    ));
    assert_eq!(
        objective_validation_command_kind(
            r##"bash -lc 'set -e
tmux list-panes -t workflow:0 -F "#{pane_index}:#{pane_current_command}" | grep -q "^0:python$"'"##
        ),
        Some(ObjectiveValidationKind::Diagnostic)
    );
    assert_eq!(
        objective_validation_command_kind(
            "bash -lc 'set -e\ntmux capture-pane -t workflow:0.0 -p | grep -q Monitoring'"
        ),
        Some(ObjectiveValidationKind::Diagnostic)
    );
    assert_eq!(
        objective_validation_command_kind(tmux_validation),
        Some(ObjectiveValidationKind::Diagnostic)
    );
    let failed_git_validation = "bash -lc 'git status --short --branch && git diff --exit-code 268903d..HEAD -- _layouts/default.html _includes/about.md && git log --oneline -3'";
    let repaired_git_validation = "bash -lc 'git checkout 268903d -- _includes/about.md _layouts/default.html && git add _includes/about.md _layouts/default.html && git commit --amend --no-edit && git diff --exit-code 268903d..HEAD -- _layouts/default.html _includes/about.md'";
    assert_eq!(
        objective_validation_command_identity(failed_git_validation),
        objective_validation_command_identity(repaired_git_validation)
    );
    assert!(!is_objective_validation_command(
        "bash -lc 'git diff --quiet 268903d HEAD || true'"
    ));
    assert!(!is_objective_validation_command(
        "bash -lc 'git merge-base --is-ancestor source HEAD\nprintf done'"
    ));
    assert!(!is_objective_validation_command(
        "bash -lc 'set -e\ngit merge-base --is-ancestor source HEAD || true'"
    ));
    assert!(!is_objective_validation_command(
        "bash -lc 'set -e\ngit merge-base --is-ancestor source HEAD | cat'"
    ));
    assert!(!is_objective_validation_command(
        "bash -lc 'set -e\nset +e\ngit merge-base --is-ancestor source HEAD'"
    ));
    assert!(!is_objective_validation_command(
        "bash -lc 'set -e\nprintf \"validation passed\\n\"'"
    ));
    assert!(!is_objective_validation_command(
        "bash -lc 'set -e\n[ \"$actual\" = \"$expected\" ]'"
    ));
    assert!(!is_objective_validation_command(
        "bash -lc 'pytest -q | tee results.txt'"
    ));
    assert!(is_objective_validation_command("test \"$size\" -lt 100000"));
    assert!(!is_objective_validation_command("test 1 -lt 100000"));
    assert!(is_objective_validation_command(
        "bash -lc 'strings artifact.bin | grep -Fq expected'"
    ));
    assert!(is_objective_validation_command(
        "bash -lc 'set -e\npython3 -c \"from pathlib import Path; actual = Path(\\\"result.txt\\\").read_text(); assert actual == \\\"expected\\\"\"\nprintf \"validated\\n\"'"
    ));
    assert!(!is_objective_validation_command(
        "bash -lc 'set -e\npython3 -c \"assert actual == expected\"\ntouch validation-marker'"
    ));
    assert!(!is_objective_validation_command(
        "bash -lc 'python3 -c \"assert actual == expected\"\nprintf done'"
    ));
    let fail_fast_setup_pipeline = r#"bash -lc 'set -e
python test.py | tee /tmp/test-output.txt
avg=$(grep Average /tmp/test-output.txt | head -1)
test "$avg" -ge 300
size=$(du -sb trained_model | cut -f1)
test "$size" -lt 100000
printf "validated\n"'"#;
    assert_eq!(
        objective_validation_command_kind(fail_fast_setup_pipeline),
        Some(ObjectiveValidationKind::Diagnostic)
    );
    assert!(objective_validation_command_identity(fail_fast_setup_pipeline).is_some());
    let fail_fast_nested_build_chain = r#"bash -lc 'set -e
make clean && make
./public-cli input.json > result.txt
python - <<"PY"
from pathlib import Path
actual = Path("result.txt").read_text()
assert actual == "expected\n"
PY'"#;
    assert_eq!(
        objective_validation_command_kind(fail_fast_nested_build_chain),
        Some(ObjectiveValidationKind::Diagnostic)
    );
    assert!(objective_validation_command_identity(fail_fast_nested_build_chain).is_some());
    let mutating_heredoc_then_validation = r#"bash -lc "set -e
python - <<'PY'
from pathlib import Path
Path('prepared.txt').write_text('ready')
PY
python - <<'PY'
from pathlib import Path
assert Path('prepared.txt').read_text() == 'ready'
PY""#;
    assert_eq!(
        objective_validation_command_kind(mutating_heredoc_then_validation),
        Some(ObjectiveValidationKind::Diagnostic)
    );
    assert!(objective_validation_command_identity(mutating_heredoc_then_validation).is_some());
    let terminal_heredoc_validation = r#"bash -lc 'cat > recovered.txt <<"EOF"
recovered
EOF
python3 - <<"PY"
from pathlib import Path
assert Path("recovered.txt").read_text() == "recovered\n"
PY'"#;
    assert_eq!(
        objective_validation_command_kind(terminal_heredoc_validation),
        Some(ObjectiveValidationKind::Diagnostic)
    );
    assert!(objective_validation_command_identity(terminal_heredoc_validation).is_some());
    assert!(!is_objective_validation_command("python verify.py"));
    assert!(!is_objective_validation_command("cargo fmt"));
    assert!(is_objective_validation_command("cargo fmt -- --check"));
    assert_eq!(
        objective_validation_command_kind("test -f result.txt"),
        Some(ObjectiveValidationKind::FileState)
    );
    assert!(!is_objective_validation_command("test expected = expected"));
    assert!(!is_objective_validation_command("echo done"));
    assert!(!is_objective_validation_command("echo tests passed"));
    assert!(!is_objective_validation_command("git status --short"));
    assert!(!is_objective_validation_command("git log --oneline -2"));
    assert!(!is_objective_validation_command("git diff --exit-code"));
    assert!(!is_objective_validation_command("git diff --quiet HEAD --"));
    assert!(is_objective_validation_command(
        "git diff --quiet 268903d -- _includes/about.md _layouts/default.html"
    ));
    assert!(is_objective_validation_command(
        "git diff --exit-code source HEAD -- src/lib.rs"
    ));
    assert!(is_objective_validation_command(
        "git merge-base --is-ancestor source HEAD"
    ));
    assert!(is_objective_validation_command(
        "cmp expected.txt actual.txt"
    ));
    assert!(is_objective_validation_command(
        "diff -q expected.txt actual.txt"
    ));
    assert!(!is_objective_validation_command("go version"));
}

#[test]
fn validation_command_classifier_uses_subcommands_goals_and_targets() {
    for command in [
        "cargo +nightly --locked test --workspace",
        "npm --workspace app run test:unit",
        "make custom-test",
        "mvn integration-test",
        "gradle :app:integrationTest",
        "swift --package-path project test",
    ] {
        assert_eq!(
            objective_validation_command_kind(command),
            Some(ObjectiveValidationKind::Test),
            "{command}"
        );
    }

    for command in [
        "cargo --config net.retry=2 check",
        "npm --prefix app run build",
        "pnpm run typecheck",
        "make -f test build",
        "mvn -f test verify",
        "gradle -p test build",
        "swift --package-path project build",
    ] {
        assert_eq!(
            objective_validation_command_kind(command),
            Some(ObjectiveValidationKind::Diagnostic),
            "{command}"
        );
    }

    for command in [
        "cargo run -- test",
        "cargo metadata --filter-platform test",
        "npm run contest",
        "pnpm exec app test",
        "make latest contest",
        "mvn -DskipTests package",
        "gradle latest contest",
        "swift run tool test",
    ] {
        assert!(!is_objective_validation_command(command), "{command}");
    }
}

#[test]
fn code_file_classifier_includes_scripts_and_build_files() {
    assert!(is_code_file(Path::new("process_data.sh")));
    assert!(is_code_file(Path::new("Makefile")));
    assert!(is_code_file(Path::new("schema.sql")));
    assert!(!is_code_file(Path::new("result.txt")));
}

#[test]
fn test_output_classifier_requires_an_executed_test() {
    assert!(line_reports_executed_tests(
        "test result: ok. 3 passed; 0 failed; 0 ignored"
    ));
    assert!(line_reports_executed_tests("running 1 test"));
    assert!(!line_reports_executed_tests(
        "test result: ok. 0 passed; 0 failed; 0 ignored"
    ));
    assert!(!line_reports_executed_tests("running 0 tests"));
    assert!(line_reports_executed_tests("2 passed in 0.10s"));
    assert!(!line_reports_executed_tests("7 successful uploads"));
    assert!(!line_reports_executed_tests("7 checks completed"));
    assert!(!line_reports_executed_tests("2 passed uploads"));
}

#[test]
fn objective_test_evidence_supports_common_runner_formats() {
    for (command, output) in [
        (
            "cargo test",
            "test result: ok. 3 passed; 0 failed; 0 ignored",
        ),
        ("python -m pytest", "2 passed in 0.10s"),
        ("npm test", "Tests: 4 passed, 4 total"),
        ("pnpm test", "Tests  3 passed (3)"),
        ("go test ./...", "ok  example.test/pkg  0.01s"),
        (
            "mvn test",
            "Tests run: 5, Failures: 0, Errors: 0, Skipped: 0",
        ),
        ("swift test", "Executed 6 tests, with 0 failures"),
        ("make custom-test", "7 tests completed"),
        (
            "cargo test --workspace",
            "running 2 tests\ntest result: ok. 2 passed; 0 failed\nrunning 0 tests",
        ),
    ] {
        let outcome =
            objective_validation_report(&objective_test_report_with_output(command, output))
                .expect("recognized objective test command");
        assert!(outcome.passed, "{command}: {output}");
    }
}

#[test]
fn objective_test_evidence_accepts_explicit_structured_execution_facts() {
    let facts = json!({
        "golutra_test_execution": {
            "schema_version": 1,
            "status": "passed",
            "executed": 2,
            "passed": 2,
            "failed": 0,
            "skipped": 0
        }
    });
    let mut report = objective_test_report("shell", Some("make test"));
    report.envelope.structured_facts["golutra_test_execution"] =
        facts["golutra_test_execution"].clone();
    let outcome = objective_validation_report(&report).expect("objective test outcome");
    assert!(outcome.passed, "{:?}", report.envelope.structured_facts);
}

#[test]
fn objective_test_evidence_rejects_untrusted_structured_shapes() {
    for facts in [
        json!({"test_results": {"executed": 2}}),
        json!({"tests_run": 3}),
        json!({"test_execution_observed": true}),
        json!({"tests": [{"name": "planned"}]}),
    ] {
        let mut report = objective_test_report_with_output("make test", "");
        report.envelope.structured_facts["test_evidence"] = facts;
        let outcome = objective_validation_report(&report).expect("objective test outcome");
        assert!(!outcome.passed, "untrusted facts must not prove execution");
    }
}

#[test]
fn objective_test_evidence_rejects_trusted_facts_with_contradictory_output() {
    let mut report = objective_test_report_with_output("make test", "no tests found");
    report.envelope.structured_facts["golutra_test_execution"] = json!({
        "schema_version": 1,
        "status": "passed",
        "executed": 2,
        "passed": 2,
        "failed": 0,
        "skipped": 0
    });
    let outcome = objective_validation_report(&report).expect("objective test outcome");
    assert!(!outcome.passed);
}

#[test]
fn objective_test_evidence_rejects_zero_test_and_unsuccessful_runs() {
    for output in [
        "running 0 tests",
        "Tests: 0 total",
        "? example.test/pkg [no test files]",
        "testing: warning: no tests to run\nPASS\nok example.test/pkg 0.01s",
        "Executed 0 checks",
        "7 successful uploads",
        "7 checks completed",
        "2 passed uploads",
        "loaded 4 test fixtures",
        "planning 3 test scenarios",
        "Tests: 4 skipped, 4 total",
        "2 passed in 0.10s\nno tests found",
    ] {
        let outcome = objective_validation_report(&objective_test_report_with_output(
            "go test ./...",
            output,
        ))
        .expect("objective test outcome");
        assert!(!outcome.passed, "{output}");
    }

    let mut split_output = objective_test_report_with_output(
        "go test ./...",
        "ok example.test/pkg 0.01s\n? example.test/empty [no test files]",
    );
    split_output
        .artifact_contents
        .push(golutra_tools::ArtifactContent {
            artifact_id: golutra_core::ArtifactId::new(),
            bytes: b"? example.test/other [no test files]".to_vec(),
        });
    assert!(
        objective_validation_report(&split_output)
            .expect("split objective test outcome")
            .passed,
        "a package with no tests must not invalidate a package that ran tests"
    );

    let failed_package = objective_test_report_with_output(
        "go test ./...",
        "ok example.test/pkg 0.01s\nFAIL example.test/broken",
    );
    assert!(
        !objective_validation_report(&failed_package)
            .expect("failed package objective test outcome")
            .passed,
        "a failed package must invalidate an otherwise successful package result"
    );

    let mut failed = objective_test_report_with_output("cargo test", "running 3 tests");
    failed.envelope.status = ToolResultStatus::Error;
    failed.envelope.structured_facts["exit_code"] = json!(1);
    assert!(
        !objective_validation_report(&failed)
            .expect("failed objective test outcome")
            .passed
    );

    let mut timed_out = objective_test_report_with_output("cargo test", "running 3 tests");
    timed_out.envelope.status = ToolResultStatus::Timeout;
    timed_out.envelope.structured_facts["timed_out"] = json!(true);
    assert!(
        !objective_validation_report(&timed_out)
            .expect("timed-out objective test outcome")
            .passed
    );
}

#[tokio::test]
async fn agent_loop_does_not_accept_a_write_to_the_wrong_requested_path() {
    let workspace = tempdir().expect("workspace");
    let provider = MockProvider::tool_call(
        "write_file",
        json!({"path": "wrong.txt", "content": "expected"}),
    );
    let executor = BasicToolExecutor::new(WorkspacePolicy::new(workspace.path()).expect("policy"));
    let agent_loop = AgentLoop::new(provider, ContextBuilder::default(), executor);

    let outcome = agent_loop
        .run(AgentTaskRequest {
            session_id: SessionId::new(),
            task_id: TaskId::new(),
            turn_id: TurnId::new(),
            objective: "write expected.txt with content expected".to_owned(),
            completion_criteria: vec!["expected.txt contains expected".to_owned()],
            output_schema: None,
            touched_code: false,
            contributors: Vec::new(),
            tools: vec!["write_file".to_owned()],
        })
        .await
        .expect("loop runs");

    assert_ne!(outcome.loop_decision.action, LoopAction::StopSuccess);
    assert!(
        outcome
            .verification
            .checks
            .iter()
            .any(|check| { check.name == "objective:path:delivery" && !check.passed })
    );
}

#[tokio::test]
async fn supporting_read_paths_do_not_fail_a_correct_delivery_path() {
    let workspace = tempdir().expect("workspace");
    fs::write(workspace.path().join("input.txt"), "source").expect("input");
    let provider = SupportThenDeliveryProvider {
        calls: Arc::new(AtomicUsize::new(0)),
        contract: MockProvider::text_response("unused").contract(),
    };
    let executor = BasicToolExecutor::new(WorkspacePolicy::new(workspace.path()).expect("policy"));
    let outcome = AgentLoop::new(provider, ContextBuilder::default(), executor)
        .run(AgentTaskRequest {
            session_id: SessionId::new(),
            task_id: TaskId::new(),
            turn_id: TurnId::new(),
            objective: "read input.txt and write results.txt; diagnostic: /tmp/very/long/verify.py"
                .to_owned(),
            completion_criteria: vec!["results.txt is delivered".to_owned()],
            output_schema: None,
            touched_code: true,
            contributors: Vec::new(),
            tools: vec!["read_file".to_owned(), "write_file".to_owned()],
        })
        .await
        .expect("loop runs");

    assert!(
        outcome
            .verification
            .checks
            .iter()
            .any(|check| { check.name == "objective:path:delivery" && check.passed })
    );
    assert!(workspace.path().join("helper.py").is_file());
    assert!(workspace.path().join("results.txt").is_file());
}

#[tokio::test]
async fn agent_loop_does_not_accept_wrong_written_content() {
    let workspace = tempdir().expect("workspace");
    let provider = MockProvider::tool_call(
        "write_file",
        json!({"path": "expected.txt", "content": "wrong"}),
    );
    let executor = BasicToolExecutor::new(WorkspacePolicy::new(workspace.path()).expect("policy"));
    let agent_loop = AgentLoop::new(provider, ContextBuilder::default(), executor);

    let outcome = agent_loop
        .run(AgentTaskRequest {
            session_id: SessionId::new(),
            task_id: TaskId::new(),
            turn_id: TurnId::new(),
            objective: "write expected.txt with content expected".to_owned(),
            completion_criteria: vec!["expected.txt contains expected".to_owned()],
            output_schema: None,
            touched_code: false,
            contributors: Vec::new(),
            tools: vec!["write_file".to_owned()],
        })
        .await
        .expect("loop runs");

    assert_ne!(outcome.loop_decision.action, LoopAction::StopSuccess);
    assert!(
        outcome
            .verification
            .checks
            .iter()
            .any(|check| { check.name == "objective:content:write_file" && !check.passed })
    );
}

#[tokio::test]
async fn agent_loop_returns_invalid_tool_calls_to_the_provider_as_tool_results() {
    let workspace = tempdir().expect("workspace");
    let provider = MockProvider::tool_call("missing_tool", json!({"bad": true}));
    let executor = BasicToolExecutor::new(WorkspacePolicy::new(workspace.path()).expect("policy"));
    let agent_loop = AgentLoop::new(provider, ContextBuilder::default(), executor);

    let outcome = agent_loop
        .run(AgentTaskRequest {
            session_id: SessionId::new(),
            task_id: TaskId::new(),
            turn_id: TurnId::new(),
            objective: "try a tool".to_owned(),
            completion_criteria: vec!["tool result returned".to_owned()],
            output_schema: None,
            touched_code: false,
            contributors: Vec::new(),
            tools: Vec::new(),
        })
        .await
        .expect("invalid tool call becomes a report");

    assert_eq!(outcome.tool_reports.len(), 1);
    assert_eq!(
        outcome.tool_reports[0].envelope.status,
        ToolResultStatus::Error
    );
    assert_eq!(
        outcome.tool_reports[0].envelope.summary,
        "tool request is invalid"
    );
}

#[tokio::test]
async fn provider_receives_only_the_model_visible_tool_result_projection() {
    let workspace = tempdir().expect("workspace");
    fs::write(workspace.path().join("input.txt"), "ok").expect("input");
    let calls = Arc::new(AtomicUsize::new(0));
    let saw_operational_facts = Arc::new(AtomicBool::new(false));
    let saw_governance_metadata = Arc::new(AtomicBool::new(false));
    let provider = ToolResultProjectionProvider {
        calls: calls.clone(),
        saw_operational_facts: saw_operational_facts.clone(),
        saw_governance_metadata: saw_governance_metadata.clone(),
        contract: MockProvider::text_response("unused").contract(),
    };
    let executor = BasicToolExecutor::new(WorkspacePolicy::new(workspace.path()).expect("policy"));
    let outcome = AgentLoop::new(provider, ContextBuilder::default(), executor)
        .run(AgentTaskRequest {
            session_id: SessionId::new(),
            task_id: TaskId::new(),
            turn_id: TurnId::new(),
            objective: "read input.txt".to_owned(),
            completion_criteria: Vec::new(),
            output_schema: None,
            touched_code: false,
            contributors: Vec::new(),
            tools: vec!["read_file".to_owned()],
        })
        .await
        .expect("loop runs");

    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert!(saw_operational_facts.load(Ordering::SeqCst));
    assert!(!saw_governance_metadata.load(Ordering::SeqCst));
    assert_eq!(outcome.loop_decision.action, LoopAction::StopSuccess);
}

#[tokio::test]
async fn progress_advisory_is_projected_back_into_model_context() {
    let workspace = tempdir().expect("workspace");
    fs::write(workspace.path().join("input.txt"), "ok").expect("input");
    let calls = Arc::new(AtomicUsize::new(0));
    let saw_advisory = Arc::new(AtomicBool::new(false));
    let provider = ProgressAdvisoryProvider {
        calls: calls.clone(),
        saw_advisory: saw_advisory.clone(),
        contract: MockProvider::text_response("unused").contract(),
    };
    let executor = BasicToolExecutor::new(WorkspacePolicy::new(workspace.path()).expect("policy"));

    let outcome = AgentLoop::new(provider, ContextBuilder::default(), executor)
        .run(AgentTaskRequest {
            session_id: SessionId::new(),
            task_id: TaskId::new(),
            turn_id: TurnId::new(),
            objective: "inspect input.txt".to_owned(),
            completion_criteria: Vec::new(),
            output_schema: None,
            touched_code: false,
            contributors: Vec::new(),
            tools: vec!["read_file".to_owned()],
        })
        .await
        .expect("loop runs");

    assert_eq!(calls.load(Ordering::SeqCst), 4);
    assert!(saw_advisory.load(Ordering::SeqCst));
    assert_eq!(outcome.loop_decision.action, LoopAction::StopSuccess);
}

#[tokio::test]
async fn correction_without_material_progress_is_advised_checkpointed_and_stopped() {
    let workspace = tempdir().expect("workspace");
    for probe in 0..4 {
        fs::write(
            workspace.path().join(format!("probe-{probe}.txt")),
            format!("probe {probe}\n"),
        )
        .expect("probe fixture");
    }
    let calls = Arc::new(AtomicUsize::new(0));
    let saw_advisory = Arc::new(AtomicBool::new(false));
    let provider = CorrectionStallProvider {
        calls: calls.clone(),
        saw_advisory: saw_advisory.clone(),
        contract: MockProvider::text_response("unused").contract(),
    };
    let governor = RuntimeGovernor::new(GovernorLimits {
        max_correction_no_progress_steps: 4,
        max_correction_no_progress_ms: 0,
        ..GovernorLimits::default()
    });
    let executor = BasicToolExecutor::new(WorkspacePolicy::new(workspace.path()).expect("policy"));
    let mut trace = Vec::new();

    let outcome = AgentLoop::new(provider, ContextBuilder::default(), executor)
        .with_governor(governor)
        .run_with_trace(
            AgentTaskRequest {
                session_id: SessionId::new(),
                task_id: TaskId::new(),
                turn_id: TurnId::new(),
                objective: "change result.py and verify its behavior".to_owned(),
                completion_criteria: vec!["tests pass".to_owned()],
                output_schema: None,
                touched_code: true,
                contributors: Vec::new(),
                tools: vec!["write_file".to_owned(), "read_file".to_owned()],
            },
            |event| trace.push(event),
        )
        .await
        .expect("bounded correction outcome");

    assert_eq!(calls.load(Ordering::SeqCst), 6);
    assert!(saw_advisory.load(Ordering::SeqCst));
    assert_eq!(outcome.loop_decision.action, LoopAction::StopFailed);
    assert!(
        trace
            .iter()
            .any(|event| matches!(event, AgentLoopTraceEvent::CorrectionIssued(_)))
    );
    assert!(trace.iter().any(|event| matches!(
        event,
        AgentLoopTraceEvent::LoopGuardTriggered {
            trigger: golutra_core::LoopGuardTrigger::NoProgress,
            reason,
        } if reason.contains("verification correction")
    )));
    assert!(trace.iter().any(|event| matches!(
        event,
        AgentLoopTraceEvent::StepCompleted(completion)
            if completion.should_stop
                && completion.correction_no_progress_steps == 4
                && completion
                    .stop_reason
                    .as_deref()
                    .is_some_and(|reason| reason.contains("verification correction"))
    )));
    assert!(trace.iter().any(|event| matches!(
        event,
        AgentLoopTraceEvent::StepCheckpointed(checkpoint)
            if checkpoint.correction_active
                && checkpoint.correction_no_progress_steps == 4
                && checkpoint.correction_no_progress_step_limit == 4
    )));
}

#[tokio::test]
async fn assistant_only_corrections_do_not_reset_the_material_progress_budget() {
    let workspace = tempdir().expect("workspace");
    let calls = Arc::new(AtomicUsize::new(0));
    let provider = AssistantOnlyCorrectionProvider {
        calls: calls.clone(),
        contract: MockProvider::text_response("unused").contract(),
    };
    let governor = RuntimeGovernor::new(GovernorLimits {
        max_correction_no_progress_steps: 2,
        max_correction_no_progress_ms: 0,
        ..GovernorLimits::default()
    });
    let executor = BasicToolExecutor::new(WorkspacePolicy::new(workspace.path()).expect("policy"));
    let agent_loop =
        AgentLoop::new(provider, ContextBuilder::default(), executor).with_governor(governor);
    let (_handle, control) = agent_execution_channel(1);
    let mut trace = Vec::new();

    let outcome = agent_loop
        .run_with_task_contract_and_observation_sink(
            AgentTaskRequest {
                session_id: SessionId::new(),
                task_id: TaskId::new(),
                turn_id: TurnId::new(),
                objective: "change result.py and prove it works".to_owned(),
                completion_criteria: vec!["tests pass".to_owned()],
                output_schema: None,
                touched_code: true,
                contributors: Vec::new(),
                tools: vec!["write_file".to_owned()],
            },
            TaskContract {
                workspace_change: golutra_core::WorkspaceChangeRequirement::Required,
                required_paths: vec!["result.py".to_owned()],
                require_objective_validation: true,
                verification: golutra_core::VerificationRequirement::Required,
                max_correction_rounds: 6,
                ..TaskContract::default()
            },
            control,
            |event| trace.push(event),
        )
        .await
        .expect("bounded correction outcome");

    assert_eq!(calls.load(Ordering::SeqCst), 4);
    assert_ne!(outcome.loop_decision.action, LoopAction::StopSuccess);
    assert!(trace.iter().any(|event| matches!(
        event,
        AgentLoopTraceEvent::LoopGuardTriggered {
            trigger: golutra_core::LoopGuardTrigger::NoProgress,
            reason,
        } if reason.contains("verification correction")
    )));
    assert!(trace.iter().any(|event| matches!(
        event,
        AgentLoopTraceEvent::StepCompleted(completion)
            if completion.should_stop
                && completion.made_progress
                && !completion.made_material_progress
                && completion.correction_no_progress_steps == 2
    )));
}

#[tokio::test]
async fn agent_loop_blocks_without_evidence() {
    let workspace = tempdir().expect("workspace");
    let provider = MockProvider::text_response("done");
    let executor = BasicToolExecutor::new(WorkspacePolicy::new(workspace.path()).expect("policy"));
    let agent_loop = AgentLoop::new(provider, ContextBuilder::default(), executor);

    let outcome = agent_loop
        .run(AgentTaskRequest {
            session_id: SessionId::new(),
            task_id: TaskId::new(),
            turn_id: TurnId::new(),
            objective: "claim done".to_owned(),
            completion_criteria: vec!["objective evidence".to_owned()],
            output_schema: None,
            touched_code: true,
            contributors: Vec::new(),
            tools: Vec::new(),
        })
        .await
        .expect("loop runs");

    assert_eq!(outcome.loop_decision.action, LoopAction::StopFailed);
    let final_message = outcome.final_message.expect("failure message");
    assert!(final_message.contains("Verification Fail"));
    assert!(final_message.contains("objective evidence"));
    assert!(final_message.contains("Verification record:"));
}

#[tokio::test]
async fn agent_loop_accepts_plain_conversation_response_without_tool_evidence() {
    let workspace = tempdir().expect("workspace");
    let provider = MockProvider::text_response("你好，我在。");
    let executor = BasicToolExecutor::new(WorkspacePolicy::new(workspace.path()).expect("policy"));
    let agent_loop = AgentLoop::new(provider, ContextBuilder::default(), executor);

    let outcome = agent_loop
        .run(AgentTaskRequest {
            session_id: SessionId::new(),
            task_id: TaskId::new(),
            turn_id: TurnId::new(),
            objective: "你好".to_owned(),
            completion_criteria: vec!["assistant response".to_owned()],
            output_schema: None,
            touched_code: false,
            contributors: Vec::new(),
            tools: Vec::new(),
        })
        .await
        .expect("loop runs");

    assert_eq!(outcome.loop_decision.action, LoopAction::StopSuccess);
    assert_eq!(outcome.verification.result, VerificationResult::Pass);
    assert_eq!(outcome.final_message, Some("你好，我在。".to_owned()));
}

#[tokio::test]
async fn explicit_task_contract_blocks_deferred_candidate_without_required_delivery() {
    let workspace = tempdir().expect("workspace");
    let executor = BasicToolExecutor::new(WorkspacePolicy::new(workspace.path()).expect("policy"));
    let mut agent_loop = AgentLoop::new(
        MockProvider::text_response("implemented everything"),
        ContextBuilder::default(),
        executor,
    );
    agent_loop.defer_external_verification = true;
    let (_handle, control) = agent_execution_channel(1);
    let mut trace = Vec::new();

    let outcome = agent_loop
        .run_with_task_contract_and_observation_sink(
            AgentTaskRequest {
                session_id: SessionId::new(),
                task_id: TaskId::new(),
                turn_id: TurnId::new(),
                objective: "do it".to_owned(),
                completion_criteria: Vec::new(),
                output_schema: None,
                touched_code: false,
                contributors: Vec::new(),
                tools: Vec::new(),
            },
            TaskContract {
                workspace_change: golutra_core::WorkspaceChangeRequirement::Required,
                required_paths: vec!["src/result.rs".to_owned()],
                verification: golutra_core::VerificationRequirement::Required,
                max_correction_rounds: 0,
                ..TaskContract::default()
            },
            control,
            |event| trace.push(event),
        )
        .await
        .expect("runtime returns governed outcome");

    assert_eq!(outcome.verification.result, VerificationResult::Fail);
    assert_ne!(outcome.loop_decision.action, LoopAction::StopSuccess);
    assert!(!outcome.candidate_ready_for_external_verification);
    assert!(
        outcome
            .verification
            .residual_risks
            .iter()
            .any(|risk| risk.contains("requires a workspace change"))
    );
    assert!(trace.iter().any(|event| matches!(
        event,
        AgentLoopTraceEvent::VerificationCompleted { terminal: true, .. }
    )));
}

#[tokio::test]
async fn required_content_contract_records_evidence_for_an_unchanged_existing_file() {
    let workspace = tempdir().expect("workspace");
    fs::write(workspace.path().join("result.txt"), "already correct\n").expect("existing result");
    let executor = BasicToolExecutor::new(WorkspacePolicy::new(workspace.path()).expect("policy"));
    let agent_loop = AgentLoop::new(
        MockProvider::text_response("The required file content is present."),
        ContextBuilder::default(),
        executor,
    );
    let (_handle, control) = agent_execution_channel(1);
    let mut trace = Vec::new();

    let outcome = agent_loop
        .run_with_task_contract_and_observation_sink(
            AgentTaskRequest {
                session_id: SessionId::new(),
                task_id: TaskId::new(),
                turn_id: TurnId::new(),
                objective: "verify result.txt without changing it".to_owned(),
                completion_criteria: Vec::new(),
                output_schema: None,
                touched_code: false,
                contributors: Vec::new(),
                tools: Vec::new(),
            },
            TaskContract {
                required_file_contents: vec![RequiredFileContent {
                    path: "result.txt".to_owned(),
                    content: "already correct\n".to_owned(),
                }],
                verification: golutra_core::VerificationRequirement::Required,
                max_correction_rounds: 0,
                ..TaskContract::default()
            },
            control,
            |event| trace.push(event),
        )
        .await
        .expect("runtime returns governed outcome");

    assert_eq!(outcome.verification.result, VerificationResult::Pass);
    assert_eq!(outcome.loop_decision.action, LoopAction::StopSuccess);
    assert!(outcome.verification.checks.iter().any(|check| {
        check.name == "objective:content:write_file"
            && check.passed
            && !check.evidence_refs.is_empty()
    }));
    assert!(outcome.tool_reports.iter().any(|report| {
        report.envelope.tool_name == "contract_file_content_verifier"
            && !report.artifact_contents.is_empty()
            && !report.evidence.is_empty()
    }));
    assert!(trace.iter().any(|event| matches!(
        event,
        AgentLoopTraceEvent::ToolCompleted(report)
            if report.envelope.tool_name == "contract_file_content_verifier"
    )));
}

#[tokio::test]
async fn required_path_contract_records_evidence_for_an_unchanged_existing_file() {
    let workspace = tempdir().expect("workspace");
    fs::write(workspace.path().join("result.txt"), "already present\n").expect("existing result");
    let executor = BasicToolExecutor::new(WorkspacePolicy::new(workspace.path()).expect("policy"));
    let agent_loop = AgentLoop::new(
        MockProvider::text_response("The required path is present."),
        ContextBuilder::default(),
        executor,
    );
    let (_handle, control) = agent_execution_channel(1);
    let mut trace = Vec::new();

    let outcome = agent_loop
        .run_with_task_contract_and_observation_sink(
            AgentTaskRequest {
                session_id: SessionId::new(),
                task_id: TaskId::new(),
                turn_id: TurnId::new(),
                objective: "verify result.txt without changing it".to_owned(),
                completion_criteria: Vec::new(),
                output_schema: None,
                touched_code: false,
                contributors: Vec::new(),
                tools: Vec::new(),
            },
            TaskContract {
                required_paths: vec!["result.txt".to_owned()],
                verification: golutra_core::VerificationRequirement::Required,
                max_correction_rounds: 0,
                ..TaskContract::default()
            },
            control,
            |event| trace.push(event),
        )
        .await
        .expect("runtime returns governed outcome");

    assert_eq!(outcome.verification.result, VerificationResult::Pass);
    assert_eq!(outcome.loop_decision.action, LoopAction::StopSuccess);
    assert!(outcome.verification.checks.iter().any(|check| {
        check.name == "objective:path:delivery" && check.passed && !check.evidence_refs.is_empty()
    }));
    assert!(outcome.tool_reports.iter().any(|report| {
        report.envelope.tool_name == "contract_path_verifier"
            && !report.artifact_contents.is_empty()
            && !report.evidence.is_empty()
    }));
    assert!(trace.iter().any(|event| matches!(
        event,
        AgentLoopTraceEvent::ToolCompleted(report)
            if report.envelope.tool_name == "contract_path_verifier"
    )));
}

#[tokio::test]
async fn output_schema_is_verified_by_the_runtime_before_success() {
    let workspace = tempdir().expect("workspace");
    let executor = BasicToolExecutor::new(WorkspacePolicy::new(workspace.path()).expect("policy"));
    let agent_loop = AgentLoop::new(
        MockProvider::text_response(r#"{"answer":"ok"}"#),
        ContextBuilder::default(),
        executor,
    );

    let outcome = agent_loop
        .run(AgentTaskRequest {
            session_id: SessionId::new(),
            task_id: TaskId::new(),
            turn_id: TurnId::new(),
            objective: "return a structured answer".to_owned(),
            completion_criteria: vec!["assistant response".to_owned()],
            output_schema: Some(json!({
                "type": "object",
                "required": ["answer"],
                "properties": {"answer": {"type": "string"}},
                "additionalProperties": false
            })),
            touched_code: false,
            contributors: Vec::new(),
            tools: Vec::new(),
        })
        .await
        .expect("schema-valid response");

    assert_eq!(outcome.verification.result, VerificationResult::Pass);
    assert!(
        outcome
            .verification
            .checks
            .iter()
            .any(|check| { check.kind == VerificationCheckKind::Schema && check.passed })
    );
}

#[tokio::test]
async fn output_schema_failure_is_a_runtime_turn_failure() {
    let workspace = tempdir().expect("workspace");
    let executor = BasicToolExecutor::new(WorkspacePolicy::new(workspace.path()).expect("policy"));
    let agent_loop = AgentLoop::new(
        MockProvider::text_response(r#"{"answer":42}"#),
        ContextBuilder::default(),
        executor,
    );

    let outcome = agent_loop
        .run(AgentTaskRequest {
            session_id: SessionId::new(),
            task_id: TaskId::new(),
            turn_id: TurnId::new(),
            objective: "return a structured answer".to_owned(),
            completion_criteria: vec!["assistant response".to_owned()],
            output_schema: Some(json!({
                "type": "object",
                "required": ["answer"],
                "properties": {"answer": {"type": "string"}},
                "additionalProperties": false
            })),
            touched_code: false,
            contributors: Vec::new(),
            tools: Vec::new(),
        })
        .await
        .expect("schema failure is represented in the outcome");

    assert_ne!(outcome.verification.result, VerificationResult::Pass);
    assert!(
        outcome
            .verification
            .checks
            .iter()
            .any(|check| { check.kind == VerificationCheckKind::Schema && !check.passed })
    );
}

#[tokio::test]
async fn queued_plain_turn_does_not_inherit_workspace_or_verifier_requirements() {
    let workspace = tempdir().expect("workspace");
    let executor = BasicToolExecutor::new(WorkspacePolicy::new(workspace.path()).expect("policy"));
    let agent_loop = AgentLoop::new(
        MockProvider::text_response("plain response"),
        ContextBuilder::default(),
        executor,
    )
    .with_external_verifiers(vec![ExternalVerificationSpec {
        program: "rustc".to_owned(),
        args: vec!["--version".to_owned()],
        cwd: ".".to_owned(),
        timeout_ms: 5_000,
        expected_exit_code: 0,
        max_output_bytes: 4_096,
    }]);
    let (handle, control) = agent_execution_channel(2);
    let queued_turn_id = TurnId::new();
    handle
        .append_turn(PendingAgentTurn {
            command_id: CommandId::new(),
            turn_id: queued_turn_id,
            content: "hello".to_owned(),
            task_contract: None,
            output_schema: None,
            external_verifiers: Vec::new(),
            max_elapsed_ms: None,
            defer_external_verification: false,
            external_verifiers_require_os_sandbox: false,
            allow_network: false,
            yolo: false,
            steer: false,
        })
        .await
        .expect("queued turn");

    let outcome = agent_loop
        .run_with_control_and_trace(
            AgentTaskRequest {
                session_id: SessionId::new(),
                task_id: TaskId::new(),
                turn_id: TurnId::new(),
                objective: "write a file".to_owned(),
                completion_criteria: vec!["assistant response".to_owned()],
                output_schema: None,
                touched_code: true,
                contributors: Vec::new(),
                tools: Vec::new(),
            },
            control,
            |_| {},
        )
        .await
        .expect("queued turn outcome");

    assert_eq!(outcome.final_turn_id, queued_turn_id);
    assert_eq!(outcome.verification.result, VerificationResult::Pass);
    assert_eq!(outcome.final_message.as_deref(), Some("plain response"));
    assert!(outcome.tool_reports.is_empty());
}

#[tokio::test]
async fn queued_turn_uses_its_own_schema_and_completion_criteria() {
    let workspace = tempdir().expect("workspace");
    let executor = BasicToolExecutor::new(WorkspacePolicy::new(workspace.path()).expect("policy"));
    let agent_loop = AgentLoop::new(
        MockProvider::text_response("plain response"),
        ContextBuilder::default(),
        executor,
    );
    let (handle, control) = agent_execution_channel(2);
    let queued_turn_id = TurnId::new();
    handle
        .append_turn(PendingAgentTurn {
            command_id: CommandId::new(),
            turn_id: queued_turn_id,
            content: "return structured output".to_owned(),
            task_contract: Some(TaskContract::conversational(Vec::new())),
            output_schema: Some(json!({
                "type": "object",
                "required": ["answer"]
            })),
            external_verifiers: Vec::new(),
            max_elapsed_ms: None,
            defer_external_verification: false,
            external_verifiers_require_os_sandbox: false,
            allow_network: false,
            yolo: false,
            steer: false,
        })
        .await
        .expect("queued turn");

    let outcome = agent_loop
        .run_with_control_and_trace(
            AgentTaskRequest {
                session_id: SessionId::new(),
                task_id: TaskId::new(),
                turn_id: TurnId::new(),
                objective: "initial prompt".to_owned(),
                completion_criteria: vec!["initial criterion".to_owned()],
                output_schema: None,
                touched_code: false,
                contributors: Vec::new(),
                tools: Vec::new(),
            },
            control,
            |_| {},
        )
        .await
        .expect("queued turn outcome");

    assert_eq!(outcome.final_turn_id, queued_turn_id);
    assert!(outcome.verification.completion_criteria.is_empty());
    assert!(
        outcome
            .verification
            .checks
            .iter()
            .any(|check| { check.kind == VerificationCheckKind::Schema && !check.passed })
    );
    assert_ne!(outcome.verification.result, VerificationResult::Pass);
}

#[tokio::test]
async fn queued_turn_uses_its_own_elapsed_budget() {
    let workspace = tempdir().expect("workspace");
    let calls = Arc::new(AtomicUsize::new(0));
    let provider = SequencedTextProvider {
        calls: calls.clone(),
        delay: Duration::ZERO,
        block_from_call: Some(1),
        contract: MockProvider::text_response("unused").contract(),
    };
    let executor = BasicToolExecutor::new(WorkspacePolicy::new(workspace.path()).expect("policy"));
    let harness =
        AgentHarness::new(provider, ContextBuilder::default(), executor).with_max_elapsed_ms(1_000);
    let (handle, control) = agent_execution_channel(2);
    let queued_turn_id = TurnId::new();
    handle
        .append_turn(PendingAgentTurn {
            command_id: CommandId::new(),
            turn_id: queued_turn_id,
            content: "finish within the queued budget".to_owned(),
            task_contract: Some(TaskContract::conversational(Vec::new())),
            output_schema: None,
            external_verifiers: Vec::new(),
            max_elapsed_ms: Some(40),
            defer_external_verification: true,
            external_verifiers_require_os_sandbox: false,
            allow_network: false,
            yolo: false,
            steer: false,
        })
        .await
        .expect("queued turn");
    let mut trace = Vec::new();

    let outcome = tokio::time::timeout(
        Duration::from_millis(500),
        harness.execute(
            AgentRun::new(AgentTaskRequest {
                session_id: SessionId::new(),
                task_id: TaskId::new(),
                turn_id: TurnId::new(),
                objective: "complete the initial turn".to_owned(),
                completion_criteria: Vec::new(),
                output_schema: None,
                touched_code: false,
                contributors: Vec::new(),
                tools: Vec::new(),
            }),
            control,
            |event| trace.push(event),
        ),
    )
    .await
    .expect("queued budget must bound the active provider")
    .expect("deadline outcome");

    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(outcome.final_turn_id, queued_turn_id);
    assert!(trace.iter().any(|event| matches!(
        event,
        AgentLoopTraceEvent::PendingTurnStarted(turn)
            if turn.max_elapsed_ms == Some(40) && turn.defer_external_verification
    )));
    assert!(trace.iter().any(|event| matches!(
        event,
        AgentLoopTraceEvent::LoopGuardTriggered {
            trigger: golutra_core::LoopGuardTrigger::RuntimeDeadline,
            ..
        }
    )));
}

#[tokio::test]
async fn queued_turn_without_override_restores_the_runtime_elapsed_budget() {
    let workspace = tempdir().expect("workspace");
    let calls = Arc::new(AtomicUsize::new(0));
    let provider = SequencedTextProvider {
        calls: calls.clone(),
        delay: Duration::from_millis(60),
        block_from_call: None,
        contract: MockProvider::text_response("unused").contract(),
    };
    let executor = BasicToolExecutor::new(WorkspacePolicy::new(workspace.path()).expect("policy"));
    let harness =
        AgentHarness::new(provider, ContextBuilder::default(), executor).with_max_elapsed_ms(500);
    let (handle, control) = agent_execution_channel(2);
    let queued_turn_id = TurnId::new();
    handle
        .append_turn(PendingAgentTurn {
            command_id: CommandId::new(),
            turn_id: queued_turn_id,
            content: "use the default runtime budget".to_owned(),
            task_contract: Some(TaskContract::conversational(Vec::new())),
            output_schema: None,
            external_verifiers: Vec::new(),
            max_elapsed_ms: None,
            defer_external_verification: false,
            external_verifiers_require_os_sandbox: false,
            allow_network: false,
            yolo: false,
            steer: false,
        })
        .await
        .expect("queued turn");
    let run = AgentRun::new(AgentTaskRequest {
        session_id: SessionId::new(),
        task_id: TaskId::new(),
        turn_id: TurnId::new(),
        objective: "complete the initial turn".to_owned(),
        completion_criteria: Vec::new(),
        output_schema: None,
        touched_code: false,
        contributors: Vec::new(),
        tools: Vec::new(),
    })
    .with_max_elapsed_ms(100);
    let mut trace = Vec::new();

    let outcome = tokio::time::timeout(
        Duration::from_secs(1),
        harness.execute(run, control, |event| trace.push(event)),
    )
    .await
    .expect("queued turn must receive a fresh default budget")
    .expect("queued turn outcome");

    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(outcome.final_turn_id, queued_turn_id);
    assert_eq!(outcome.loop_decision.action, LoopAction::StopSuccess);
    assert!(!trace.iter().any(|event| matches!(
        event,
        AgentLoopTraceEvent::LoopGuardTriggered {
            trigger: golutra_core::LoopGuardTrigger::RuntimeDeadline,
            ..
        }
    )));
}

#[tokio::test]
async fn queued_turn_resets_deferred_external_verification() {
    let workspace = tempdir().expect("workspace");
    let calls = Arc::new(AtomicUsize::new(0));
    let provider = QueuedWriteCorrectionProvider {
        calls: calls.clone(),
        contract: MockProvider::text_response("unused").contract(),
    };
    let executor = BasicToolExecutor::new(WorkspacePolicy::new(workspace.path()).expect("policy"));
    let harness = AgentHarness::new(provider, ContextBuilder::default(), executor)
        .with_deferred_external_verification(true);
    let (handle, control) = agent_execution_channel(2);
    let queued_turn_id = TurnId::new();
    let queued_contract = TaskContract {
        workspace_change: WorkspaceChangeRequirement::Required,
        require_objective_validation: true,
        max_correction_rounds: 1,
        ..TaskContract::default()
    };
    handle
        .append_turn(PendingAgentTurn {
            command_id: CommandId::new(),
            turn_id: queued_turn_id,
            content: "write and verify result.py".to_owned(),
            task_contract: Some(queued_contract),
            output_schema: None,
            external_verifiers: Vec::new(),
            max_elapsed_ms: None,
            defer_external_verification: false,
            external_verifiers_require_os_sandbox: false,
            allow_network: false,
            yolo: false,
            steer: false,
        })
        .await
        .expect("queued turn");
    let mut trace = Vec::new();

    let outcome = harness
        .execute(
            AgentRun::new(AgentTaskRequest {
                session_id: SessionId::new(),
                task_id: TaskId::new(),
                turn_id: TurnId::new(),
                objective: "complete the initial turn".to_owned(),
                completion_criteria: Vec::new(),
                output_schema: None,
                touched_code: false,
                contributors: Vec::new(),
                tools: vec!["write_file".to_owned()],
            })
            .with_deferred_external_verification(true),
            control,
            |event| trace.push(event),
        )
        .await
        .expect("queued turn outcome");

    assert_eq!(outcome.final_turn_id, queued_turn_id);
    assert_eq!(calls.load(Ordering::SeqCst), 4);
    assert!(
        trace
            .iter()
            .any(|event| matches!(event, AgentLoopTraceEvent::CorrectionIssued(_)))
    );
}

#[tokio::test]
async fn explicit_read_contract_requires_objective_evidence() {
    let workspace = tempdir().expect("workspace");
    let provider = MockProvider::text_response("README looks fine.");
    let executor = BasicToolExecutor::new(WorkspacePolicy::new(workspace.path()).expect("policy"));
    let agent_loop = AgentLoop::new(provider, ContextBuilder::default(), executor);

    let (_handle, control) = agent_execution_channel(1);
    let outcome = agent_loop
        .run_with_task_contract_and_observation_sink(
            AgentTaskRequest {
                session_id: SessionId::new(),
                task_id: TaskId::new(),
                turn_id: TurnId::new(),
                objective: "read README.md".to_owned(),
                completion_criteria: vec!["file read evidence".to_owned()],
                output_schema: None,
                touched_code: false,
                contributors: Vec::new(),
                tools: vec!["read_file".to_owned()],
            },
            TaskContract {
                require_objective_validation: true,
                verification: golutra_core::VerificationRequirement::Required,
                max_correction_rounds: 0,
                ..TaskContract::default()
            },
            control,
            |_| {},
        )
        .await
        .expect("loop runs");

    assert_eq!(outcome.loop_decision.action, LoopAction::StopFailed);
    assert_eq!(outcome.verification.result, VerificationResult::Fail);
}

#[tokio::test]
async fn failed_required_read_is_not_hidden_by_unrelated_successful_evidence() {
    let workspace = tempdir().expect("workspace");
    fs::write(workspace.path().join("required.bin"), [0xff]).expect("binary fixture");
    fs::write(workspace.path().join("available.txt"), "available\n").expect("readable fixture");
    let provider = RequiredReadProvider {
        calls: Arc::new(AtomicUsize::new(0)),
        contract: MockProvider::text_response("unused").contract(),
    };
    let executor = BasicToolExecutor::new(WorkspacePolicy::new(workspace.path()).expect("policy"));
    let agent_loop = AgentLoop::new(provider, ContextBuilder::default(), executor);

    let outcome = agent_loop
        .run(AgentTaskRequest {
            session_id: SessionId::new(),
            task_id: TaskId::new(),
            turn_id: TurnId::new(),
            objective: "read required.bin and report its contents".to_owned(),
            completion_criteria: vec!["required.bin contents are reported".to_owned()],
            output_schema: None,
            touched_code: false,
            contributors: Vec::new(),
            tools: vec!["read_file".to_owned()],
        })
        .await
        .expect("read failures become a verification outcome");

    assert!(outcome.tool_reports.iter().any(|report| {
        report.envelope.tool_name == "read_file" && report.envelope.status == ToolResultStatus::Ok
    }));
    assert!(outcome.verification.checks.iter().any(|check| {
        check
            .name
            .starts_with("objective:diagnostic:read_file:identity:")
            && !check.passed
    }));
    assert_eq!(outcome.verification.result, VerificationResult::Fail);
    assert_eq!(outcome.loop_decision.action, LoopAction::StopFailed);
}

#[tokio::test]
async fn agent_loop_returns_recoverable_tool_failure_to_the_provider() {
    let workspace = tempdir().expect("workspace");
    let provider = MockProvider::tool_call("read_file", json!({"path": "missing.md"}));
    let executor = BasicToolExecutor::new(WorkspacePolicy::new(workspace.path()).expect("policy"));
    let agent_loop = AgentLoop::new(provider, ContextBuilder::default(), executor);
    let mut trace = Vec::new();

    let outcome = agent_loop
        .run_with_trace(
            AgentTaskRequest {
                session_id: SessionId::new(),
                task_id: TaskId::new(),
                turn_id: TurnId::new(),
                objective: "read missing file".to_owned(),
                completion_criteria: vec!["file read evidence".to_owned()],
                output_schema: None,
                touched_code: false,
                contributors: Vec::new(),
                tools: vec!["read_file".to_owned()],
            },
            |event| trace.push(event),
        )
        .await
        .expect("loop runs");

    assert_eq!(outcome.loop_decision.action, LoopAction::StopFailed);
    assert!(
        !outcome
            .loop_decision
            .reason
            .contains("security or policy boundary rejected"),
        "{:?}",
        outcome.loop_decision
    );
    assert_eq!(
        outcome.tool_reports[0].policy_evaluation.block_disposition,
        Some(PolicyBlockDisposition::Recoverable)
    );
    assert_eq!(
        trace
            .iter()
            .filter(|event| matches!(event, AgentLoopTraceEvent::ProviderCompleted { .. }))
            .count(),
        2,
        "the blocked tool result must reach a follow-up provider turn"
    );
    assert_eq!(outcome.verification.result, VerificationResult::Fail);
}

#[tokio::test]
async fn duplicate_failures_in_one_provider_round_can_recover_on_the_next_round() {
    let workspace = tempdir().expect("workspace");
    fs::write(workspace.path().join("expected.txt"), "recovered\n").expect("expected result");
    let calls = Arc::new(AtomicUsize::new(0));
    let saw_duplicate_results = Arc::new(AtomicBool::new(false));
    let provider = DuplicateFailureRecoveryProvider {
        calls: calls.clone(),
        saw_duplicate_results: saw_duplicate_results.clone(),
        contract: MockProvider::text_response("unused").contract(),
    };
    let executor = BasicToolExecutor::new(WorkspacePolicy::new(workspace.path()).expect("policy"));
    let mut trace = Vec::new();

    let outcome = AgentLoop::new(provider, ContextBuilder::default(), executor)
        .with_external_verifiers(vec![ExternalVerificationSpec {
            program: "cmp".to_owned(),
            args: vec!["expected.txt".to_owned(), "result.txt".to_owned()],
            cwd: ".".to_owned(),
            timeout_ms: 5_000,
            expected_exit_code: 0,
            max_output_bytes: 1024,
        }])
        .run_with_trace(
            AgentTaskRequest {
                session_id: SessionId::new(),
                task_id: TaskId::new(),
                turn_id: TurnId::new(),
                objective: "write the recovered delivery to result.txt".to_owned(),
                completion_criteria: vec!["result.txt is delivered".to_owned()],
                output_schema: None,
                touched_code: false,
                contributors: Vec::new(),
                tools: vec!["shell".to_owned(), "write_file".to_owned()],
            },
            |event| trace.push(event),
        )
        .await
        .expect("loop recovers");

    assert_eq!(calls.load(Ordering::SeqCst), 3);
    assert!(saw_duplicate_results.load(Ordering::SeqCst));
    assert_eq!(
        fs::read_to_string(workspace.path().join("result.txt")).expect("result"),
        "recovered\n"
    );
    assert_eq!(
        outcome.verification.result,
        VerificationResult::Pass,
        "verification={:#?}\nplan={:#?}\nreports={:#?}",
        outcome.verification,
        outcome.verification_plan,
        outcome.tool_reports
    );
    assert_eq!(outcome.loop_decision.action, LoopAction::StopSuccess);
    assert_eq!(
        outcome.final_message.as_deref(),
        Some("recovered after duplicate failures")
    );
    assert!(!trace.iter().any(|event| matches!(
        event,
        AgentLoopTraceEvent::LoopGuardTriggered {
            trigger: golutra_core::LoopGuardTrigger::RepeatedToolFailure,
            ..
        }
    )));
}

#[tokio::test]
async fn agent_loop_stops_after_a_terminal_sensitive_path_block() {
    let workspace = tempdir().expect("workspace");
    fs::create_dir(workspace.path().join(".git")).expect("git directory");
    fs::write(workspace.path().join(".git/config"), "secret").expect("git config");
    let provider = MockProvider::tool_call("read_file", json!({"path": ".git/config"}));
    let executor = BasicToolExecutor::new(WorkspacePolicy::new(workspace.path()).expect("policy"));
    let agent_loop = AgentLoop::new(provider, ContextBuilder::default(), executor);
    let mut trace = Vec::new();

    let outcome = agent_loop
        .run_with_trace(
            AgentTaskRequest {
                session_id: SessionId::new(),
                task_id: TaskId::new(),
                turn_id: TurnId::new(),
                objective: "read internal git configuration".to_owned(),
                completion_criteria: vec!["git configuration returned".to_owned()],
                output_schema: None,
                touched_code: false,
                contributors: Vec::new(),
                tools: vec!["read_file".to_owned()],
            },
            |event| trace.push(event),
        )
        .await
        .expect("loop runs");

    assert_eq!(outcome.loop_decision.action, LoopAction::Blocked);
    assert!(
        outcome
            .loop_decision
            .reason
            .contains("security or policy boundary rejected")
    );
    assert_eq!(
        outcome.tool_reports[0].policy_evaluation.block_disposition,
        Some(PolicyBlockDisposition::Terminal)
    );
    assert_eq!(
        trace
            .iter()
            .filter(|event| matches!(event, AgentLoopTraceEvent::ProviderCompleted { .. }))
            .count(),
        1,
        "terminal policy blocks must not start another provider turn"
    );
    assert_eq!(outcome.verification.result, VerificationResult::Fail);
}

#[tokio::test]
async fn hard_tool_execution_errors_still_emit_a_terminal_report() {
    let workspace = tempdir().expect("workspace");
    fs::write(workspace.path().join("binary.txt"), [0xff]).expect("binary fixture");
    let provider = MockProvider::tool_call("read_file", json!({"path": "binary.txt"}));
    let executor = BasicToolExecutor::new(WorkspacePolicy::new(workspace.path()).expect("policy"));
    let agent_loop = AgentLoop::new(provider, ContextBuilder::default(), executor);
    let mut trace = Vec::new();

    let outcome = agent_loop
        .run_with_trace(
            AgentTaskRequest {
                session_id: SessionId::new(),
                task_id: TaskId::new(),
                turn_id: TurnId::new(),
                objective: "read binary.txt".to_owned(),
                completion_criteria: vec!["file read evidence".to_owned()],
                output_schema: None,
                touched_code: false,
                contributors: Vec::new(),
                tools: vec!["read_file".to_owned()],
            },
            |event| trace.push(event),
        )
        .await
        .expect("execution error becomes a terminal report");

    assert_eq!(outcome.tool_reports.len(), 1);
    assert_eq!(
        outcome.tool_reports[0].envelope.status,
        ToolResultStatus::Error
    );
    assert_eq!(
        outcome.tool_reports[0].envelope.summary,
        "tool execution failed"
    );
    assert!(trace.iter().any(|event| matches!(
        event,
        AgentLoopTraceEvent::ToolCompleted(report)
            if report.envelope.status == ToolResultStatus::Error
    )));
}

#[tokio::test]
async fn agent_loop_waits_for_approval_before_process_execution() {
    let workspace = tempdir().expect("workspace");
    let provider = MockProvider::tool_call("shell", json!({"command": "echo approved"}));
    let executor = BasicToolExecutor::new(WorkspacePolicy::new(workspace.path()).expect("policy"));
    let agent_loop = AgentLoop::new(provider, ContextBuilder::default(), executor);
    let (handle, control) = agent_execution_channel(4);
    let (trace_tx, mut trace_rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(async move {
        agent_loop
            .run_with_control_and_trace(
                AgentTaskRequest {
                    session_id: SessionId::new(),
                    task_id: TaskId::new(),
                    turn_id: TurnId::new(),
                    objective: "run approved command".to_owned(),
                    completion_criteria: vec!["command evidence".to_owned()],
                    output_schema: None,
                    touched_code: false,
                    contributors: Vec::new(),
                    tools: vec!["shell".to_owned()],
                },
                control,
                move |event| {
                    let _ = trace_tx.send(event);
                },
            )
            .await
    });
    let approval = loop {
        let event = trace_rx.recv().await.expect("approval trace");
        if let AgentLoopTraceEvent::ApprovalRequested(approval) = event {
            break approval;
        }
    };

    assert!(!task.is_finished());
    handle
        .resolve_approval(ApprovalResolution {
            approval_id: approval.approval_id,
            decision: ApprovalDecision::Approved,
            scope: ApprovalScope::Once,
            resource_prefix: None,
            reason: "approved by test".to_owned(),
        })
        .await
        .expect("approval resolves");
    let outcome = task.await.expect("task joins").expect("loop completes");

    assert_eq!(outcome.tool_reports.len(), 1);
    assert_eq!(
        outcome.tool_reports[0].envelope.status,
        ToolResultStatus::Ok,
        "{:?}",
        outcome.tool_reports[0]
    );
}

fn approval_request(tool_name: &str, resource: &str) -> ApprovalRequest {
    ApprovalRequest {
        approval_id: ApprovalId::new(),
        task_id: TaskId::new(),
        turn_id: TurnId::new(),
        tool_call_id: ToolCallId::new(),
        tool_name: tool_name.to_owned(),
        resource: resource.to_owned(),
        reason: "test approval".to_owned(),
    }
}

#[tokio::test]
async fn approval_scope_once_never_creates_a_grant() {
    let (handle, mut control) = agent_execution_channel(4);
    let request = approval_request("shell", "cargo test");
    handle
        .resolve_approval(ApprovalResolution {
            approval_id: request.approval_id,
            decision: ApprovalDecision::Approved,
            scope: ApprovalScope::Once,
            resource_prefix: None,
            reason: "once".to_owned(),
        })
        .await
        .expect("resolution queued");

    let resolution = control
        .wait_for_approval(&request)
        .await
        .expect("resolution accepted");
    assert_eq!(resolution.scope, ApprovalScope::Once);
    assert!(control.approval_grants.is_empty());
    assert!(
        control
            .scoped_approval(&approval_request("shell", "cargo test"))
            .is_none()
    );
}

#[tokio::test]
async fn resource_approval_only_matches_the_same_tool_and_prefix() {
    let (handle, mut control) = agent_execution_channel(4);
    let request = approval_request("shell", "cargo test -p golutra-runtime");
    handle
        .resolve_approval(ApprovalResolution {
            approval_id: request.approval_id,
            decision: ApprovalDecision::Approved,
            scope: ApprovalScope::ResourcePrefix,
            resource_prefix: Some("cargo test".to_owned()),
            reason: "cargo tests".to_owned(),
        })
        .await
        .expect("resolution queued");
    control
        .wait_for_approval(&request)
        .await
        .expect("resolution accepted");

    assert!(
        control
            .scoped_approval(&approval_request("shell", "cargo test -p golutra-client"))
            .is_some()
    );
    assert!(
        control
            .scoped_approval(&approval_request(
                "write_file",
                "cargo test -p golutra-client"
            ))
            .is_none()
    );
    assert!(
        control
            .scoped_approval(&approval_request("shell", "cargo check"))
            .is_none()
    );
    assert!(
        control
            .scoped_approval(&approval_request("shell", "cargo test; rm -rf /"))
            .is_none()
    );
}

#[tokio::test]
async fn session_approval_matches_later_requests_in_the_same_execution() {
    let (handle, mut control) = agent_execution_channel(4);
    let request = approval_request("shell", "cargo test");
    handle
        .resolve_approval(ApprovalResolution {
            approval_id: request.approval_id,
            decision: ApprovalDecision::Approved,
            scope: ApprovalScope::Session,
            resource_prefix: None,
            reason: "task scope".to_owned(),
        })
        .await
        .expect("resolution queued");
    control
        .wait_for_approval(&request)
        .await
        .expect("resolution accepted");

    assert!(
        control
            .scoped_approval(&approval_request("write_file", "outside.txt"))
            .is_some()
    );
    let (_, fresh_control) = agent_execution_channel(4);
    assert!(
        fresh_control
            .scoped_approval(&approval_request("shell", "cargo test"))
            .is_none()
    );
}

#[tokio::test]
async fn invalid_prefixes_and_denials_never_create_grants() {
    for (decision, prefix) in [
        (ApprovalDecision::Approved, Some("outside/".to_owned())),
        (ApprovalDecision::Denied, Some("src/".to_owned())),
    ] {
        let (handle, mut control) = agent_execution_channel(4);
        let request = approval_request("read_file", "src/runtime.rs");
        handle
            .resolve_approval(ApprovalResolution {
                approval_id: request.approval_id,
                decision,
                scope: ApprovalScope::ResourcePrefix,
                resource_prefix: prefix,
                reason: "invalid grant".to_owned(),
            })
            .await
            .expect("resolution queued");
        let resolution = control
            .wait_for_approval(&request)
            .await
            .expect("resolution accepted");

        assert_eq!(resolution.scope, ApprovalScope::Once);
        assert_eq!(resolution.resource_prefix, None);
        assert!(control.approval_grants.is_empty());
    }
}

#[tokio::test]
async fn structured_question_round_trip_is_validated_and_model_visible() {
    let workspace = tempdir().expect("workspace");
    let calls = Arc::new(AtomicUsize::new(0));
    let saw_answer = Arc::new(AtomicBool::new(false));
    let provider = StructuredQuestionProvider {
        calls,
        saw_answer: saw_answer.clone(),
        contract: MockProvider::text_response("unused").contract(),
    };
    let executor = BasicToolExecutor::new(WorkspacePolicy::new(workspace.path()).expect("policy"));
    let agent_loop = AgentLoop::new(provider, ContextBuilder::default(), executor);
    let (handle, control) = agent_execution_channel(4);
    let (trace_tx, mut trace_rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(async move {
        agent_loop
            .run_with_control_and_trace(
                AgentTaskRequest {
                    session_id: SessionId::new(),
                    task_id: TaskId::new(),
                    turn_id: TurnId::new(),
                    objective: "ask for an output format".to_owned(),
                    completion_criteria: Vec::new(),
                    output_schema: None,
                    touched_code: false,
                    contributors: Vec::new(),
                    tools: vec!["ask_user".to_owned()],
                },
                control,
                move |event| {
                    let _ = trace_tx.send(event);
                },
            )
            .await
    });
    let question = loop {
        let event = trace_rx.recv().await.expect("question trace");
        if let AgentLoopTraceEvent::UserQuestionRequested(question) = event {
            break question;
        }
    };
    assert!(!task.is_finished());
    handle
        .resolve_question(UserQuestionResolution {
            question_id: question.question_id,
            answers: vec![UserQuestionAnswer {
                question_id: "format".to_owned(),
                selected_option_ids: vec!["json".to_owned()],
                free_text: Some("Pretty-print with two-space indentation".to_owned()),
            }],
            reason: "test answer".to_owned(),
        })
        .await
        .expect("answer queued");

    let outcome = task.await.expect("task joins").expect("loop completes");
    assert!(saw_answer.load(Ordering::SeqCst));
    assert_eq!(outcome.final_message.as_deref(), Some("JSON selected"));
    assert!(outcome.tool_reports.iter().any(|report| {
        report.envelope.tool_name == "ask_user"
            && report.envelope.structured_facts["answers"][0]["selected_option_ids"][0] == "json"
            && report.envelope.structured_facts["answers"][0]["free_text"]
                == "Pretty-print with two-space indentation"
    }));
}

#[cfg(unix)]
#[tokio::test]
async fn paused_approval_does_not_execute_tool_until_resume() {
    let workspace = tempdir().expect("workspace");
    let output = workspace.path().join("paused.txt");
    let provider = MockProvider::tool_call("shell", json!({"command": "touch paused.txt"}));
    let executor = BasicToolExecutor::new(WorkspacePolicy::new(workspace.path()).expect("policy"));
    let agent_loop = AgentLoop::new(provider, ContextBuilder::default(), executor);
    let (handle, control) = agent_execution_channel(4);
    let (trace_tx, mut trace_rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(async move {
        agent_loop
            .run_with_control_and_trace(
                AgentTaskRequest {
                    session_id: SessionId::new(),
                    task_id: TaskId::new(),
                    turn_id: TurnId::new(),
                    objective: "run command after resume".to_owned(),
                    completion_criteria: vec!["command evidence".to_owned()],
                    output_schema: None,
                    touched_code: false,
                    contributors: Vec::new(),
                    tools: vec!["shell".to_owned()],
                },
                control,
                move |event| {
                    let _ = trace_tx.send(event);
                },
            )
            .await
    });
    let approval = loop {
        let event = trace_rx.recv().await.expect("approval trace");
        if let AgentLoopTraceEvent::ApprovalRequested(approval) = event {
            break approval;
        }
    };

    handle.pause();
    handle
        .resolve_approval(ApprovalResolution {
            approval_id: approval.approval_id,
            decision: ApprovalDecision::Approved,
            scope: ApprovalScope::Once,
            resource_prefix: None,
            reason: "approved while paused".to_owned(),
        })
        .await
        .expect("approval resolves");
    tokio::time::sleep(Duration::from_millis(50)).await;

    assert!(!output.exists());
    assert!(!task.is_finished());
    handle.resume();
    let outcome = task.await.expect("task joins").expect("loop completes");
    assert!(output.exists(), "{:?}", outcome.tool_reports);
    assert_eq!(
        outcome.tool_reports[0].envelope.status,
        ToolResultStatus::Ok
    );
}

#[test]
fn checkpoint_restores_file_before_image_without_touching_git() {
    let workspace = tempdir().expect("workspace");
    let checkpoint_root = tempdir().expect("checkpoint");
    let source = workspace.path().join("src/lib.rs");
    fs::create_dir_all(source.parent().unwrap()).expect("parent");
    fs::write(&source, "pub fn value() -> u8 { 1 }").expect("source");
    let manager = WorkspaceCheckpointManager::new(workspace.path(), checkpoint_root.path());

    let checkpoint = manager
        .create_checkpoint(
            WorkspaceId::new(),
            TaskId::new(),
            TurnId::new(),
            &[FileBeforeImage {
                path: PathBuf::from("src/lib.rs"),
                content: Some(b"pub fn value() -> u8 { 1 }".to_vec()),
                unix_mode: Some(0o755),
                metadata: None,
            }],
            ToolCallId::new(),
        )
        .expect("checkpoint");

    fs::write(&source, "pub fn value() -> u8 { 2 }").expect("updated source");
    manager
        .restore_checkpoint(checkpoint.checkpoint_id)
        .expect("checkpoint restores");

    assert_eq!(checkpoint.changed_files, vec!["src/lib.rs"]);
    assert!(checkpoint_fingerprint(&checkpoint).starts_with("sha256:"));
    assert_eq!(
        fs::read_to_string(&source).expect("restored source"),
        "pub fn value() -> u8 { 1 }"
    );
    assert!(!workspace.path().join(".git").exists());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let checkpoint_dir = checkpoint_root
            .path()
            .join(checkpoint.checkpoint_id.to_string());
        let mode = |path: &Path| {
            fs::metadata(path)
                .expect("checkpoint metadata")
                .permissions()
                .mode()
                & 0o777
        };
        assert_eq!(mode(checkpoint_root.path()), 0o700);
        assert_eq!(mode(&checkpoint_dir), 0o700);
        assert_eq!(mode(&checkpoint_dir.join("manifest.json")), 0o600);
        assert_eq!(mode(&checkpoint_dir.join("files/src/lib.rs")), 0o600);
        assert_eq!(mode(&source), 0o755);
    }
}

#[cfg(unix)]
#[test]
fn checkpoints_hard_link_identical_before_images() {
    use std::os::unix::fs::MetadataExt;

    let workspace = tempdir().expect("workspace");
    let checkpoint_root = tempdir().expect("checkpoint");
    fs::write(workspace.path().join("shared.txt"), "same baseline").expect("baseline");
    let manager = WorkspaceCheckpointManager::new(workspace.path(), checkpoint_root.path());
    let before_image = FileBeforeImage {
        path: PathBuf::from("shared.txt"),
        content: Some(b"same baseline".to_vec()),
        unix_mode: Some(0o644),
        metadata: None,
    };
    let first = manager
        .create_checkpoint(
            WorkspaceId::new(),
            TaskId::new(),
            TurnId::new(),
            std::slice::from_ref(&before_image),
            ToolCallId::new(),
        )
        .expect("first checkpoint");
    let second = manager
        .create_checkpoint(
            WorkspaceId::new(),
            TaskId::new(),
            TurnId::new(),
            std::slice::from_ref(&before_image),
            ToolCallId::new(),
        )
        .expect("second checkpoint");

    let first_path = checkpoint_root
        .path()
        .join(first.checkpoint_id.to_string())
        .join("files/shared.txt");
    let second_path = checkpoint_root
        .path()
        .join(second.checkpoint_id.to_string())
        .join("files/shared.txt");
    let first_metadata = fs::metadata(first_path).expect("first checkpoint file");
    let second_metadata = fs::metadata(second_path).expect("second checkpoint file");

    assert_eq!(first_metadata.ino(), second_metadata.ino());
    assert!(first_metadata.nlink() >= 3);
}

#[test]
fn checkpoint_retention_keeps_only_the_latest_bounded_set() {
    let workspace = tempdir().expect("workspace");
    let checkpoint_root = tempdir().expect("checkpoint");
    let manager = WorkspaceCheckpointManager::new(workspace.path(), checkpoint_root.path());
    for _ in 0..3 {
        manager
            .create_checkpoint(
                WorkspaceId::new(),
                TaskId::new(),
                TurnId::new(),
                &[],
                ToolCallId::new(),
            )
            .expect("checkpoint");
    }

    assert_eq!(manager.checkpoint_count().expect("count"), 3);
    assert_eq!(manager.prune_checkpoints(1).expect("prune"), 2);
    assert_eq!(manager.checkpoint_count().expect("count"), 1);
}

#[test]
fn checkpoint_restore_removes_file_created_by_task() {
    let workspace = tempdir().expect("workspace");
    let checkpoint_root = tempdir().expect("checkpoint");
    let source = workspace.path().join("created.txt");
    let manager = WorkspaceCheckpointManager::new(workspace.path(), checkpoint_root.path());

    let checkpoint = manager
        .create_checkpoint(
            WorkspaceId::new(),
            TaskId::new(),
            TurnId::new(),
            &[FileBeforeImage {
                path: PathBuf::from("created.txt"),
                content: None,
                unix_mode: None,
                metadata: None,
            }],
            ToolCallId::new(),
        )
        .expect("checkpoint");

    fs::write(&source, "created by task").expect("created source");
    manager
        .restore_checkpoint(checkpoint.checkpoint_id)
        .expect("checkpoint restores");

    assert!(!source.exists());
}

#[test]
fn checkpoint_rejects_parent_directory_escape() {
    let workspace = tempdir().expect("workspace");
    let outside = tempdir().expect("outside");
    let outside_file = outside.path().join("outside.txt");
    fs::write(&outside_file, "secret").expect("outside file");
    let checkpoint_root = tempdir().expect("checkpoint");
    let manager = WorkspaceCheckpointManager::new(workspace.path(), checkpoint_root.path());

    let result = manager.create_checkpoint(
        WorkspaceId::new(),
        TaskId::new(),
        TurnId::new(),
        &[FileBeforeImage {
            path: outside_file,
            content: Some(b"secret".to_vec()),
            unix_mode: None,
            metadata: None,
        }],
        ToolCallId::new(),
    );

    assert!(matches!(result, Err(CheckpointError::OutsideWorkspace(_))));
}

#[test]
fn checkpoint_restore_rejects_traversal_in_a_tampered_manifest() {
    let root = tempdir().expect("root");
    let workspace = root.path().join("workspace");
    let checkpoint_root = root.path().join("checkpoints");
    fs::create_dir(&workspace).expect("workspace");
    let outside = root.path().join("outside.txt");
    fs::write(&outside, "keep").expect("outside file");
    let manager = WorkspaceCheckpointManager::new(&workspace, &checkpoint_root);
    let checkpoint = manager
        .create_checkpoint(
            WorkspaceId::new(),
            TaskId::new(),
            TurnId::new(),
            &[FileBeforeImage {
                path: PathBuf::from("created.txt"),
                content: None,
                unix_mode: None,
                metadata: None,
            }],
            ToolCallId::new(),
        )
        .expect("checkpoint");
    let manifest = checkpoint_root
        .join(checkpoint.checkpoint_id.to_string())
        .join("manifest.json");
    fs::write(
        manifest,
        serde_json::to_vec(&json!({
            "entries": [{
                "path": "../outside.txt",
                "existed": false,
                "checksum": null
            }]
        }))
        .expect("manifest"),
    )
    .expect("tamper manifest");

    let error = manager
        .restore_checkpoint(checkpoint.checkpoint_id)
        .expect_err("traversal must be rejected");

    assert!(matches!(error, CheckpointError::InvalidManifest(_)));
    assert_eq!(
        fs::read_to_string(outside).expect("outside remains"),
        "keep"
    );
}

#[test]
fn checkpoint_validates_every_entry_before_restoring_any_file() {
    let workspace = tempdir().expect("workspace");
    let checkpoint_root = tempdir().expect("checkpoint");
    let first = workspace.path().join("first.txt");
    let second = workspace.path().join("second.txt");
    fs::write(&first, "first before").expect("first before");
    fs::write(&second, "second before").expect("second before");
    let manager = WorkspaceCheckpointManager::new(workspace.path(), checkpoint_root.path());
    let checkpoint = manager
        .create_checkpoint(
            WorkspaceId::new(),
            TaskId::new(),
            TurnId::new(),
            &[
                FileBeforeImage {
                    path: PathBuf::from("first.txt"),
                    content: Some(b"first before".to_vec()),
                    unix_mode: None,
                    metadata: None,
                },
                FileBeforeImage {
                    path: PathBuf::from("second.txt"),
                    content: Some(b"second before".to_vec()),
                    unix_mode: None,
                    metadata: None,
                },
            ],
            ToolCallId::new(),
        )
        .expect("checkpoint");
    fs::write(&first, "first after").expect("first after");
    fs::write(&second, "second after").expect("second after");
    let manifest = checkpoint_root
        .path()
        .join(checkpoint.checkpoint_id.to_string())
        .join("manifest.json");
    let mut value: serde_json::Value =
        serde_json::from_slice(&fs::read(&manifest).expect("manifest")).expect("manifest JSON");
    value["entries"][1]["checksum"] = json!("sha256:tampered");
    fs::write(
        &manifest,
        serde_json::to_vec(&value).expect("manifest JSON"),
    )
    .expect("tamper manifest");

    assert!(matches!(
        manager.restore_checkpoint(checkpoint.checkpoint_id),
        Err(CheckpointError::InvalidManifest(_))
    ));
    assert_eq!(fs::read_to_string(first).unwrap(), "first after");
    assert_eq!(fs::read_to_string(second).unwrap(), "second after");
}

#[test]
fn checkpoint_rejects_gitignored_before_images() {
    let workspace = tempdir().expect("workspace");
    let checkpoint_root = tempdir().expect("checkpoint");
    fs::write(workspace.path().join(".gitignore"), "ignored/\n*.secret\n").expect("gitignore");
    fs::create_dir(workspace.path().join("ignored")).expect("ignored directory");
    let manager = WorkspaceCheckpointManager::new(workspace.path(), checkpoint_root.path());

    for path in ["ignored/new.txt", "token.secret"] {
        let result = manager.create_checkpoint(
            WorkspaceId::new(),
            TaskId::new(),
            TurnId::new(),
            &[FileBeforeImage {
                path: workspace.path().join(path),
                content: None,
                unix_mode: None,
                metadata: None,
            }],
            ToolCallId::new(),
        );

        assert!(
            matches!(result, Err(CheckpointError::Excluded(_))),
            "{path}"
        );
    }
}

#[test]
fn partial_checkpoint_filter_omits_ignored_images_but_keeps_safe_files() {
    let workspace = tempdir().expect("workspace");
    let outside = tempdir().expect("outside");
    let checkpoint_root = tempdir().expect("checkpoint");
    fs::write(
        workspace.path().join(".gitignore"),
        ".gitignore\n*.secret\n",
    )
    .expect("gitignore");
    fs::write(workspace.path().join("safe.txt"), "safe").expect("safe file");
    fs::write(workspace.path().join("token.secret"), "secret").expect("ignored file");
    fs::write(outside.path().join("external.txt"), "external").expect("outside file");
    let manager = WorkspaceCheckpointManager::new(workspace.path(), checkpoint_root.path());
    let before_images = [
        FileBeforeImage {
            path: workspace.path().join(".gitignore"),
            content: Some(b".gitignore\n*.secret\n".to_vec()),
            unix_mode: None,
            metadata: None,
        },
        FileBeforeImage {
            path: workspace.path().join("safe.txt"),
            content: Some(b"safe".to_vec()),
            unix_mode: None,
            metadata: None,
        },
        FileBeforeImage {
            path: workspace.path().join("token.secret"),
            content: Some(b"secret".to_vec()),
            unix_mode: None,
            metadata: None,
        },
        FileBeforeImage {
            path: outside.path().join("external.txt"),
            content: Some(b"external".to_vec()),
            unix_mode: None,
            metadata: None,
        },
    ];

    let (retained, excluded_count) = manager
        .filter_checkpointable_before_images(&before_images)
        .expect("partial selection");
    let checkpoint = manager
        .create_checkpoint(
            WorkspaceId::new(),
            TaskId::new(),
            TurnId::new(),
            &retained,
            ToolCallId::new(),
        )
        .expect("partial checkpoint");

    assert_eq!(excluded_count, 3);
    assert_eq!(retained.len(), 1);
    assert_eq!(checkpoint.changed_files, vec!["safe.txt"]);
}

#[test]
fn partial_checkpoint_filter_bounds_large_workspace_snapshots() {
    let workspace = tempdir().expect("workspace");
    let checkpoint_root = tempdir().expect("checkpoint");
    let manager = WorkspaceCheckpointManager::new(workspace.path(), checkpoint_root.path());
    let before_images = (0..130)
        .map(|index| {
            let path = workspace.path().join(format!("file-{index:03}.txt"));
            fs::write(&path, index.to_string()).expect("workspace file");
            FileBeforeImage {
                path,
                content: Some(index.to_string().into_bytes()),
                unix_mode: None,
                metadata: None,
            }
        })
        .collect::<Vec<_>>();

    let (retained, excluded_count) = manager
        .filter_checkpointable_before_images(&before_images)
        .expect("bounded partial selection");

    assert_eq!(retained.len(), 128);
    assert_eq!(excluded_count, 2);
    assert_eq!(
        retained.first().map(|image| &image.path),
        Some(&before_images[0].path)
    );
    assert_eq!(
        retained.last().map(|image| &image.path),
        Some(&before_images[127].path)
    );
}
