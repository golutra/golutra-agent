//! 复现后台验证与空响应交错；固定进程身份、实际测试证据和工作区变更边界。
use super::*;

fn pending() -> ToolExecutionReport {
    let mut report = objective_test_report("shell", Some("python3 -m unittest -v"));
    report.envelope.structured_facts = json!({
        "command":"python3 -m unittest -v", "workdir":"/workspace",
        "process_id":"owned-process", "authoritative_pid":42,
        "process_state":"running", "terminal":false, "exit_code":null,
    });
    report
}

fn observation(start: &ToolExecutionReport, terminal: bool, output: &str) -> ToolExecutionReport {
    let mut report = start.clone();
    report.envelope.tool_call_id = ToolCallId::new();
    report.envelope.tool_name = "shell_session".into();
    let facts = &mut report.envelope.structured_facts;
    facts["action"] = json!("wait");
    facts["terminal"] = json!(terminal);
    if terminal {
        facts["process_state"] = json!("exited");
        facts["exit_code"] = json!(0);
        facts["terminal_event_id"] = json!(10);
    }
    report
        .artifact_contents
        .push(golutra_agent_tools::ArtifactContent {
            artifact_id: golutra_agent_core::ArtifactId::new(),
            bytes: output.as_bytes().to_vec(),
        });
    report
        .envelope
        .evidence_refs
        .push(golutra_agent_core::EvidenceId::new());
    report
}

fn resolved(
    start: &ToolExecutionReport,
    reports: &[ToolExecutionReport],
) -> ObjectiveValidationOutcome {
    objective_validation_report(&process_validation::resolve_validation_report(
        start, reports,
    ))
    .unwrap()
}

#[test]
fn background_validation_waits_for_terminal_and_preserves_earlier_output() {
    let start = pending();
    let chunk = observation(&start, false, "Ran 4 tests in 0.565s\nOK\n");
    let end = observation(&start, true, "No broken requirements found.\n");
    assert_eq!(
        resolved(&start, &[start.clone(), chunk.clone()]).status,
        ObjectiveValidationStatus::Unknown
    );
    let reports = vec![start.clone(), chunk.clone(), end.clone()];
    let merged = process_validation::resolve_validation_report(&start, &reports);
    assert!(objective_validation_report(&merged).unwrap().passed);
    assert!(
        merged
            .envelope
            .evidence_refs
            .contains(&end.envelope.evidence_refs[0])
    );
    assert_eq!(merged.envelope.tool_call_id, start.envelope.tool_call_id);
    assert_eq!(
        reports[0], start,
        "raw launch evidence must remain unchanged"
    );
}

fn output_range(report: &mut ToolExecutionReport, start: u64, end: u64) {
    report.envelope.structured_facts["output_start_cursor"] = json!(start);
    report.envelope.structured_facts["output_end_cursor"] = json!(end);
    report.envelope.structured_facts["output_lost"] = json!(false);
}

#[test]
fn validation_output_is_invariant_under_every_summary_split() {
    let text = "Ran 4 tests in 0.1s\nOK\n";
    for split in 1..text.len() {
        let start = pending();
        let mut chunk = observation(&start, false, &text[..split]);
        output_range(&mut chunk, 0, split as u64);
        let mut end = observation(&start, true, &text[split..]);
        output_range(&mut end, split as u64, text.len() as u64);
        assert!(
            resolved(&start, &[start.clone(), chunk, end]).passed,
            "split {split}"
        );
    }
}

#[test]
fn validation_never_stitches_gaps_lost_redacted_or_other_process_output() {
    for boundary in ["gap", "lost", "redacted", "overlap", "process"] {
        let start = pending();
        let mut chunk = observation(&start, false, "Ran ");
        output_range(&mut chunk, 0, 4);
        let mut end = observation(&start, true, "4 tests in 0.1s\nOK\n");
        let len = end.artifact_contents.last().unwrap().bytes.len() as u64;
        output_range(&mut end, 4, 4 + len);
        match boundary {
            "gap" => output_range(&mut end, 5, 5 + len),
            "lost" => end.envelope.structured_facts["output_lost"] = json!(true),
            "redacted" => output_range(&mut end, 4, 99),
            "overlap" => output_range(&mut end, 0, len),
            "process" => end.envelope.structured_facts["process_id"] = json!("other"),
            _ => unreachable!(),
        }
        assert!(
            !resolved(&start, &[start.clone(), chunk, end]).passed,
            "{boundary}"
        );
    }
}

#[test]
fn terminal_validation_is_material_progress_once_and_remains_subject_to_freshness() {
    let start = pending();
    let end = observation(&start, true, "Ran 4 tests in 0.1s\nOK\n");
    let mut reports = vec![start.clone(), end.clone()];
    assert!(new_validation_progress(&end, &reports, 1));
    let repeated = observation(&start, true, "Ran 4 tests in 0.1s\nOK\n");
    reports.push(repeated.clone());
    assert!(!new_validation_progress(&repeated, &reports, 2));
    let mut edit = objective_test_report("write_file", None);
    edit.changed_files.push(PathBuf::from("source.py"));
    reports.insert(1, edit);
    assert!(!new_validation_progress(&end, &reports, 2));
}

#[test]
fn background_validation_reads_truncated_terminal_output_without_changing_terminal_status() {
    let start = pending();
    let end = observation(&start, true, "verbose output before tests\n");
    let mut tail = observation(&start, true, "Ran 4 tests in 0.1s\nOK\n");
    tail.envelope.structured_facts["action"] = json!("read");
    assert!(resolved(&start, &[start.clone(), end.clone(), tail.clone()]).passed);
    tail.envelope.structured_facts["terminal_event_id"] = json!(11);
    assert!(!resolved(&start, &[start.clone(), end, tail]).passed);
}

#[test]
fn background_validation_requires_same_owned_process_and_execution_scope() {
    let start = pending();
    for (key, value) in [
        ("process_id", json!("other")),
        ("workdir", json!("/elsewhere")),
        ("command", json!("python3 -m unittest other")),
        ("authoritative_pid", json!(99)),
        ("action", json!("list")),
    ] {
        let mut end = observation(&start, true, "Ran 4 tests in 0.1s\nOK\n");
        end.envelope.structured_facts[key] = value;
        assert_eq!(
            resolved(&start, &[start.clone(), end]).status,
            ObjectiveValidationStatus::Unknown,
            "{key}"
        );
    }
    let mut end = observation(&start, true, "Ran 4 tests in 0.1s\nOK\n");
    end.envelope.risk = "external_mcp_tool".into();
    assert!(!resolved(&start, &[start.clone(), end]).passed);
}

#[test]
fn background_validation_never_overrides_failure_timeout_cancellation_or_zero_tests() {
    let start = pending();
    for (status, code, state) in [
        (ToolResultStatus::Error, 1, "failed"),
        (ToolResultStatus::Timeout, 0, "timed_out"),
        (ToolResultStatus::Cancelled, 0, "cancelled"),
    ] {
        let mut end = observation(&start, true, "Ran 4 tests in 0.1s\n");
        end.envelope.status = status;
        end.envelope.structured_facts["exit_code"] = json!(code);
        end.envelope.structured_facts["process_state"] = json!(state);
        let later = observation(&start, true, "Ran 4 tests in 0.1s\nOK\n");
        assert_eq!(
            resolved(&start, &[start.clone(), end, later]).status,
            ObjectiveValidationStatus::Failed
        );
    }
    let end = observation(&start, true, "Ran 0 tests in 0.1s\nOK\n");
    assert_eq!(
        resolved(&start, &[start.clone(), end]).status,
        ObjectiveValidationStatus::Unknown
    );
}

#[test]
fn background_validation_preserves_prepared_identity_and_checks_intervening_changes() {
    let mut start = pending();
    start.envelope.structured_facts["command"] = json!("<redacted>");
    attach_prepared_objective_validation(
        &mut start,
        Some(json!({"kind":"test","identity":"safe-original-command"})),
    );
    let expected = objective_validation_report(&start).unwrap().identity;
    let end = observation(&start, true, "Ran 4 tests in 0.1s\nOK\n");
    let mut edit = objective_test_report("write_file", None);
    edit.changed_files.push(PathBuf::from("source.py"));
    let reports = vec![start.clone(), edit, end];
    let validation = resolved(&start, &reports);
    assert!(validation.passed);
    assert_eq!(validation.identity, expected);
    let contract = TaskContract::open(Vec::new());
    let context = ObjectiveValidationRecoveryContext {
        objective: "test",
        completion_criteria: &[],
        contract: &contract,
        workspace_root: Path::new("/workspace"),
        reports: &reports,
        attempts: &[],
    };
    assert_eq!(
        objective_validation_check_status(&start, &validation, &context),
        (false, false)
    );
}

struct EmptySequence {
    calls: AtomicUsize,
    tool_between: bool,
}

#[async_trait]
impl LlmProvider for EmptySequence {
    fn contract(&self) -> ProviderContract {
        MockProvider::text_response("").contract()
    }
    async fn complete(&self, request: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        let step = self.calls.fetch_add(1, Ordering::SeqCst);
        let mut response = MockProvider::text_response("Finished.")
            .complete(request)
            .await?;
        assert!(
            step < 4,
            "empty response recovery exceeded fixture sequence"
        );
        if step == 2 {
            response.message.as_mut().unwrap().content = " \n\t".into();
        } else if step != 3 {
            response.message = None;
        }
        if self.tool_between && step == 1 {
            response.finish_reason = ProviderFinishReason::ToolCalls;
            response.tool_calls = vec![ProviderToolCall {
                tool_call_id: "valid-tool".into(),
                tool_name: "read_file".into(),
                arguments: json!({"path":"README.md"}),
            }];
        }
        Ok(response)
    }
}

#[tokio::test]
async fn empty_response_guard_resets_after_a_valid_tool_but_stops_consecutive_empty_replies() {
    for tool_between in [false, true] {
        let root = tempdir().unwrap();
        fs::write(root.path().join("README.md"), "ready").unwrap();
        let agent = AgentLoop::new(
            EmptySequence {
                calls: AtomicUsize::new(0),
                tool_between,
            },
            ContextBuilder::default(),
            BasicToolExecutor::new(WorkspacePolicy::new(root.path()).unwrap()),
        );
        let mut trace = Vec::new();
        let outcome = agent
            .run_with_trace(
                AgentTaskRequest {
                    session_id: SessionId::new(),
                    task_id: TaskId::new(),
                    turn_id: TurnId::new(),
                    objective: "read README and finish".into(),
                    completion_criteria: Vec::new(),
                    output_schema: None,
                    touched_code: false,
                    contributors: Vec::new(),
                    tools: vec!["read_file".into()],
                },
                |e| trace.push(e),
            )
            .await
            .unwrap();
        assert_eq!(
            agent.provider.calls.load(Ordering::SeqCst),
            if tool_between { 4 } else { 2 }
        );
        let stopped = trace.iter().any(|e| {
            matches!(
                e,
                AgentLoopTraceEvent::LoopGuardTriggered {
                    trigger: golutra_agent_core::LoopGuardTrigger::EmptyResponse,
                    ..
                }
            )
        });
        assert_eq!(stopped, !tool_between);
        if tool_between {
            assert_eq!(outcome.final_message.as_deref(), Some("Finished."));
        }
    }
}

struct BackgroundValidationProvider(AtomicUsize);

#[async_trait]
impl LlmProvider for BackgroundValidationProvider {
    fn contract(&self) -> ProviderContract {
        MockProvider::text_response("").contract()
    }
    async fn complete(&self, request: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        let step = self.0.fetch_add(1, Ordering::SeqCst);
        let action = match step {
            0 => Some((
                "shell",
                json!({"command":"python3 -B -m unittest -v", "background":true,"yield_time_ms":1}),
            )),
            1 => {
                let message = request
                    .messages
                    .iter()
                    .rev()
                    .find(|m| m.role == ProviderRole::Tool)
                    .unwrap();
                let tool: Value = serde_json::from_str(&message.content).unwrap();
                let facts = &tool["structured_facts"];
                assert_eq!(facts["process_state"], "running");
                Some((
                    "shell_session",
                    json!({"action":"wait","process_id":facts["process_id"],"wait_ms":5000,"wait_for_terminal":true}),
                ))
            }
            2 => None,
            _ => panic!("completed background tests must not need a correction round"),
        };
        let mut response = MockProvider::text_response("Tests passed.")
            .complete(request)
            .await?;
        if let Some((tool_name, arguments)) = action {
            response.message = None;
            response.finish_reason = ProviderFinishReason::ToolCalls;
            response.tool_calls = vec![ProviderToolCall {
                tool_call_id: format!("step-{step}"),
                tool_name: tool_name.into(),
                arguments,
            }];
        }
        Ok(response)
    }
}

#[tokio::test]
async fn real_background_test_completion_satisfies_required_validation() {
    let root = tempdir().unwrap();
    fs::write(root.path().join("test_slow.py"),"import time, unittest\nclass TestSlow(unittest.TestCase):\n def test_value(self):\n  time.sleep(0.4)\n  self.assertEqual(2+2,4)\n").unwrap();
    let harness = AgentHarness::new(
        BackgroundValidationProvider(AtomicUsize::new(0)),
        ContextBuilder::default(),
        BasicToolExecutor::new(
            WorkspacePolicy::new(root.path())
                .unwrap()
                .with_unrestricted_access(true),
        ),
    );
    let request = AgentTaskRequest {
        session_id: SessionId::new(),
        task_id: TaskId::new(),
        turn_id: TurnId::new(),
        objective: "Run the existing tests in the background and wait for completion".into(),
        completion_criteria: Vec::new(),
        output_schema: None,
        touched_code: false,
        contributors: Vec::new(),
        tools: vec!["shell".into(), "shell_session".into()],
    };
    let run = ConfiguredAgentRun::new(request)
        .with_execution_mode(Some(AgentExecutionMode::Open))
        .with_task_contract(TaskContract {
            require_objective_validation: true,
            max_correction_rounds: Some(0),
            ..TaskContract::open(Vec::new())
        });
    let (_handle, control) = agent_execution_channel(1);
    let result = harness
        .execute_configured(run, control, |_| {})
        .await
        .unwrap();
    assert_eq!(
        result.loop_decision.action,
        LoopAction::StopSuccess,
        "{:?}",
        result.verification
    );
    assert_eq!(result.verification.result, VerificationResult::Pass);
    assert_eq!(result.tool_reports.len(), 2);
    let validation = result
        .verification
        .checks
        .iter()
        .find(|c| c.kind == VerificationCheckKind::ObjectiveValidation)
        .unwrap();
    assert!(validation.passed);
    assert!(
        validation
            .evidence_refs
            .contains(&result.tool_reports[1].envelope.evidence_refs[0])
    );
}
