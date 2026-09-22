//! 用真实文件和进程复现可选验证死循环，并证明显式验收和长任务仍保留原有边界。
use super::*;

struct CompletionSequence {
    actions: Vec<Option<(&'static str, Value)>>,
    next: AtomicUsize,
}

#[async_trait]
impl LlmProvider for CompletionSequence {
    fn contract(&self) -> ProviderContract {
        MockProvider::text_response("").contract()
    }

    async fn complete(&self, request: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        let step = self.next.fetch_add(1, Ordering::SeqCst);
        let action = self.actions.get(step).unwrap_or_else(|| {
            panic!(
                "runtime requested an unnecessary continuation: {:?}",
                request.messages.last()
            )
        });
        let mut response = MockProvider::text_response("Finished.")
            .complete(request)
            .await?;
        if let Some((name, arguments)) = action {
            response.message = None;
            response.finish_reason = ProviderFinishReason::ToolCalls;
            response.tool_calls = vec![ProviderToolCall {
                tool_call_id: format!("step-{step}"),
                tool_name: (*name).into(),
                arguments: arguments.clone(),
            }];
        }
        Ok(response)
    }
}

fn write(path: &str, content: &str) -> Option<(&'static str, Value)> {
    Some(("write_file", json!({"path":path, "content":content})))
}

fn shell(command: &str) -> Option<(&'static str, Value)> {
    Some(("shell", json!({"command":command})))
}

async fn run(
    contract: TaskContract,
    actions: Vec<Option<(&'static str, Value)>>,
) -> (AgentLoopOutcome, Vec<AgentLoopTraceEvent>) {
    let root = tempdir().unwrap();
    fs::write(root.path().join("test_answer.py"), "import unittest\nfrom answer import value\nclass Answer(unittest.TestCase):\n def test_value(self): self.assertEqual(value, 42)\n").unwrap();
    let harness = AgentHarness::new(
        CompletionSequence {
            actions,
            next: AtomicUsize::new(0),
        },
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
        objective: "Implement answer.py with value 42".into(),
        completion_criteria: Vec::new(),
        output_schema: None,
        touched_code: false,
        contributors: Vec::new(),
        tools: vec!["write_file".into(), "edit_file".into(), "shell".into()],
    };
    let run = ConfiguredAgentRun::new(request)
        .with_execution_mode(Some(AgentExecutionMode::Open))
        .with_task_contract(contract);
    let (_, control) = agent_execution_channel(1);
    let mut trace = Vec::new();
    let outcome = harness
        .execute_configured(run, control, |event| trace.push(event))
        .await
        .unwrap();
    (outcome, trace)
}

fn corrections(trace: &[AgentLoopTraceEvent]) -> usize {
    trace
        .iter()
        .filter(|e| matches!(e, AgentLoopTraceEvent::CorrectionIssued(_)))
        .count()
}

#[tokio::test]
async fn best_effort_finishes_after_piped_tests_despite_a_historical_edit_error() {
    let (outcome, trace) = run(TaskContract::open(Vec::new()), vec![
        write("answer.py", "value = 42\n"),
        Some(("edit_file", json!({"path":"answer.py", "edits":[{"old_text":"missing anchor", "new_text":"value = 42"}]}))),
        shell("python3 -m unittest discover -v 2>&1 | tail -20"), None,
    ]).await;
    assert_eq!(outcome.loop_decision.action, LoopAction::StopSuccess);
    assert_eq!(outcome.verification.result, VerificationResult::Partial);
    assert_eq!(outcome.final_message.as_deref(), Some("Finished."));
    assert_eq!(corrections(&trace), 0);
    assert!(
        !outcome
            .verification
            .checks
            .iter()
            .any(|c| c.kind == VerificationCheckKind::ObjectiveValidation)
    );
    assert!(
        outcome
            .tool_reports
            .iter()
            .any(|r| r.envelope.status != ToolResultStatus::Ok)
    );
    assert!(
        outcome
            .tool_reports
            .last()
            .unwrap()
            .envelope
            .model_visible_excerpt
            .as_ref()
            .unwrap()
            .contains("OK")
    );
}

#[tokio::test]
async fn missing_required_validation_stops_repeating_even_when_more_files_are_written() {
    let contract = TaskContract {
        require_objective_validation: true,
        ..TaskContract::open(Vec::new())
    };
    let (outcome, trace) = run(
        contract,
        vec![
            write("answer.py", "value = 42\n"),
            None,
            write("extra_test.py", "assert True\n"),
            None,
        ],
    )
    .await;
    assert_eq!(corrections(&trace), 1);
    assert_ne!(outcome.loop_decision.action, LoopAction::StopSuccess);
    assert!(
        outcome
            .verification
            .residual_risks
            .iter()
            .any(|r| r.contains("automatic correction stopped"))
    );
}

#[tokio::test]
async fn explicit_validation_can_be_satisfied_in_the_same_task() {
    let contract = TaskContract {
        require_objective_validation: true,
        ..TaskContract::open(Vec::new())
    };
    let (outcome, trace) = run(
        contract,
        vec![
            write("answer.py", "value = 42\n"),
            None,
            shell("python3 -m unittest discover -v"),
            None,
        ],
    )
    .await;
    assert_eq!(corrections(&trace), 1);
    assert_eq!(outcome.loop_decision.action, LoopAction::StopSuccess);
    assert_eq!(outcome.verification.result, VerificationResult::Pass);
}

#[tokio::test]
async fn observed_failed_test_is_not_best_effort_success_and_identical_reruns_stop() {
    let (outcome, trace) = run(
        TaskContract::open(Vec::new()),
        vec![
            write("answer.py", "value = 0\n"),
            shell("python3 -m unittest discover -v"),
            None,
            shell("python3 -m unittest discover -v"),
            None,
        ],
    )
    .await;
    assert_eq!(corrections(&trace), 1);
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
async fn required_paths_and_contents_are_not_optional_in_best_effort_mode() {
    let contract = TaskContract {
        required_file_contents: vec![RequiredFileContent {
            path: "answer.py".into(),
            content: "value = 42\n".into(),
        }],
        ..TaskContract::open(Vec::new())
    };
    let (outcome, trace) = run(
        contract,
        vec![
            write("answer.py", "value = 0\n"),
            None,
            write("answer.py", "value = 42\n"),
            None,
        ],
    )
    .await;
    assert_eq!(corrections(&trace), 1);
    assert_eq!(outcome.loop_decision.action, LoopAction::StopSuccess);
}

#[test]
fn native_node_tests_need_actual_execution_and_an_unmasked_exit_status() {
    for command in [
        "node --test",
        "node --test tests/mailbox.test.cjs",
        "node --test --test-reporter=tap tests/mailbox.test.cjs",
    ] {
        let report = objective_test_report_with_output(command, "# tests 2\n# pass 2\n# fail 0\n");
        assert!(
            objective_validation_report(&report).unwrap().passed,
            "{command}"
        );
    }
    for output in [
        "# tests 0\n# pass 0\n",
        "ok 1 - skipped # SKIP\n# tests 1\n# pass 0\n",
        "ℹ tests 0\nℹ pass 0\n",
    ] {
        let report = objective_test_report_with_output("node --test", output);
        assert!(
            !objective_validation_report(&report).unwrap().passed,
            "{output}"
        );
    }
    for command in [
        "node script.js --test",
        "node -e 'console.log(1)' --test",
        "node --test --help",
        "node --test --eval='1'",
        "node --test | tail -20",
        "npm run test:mailbox | grep pass",
    ] {
        assert!(
            objective_validation_command_kind(command).is_none(),
            "{command}"
        );
    }
    let mut failed =
        objective_test_report_with_output("node --test", "# tests 2\n# pass 1\n# fail 1\n");
    failed.envelope.status = ToolResultStatus::Error;
    failed.envelope.structured_facts["exit_code"] = json!(1);
    assert!(!objective_validation_report(&failed).unwrap().passed);
}

#[tokio::test]
async fn native_node_test_results_are_recognized_from_a_real_process() {
    if std::process::Command::new("node")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("Node.js unavailable: skipping real Node process acceptance");
        return;
    }
    let (outcome, trace) = run(TaskContract { require_objective_validation: true, ..TaskContract::open(Vec::new()) }, vec![
        write("answer.test.cjs", "const test = require('node:test'); const assert = require('node:assert/strict'); test('answer', () => assert.equal(6 * 7, 42));\n"),
        shell("node --test --test-reporter=tap answer.test.cjs"), None,
    ]).await;
    assert_eq!(outcome.loop_decision.action, LoopAction::StopSuccess);
    assert_eq!(outcome.verification.result, VerificationResult::Pass);
    assert_eq!(corrections(&trace), 0);
}
