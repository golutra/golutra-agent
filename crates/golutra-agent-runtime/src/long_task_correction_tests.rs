//! 验证执行前参数反馈与默认 Open 纠偏的真实文件副作用、独立验收及预算边界。

use super::*;

#[tokio::test]
async fn runtime_status_reads_current_loop_facts_without_an_extra_provider_call() {
    let root = tempdir().unwrap();
    let harness = AgentHarness::new(
        MockProvider::tool_call("runtime_status", json!({})),
        ContextBuilder::default(),
        BasicToolExecutor::new(WorkspacePolicy::new(root.path()).unwrap()),
    );
    let mut req = request();
    req.tools = vec!["runtime_status".into()];
    let task_id = req.task_id;
    let run =
        ConfiguredAgentRun::new(req).with_task_contract(TaskContract::conversational(Vec::new()));
    let (_handle, control) = agent_execution_channel(1);
    let outcome = harness
        .execute_configured(run, control, |_| {})
        .await
        .unwrap();
    let facts = &outcome.tool_reports[0].envelope.structured_facts;
    assert_eq!(facts["runtime"]["task_id"], json!(task_id));
    assert_eq!(facts["runtime"]["completed_provider_requests"], 1);
    assert_eq!(facts["runtime"]["tool_results"], 0);
    assert_eq!(facts["running_process_count"], 0);
    assert!(facts["runtime"].get("known_input_tokens").is_some());
    assert!(outcome.tool_reports[0].changed_files.is_empty());
    assert_eq!(outcome.loop_decision.action, LoopAction::StopSuccess);
}

#[test]
fn validation_after_edits_is_required_but_documentation_does_not_expire_code_tests() {
    let root = tempdir().unwrap();
    let contract = TaskContract::open(Vec::new());
    let successful = objective_test_report_with_output(
        "cargo test",
        "running 1 test\ntest check ... ok\ntest result: ok. 1 passed; 0 failed",
    );
    let validation = objective_validation_report(&successful).unwrap();
    assert!(validation.passed);
    for (path, expected) in [("src/lib.rs", false), ("README.md", true)] {
        let mut mutation = objective_test_report("write_file", None);
        mutation.changed_files = vec![PathBuf::from(path)];
        let mut reports = vec![successful.clone(), mutation];
        let mut attempts = reports
            .iter()
            .enumerate()
            .map(|(index, report)| ToolAttemptMetadata {
                tool_call_id: report.envelope.tool_call_id,
                signature: format!("test-{index}"),
                step_no: index as u32,
                status: report.envelope.status,
                recoverable_failure: false,
            })
            .collect::<Vec<_>>();
        let check = |reports: &[ToolExecutionReport], attempts: &[ToolAttemptMetadata]| {
            objective_validation_check_status(
                &reports[0],
                &validation,
                &ObjectiveValidationRecoveryContext {
                    objective: "implement",
                    completion_criteria: &[],
                    contract: &contract,
                    workspace_root: root.path(),
                    reports,
                    attempts,
                },
            )
        };
        assert_eq!(check(&reports, &attempts).0, expected, "{path}");
        let mut rerun = successful.clone();
        rerun.envelope.tool_call_id = ToolCallId::new();
        attempts.push(ToolAttemptMetadata {
            tool_call_id: rerun.envelope.tool_call_id,
            signature: "rerun".into(),
            step_no: 2,
            status: ToolResultStatus::Ok,
            recoverable_failure: false,
        });
        reports.push(rerun);
        assert!(
            check(&reports, &attempts).0,
            "successful recheck after {path}"
        );
    }
}

#[test]
fn direct_file_validation_survives_an_unrelated_source_edit() {
    let previous = objective_test_report_with_output("test -f generated/report.json", "");
    let mut unrelated = objective_test_report("write_file", None);
    unrelated.changed_files = vec![PathBuf::from("src/other.rs")];
    assert!(validation_is_current(
        &previous,
        &[previous.clone(), unrelated]
    ));
    let mut target = objective_test_report("write_file", None);
    target.changed_files = vec![PathBuf::from("generated/report.json")];
    assert!(!validation_is_current(
        &previous,
        &[previous.clone(), target]
    ));
}

#[test]
fn passing_python_tests_do_not_expire_previous_checks_by_writing_bytecode_cache() {
    let previous = objective_test_report_with_output(
        "python3 -m unittest test_existing",
        "Ran 1 test in 0.01s\nOK\n",
    );
    let mut next = objective_test_report_with_output(
        "python3 -m unittest test_added",
        "Ran 1 test in 0.01s\nOK\n",
    );
    next.envelope.structured_facts["workspace_changes_known"] = json!(true);
    next.envelope.structured_facts["workspace_only_derived_changes"] = json!(true);
    next.changed_files = vec!["__pycache__/test_added.cpython-314.pyc".into()];
    let current = |next: &ToolExecutionReport| {
        validation_is_current(&previous, &[previous.clone(), next.clone()])
    };
    assert!(current(&next));
    let mut source_edit = next.clone();
    source_edit.changed_files.push("test_added.py".into());
    source_edit.envelope.structured_facts["workspace_only_derived_changes"] = json!(false);
    assert!(!current(&source_edit));
    let mut explicit_edit = next.clone();
    explicit_edit.envelope.tool_name = "write_file".into();
    assert!(!current(&explicit_edit));
    let mut unknown = next.clone();
    unknown.envelope.structured_facts["workspace_changes_known"] = json!(false);
    assert!(!current(&unknown));
    let mut failed = next.clone();
    failed.envelope.status = ToolResultStatus::Error;
    assert!(!current(&failed));
    let mut standalone = next.clone();
    standalone.changed_files = vec!["module.pyc".into()];
    standalone.envelope.structured_facts["workspace_only_derived_changes"] = json!(false);
    assert!(!current(&standalone));
}

struct CorrectionSequence {
    actions: Vec<Option<Value>>,
    requests: Mutex<Vec<ProviderRequest>>,
    workspace: PathBuf,
}

#[test]
fn source_backed_derived_changes_do_not_expire_tests_but_real_edits_do() {
    let previous = objective_test_report_with_output(
        "python3 -m unittest discover -v",
        "Ran 1 test in 0.01s\nOK\n",
    );
    let mut compile =
        objective_test_report_with_output("python3 -m py_compile prices.py receipt.py", "");
    compile.envelope.structured_facts["workspace_changes_known"] = json!(true);
    compile.envelope.structured_facts["workspace_only_derived_changes"] = json!(true);
    compile.changed_files = vec!["__pycache__/prices.cpython-314.pyc".into()];
    let current = |later: &ToolExecutionReport| {
        validation_is_current(&previous, &[previous.clone(), later.clone()])
    };
    assert!(
        current(&compile),
        "successful syntax checking only rewrote derived bytecode"
    );
    let mut source_edit = compile.clone();
    source_edit.changed_files.push("prices.py".into());
    source_edit.envelope.structured_facts["workspace_only_derived_changes"] = json!(false);
    assert!(!current(&source_edit));
    let mut unknown = compile.clone();
    unknown.envelope.structured_facts["workspace_changes_known"] = json!(false);
    assert!(!current(&unknown));
    let mut failed = compile.clone();
    failed.envelope.status = ToolResultStatus::Error;
    assert!(!current(&failed));
    let mut direct_write = compile.clone();
    direct_write.envelope.tool_name = "write_file".into();
    assert!(!current(&direct_write));
    let mut unproven = compile.clone();
    unproven.envelope.structured_facts["workspace_only_derived_changes"] = Value::Null;
    assert!(!current(&unproven));
}

#[tokio::test]
async fn real_process_snapshots_preserve_validation_across_different_cache_producers() {
    // 用真实进程和前后快照证明派生关系，不能靠模型命令名或伪造“测试通过”输出放行。
    let workspace = tempdir().unwrap();
    fs::write(workspace.path().join("module.py"), "value = 42\n").unwrap();
    fs::write(workspace.path().join("test_module.py"),
        "import unittest\nfrom module import value\nclass Module(unittest.TestCase):\n def test_value(self): self.assertEqual(value, 42)\n").unwrap();
    let executor = BasicToolExecutor::new(
        WorkspacePolicy::new(workspace.path())
            .unwrap()
            .with_unrestricted_access(true),
    );
    let shell = |command: &str| golutra_agent_tools::ToolRequest {
        tool_call_id: ToolCallId::new(),
        provider_tool_call_id: None,
        session_id: SessionId::new(),
        turn_id: None,
        tool_name: "shell".into(),
        arguments: json!({"command":command}),
    };
    let test = executor
        .execute(
            shell("python3 -m unittest discover -v"),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(objective_validation_report(&test).unwrap().passed);
    for command in [
        "python3 -m py_compile module.py",
        "python3 -c \"import py_compile; py_compile.compile('module.py', dfile='review/module.py', doraise=True)\"",
    ] {
        let compile = executor
            .execute(shell(command), CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(compile.envelope.status, ToolResultStatus::Ok);
        assert!(!compile.changed_files.is_empty());
        assert_eq!(
            compile.envelope.structured_facts["workspace_only_derived_changes"],
            true
        );
        assert!(
            validation_is_current(&test, &[test.clone(), compile]),
            "{command}"
        );
    }
    let edit = executor.execute(shell("python3 -c \"from pathlib import Path; Path('module.py').write_text('value = 43\\n')\""), CancellationToken::new()).await.unwrap();
    assert_eq!(
        edit.envelope.structured_facts["workspace_only_derived_changes"],
        false
    );
    assert!(!validation_is_current(&test, &[test.clone(), edit]));
}

struct ReadImplementVerifyProvider(AtomicUsize, bool);

#[async_trait]
impl LlmProvider for ReadImplementVerifyProvider {
    async fn complete(&self, request: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        let index = self.0.fetch_add(1, Ordering::SeqCst);
        let mut response = MockProvider::text_response("Implemented and tested answer.py.")
            .complete(request)
            .await?;
        let tool = if self.1 && index == 1 {
            Some((
                "shell",
                json!({"command":"python3 -c 'from answer import value; assert value == 0'"}),
            ))
        } else {
            match index - usize::from(self.1 && index > 1) {
                0 => Some(("read_file", json!({"path":"answer.py"}))),
                1 => Some((
                    "write_file",
                    json!({"path":"answer.py", "content":"value = 42\n"}),
                )),
                2 => Some((
                    "shell",
                    json!({"command":"python3 -m unittest discover -s tests -v"}),
                )),
                _ => None,
            }
        };
        if let Some((name, arguments)) = tool {
            response.message = None;
            response.finish_reason = ProviderFinishReason::ToolCalls;
            response.tool_calls = vec![ProviderToolCall {
                tool_call_id: format!("read-implement-{index}"),
                tool_name: name.into(),
                arguments,
            }];
        }
        Ok(response)
    }

    fn contract(&self) -> ProviderContract {
        MockProvider::text_response("").contract()
    }
}

#[tokio::test]
async fn reading_then_implementing_and_testing_does_not_require_reading_again() {
    // 真实任务曾因初始阅读“过期”反复纠偏；读取证据不承担最终代码断言。
    for diagnostic in [false, true] {
        let workspace = tempdir().unwrap();
        fs::write(workspace.path().join("answer.py"), "value = 0\n").unwrap();
        fs::create_dir(workspace.path().join("tests")).unwrap();
        fs::write(workspace.path().join("tests/test_answer.py"),
        "import unittest\nfrom answer import value\nclass AnswerTest(unittest.TestCase):\n    def test_value(self):\n        self.assertEqual(value, 42)\n").unwrap();
        let harness = AgentHarness::new(
            ReadImplementVerifyProvider(AtomicUsize::new(0), diagnostic),
            ContextBuilder::default(),
            BasicToolExecutor::new(
                WorkspacePolicy::new(workspace.path())
                    .unwrap()
                    .with_unrestricted_access(true),
            ),
        );
        let mut req = request();
        req.objective =
            "Implement answer.py and run python3 -m unittest discover -s tests -v".into();
        req.tools = vec!["read_file".into(), "write_file".into(), "shell".into()];
        let run = ConfiguredAgentRun::new(req)
            .with_execution_mode(Some(AgentExecutionMode::Open))
            .with_task_contract(TaskContract {
                max_correction_rounds: Some(0),
                ..TaskContract::open(Vec::new())
            });
        let (_handle, control) = agent_execution_channel(1);
        let outcome = harness
            .execute_configured(run, control, |_| {})
            .await
            .unwrap();
        assert_eq!(
            outcome.loop_decision.action,
            LoopAction::StopSuccess,
            "{:?}",
            outcome.verification
        );
        assert_eq!(outcome.tool_reports.len(), 3 + usize::from(diagnostic));
        assert!(
            outcome
                .verification
                .checks
                .iter()
                .any(|check| check.passed
                    && check.name.starts_with("objective:diagnostic:read_file"))
        );
        assert_eq!(
            fs::read_to_string(workspace.path().join("answer.py")).unwrap(),
            "value = 42\n"
        );
    }
}

#[async_trait]
impl LlmProvider for CorrectionSequence {
    async fn complete(&self, request: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        let index = {
            let mut requests = self.requests.lock().unwrap();
            let index = requests.len();
            requests.push(request.clone());
            index
        };
        if index == 1 && self.actions[0].as_ref().is_some_and(|v| !v.is_object()) {
            assert!(!self.workspace.join("result.txt").exists());
            assert!(
                request
                    .messages
                    .iter()
                    .any(|m| m.role == ProviderRole::Tool && m.content.contains("invalid")),
                "schema rejection must reach the model"
            );
        }
        let mut response = MockProvider::text_response("Finished.")
            .complete(request)
            .await?;
        if let Some(arguments) = &self.actions[index.min(self.actions.len() - 1)] {
            response.finish_reason = ProviderFinishReason::ToolCalls;
            response.tool_calls = vec![ProviderToolCall {
                tool_call_id: format!("attempt-{index}"),
                tool_name: "write_file".into(),
                arguments: arguments.clone(),
            }];
            response.message = None;
        }
        Ok(response)
    }

    fn contract(&self) -> ProviderContract {
        MockProvider::text_response("").contract()
    }
}

fn request() -> AgentTaskRequest {
    AgentTaskRequest {
        session_id: SessionId::new(),
        task_id: TaskId::new(),
        turn_id: TurnId::new(),
        objective: "Write result.txt with the requested content and verify it".into(),
        completion_criteria: Vec::new(),
        output_schema: None,
        touched_code: false,
        contributors: Vec::new(),
        tools: vec!["write_file".into()],
    }
}

fn write(content: &str) -> Option<Value> {
    Some(json!({"path":"result.txt", "content":content}))
}

async fn run_sequence(
    actions: Vec<Option<Value>>,
    correction_limit: Option<u32>,
) -> (AgentLoopOutcome, Vec<AgentLoopTraceEvent>, usize) {
    let workspace = tempdir().unwrap();
    let expects_no_write = actions
        .iter()
        .all(|a| a.as_ref().is_some_and(|v| !v.is_object()));
    fs::write(workspace.path().join("expected.txt"), "correct").unwrap();
    let provider = CorrectionSequence {
        actions,
        requests: Mutex::new(Vec::new()),
        workspace: workspace.path().into(),
    };
    let harness = AgentHarness::new(
        provider,
        ContextBuilder::default(),
        BasicToolExecutor::new(WorkspacePolicy::new(workspace.path()).unwrap()),
    )
    .with_external_verifiers(vec![ExternalVerificationSpec {
        program: "cmp".into(),
        args: vec!["expected.txt".into(), "result.txt".into()],
        cwd: ".".into(),
        timeout_ms: 5_000,
        expected_exit_code: 0,
        max_output_bytes: 1024,
    }]);
    let mut run =
        ConfiguredAgentRun::new(request()).with_execution_mode(Some(AgentExecutionMode::Open));
    if let Some(limit) = correction_limit {
        run = run.with_task_contract(TaskContract {
            max_correction_rounds: Some(limit),
            ..TaskContract::open(Vec::new())
        });
    }
    let (_handle, control) = agent_execution_channel(1);
    let mut trace = Vec::new();
    let outcome = harness
        .execute_configured(run, control, |e| trace.push(e))
        .await
        .unwrap();
    let calls = trace
        .iter()
        .filter(|e| matches!(e, AgentLoopTraceEvent::ProviderCompleted { .. }))
        .count();
    if outcome.loop_decision.action == LoopAction::StopSuccess {
        assert_eq!(
            fs::read_to_string(workspace.path().join("result.txt")).unwrap(),
            "correct"
        );
    }
    if expects_no_write {
        assert!(!workspace.path().join("result.txt").exists());
    }
    (outcome, trace, calls)
}

#[tokio::test]
async fn invalid_arguments_are_corrected_before_any_write_and_do_not_poison_verification() {
    for invalid in [json!("{\"path\":"), json!([]), json!(null)] {
        let (outcome, _, calls) =
            run_sequence(vec![Some(invalid), write("correct"), None], None).await;
        assert_eq!(calls, 3);
        assert_eq!(outcome.loop_decision.action, LoopAction::StopSuccess);
        assert_ne!(
            outcome.tool_reports[0].envelope.status,
            ToolResultStatus::Ok
        );
        assert_eq!(
            outcome.tool_reports[0].envelope.structured_facts["rejected_before_execution"],
            true
        );
        assert!(
            outcome
                .verification
                .checks
                .iter()
                .any(|c| c.passed && c.message.contains("original error evidence retained"))
        );
    }
}

#[tokio::test]
async fn default_open_corrects_beyond_eight_failed_candidates() {
    let mut actions = Vec::new();
    for index in 0..12 {
        actions.extend([write(&format!("wrong-{index}")), None]);
    }
    actions.extend([write("correct"), None]);
    let (outcome, trace, calls) = run_sequence(actions, None).await;
    assert_eq!(outcome.loop_decision.action, LoopAction::StopSuccess);
    assert_eq!(calls, 26);
    assert_eq!(
        trace
            .iter()
            .filter(|event| matches!(event, AgentLoopTraceEvent::CorrectionIssued(_)))
            .count(),
        12
    );
}

#[tokio::test]
async fn default_open_corrects_two_failed_candidates_in_the_same_task() {
    let (outcome, trace, calls) = run_sequence(
        vec![
            write("first"),
            None,
            write("second"),
            None,
            write("correct"),
            None,
        ],
        None,
    )
    .await;
    assert_eq!(outcome.loop_decision.action, LoopAction::StopSuccess);
    assert_eq!(calls, 6);
    assert_eq!(
        trace
            .iter()
            .filter(|e| matches!(e, AgentLoopTraceEvent::CorrectionIssued(_)))
            .count(),
        2
    );
}

struct FeedbackAwareProvider(AtomicUsize);

#[async_trait]
impl LlmProvider for FeedbackAwareProvider {
    async fn complete(&self, request: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        let index = self.0.fetch_add(1, Ordering::SeqCst);
        if index == 2 {
            let feedback = &request.messages.last().unwrap().content;
            assert!(
                feedback.contains("cmp expected.txt result.txt"),
                "{feedback}"
            );
            assert!(
                feedback.contains("differ"),
                "missing verifier output: {feedback}"
            );
            assert!(feedback.contains("exit_code"), "{feedback}");
        }
        let mut response = MockProvider::text_response("Finished.")
            .complete(request)
            .await?;
        if index == 0 || index == 2 {
            response.message = None;
            response.finish_reason = ProviderFinishReason::ToolCalls;
            response.tool_calls = vec![ProviderToolCall {
                tool_call_id: format!("feedback-{index}"),
                tool_name: "write_file".into(),
                arguments: json!({"path":"result.txt", "content": if index == 0 {"wrong"} else {"correct"}}),
            }];
        }
        Ok(response)
    }

    fn contract(&self) -> ProviderContract {
        MockProvider::text_response("").contract()
    }
}

#[tokio::test]
async fn correction_delivers_external_verifier_command_and_error_to_the_model() {
    let workspace = tempdir().unwrap();
    fs::write(workspace.path().join("expected.txt"), "correct").unwrap();
    let harness = AgentHarness::new(
        FeedbackAwareProvider(AtomicUsize::new(0)),
        ContextBuilder::default(),
        BasicToolExecutor::new(WorkspacePolicy::new(workspace.path()).unwrap()),
    )
    .with_external_verifiers(vec![ExternalVerificationSpec {
        program: "cmp".into(),
        args: vec!["expected.txt".into(), "result.txt".into()],
        cwd: ".".into(),
        timeout_ms: 5_000,
        expected_exit_code: 0,
        max_output_bytes: 1024,
    }]);
    let run =
        ConfiguredAgentRun::new(request()).with_execution_mode(Some(AgentExecutionMode::Open));
    let (_handle, control) = agent_execution_channel(1);
    let outcome = harness
        .execute_configured(run, control, |_| {})
        .await
        .unwrap();
    assert_eq!(outcome.loop_decision.action, LoopAction::StopSuccess);
    assert_eq!(
        fs::read_to_string(workspace.path().join("result.txt")).unwrap(),
        "correct"
    );
}

struct RevisedFormatProvider(AtomicUsize);

#[cfg(unix)]
struct PublishedSnapshotProvider(AtomicUsize);

#[cfg(unix)]
#[async_trait]
impl LlmProvider for PublishedSnapshotProvider {
    async fn complete(&self, request: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        let index = self.0.fetch_add(1, Ordering::SeqCst);
        if index == 3 {
            let feedback = &request.messages.last().unwrap().content;
            assert!(
                feedback.contains("cmp published.snapshot result.py"),
                "{feedback}"
            );
            assert!(feedback.contains("differ"), "{feedback}");
        }
        let action = match index {
            0 => Some((
                "write_file",
                json!({"path":"result.py", "content":"value = 2\n"}),
            )),
            1 | 4 => Some((
                "shell",
                json!({"command":"python3 -m unittest discover -v"}),
            )),
            3 => Some((
                "shell",
                json!({"command":"cp result.py published.snapshot"}),
            )),
            _ => None,
        };
        let mut response = MockProvider::text_response("Tests passed; delivery complete.")
            .complete(request)
            .await?;
        if let Some((name, arguments)) = action {
            response.message = None;
            response.finish_reason = ProviderFinishReason::ToolCalls;
            response.tool_calls = vec![ProviderToolCall {
                tool_call_id: format!("snapshot-{index}"),
                tool_name: name.into(),
                arguments,
            }];
        }
        Ok(response)
    }

    fn contract(&self) -> ProviderContract {
        MockProvider::text_response("").contract()
    }
}

#[cfg(unix)]
#[tokio::test]
async fn passing_tests_cannot_hide_stale_published_snapshot_and_feedback_repairs_same_task() {
    // 单元测试不能证明发布顺序正确；显式验收拒绝旧快照，默认纠偏在原任务重新发布。
    for correction_limit in [Some(0), None] {
        let workspace = tempdir().unwrap();
        fs::write(workspace.path().join("published.snapshot"), "value = 1\n").unwrap();
        fs::write(workspace.path().join("test_result.py"),
            "import unittest\nfrom result import value\nclass Result(unittest.TestCase):\n def test_value(self): self.assertEqual(value, 2)\n").unwrap();
        let harness = AgentHarness::new(
            PublishedSnapshotProvider(AtomicUsize::new(0)),
            ContextBuilder::default(),
            BasicToolExecutor::new(
                WorkspacePolicy::new(workspace.path())
                    .unwrap()
                    .with_unrestricted_access(true),
            ),
        )
        .with_external_verifiers(vec![ExternalVerificationSpec {
            program: "cmp".into(),
            args: vec!["published.snapshot".into(), "result.py".into()],
            cwd: ".".into(),
            timeout_ms: 5_000,
            expected_exit_code: 0,
            max_output_bytes: 1024,
        }]);
        let mut req = request();
        req.objective = "Implement value = 2, test it and publish an identical snapshot. Republish if validation finds an outdated snapshot.".into();
        req.tools = vec!["write_file".into(), "shell".into()];
        let run = ConfiguredAgentRun::new(req)
            .with_execution_mode(Some(AgentExecutionMode::Open))
            .with_task_contract(TaskContract {
                max_correction_rounds: correction_limit,
                ..TaskContract::open(Vec::new())
            });
        let (_handle, control) = agent_execution_channel(1);
        let mut trace = Vec::new();
        let outcome = tokio::time::timeout(
            Duration::from_secs(30),
            harness.execute_configured(run, control, |event| trace.push(event)),
        )
        .await
        .expect("snapshot correction test timed out")
        .unwrap();
        assert!(outcome.tool_reports.iter().any(|report| {
            objective_validation_report(report).is_some_and(|validation| validation.passed)
                && report.envelope.tool_name == "shell"
        }));
        assert_eq!(
            outcome.loop_decision.action == LoopAction::StopSuccess,
            correction_limit.is_none()
        );
        assert_eq!(
            trace
                .iter()
                .filter(|event| matches!(event, AgentLoopTraceEvent::CorrectionIssued(_)))
                .count(),
            usize::from(correction_limit.is_none())
        );
        assert_eq!(
            fs::read_to_string(workspace.path().join("published.snapshot")).unwrap(),
            if correction_limit.is_none() {
                "value = 2\n"
            } else {
                "value = 1\n"
            }
        );
    }
}

#[async_trait]
impl LlmProvider for RevisedFormatProvider {
    async fn complete(&self, request: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        let index = self.0.fetch_add(1, Ordering::SeqCst);
        let action = match index {
            0 => Some((
                "write_file",
                json!({"path":"report.csv", "content":"name\nAda\n"}),
            )),
            1 => Some((
                "shell",
                json!({"command":"python3 -c 'from pathlib import Path; assert Path(\"report.csv\").read_bytes() == b\"name\\nAda\\n\"'"}),
            )),
            2 => Some((
                "write_file",
                json!({"path":"report.csv", "content":"name\r\nAda\r\n"}),
            )),
            _ => None,
        };
        let mut response = MockProvider::text_response("Finished.")
            .complete(request)
            .await?;
        if let Some((name, arguments)) = action {
            response.message = None;
            response.finish_reason = ProviderFinishReason::ToolCalls;
            response.tool_calls = vec![ProviderToolCall {
                tool_call_id: format!("format-{index}"),
                tool_name: name.into(),
                arguments,
            }];
        }
        Ok(response)
    }
    fn contract(&self) -> ProviderContract {
        MockProvider::text_response("").contract()
    }
}

#[tokio::test]
async fn independent_current_verification_supersedes_stale_successful_exploration_in_open_mode() {
    for mode in [AgentExecutionMode::Open, AgentExecutionMode::Strict] {
        let workspace = tempdir().unwrap();
        fs::write(workspace.path().join("expected.csv"), "name\r\nAda\r\n").unwrap();
        let harness = AgentHarness::new(
            RevisedFormatProvider(AtomicUsize::new(0)),
            ContextBuilder::default(),
            BasicToolExecutor::new(
                WorkspacePolicy::new(workspace.path())
                    .unwrap()
                    .with_unrestricted_access(true),
            ),
        )
        .with_external_verifiers(vec![ExternalVerificationSpec {
            program: "cmp".into(),
            args: vec!["expected.csv".into(), "report.csv".into()],
            cwd: ".".into(),
            timeout_ms: 5_000,
            expected_exit_code: 0,
            max_output_bytes: 1024,
        }]);
        let mut req = request();
        req.objective = "Prepare report.csv for the independent import verifier".into();
        req.tools = vec!["write_file".into(), "shell".into()];
        let run = ConfiguredAgentRun::new(req)
            .with_execution_mode(Some(mode))
            .with_task_contract(TaskContract {
                verification: VerificationRequirement::Independent,
                max_correction_rounds: Some(0),
                ..TaskContract::open(Vec::new())
            });
        let (_handle, control) = agent_execution_channel(1);
        let outcome = harness
            .execute_configured(run, control, |_| {})
            .await
            .unwrap();
        assert_eq!(
            outcome.loop_decision.action == LoopAction::StopSuccess,
            mode == AgentExecutionMode::Open,
            "{mode:?}: {:?}",
            outcome.verification.checks
        );
        assert_eq!(
            fs::read(workspace.path().join("report.csv")).unwrap(),
            b"name\r\nAda\r\n"
        );
        assert!(
            outcome
                .tool_reports
                .iter()
                .any(|r| r.envelope.tool_name == "shell"),
            "original diagnostic remains observable"
        );
    }
}

#[test]
fn current_formal_tests_supersede_old_exploration_under_required_validation() {
    let diagnostic = objective_test_report(
        "shell",
        Some("python3 -c 'from pathlib import Path; assert Path(\"result.txt\").exists()'"),
    );
    let mut changed = objective_test_report("write_file", None);
    changed.changed_files.push("result.txt".into());
    let formal =
        objective_test_report_with_output("python3 -m unittest", "Ran 1 test in 0.01s\nOK\n");
    let attempts = [&diagnostic, &formal]
        .iter()
        .enumerate()
        .map(|(step, report)| ToolAttemptMetadata {
            tool_call_id: report.envelope.tool_call_id,
            signature: format!("check-{step}"),
            step_no: step as u32,
            status: report.envelope.status,
            recoverable_failure: false,
        })
        .collect::<Vec<_>>();
    let reports = vec![diagnostic.clone(), changed.clone(), formal.clone()];
    let contract = TaskContract {
        require_objective_validation: true,
        verification: VerificationRequirement::Required,
        ..TaskContract::open(Vec::new())
    };
    assert!(optional_open_attempt(
        &diagnostic,
        Some(AgentExecutionMode::Open),
        &contract,
        &attempts,
        &reports
    ));
    assert!(!optional_open_attempt(
        &diagnostic,
        Some(AgentExecutionMode::Strict),
        &contract,
        &attempts,
        &reports
    ));
    let mut stale = reports;
    stale.push(changed);
    assert!(!optional_open_attempt(
        &diagnostic,
        Some(AgentExecutionMode::Open),
        &contract,
        &attempts,
        &stale
    ));
}

#[test]
fn independent_supersession_keeps_failed_diagnostics_tests_and_explicit_criteria_blocking() {
    let diagnostic = objective_test_report(
        "shell",
        Some("python3 -c 'from pathlib import Path; assert Path(\"report.csv\").exists()'"),
    );
    let mut changed = objective_test_report("write_file", None);
    changed.changed_files.push("report.csv".into());
    let verifier = objective_test_report("external_verifier", None);
    let contract = TaskContract {
        verification: VerificationRequirement::Independent,
        ..TaskContract::open(Vec::new())
    };
    let eligible =
        |report: &ToolExecutionReport, contract: &TaskContract, after: Vec<ToolExecutionReport>| {
            let attempts = vec![ToolAttemptMetadata {
                tool_call_id: report.envelope.tool_call_id,
                signature: "shell:diagnostic".into(),
                step_no: 0,
                status: report.envelope.status,
                recoverable_failure: true,
            }];
            let reports = std::iter::once(report.clone())
                .chain(after)
                .collect::<Vec<_>>();
            optional_open_attempt(
                report,
                Some(AgentExecutionMode::Open),
                contract,
                &attempts,
                &reports,
            )
        };
    assert!(eligible(
        &diagnostic,
        &contract,
        vec![changed.clone(), verifier.clone()]
    ));
    let mut failed = diagnostic.clone();
    failed.envelope.status = ToolResultStatus::Error;
    failed.envelope.structured_facts["exit_code"] = json!(1);
    assert!(!eligible(
        &failed,
        &contract,
        vec![changed.clone(), verifier.clone()]
    ));
    let formal =
        objective_test_report_with_output("python3 -m unittest", "Ran 1 test in 0.01s\nOK\n");
    assert!(!eligible(
        &formal,
        &contract,
        vec![changed.clone(), verifier.clone()]
    ));
    let explicit = TaskContract {
        completion_criteria: vec!["retain the explicit diagnostic obligation".into()],
        ..contract.clone()
    };
    assert!(!eligible(
        &diagnostic,
        &explicit,
        vec![changed.clone(), verifier.clone()]
    ));
    assert!(!eligible(&diagnostic, &contract, vec![verifier, changed]));
}

#[tokio::test]
async fn corrected_schema_cannot_hide_incorrect_delivery() {
    let (outcome, _, calls) =
        run_sequence(vec![Some(json!("invalid")), write("wrong"), None], Some(0)).await;
    assert_eq!(calls, 3);
    assert_ne!(outcome.loop_decision.action, LoopAction::StopSuccess);
    assert!(
        outcome
            .verification
            .checks
            .iter()
            .any(|c| c.kind == VerificationCheckKind::ObjectiveValidation && !c.passed)
    );
}

#[tokio::test]
async fn repeated_invalid_arguments_can_be_corrected_beyond_old_limits() {
    let mut sequence = vec![Some(json!("invalid")); 20];
    sequence.extend([write("correct"), None]);
    let (outcome, _, calls) = run_sequence(sequence, None).await;
    assert_eq!(outcome.loop_decision.action, LoopAction::StopSuccess);
    assert_eq!(calls, 22);
    assert_eq!(
        outcome
            .tool_reports
            .iter()
            .filter(|r| r.envelope.tool_name == "write_file"
                && r.envelope.status == ToolResultStatus::Ok)
            .count(),
        1
    );
}

#[tokio::test]
async fn explicit_zero_corrections_stops_at_first_failed_candidate() {
    let (outcome, trace, calls) =
        run_sequence(vec![write("wrong"), None, write("correct"), None], Some(0)).await;
    assert_ne!(outcome.loop_decision.action, LoopAction::StopSuccess);
    assert_eq!(calls, 2);
    assert!(
        !trace
            .iter()
            .any(|e| matches!(e, AgentLoopTraceEvent::CorrectionIssued(_)))
    );
}

#[tokio::test]
async fn default_open_plain_conversation_uses_one_provider_request() {
    let workspace = tempdir().unwrap();
    let harness = AgentHarness::new(
        MockProvider::text_response("Hello"),
        ContextBuilder::default(),
        BasicToolExecutor::new(WorkspacePolicy::new(workspace.path()).unwrap()),
    );
    let mut req = request();
    req.objective = "Hello".into();
    let run = ConfiguredAgentRun::new(req).with_execution_mode(Some(AgentExecutionMode::Open));
    let (_handle, control) = agent_execution_channel(1);
    let mut calls = 0;
    let result = harness
        .execute_configured(run, control, |e| {
            if matches!(e, AgentLoopTraceEvent::ProviderCompleted { .. }) {
                calls += 1;
            }
        })
        .await
        .unwrap();
    assert_eq!(result.loop_decision.action, LoopAction::StopSuccess);
    assert_eq!(calls, 1);
}

#[tokio::test]
async fn response_schema_repair_finishes_without_tools_or_workspace_changes() {
    struct RepairProvider(std::sync::Mutex<Vec<ProviderRequest>>);
    #[async_trait]
    impl LlmProvider for RepairProvider {
        fn contract(&self) -> golutra_agent_core::ProviderContract {
            MockProvider::text_response("").contract()
        }
        async fn complete(&self, req: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
            let first = {
                let mut requests = self.0.lock().unwrap();
                let first = requests.is_empty();
                requests.push(req.clone());
                first
            };
            if !first {
                let feedback = &req.messages.last().unwrap().content;
                assert!(
                    feedback.contains("Correct the final response"),
                    "{feedback}"
                );
                assert!(!feedback.contains("rerun"), "{feedback}");
            }
            MockProvider::text_response(if first { "not JSON" } else { "{\"result\":42}" })
                .complete(req)
                .await
        }
    }
    let root = tempdir().unwrap();
    let harness = AgentHarness::new(
        RepairProvider(std::sync::Mutex::new(Vec::new())),
        ContextBuilder::default(),
        BasicToolExecutor::new(WorkspacePolicy::new(root.path()).unwrap()),
    );
    let mut req = request();
    req.objective = "Return result 42 as JSON".into();
    req.output_schema = Some(json!({"type":"object", "required":["result"],
        "properties":{"result":{"type":"integer"}}, "additionalProperties":false}));
    let run = ConfiguredAgentRun::new(req).with_task_contract(TaskContract {
        max_correction_rounds: Some(2),
        ..TaskContract::open(Vec::new())
    });
    let (_handle, control) = agent_execution_channel(1);
    let mut calls = 0;
    let outcome = harness
        .execute_configured(run, control, |event| {
            if matches!(event, AgentLoopTraceEvent::ProviderCompleted { .. }) {
                calls += 1;
            }
        })
        .await
        .unwrap();
    assert_eq!(outcome.loop_decision.action, LoopAction::StopSuccess);
    assert_eq!(calls, 2);
    assert!(outcome.tool_reports.is_empty());
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
}

#[test]
fn admission_recovery_requires_a_later_same_tool_success_and_keeps_hard_failures() {
    let mut report = objective_test_report("write_file", None);
    report.envelope.status = ToolResultStatus::Error;
    report.envelope.structured_facts = json!({"rejected_before_execution":true});
    report.policy_evaluation.decision = PolicyDecision::Block;
    let mut attempts = vec![
        ToolAttemptMetadata {
            tool_call_id: report.envelope.tool_call_id,
            signature: tool_attempt_signature("write_file", &json!("invalid")),
            step_no: 1,
            status: ToolResultStatus::Error,
            recoverable_failure: true,
        },
        ToolAttemptMetadata {
            tool_call_id: golutra_agent_core::ToolCallId::new(),
            signature: tool_attempt_signature("read_file", &json!({"path":"other"})),
            step_no: 2,
            status: ToolResultStatus::Ok,
            recoverable_failure: false,
        },
    ];
    assert_eq!(
        tool_execution_check_status(&report, &attempts),
        (false, false)
    );
    attempts[1].signature =
        tool_attempt_signature("write_file", &json!({"path":"result.txt","content":"ok"}));
    attempts[1].step_no = 1;
    assert_eq!(
        tool_execution_check_status(&report, &attempts),
        (false, false)
    );
    attempts[1].step_no = 2;
    assert_eq!(
        tool_execution_check_status(&report, &attempts),
        (true, true)
    );
    attempts[0].recoverable_failure = false;
    assert_eq!(
        tool_execution_check_status(&report, &attempts),
        (false, false)
    );
}

struct ExplorationProvider {
    calls: AtomicUsize,
    fail_test: bool,
}

#[async_trait]
impl LlmProvider for ExplorationProvider {
    async fn complete(&self, request: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        let index = self.calls.fetch_add(1, Ordering::SeqCst);
        let mut response = MockProvider::text_response("Delivered and checked result.txt.")
            .complete(request)
            .await?;
        let tool = match index {
            0 if self.fail_test => Some((
                "shell",
                json!({"argv":["python3","-m","unittest","missing_test_suite"]}),
            )),
            0 => Some((
                "shell",
                json!({"argv":["python3","-c","assert False, 'exploratory hypothesis disproved'"]}),
            )),
            1 => Some((
                "write_file",
                json!({"path":"result.txt","content":"correct"}),
            )),
            _ => None,
        };
        if let Some((name, arguments)) = tool {
            response.message = None;
            response.finish_reason = ProviderFinishReason::ToolCalls;
            response.tool_calls = vec![ProviderToolCall {
                tool_call_id: format!("exploration-{index}"),
                tool_name: name.into(),
                arguments,
            }];
        }
        Ok(response)
    }
    fn contract(&self) -> ProviderContract {
        MockProvider::text_response("").contract()
    }
}

#[tokio::test]
async fn open_checks_delivery_without_requiring_every_exploration_to_succeed_but_keeps_failed_tests()
 {
    for (mode, fail_test, expected_success) in [
        (AgentExecutionMode::Open, false, true),
        (AgentExecutionMode::Strict, false, false),
        (AgentExecutionMode::Open, true, false),
    ] {
        let workspace = tempdir().unwrap();
        fs::write(workspace.path().join("expected.txt"), "correct").unwrap();
        let harness = AgentHarness::new(
            ExplorationProvider {
                calls: AtomicUsize::new(0),
                fail_test,
            },
            ContextBuilder::default(),
            BasicToolExecutor::new(
                WorkspacePolicy::new(workspace.path())
                    .unwrap()
                    .with_unrestricted_access(true),
            ),
        )
        .with_external_verifiers(vec![ExternalVerificationSpec {
            program: "cmp".into(),
            args: vec!["expected.txt".into(), "result.txt".into()],
            cwd: ".".into(),
            timeout_ms: 5_000,
            expected_exit_code: 0,
            max_output_bytes: 1024,
        }]);
        let mut req = request();
        req.tools = vec!["read_file".into(), "write_file".into(), "shell".into()];
        let run = ConfiguredAgentRun::new(req)
            .with_execution_mode(Some(mode))
            .with_task_contract(TaskContract {
                max_correction_rounds: Some(0),
                ..TaskContract::open(Vec::new())
            });
        let (_handle, control) = agent_execution_channel(1);
        let outcome = harness
            .execute_configured(run, control, |_| {})
            .await
            .unwrap();
        assert_eq!(
            outcome.loop_decision.action == LoopAction::StopSuccess,
            expected_success,
            "{mode:?} fail_test={fail_test}: {:?}",
            outcome.verification.checks
        );
        assert_ne!(
            outcome.tool_reports[0].envelope.status,
            ToolResultStatus::Ok,
            "original failure is preserved"
        );
        assert_eq!(
            fs::read_to_string(workspace.path().join("result.txt")).unwrap(),
            "correct"
        );
    }
}

#[test]
fn open_optional_attempts_never_ignore_latest_unknown_or_hard_failures() {
    let mut report = objective_test_report("shell", None);
    report.envelope.status = ToolResultStatus::Error;
    let mut attempts = vec![ToolAttemptMetadata {
        tool_call_id: report.envelope.tool_call_id,
        signature: "shell:failed".into(),
        step_no: 1,
        status: ToolResultStatus::Error,
        recoverable_failure: true,
    }];
    let contract = TaskContract::open(Vec::new());
    assert!(!optional_open_attempt(
        &report,
        Some(AgentExecutionMode::Open),
        &contract,
        &attempts,
        &[]
    ));
    attempts.push(ToolAttemptMetadata {
        tool_call_id: golutra_agent_core::ToolCallId::new(),
        signature: "shell:success".into(),
        step_no: 2,
        status: ToolResultStatus::Ok,
        recoverable_failure: false,
    });
    assert!(
        !optional_open_attempt(
            &report,
            Some(AgentExecutionMode::Open),
            &contract,
            &attempts,
            &[]
        ),
        "an unrelated successful tool call is not validation evidence"
    );
    let verified = objective_test_report("external_verifier", None);
    assert!(optional_open_attempt(
        &report,
        Some(AgentExecutionMode::Open),
        &contract,
        &attempts,
        std::slice::from_ref(&verified)
    ));
    for facts in [
        json!({"workspace_changes_known":false}),
        json!({"hard_failure":true}),
        json!({"timed_out":true}),
        json!({"cancelled":true}),
    ] {
        report.envelope.structured_facts = facts;
        assert!(!optional_open_attempt(
            &report,
            Some(AgentExecutionMode::Open),
            &contract,
            &attempts,
            std::slice::from_ref(&verified)
        ));
    }
}
