use super::*;

#[tokio::test]
async fn ordinary_output_budget_is_soft_and_completed_output_is_optional() {
    let root = tempdir().unwrap();
    // Comparable to the reported combined README/Cargo output, with multibyte text.
    let content = "中文🙂 project information\n".repeat(750);
    assert!(content.len() > 20_000 && content.len() < 24_000);
    fs::write(root.path().join("output.txt"), &content).unwrap();
    let executor = executor(root.path());
    let session = SessionId::new();
    for arguments in [
        json!({}),
        json!({"max_output_bytes":12000}),
        json!({"max_output_bytes":20000}),
    ] {
        let result = start_terminal(&executor, session, "cat output.txt", arguments).await;
        assert_eq!(result.envelope.status, ToolResultStatus::Ok);
        assert_eq!(result.envelope.structured_facts["process_state"], "exited");
        assert_eq!(result.envelope.structured_facts["terminal"], true);
        assert_eq!(
            result.envelope.structured_facts["next_action"]["action"],
            "read"
        );
        assert_eq!(
            result.envelope.structured_facts["next_action"]["optional"],
            true
        );
        assert_eq!(artifact_text(&result), content);
        let first = model_visible_tool_result_with_token_budget(&result.envelope, 4096);
        let (header, first_text) = first.split_once(MODEL_OUTPUT_SEPARATOR).unwrap();
        let facts: Value = serde_json::from_str(header).unwrap();
        assert_eq!(
            first_text,
            result.envelope.model_visible_excerpt.as_deref().unwrap()
        );
        assert!(first_text.len() > 11_900 && first_text.len() <= 12_288);
        let rest = read_terminal(
            &executor,
            session,
            &result.envelope.structured_facts["process_id"],
            json!({
                "action":"read", "cursor":facts["structured_facts"]["output_cursor"]
            }),
        )
        .await;
        let second = model_visible_tool_result_with_token_budget(&rest.envelope, 4096);
        let (_, second_text) = second.split_once(MODEL_OUTPUT_SEPARATOR).unwrap();
        assert_eq!(format!("{first_text}{second_text}"), content);
        assert_eq!(rest.envelope.structured_facts["output_has_more"], false);
        assert_eq!(
            rest.envelope.structured_facts["next_action"]["kind"],
            "terminal"
        );
    }
}

#[tokio::test]
async fn read_available_output_does_not_wait_for_running_process() {
    let root = tempdir().unwrap();
    let executor = executor(root.path());
    let session = SessionId::new();
    let start = start_terminal(&executor, session, "sleep 30", json!({"background":true})).await;
    let process = &start.envelope.structured_facts["process_id"];
    let read = tokio::time::timeout(
        Duration::from_secs(2),
        read_terminal(
            &executor,
            session,
            process,
            json!({"action":"read", "wait_ms":10000, "wait_for_terminal":true}),
        ),
    )
    .await
    .expect("read must not wait for execution");
    assert_eq!(read.envelope.structured_facts["process_state"], "running");
    assert_eq!(
        read.envelope.structured_facts["next_action"]["kind"],
        "wait"
    );
    read_terminal(&executor, session, process, json!({"action":"terminate"})).await;
}

#[tokio::test]
async fn failed_command_keeps_exit_status_when_output_remains() {
    let root = tempdir().unwrap();
    fs::write(root.path().join("fail.sh"), "cat output.txt\nexit 7\n").unwrap();
    fs::write(
        root.path().join("output.txt"),
        "failure detail\n".repeat(2000),
    )
    .unwrap();
    let executor = executor(root.path());
    let result = start_terminal(&executor, SessionId::new(), "sh fail.sh", json!({})).await;
    assert_eq!(result.envelope.status, ToolResultStatus::Error);
    let projected = model_visible_tool_result_with_token_budget(&result.envelope, 4096);
    let facts: Value =
        serde_json::from_str(projected.split(MODEL_OUTPUT_SEPARATOR).next().unwrap()).unwrap();
    assert_eq!(facts["status"], "error");
    assert_eq!(facts["structured_facts"]["exit_code"], 7);
    assert_eq!(facts["structured_facts"]["process_state"], "failed");
    assert_eq!(facts["structured_facts"]["output_has_more"], true);
    assert_eq!(facts["structured_facts"]["next_action"]["action"], "read");
}

#[tokio::test]
async fn pty_terminal_accepts_input_and_reports_exit() {
    let root = tempdir().unwrap();
    fs::write(root.path().join("interactive.sh"), "test -t 0 || exit 42\nprintf 'ready\\n'\nread -r value\nprintf 'received:%s\\n' \"$value\"\n").unwrap();
    let executor = executor(root.path());
    let session = SessionId::new();
    let start = start_terminal(
        &executor,
        session,
        "sh interactive.sh",
        json!({"tty":true,"yield_time_ms":100}),
    )
    .await;
    assert_eq!(start.envelope.structured_facts["process_state"], "running");
    let id = &start.envelope.structured_facts["process_id"];
    let write = executor
        .execute(
            request_for_session(
                session,
                "shell_session",
                json!({"action":"write","process_id":id,"input":"hello\n","wait_ms":1000}),
            ),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let mut output = write.envelope.model_visible_excerpt.unwrap();
    let end = read_terminal(&executor, session, id, json!({})).await;
    output.push_str(end.envelope.model_visible_excerpt.as_deref().unwrap());
    assert_eq!(end.envelope.structured_facts["exit_code"], 0);
    assert!(output.contains("received:hello"));
}

#[tokio::test]
async fn small_model_budget_returns_an_explicit_replay_cursor() {
    let root = tempdir().unwrap();
    fs::write(
        root.path().join("big.txt"),
        format!(
            "[earlier process output omitted]\n{}",
            "text\n".repeat(1000)
        ),
    )
    .unwrap();
    let executor = executor(root.path());
    let session = SessionId::new();
    let result = start_terminal(
        &executor,
        session,
        "cat big.txt",
        json!({"max_output_bytes":4096}),
    )
    .await;
    let projected = model_visible_tool_result_with_limit(&result.envelope, 1024);
    assert!(projected.len() <= 1024);
    let facts: Value =
        serde_json::from_str(projected.split(MODEL_OUTPUT_SEPARATOR).next().unwrap()).unwrap();
    assert_eq!(facts["structured_facts"]["output_has_more"], true);
    assert_eq!(facts["structured_facts"]["next_action"]["cursor"], 0);
    assert_eq!(
        facts["structured_facts"]["next_action"]["max_output_bytes"],
        256
    );
    let replay = executor
        .execute(
            request_for_session(
                session,
                "shell_session",
                facts["structured_facts"]["next_action"].clone(),
            ),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(
        replay
            .envelope
            .model_visible_excerpt
            .as_deref()
            .unwrap()
            .starts_with("[earlier process output omitted]\ntext\n")
    );
    // The provider receives the projected result, not the larger durable envelope.
    // Check forward progress and complete recovery through that actual surface.
    let mut page = replay;
    let mut recovered = String::new();
    for _ in 0..32 {
        let projected = model_visible_tool_result_with_limit(&page.envelope, 1024);
        assert!(projected.len() <= 1024);
        let (header, text) = projected.split_once(MODEL_OUTPUT_SEPARATOR).unwrap();
        let facts: Value = serde_json::from_str(header).unwrap();
        let facts = &facts["structured_facts"];
        let cursor = facts["output_cursor"].as_u64().unwrap();
        assert!(
            cursor > recovered.len() as u64,
            "small preview must advance: {projected}"
        );
        recovered.push_str(text);
        if facts["output_has_more"] == false {
            break;
        }
        page = read_terminal(
            &executor,
            session,
            &facts["process_id"],
            json!({
                "action":"read", "cursor":cursor, "max_output_bytes":256
            }),
        )
        .await;
    }
    assert_eq!(recovered, artifact_text(&result));
}

#[tokio::test]
async fn overlapping_mutable_terminals_do_not_claim_each_others_file_changes() {
    let root = tempdir().unwrap();
    fs::write(root.path().join("a.sh"), "sleep 0.1\nprintf a > a.txt\n").unwrap();
    fs::write(root.path().join("b.sh"), "sleep 0.1\nprintf b > b.txt\n").unwrap();
    let executor = executor(root.path());
    let session = SessionId::new();
    let a = start_terminal(&executor, session, "sh a.sh", json!({"background":true})).await;
    let b = start_terminal(&executor, session, "sh b.sh", json!({"background":true})).await;
    executor
        .process_supervisor
        .wait_for_scan(
            session,
            a.envelope.structured_facts["process_id"].as_str().unwrap(),
            0,
            2000,
        )
        .await
        .unwrap();
    executor
        .process_supervisor
        .wait_for_scan(
            session,
            b.envelope.structured_facts["process_id"].as_str().unwrap(),
            0,
            2000,
        )
        .await
        .unwrap();
    for start in [a, b] {
        let result = read_terminal(
            &executor,
            session,
            &start.envelope.structured_facts["process_id"],
            json!({}),
        )
        .await;
        assert_eq!(result.envelope.structured_facts["workspace_overlap"], true);
        assert_eq!(
            result.envelope.structured_facts["workspace_changes_known"],
            false
        );
        assert!(result.changed_files.is_empty());
    }
    assert_eq!(fs::read_to_string(root.path().join("a.txt")).unwrap(), "a");
    assert_eq!(fs::read_to_string(root.path().join("b.txt")).unwrap(), "b");
}

async fn start_terminal(
    executor: &ToolRuntime,
    session: SessionId,
    command: &str,
    extra: Value,
) -> ToolExecutionReport {
    let mut args = json!({"command": command});
    args.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    execute_approved(
        executor,
        request_for_session(session, "shell", args),
        CancellationToken::new(),
    )
    .await
}

async fn read_terminal(
    executor: &ToolRuntime,
    session: SessionId,
    process: &Value,
    extra: Value,
) -> ToolExecutionReport {
    let mut args =
        json!({"action":"wait", "process_id": process, "wait_ms": 1000, "wait_for_terminal":true});
    args.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    if args["action"] == "terminate" {
        args.as_object_mut().unwrap().remove("wait_ms");
        args.as_object_mut().unwrap().remove("wait_for_terminal");
    }
    executor
        .execute(
            request_for_session(session, "shell_session", args),
            CancellationToken::new(),
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn ordinary_shell_yields_without_killing_and_continues_by_handle() {
    let root = tempdir().unwrap();
    fs::write(
        root.path().join("job.sh"),
        "sleep 0.15\nprintf 'finished\\n'\n",
    )
    .unwrap();
    let executor = executor(root.path());
    let session = SessionId::new();
    let start = start_terminal(&executor, session, "sh job.sh", json!({"yield_time_ms":1})).await;
    let facts = &start.envelope.structured_facts;
    assert_eq!(facts["process_state"], "running");
    assert_eq!(facts["command"], "sh job.sh");
    assert_eq!(
        facts["workdir"],
        root.path().canonicalize().unwrap().to_str().unwrap()
    );
    let result = read_terminal(&executor, session, &facts["process_id"], json!({})).await;
    assert_eq!(result.envelope.structured_facts["exit_code"], 0);
    assert!(
        result.envelope.structured_facts["elapsed_ms"]
            .as_u64()
            .unwrap()
            >= 100
    );
    assert_eq!(
        result.envelope.model_visible_excerpt.as_deref(),
        Some("finished\n")
    );
}

#[tokio::test]
async fn output_pages_are_complete_repeatable_and_continue_after_exit() {
    let root = tempdir().unwrap();
    let content = "中文🙂 output\n".repeat(400);
    fs::write(root.path().join("output.txt"), &content).unwrap();
    let executor = executor(root.path());
    let session = SessionId::new();
    let start = start_terminal(
        &executor,
        session,
        "cat output.txt",
        json!({"max_output_bytes":256}),
    )
    .await;
    assert_eq!(artifact_text(&start), content);
    let process = &start.envelope.structured_facts["process_id"];
    let listed = executor
        .execute(
            request_for_session(session, "shell_session", json!({"action":"list"})),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let projected: Value = serde_json::from_str(&model_visible_tool_result_with_limit(
        &listed.envelope,
        1024,
    ))
    .unwrap();
    assert_eq!(
        projected["structured_facts"]["processes"][0]["process_id"],
        *process
    );
    let listed = &listed.envelope.structured_facts["processes"][0];
    assert_eq!(listed["output_has_more"], true);
    assert_eq!(listed["next_action"]["kind"], "read");
    assert_eq!(listed["next_action"]["optional"], true);
    assert_eq!(listed["terminal"], true);
    assert_eq!(
        listed["next_action"]["cursor"],
        start.envelope.structured_facts["output_cursor"]
    );
    let mut collected = start.envelope.model_visible_excerpt.clone().unwrap();
    let replay = read_terminal(
        &executor,
        session,
        process,
        json!({"cursor":0,"max_output_bytes":256}),
    )
    .await;
    assert_eq!(
        replay.envelope.model_visible_excerpt,
        start.envelope.model_visible_excerpt
    );
    let mut more = start.envelope.structured_facts["output_has_more"] == true;
    for _ in 0..100 {
        if !more {
            break;
        }
        let page =
            read_terminal(&executor, session, process, json!({"max_output_bytes":256})).await;
        assert_eq!(page.envelope.structured_facts["exit_code"], 0);
        let text = page.envelope.model_visible_excerpt.unwrap();
        assert!(text.len() <= 256);
        assert!(!text.contains('\u{fffd}'));
        collected.push_str(&text);
        more = page.envelope.structured_facts["output_has_more"] == true;
    }
    assert!(!more);
    assert_eq!(collected, content);
    let empty = read_terminal(&executor, session, process, json!({})).await;
    assert_eq!(empty.envelope.model_visible_excerpt.as_deref(), Some(""));
}

#[tokio::test]
async fn fragmented_utf8_and_terminal_updates_work_without_polling() {
    let root = tempdir().unwrap();
    fs::write(
        root.path().join("utf8.sh"),
        "printf '\\344'\nsleep 0.05\nprintf '\\270'\nsleep 0.05\nprintf '\\255\\n'\n",
    )
    .unwrap();
    let supervisor = ProcessSupervisor::new();
    let mut updates = supervisor.subscribe_updates();
    let executor = executor(root.path()).with_process_supervisor(supervisor);
    let session = SessionId::new();
    let start = start_terminal(&executor, session, "sh utf8.sh", json!({"background":true})).await;
    let terminal = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let update = updates.recv().await.unwrap();
            assert!(
                !update.payload["output_excerpt"]
                    .as_str()
                    .unwrap()
                    .contains('\u{fffd}')
            );
            if update.payload["terminal"] == true {
                break update;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(terminal.payload["output_excerpt"], "中\n");
    assert_eq!(terminal.payload["exit_code"], 0);
    let page = read_terminal(
        &executor,
        session,
        &start.envelope.structured_facts["process_id"],
        json!({}),
    )
    .await;
    assert_eq!(page.envelope.model_visible_excerpt.as_deref(), Some("中\n"));
}

#[tokio::test]
async fn terminal_handles_remain_session_scoped_without_pid_arguments() {
    let root = tempdir().unwrap();
    let executor = executor(root.path());
    let session = SessionId::new();
    let start = start_terminal(&executor, session, "sleep 5", json!({"background":true})).await;
    let id = &start.envelope.structured_facts["process_id"];
    let wrong = executor
        .execute(
            request_for_session(
                SessionId::new(),
                "shell_session",
                json!({"action":"wait","process_id":id,"wait_ms":0}),
            ),
            CancellationToken::new(),
        )
        .await;
    assert!(wrong.unwrap_err().to_string().contains("different session"));
    let list = executor
        .execute(
            request_for_session(session, "shell_session", json!({"action":"list"})),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(
        list.envelope
            .structured_facts
            .to_string()
            .contains(id.as_str().unwrap())
    );
    let stop = executor
        .execute(
            request_for_session(
                session,
                "shell_session",
                json!({"action":"terminate","process_id":id}),
            ),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(stop.envelope.status, ToolResultStatus::Ok);
    assert_eq!(
        stop.envelope.structured_facts["process_state"],
        "terminated"
    );
}

#[tokio::test]
async fn wait_deadline_preserves_process_and_different_terminals_wait_concurrently() {
    let root = tempdir().unwrap();
    let executor = executor(root.path());
    let session = SessionId::new();
    let a = start_terminal(&executor, session, "sleep 5", json!({"background":true})).await;
    let b = start_terminal(&executor, session, "sleep 5", json!({"background":true})).await;
    let started = Instant::now();
    let (a_wait, b_wait) = tokio::join!(
        read_terminal(
            &executor,
            session,
            &a.envelope.structured_facts["process_id"],
            json!({"wait_ms":300})
        ),
        read_terminal(
            &executor,
            session,
            &b.envelope.structured_facts["process_id"],
            json!({"wait_ms":300})
        ),
    );
    assert!(started.elapsed() < Duration::from_millis(550));
    assert_eq!(a_wait.envelope.structured_facts["process_state"], "running");
    assert_eq!(b_wait.envelope.structured_facts["process_state"], "running");
    executor
        .process_supervisor
        .shutdown_and_wait()
        .await
        .unwrap();
}
