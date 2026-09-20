//! 同一 Shell 程序不应因是否显式包装而改变验证语义；失败掩盖仍不能产生通过证据。

use super::*;

#[test]
fn validation_report_identity_preserves_the_executor_workdir() {
    let mut first =
        objective_test_report_with_output("cargo test", "running 1 test\ntest result: ok\n");
    first.envelope.structured_facts["workdir"] = json!("/workspace/one");
    let expected = objective_validation_report(&first).unwrap().identity;
    let mut prepared = first.clone();
    attach_prepared_objective_validation(
        &mut prepared,
        Some(json!({
            "kind": "test", "identity": objective_validation_command_identity("cargo test").unwrap(),
        })),
    );
    assert_eq!(
        objective_validation_report(&prepared).unwrap().identity,
        expected
    );
    for mut report in [first, prepared] {
        report.envelope.structured_facts["workdir"] = json!("/workspace/two");
        assert_ne!(
            objective_validation_report(&report).unwrap().identity,
            expected
        );
    }
}

#[test]
fn compound_validation_identity_preserves_execution_setup() {
    for (left, right) in [
        ("cd one && cargo test", "cd two && cargo test"),
        ("MODE=one; cargo test", "MODE=two; cargo test"),
        (
            "read MODE < one.txt; cargo test",
            "read MODE < two.txt; cargo test",
        ),
        (
            "printf -v MODE one; cargo test",
            "printf -v MODE two; cargo test",
        ),
        (
            "cargo test && cd one && cargo test",
            "cargo test && cd two && cargo test",
        ),
    ] {
        let first = objective_validation_command_identity(left).expect("recognized validation");
        let second = objective_validation_command_identity(right).expect("recognized validation");
        assert_ne!(
            first, second,
            "setup must be part of validation identity: {left}"
        );
        let wrapped = format!("bash -lc {}", shlex::try_quote(left).unwrap());
        assert_eq!(Some(first), objective_validation_command_identity(&wrapped));
    }
    assert_eq!(
        objective_validation_command_identity("cargo test"),
        objective_validation_command_identity("cargo test && printf 'done\\n'")
    );
}

#[test]
fn compound_validation_uses_shell_semantics_with_or_without_a_wrapper() {
    for script in [
        "cargo check && cargo test",
        "python3 -m unittest discover -v && python3 -m py_compile module.py",
        "npm run typecheck && npm test",
        "set -e\ncargo check\ncargo test",
        "cargo test || true",
        "cargo test; true",
        "cargo test | cat",
        "cargo test &",
        "cargo test -- 'name&&literal'",
    ] {
        let wrapped = format!("bash -lc {}", shlex::try_quote(script).unwrap());
        assert_eq!(
            objective_validation_command_kind(script),
            objective_validation_command_kind(&wrapped),
            "classification of {script}"
        );
        assert_eq!(
            objective_validation_command_identity(script),
            objective_validation_command_identity(&wrapped),
            "identity of {script}"
        );
    }
}

struct CompoundRevalidationProvider(AtomicUsize);

#[async_trait]
impl LlmProvider for CompoundRevalidationProvider {
    fn contract(&self) -> ProviderContract {
        MockProvider::text_response("").contract()
    }

    async fn complete(&self, request: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        let round = self.0.fetch_add(1, Ordering::SeqCst);
        let mut response = MockProvider::text_response("Implemented and verified.")
            .complete(request)
            .await?;
        let action = match round {
            0 => Some((
                "shell",
                json!({"command": "python3 -m unittest discover -v"}),
            )),
            1 => Some((
                "write_file",
                json!({"path": "answer.py", "content": "value = 42\n"}),
            )),
            2 => Some((
                "shell",
                json!({"command": "python3 -m unittest discover -v && python3 -m py_compile answer.py"}),
            )),
            _ => None,
        };
        if let Some((name, arguments)) = action {
            response.finish_reason = ProviderFinishReason::ToolCalls;
            response.tool_calls = vec![ProviderToolCall {
                tool_call_id: format!("step-{round}"),
                tool_name: name.into(),
                arguments,
            }];
            response.message = None;
        }
        Ok(response)
    }
}

#[tokio::test]
async fn real_compound_revalidation_completes_without_an_extra_correction_round() {
    let root = tempdir().unwrap();
    fs::write(root.path().join("answer.py"), "value = 0\n").unwrap();
    fs::write(root.path().join("test_answer.py"),
        "import unittest\nfrom answer import value\nclass TestAnswer(unittest.TestCase):\n def test_number(self): self.assertIsInstance(value, int)\n").unwrap();
    let harness = AgentHarness::new(
        CompoundRevalidationProvider(AtomicUsize::new(0)),
        ContextBuilder::default(),
        BasicToolExecutor::new(
            WorkspacePolicy::new(root.path())
                .unwrap()
                .with_unrestricted_access(true),
        ),
    );
    let req = AgentTaskRequest {
        session_id: SessionId::new(),
        task_id: TaskId::new(),
        turn_id: TurnId::new(),
        objective: "Set answer.value to 42, preserving and rerunning the existing tests".into(),
        completion_criteria: Vec::new(),
        output_schema: None,
        touched_code: false,
        contributors: Vec::new(),
        tools: vec!["shell".into(), "write_file".into()],
    };
    let run = ConfiguredAgentRun::new(req)
        .with_execution_mode(Some(AgentExecutionMode::Open))
        .with_task_contract(TaskContract {
            require_objective_validation: true,
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
    assert_eq!(outcome.tool_reports.len(), 3);
    assert_eq!(
        fs::read_to_string(root.path().join("answer.py")).unwrap(),
        "value = 42\n"
    );
}

#[test]
fn shell_failure_masking_does_not_become_successful_test_evidence() {
    for command in [
        "cargo test || true",
        "cargo test; true",
        "cargo test | cat",
        "cargo test &",
    ] {
        let report = objective_test_report_with_output(command, "running 1 test\ntest failed\n");
        assert!(objective_validation_report(&report).is_none(), "{command}");
    }
}

#[test]
fn compound_revalidation_reuses_the_same_test_identity_after_a_source_edit() {
    let root = tempdir().unwrap();
    let contract = TaskContract::open(Vec::new());
    let first = objective_test_report_with_output(
        "python3 -m unittest discover -v",
        "Ran 1 test in 0.01s\nOK\n",
    );
    let validation = objective_validation_report(&first).unwrap();
    let mut edit = objective_test_report("edit_file", None);
    edit.changed_files.push("module.py".into());
    let later = objective_test_report_with_output(
        "python3 -m unittest discover -v && python3 -m py_compile module.py",
        "Ran 1 test in 0.01s\nOK\n",
    );
    let reports = vec![first.clone(), edit, later];
    let attempts = reports
        .iter()
        .enumerate()
        .map(|(i, report)| ToolAttemptMetadata {
            tool_call_id: report.envelope.tool_call_id,
            signature: format!("attempt-{i}"),
            step_no: i as u32,
            status: report.envelope.status,
            recoverable_failure: false,
        })
        .collect::<Vec<_>>();
    let check = |reports: &[ToolExecutionReport]| {
        objective_validation_check_status(
            &first,
            &validation,
            &ObjectiveValidationRecoveryContext {
                objective: "implement and test",
                completion_criteria: &[],
                contract: &contract,
                workspace_root: root.path(),
                reports,
                attempts: &attempts,
            },
        )
    };
    assert_eq!(check(&reports), (true, true));
    let mut no_tests = reports.clone();
    no_tests[2] = objective_test_report_with_output(
        "python3 -m unittest discover -v && python3 -m py_compile module.py",
        "Ran 0 tests in 0.01s\nOK\n",
    );
    no_tests[2].envelope.tool_call_id = reports[2].envelope.tool_call_id;
    assert_eq!(check(&no_tests), (false, false));
    let mut different_tests = reports.clone();
    different_tests[2].envelope.structured_facts["command"] =
        json!("python3 -m unittest other_test && python3 -m py_compile module.py");
    assert_eq!(check(&different_tests), (false, false));
}
