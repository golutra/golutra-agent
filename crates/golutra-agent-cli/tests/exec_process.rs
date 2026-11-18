use std::{fs, path::Path, process::Stdio, time::Duration};

use golutra_agent_client::{AppServerInfo, RuntimeClient, RuntimeTransport};
use secrecy::SecretString;
use serde_json::{Value, json};
use tempfile::tempdir;
use tokio::{io::AsyncWriteExt, process::Command};

#[tokio::test]
async fn exec_provider_flags_override_project_then_global_without_changing_host_preferences() {
    let home = tempdir().unwrap();
    let workspace = tempdir().unwrap();
    install_mock_provider(home.path());
    let provider_path = home.path().join("provider.json");
    let mut provider: Value = serde_json::from_slice(&fs::read(&provider_path).unwrap()).unwrap();
    provider["profiles"][0]["generation_config"] =
        json!({"reasoning_effort":"high", "max_tokens":4096});
    fs::write(&provider_path, serde_json::to_vec(&provider).unwrap()).unwrap();
    let original_provider = fs::read(&provider_path).unwrap();
    fs::write(
        home.path().join("runtime.json"),
        r#"{"reasoning_effort":"medium"}"#,
    )
    .unwrap();
    fs::create_dir(workspace.path().join(".golutra-agent")).unwrap();
    let project_path = workspace.path().join(".golutra-agent/runtime.json");
    fs::write(
        &project_path,
        r#"{"reasoning_effort":"low","model":"project-model","tool_profile":"full"}"#,
    )
    .unwrap();
    let original_project = fs::read(&project_path).unwrap();
    let (mut server, stderr) = start_app_server(home.path());
    let (info, token, transport) =
        wait_for_runtime(home.path(), workspace.path(), &mut server, stderr.path()).await;
    for (flags, effort, model, profile) in [
        (vec![], json!("low"), "project-model", "full"),
        (
            vec![
                "--reasoning-effort",
                "high",
                "--model",
                "flag-model",
                "--tool-profile",
                "coding",
            ],
            json!("high"),
            "flag-model",
            "coding",
        ),
        (
            vec!["--reasoning-effort", "default"],
            Value::Null,
            "project-model",
            "full",
        ),
    ] {
        let mut args = vec!["exec", "--json"];
        args.extend(flags);
        args.push("Reply with a short acknowledgement.");
        let output = run_cli(home.path(), workspace.path(), &info, &token, &args).await;
        assert!(output.status.success(), "{output:?}");
        let session = String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .find(|event| event["type"] == "thread.started")
            .unwrap()["session_id"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        let events = transport
            .replay_events(golutra_agent_protocol::EventFilter {
                session_id: session,
                task_id: None,
                after_sequence_no: None,
            })
            .await
            .unwrap();
        let task = events
            .iter()
            .find(|event| event["event_type"] == "task_created")
            .unwrap();
        let payload = &task["payload"]["payload"];
        assert_eq!(
            payload["provider_generation_config"]["reasoning_effort"],
            effort
        );
        assert_eq!(payload["provider_generation_config"]["max_tokens"], 4096);
        assert_eq!(payload["provider_model"], model);
        assert_eq!(payload["tool_profile"], profile);
        assert!(payload.get("provider_reasoning_effort").is_none());
    }
    assert_eq!(fs::read(&provider_path).unwrap(), original_provider);
    assert_eq!(fs::read(&project_path).unwrap(), original_project);
    server.kill().await.unwrap();
}

#[tokio::test]
async fn exec_and_exec_resume_work_across_independent_processes() {
    let home = tempdir().expect("home");
    let workspace = tempdir().expect("workspace");
    install_mock_provider(home.path());
    let (mut app_server, stderr) = start_app_server(home.path());
    let (info, token, transport) = wait_for_runtime(
        home.path(),
        workspace.path(),
        &mut app_server,
        stderr.path(),
    )
    .await;

    let first = run_cli(
        home.path(),
        workspace.path(),
        &info,
        &token,
        &["exec", "--yolo", "reply with a short acknowledgement"],
    )
    .await;
    assert!(first.status.success(), "first exec failed: {first:?}");
    let first_stdout = String::from_utf8_lossy(&first.stdout);
    let first_stderr = String::from_utf8_lossy(&first.stderr);
    assert!(first_stdout.contains("mock provider completed"));
    assert!(
        !first_stderr.trim().is_empty(),
        "progress belongs on stderr"
    );

    let thread = transport
        .list_threads(20)
        .await
        .expect("thread list")
        .into_iter()
        .next()
        .expect("created thread");
    let thread_id = thread.thread_id.to_string();
    let resumed = run_cli(
        home.path(),
        workspace.path(),
        &info,
        &token,
        &["exec", "resume", &thread_id, "--yolo", "reply again"],
    )
    .await;
    assert!(resumed.status.success(), "exec resume failed: {resumed:?}");
    assert!(String::from_utf8_lossy(&resumed.stdout).contains("mock provider completed"));

    let daemon = run_daemon_cli(
        home.path(),
        workspace.path(),
        &["exec", "--yolo", "reply through the local daemon"],
    )
    .await;
    assert!(daemon.status.success(), "daemon exec failed: {daemon:?}");
    assert!(String::from_utf8_lossy(&daemon.stdout).contains("mock provider completed"));

    let json_output = run_cli(
        home.path(),
        workspace.path(),
        &info,
        &token,
        &["exec", "--json", "reply once more"],
    )
    .await;
    assert!(
        json_output.status.success(),
        "JSON exec failed: {json_output:?}"
    );
    let json_stdout = String::from_utf8_lossy(&json_output.stdout);
    let json_stderr = String::from_utf8_lossy(&json_output.stderr);
    assert!(!json_stdout.trim().is_empty());
    for line in json_stdout.lines() {
        let value: Value = serde_json::from_str(line).expect("JSONL stdout event");
        assert!(value.get("type").is_some(), "event type missing: {value}");
    }
    assert!(json_stderr.trim().is_empty());

    app_server.kill().await.expect("stop app server");
}

#[tokio::test]
async fn exec_reads_a_piped_prompt_without_mixing_progress_into_stdout() {
    let home = tempdir().expect("home");
    let workspace = tempdir().expect("workspace");
    install_mock_provider(home.path());
    let (mut app_server, stderr) = start_app_server(home.path());
    let (info, token, _transport) = wait_for_runtime(
        home.path(),
        workspace.path(),
        &mut app_server,
        stderr.path(),
    )
    .await;

    let mut child = Command::new(env!("CARGO_BIN_EXE_golutra-agent"))
        .arg("--cwd")
        .arg(workspace.path())
        .arg("--connect")
        .arg(&info.base_url)
        .arg("exec")
        .arg("-")
        .env("GOLUTRA_AGENT_HOME", home.path())
        .env("GOLUTRA_AGENT_TRANSPORT_TOKEN", &token)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("exec process");
    child
        .stdin
        .take()
        .expect("exec stdin")
        .write_all(b"reply from stdin\n")
        .await
        .expect("write piped prompt");
    let output = tokio::time::timeout(Duration::from_secs(60), child.wait_with_output())
        .await
        .expect("piped exec timeout")
        .expect("piped exec output");
    assert!(output.status.success(), "piped exec failed: {output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stdout.contains("mock provider completed"));
    assert!(!stderr.trim().is_empty(), "progress belongs on stderr");

    app_server.kill().await.expect("stop app server");
}

#[tokio::test]
async fn exec_run_dir_retains_an_isolated_structured_runtime_bundle() {
    let home = tempdir().expect("home");
    let workspace = tempdir().expect("workspace");
    let export_parent = tempdir().expect("export parent");
    let state_dir = export_parent.path().join("runtime");
    let task_contract = export_parent.path().join("task-contract.json");
    // The Cargo test executable lives outside this temporary workspace and is
    // intentionally not mounted by Linux's filesystem sandbox. Use standard
    // sandbox-visible tools on Unix rather than widening its read permissions.
    #[cfg(unix)]
    let (verifier, verifier_args) = (
        std::path::PathBuf::from("/bin/sh"),
        vec!["-c", "printf %s retained | cmp -s retained.txt -"],
    );
    #[cfg(not(unix))]
    let (verifier, verifier_args) = (
        std::env::current_exe().expect("current test executable"),
        vec!["--ignored", "--exact", "retained_file_verifier_helper"],
    );
    fs::write(
        &task_contract,
        serde_json::to_vec_pretty(&json!({
            "schema_version": 1,
            "workspace_change": "required",
            "required_paths": ["retained.txt"],
            "required_file_contents": [{
                "path": "retained.txt",
                "content": "retained"
            }],
            "require_objective_validation": true,
            "verification": "required",
            "max_correction_rounds": 1
        }))
        .expect("task contract JSON"),
    )
    .expect("task contract");
    install_mock_provider(home.path());

    // Exercise embedded command dispatch with Windows' default main-stack
    // budget on Unix too, without changing any production stack settings.
    #[cfg(unix)]
    let mut cli = {
        let mut command = Command::new("/bin/sh");
        command
            .arg("-c")
            .arg("ulimit -s 1024 && exec \"$@\"")
            .arg("exec-stack-regression")
            .arg(env!("CARGO_BIN_EXE_golutra-agent"));
        command
    };
    #[cfg(not(unix))]
    let mut cli = Command::new(env!("CARGO_BIN_EXE_golutra-agent"));

    let output = tokio::time::timeout(
        Duration::from_secs(60),
        cli.arg("--cwd")
            .arg(workspace.path())
            .arg("exec")
            .arg("--run-dir")
            .arg(&state_dir)
            .arg("--full-run-export")
            .arg("--task-contract")
            .arg(&task_contract)
            .arg("--allow-network")
            .arg("--approval-mode")
            .arg("auto")
            .arg("--verify-program")
            .arg(&verifier)
            .args(
                verifier_args
                    .into_iter()
                    .flat_map(|arg| ["--verify-arg", arg]),
            )
            .arg("write file retained.txt with content retained")
            .env("GOLUTRA_AGENT_HOME", home.path())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .expect("run-dir exec timeout")
    .expect("run-dir exec output");

    if !output.status.success() {
        let evidence = export_parent.keep();
        panic!(
            "persisted run-dir exec failed; bundle retained at {}; stdout={}; stderr={}",
            evidence.display(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
    assert_eq!(
        fs::read_to_string(workspace.path().join("retained.txt")).expect("workspace file"),
        "retained"
    );
    assert!(state_dir.join("state/runtime.sqlite").is_file());
    assert!(state_dir.join("state/artifacts").is_dir());
    assert!(state_dir.join("manifest.json").is_file());
    assert!(state_dir.join("observations/manifest.json").is_file());
    assert!(state_dir.join("debug-export/manifest.json").is_file());
    assert!(
        fs::read_dir(state_dir.join("debug-export/sessions"))
            .expect("exported sessions")
            .next()
            .is_some(),
        "the debug export must contain the ephemeral session"
    );
    assert!(!state_dir.join("provider.json").exists());
    assert!(!state_dir.join("credentials.json").exists());
    let manifest: Value = serde_json::from_slice(
        &fs::read(state_dir.join("manifest.json")).expect("run bundle manifest"),
    )
    .expect("valid run bundle manifest");
    assert_eq!(manifest["format"], "golutra-agent-run-bundle");
    assert_eq!(manifest["mode"], "full-owner-only");
    assert_eq!(manifest["terminal_outcome"]["kind"], "result");
    assert_eq!(manifest["raw_state"]["runtime_database"]["present"], true);
    let observations: Value = serde_json::from_slice(
        &fs::read(state_dir.join("observations/manifest.json")).expect("observation manifest"),
    )
    .expect("valid observation manifest");
    assert_eq!(observations["format"], "golutra-agent-runtime-observation");
    assert_eq!(observations["disclosure"], "full-owner-only");
    assert_eq!(observations["complete"], true);
    assert!(
        observations["files"]
            .as_array()
            .is_some_and(|files| !files.is_empty())
    );
    let session = observations["sessions"]
        .as_array()
        .and_then(|sessions| sessions.first())
        .expect("one observed session");
    let session_id = session["session_id"].as_str().expect("session id");
    let task_id = session["tasks"]
        .as_array()
        .and_then(|tasks| tasks.first())
        .and_then(|task| task["task_id"].as_str())
        .expect("observed task id");
    let observation_session = state_dir.join("observations/sessions").join(session_id);
    assert!(observation_session.join("events.jsonl").is_file());
    let conversation = fs::read_to_string(observation_session.join("conversation.jsonl"))
        .expect("full conversation history");
    assert!(conversation.contains("write file retained.txt"));
    let trace: Value = serde_json::from_slice(
        &fs::read(
            observation_session
                .join("tasks")
                .join(task_id)
                .join("trace.json"),
        )
        .expect("task trace"),
    )
    .expect("valid task trace");
    assert!(
        trace["events"]
            .as_array()
            .is_some_and(|events| !events.is_empty())
    );
    let task_created = trace["events"]
        .as_array()
        .and_then(|events| {
            events
                .iter()
                .find(|event| event["event_type"] == "task_created")
        })
        .expect("task-created event");
    assert_eq!(
        task_created["payload"]["execution_capabilities"]["network"]["requested"],
        true
    );
    assert_eq!(
        task_created["payload"]["execution_capabilities"]["network"]["enabled"],
        true
    );
    assert!(!trace["verification"].is_null());
    assert_eq!(trace["integrity"]["complete"], true);
    assert_eq!(trace["evaluation"]["terminal"], true);
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("golutra run bundle retained"),
        "retention receipt belongs on stderr"
    );
}

#[test]
#[cfg(not(unix))]
#[ignore = "invoked by the run-dir process test as an external verifier"]
fn retained_file_verifier_helper() {
    assert_eq!(
        fs::read_to_string("retained.txt").expect("retained file"),
        "retained"
    );
}

#[tokio::test]
async fn exec_network_capability_is_rejected_for_remote_runtime_ownership() {
    let workspace = tempdir().expect("workspace");
    let output = Command::new(env!("CARGO_BIN_EXE_golutra-agent"))
        .arg("--cwd")
        .arg(workspace.path())
        .arg("--connect")
        .arg("http://127.0.0.1:1")
        .arg("exec")
        .arg("--allow-network")
        .arg("reply")
        .output()
        .await
        .expect("remote network command");

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("exec --allow-network requires an embedded runtime")
    );
}

#[tokio::test]
async fn failed_exec_run_dir_still_retains_verification_observations() {
    let home = tempdir().expect("home");
    let workspace = tempdir().expect("workspace");
    let export_parent = tempdir().expect("export parent");
    let state_dir = export_parent.path().join("failed-runtime");
    install_mock_provider(home.path());
    let task_contract = write_bounded_mock_contract(home.path());

    let output = tokio::time::timeout(
        Duration::from_secs(60),
        Command::new(env!("CARGO_BIN_EXE_golutra-agent"))
            .arg("--cwd")
            .arg(workspace.path())
            .arg("exec")
            .arg("--run-dir")
            .arg(&state_dir)
            .arg("--task-contract")
            .arg(&task_contract)
            .arg("--verify-program")
            .arg("false")
            .arg("reply after a failing verifier")
            .env("GOLUTRA_AGENT_HOME", home.path())
            .output(),
    )
    .await
    .expect("failed run-dir exec timeout")
    .expect("failed run-dir exec output");

    assert!(
        !output.status.success(),
        "failing verifier unexpectedly passed"
    );
    let manifest: Value = serde_json::from_slice(
        &fs::read(state_dir.join("manifest.json")).expect("failed run bundle manifest"),
    )
    .expect("valid failed run bundle manifest");
    assert_eq!(manifest["terminal_outcome"]["kind"], "result");
    assert_eq!(manifest["terminal_outcome"]["result"]["status"], "failed");
    assert_eq!(manifest["debug_export"]["state"], "deferred");
    assert!(!state_dir.join("debug-export").exists());
    let handoff = export_parent.path().join("explicit-debug");
    let exported = Command::new(env!("CARGO_BIN_EXE_golutra-agent"))
        .arg("--run-bundle")
        .arg(&state_dir)
        .arg("export")
        .arg(&handoff)
        .env("GOLUTRA_AGENT_HOME", home.path())
        .output()
        .await
        .expect("explicit export");
    assert!(exported.status.success(), "{exported:?}");
    assert!(handoff.join("manifest.json").is_file());
    let observations: Value = serde_json::from_slice(
        &fs::read(state_dir.join("observations/manifest.json"))
            .expect("failed observation manifest"),
    )
    .expect("valid failed observation manifest");
    let session = observations["sessions"]
        .as_array()
        .and_then(|sessions| sessions.first())
        .expect("observed failed session");
    let session_id = session["session_id"].as_str().expect("session id");
    let task_id = session["tasks"]
        .as_array()
        .and_then(|tasks| tasks.first())
        .and_then(|task| task["task_id"].as_str())
        .expect("failed task id");
    let trace: Value = serde_json::from_slice(
        &fs::read(
            state_dir
                .join("observations/sessions")
                .join(session_id)
                .join("tasks")
                .join(task_id)
                .join("trace.json"),
        )
        .expect("failed task trace"),
    )
    .expect("valid failed task trace");
    assert_eq!(trace["verification"]["result"], "fail");
    assert!(
        trace["verification"]["checks"]
            .as_array()
            .is_some_and(|checks| checks
                .iter()
                .any(|check| { check["name"] == "objective:test:external_verifier" }))
    );
    assert!(state_dir.join("state/runtime.sqlite").is_file());
    assert!(String::from_utf8_lossy(&output.stderr).contains("golutra run bundle retained"));
}

#[cfg(unix)]
#[tokio::test]
async fn interrupted_exec_finalizes_its_recoverable_run_checkpoint() {
    let home = tempdir().expect("home");
    let workspace = tempdir().expect("workspace");
    let export_parent = tempdir().expect("export parent");
    let state_dir = export_parent.path().join("interrupted-runtime");
    let shell_started = workspace.path().join(".interruptible-shell-started");
    install_mock_provider(home.path());

    let mut child = Command::new(env!("CARGO_BIN_EXE_golutra-agent"))
        .arg("--cwd")
        .arg(workspace.path())
        .arg("exec")
        .arg("--run-dir")
        .arg(&state_dir)
        .arg("--approval-mode")
        .arg("auto")
        .arg("sleep before completing")
        .env("GOLUTRA_AGENT_HOME", home.path())
        .env(
            "GOLUTRA_AGENT_TEST_INTERRUPTIBLE_SHELL_MARKER",
            &shell_started,
        )
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("interruptible exec process");

    let manifest_path = state_dir.join("manifest.json");
    let mut early_exit = None;
    for _ in 0..1200 {
        if manifest_path.is_file() {
            break;
        }
        early_exit = child.try_wait().expect("poll exec process");
        if early_exit.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    if !manifest_path.is_file() {
        if early_exit.is_none() {
            child.start_kill().expect("stop exec without checkpoint");
        }
        let output = child
            .wait_with_output()
            .await
            .expect("exec output without checkpoint");
        panic!(
            "exec produced no checkpoint; status={:?}; stdout={}; stderr={}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
    let checkpoint: Value =
        serde_json::from_slice(&fs::read(&manifest_path).expect("in-progress checkpoint manifest"))
            .expect("valid checkpoint manifest");
    assert_eq!(checkpoint["terminal_outcome"]["kind"], "in_progress");

    // Wait for the real tool process to cross its start marker. This avoids
    // guessing at scheduler timing after the initial checkpoint is published.
    let mut marker_seen = false;
    for _ in 0..1200 {
        if shell_started.is_file() {
            marker_seen = true;
            break;
        }
        if child.try_wait().expect("poll interruptible exec").is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        shell_started.is_file() && marker_seen,
        "interruptible shell did not start"
    );
    let pid = child.id().expect("exec process id");
    let signal = Command::new("kill")
        .arg("-INT")
        .arg(pid.to_string())
        .status()
        .await
        .expect("send SIGINT");
    assert!(signal.success(), "SIGINT command failed: {signal:?}");

    let output = tokio::time::timeout(Duration::from_secs(60), child.wait_with_output())
        .await
        .expect("interrupted exec did not settle")
        .expect("interrupted exec output");
    assert!(
        !output.status.success(),
        "interrupted exec unexpectedly succeeded: {output:?}"
    );
    let manifest: Value =
        serde_json::from_slice(&fs::read(&manifest_path).expect("final run manifest"))
            .expect("valid final run manifest");
    assert_eq!(
        manifest["terminal_outcome"]["kind"],
        "result",
        "unexpected terminal outcome: {}; stderr={}",
        manifest["terminal_outcome"],
        String::from_utf8_lossy(&output.stderr),
    );
    assert_eq!(
        manifest["terminal_outcome"]["result"]["status"],
        "cancelled"
    );
    assert!(state_dir.join("observations/manifest.json").is_file());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("golutra runtime checkpoint retained"));
    assert!(stderr.contains("interrupt requested; waiting for runtime to settle"));
    assert!(stderr.contains("golutra run bundle retained"));
}

#[cfg(unix)]
#[tokio::test]
async fn killed_exec_checkpoint_can_be_reopened_for_recovery() {
    let home = tempdir().expect("home");
    let workspace = tempdir().expect("workspace");
    let export_parent = tempdir().expect("export parent");
    let state_dir = export_parent.path().join("killed-runtime");
    install_mock_provider(home.path());

    let mut child = Command::new(env!("CARGO_BIN_EXE_golutra-agent"))
        .arg("--cwd")
        .arg(workspace.path())
        .arg("exec")
        .arg("--run-dir")
        .arg(&state_dir)
        .arg("--approval-mode")
        .arg("auto")
        .arg("sleep before an external kill")
        .env("GOLUTRA_AGENT_HOME", home.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("killable exec process");
    let manifest_path = state_dir.join("manifest.json");
    for _ in 0..1200 {
        if manifest_path.is_file() {
            break;
        }
        if child.try_wait().expect("poll killable exec").is_some() {
            panic!("killable exec exited before writing checkpoint");
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(manifest_path.is_file(), "checkpoint was not written");

    let pid = child.id().expect("killable exec process id");
    let signal = Command::new("kill")
        .arg("-KILL")
        .arg(pid.to_string())
        .status()
        .await
        .expect("send SIGKILL");
    assert!(signal.success(), "SIGKILL command failed: {signal:?}");
    let _ = child.wait().await.expect("wait for killed exec");

    let reopened = Command::new(env!("CARGO_BIN_EXE_golutra-agent"))
        .arg("--run-bundle")
        .arg(&state_dir)
        .arg("status")
        .env("GOLUTRA_AGENT_HOME", home.path())
        .output()
        .await
        .expect("reopen persisted checkpoint");
    assert!(
        reopened.status.success(),
        "reopening killed checkpoint failed: {reopened:?}"
    );
    let status: Value = serde_json::from_slice(&reopened.stdout).expect("reopened status JSON");
    assert!(
        status.get("task_status").is_some(),
        "task status missing: {status}"
    );
}

#[tokio::test]
async fn exec_runs_caller_declared_verifier_across_the_app_server_boundary() {
    let home = tempdir().expect("home");
    let workspace = tempdir().expect("workspace");
    install_mock_provider(home.path());
    let task_contract = write_bounded_mock_contract(home.path());
    let (mut app_server, stderr) = start_app_server(home.path());
    let (info, token, _transport) = wait_for_runtime(
        home.path(),
        workspace.path(),
        &mut app_server,
        stderr.path(),
    )
    .await;

    let passed = run_cli(
        home.path(),
        workspace.path(),
        &info,
        &token,
        &[
            "exec",
            "--completion-criterion",
            "tests pass",
            "--verify-program",
            "true",
            "reply after verification",
        ],
    )
    .await;
    assert!(
        passed.status.success(),
        "passing verifier failed: {passed:?}"
    );

    let failed = run_cli(
        home.path(),
        workspace.path(),
        &info,
        &token,
        &[
            "exec",
            "--completion-criterion",
            "tests pass",
            "--verify-program",
            "false",
            "--task-contract",
            task_contract.to_str().expect("contract path"),
            "reply after verification",
        ],
    )
    .await;
    assert!(
        !failed.status.success(),
        "failing verifier unexpectedly completed: {failed:?}"
    );
    assert!(
        String::from_utf8_lossy(&failed.stderr).contains("agent turn ended with status Failed")
    );

    app_server.kill().await.expect("stop app server");
}

async fn run_cli(
    home: &Path,
    workspace: &Path,
    info: &AppServerInfo,
    token: &str,
    args: &[&str],
) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_golutra-agent"))
        .arg("--cwd")
        .arg(workspace)
        .arg("--connect")
        .arg(&info.base_url)
        .args(args)
        .env("GOLUTRA_AGENT_HOME", home)
        .env("GOLUTRA_AGENT_TRANSPORT_TOKEN", token)
        .output()
        .await
        .expect("CLI process output")
}

async fn run_daemon_cli(home: &Path, workspace: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_golutra-agent"))
        .arg("--cwd")
        .arg(workspace)
        .arg("--daemon")
        .args(args)
        .env("GOLUTRA_AGENT_HOME", home)
        .output()
        .await
        .expect("daemon CLI process output")
}

async fn wait_for_runtime(
    home: &Path,
    workspace: &Path,
    app_server: &mut tokio::process::Child,
    stderr: &Path,
) -> (AppServerInfo, String, RuntimeTransport) {
    let endpoint = home.join("app-server/app-server.json");
    let token_path = home.join("app-server/transport.token");
    let mut last_error = None;
    // App-server startup has to initialize an isolated runtime home. Under a
    // parallel process-test load that can exceed the old five-second window.
    for _ in 0..800 {
        if let Some(status) = app_server.try_wait().expect("app server process status") {
            panic!(
                "app server exited before becoming ready ({status}): {}",
                fs::read_to_string(stderr).unwrap_or_else(|error| error.to_string())
            );
        }
        let attempt = async {
            let info: AppServerInfo = serde_json::from_slice(
                &tokio::fs::read(&endpoint)
                    .await
                    .map_err(|error| error.to_string())?,
            )
            .map_err(|error| error.to_string())?;
            let token = tokio::fs::read_to_string(&token_path)
                .await
                .map_err(|error| error.to_string())?;
            let token = token.trim().to_owned();
            let transport = RuntimeTransport::connect_with_token(
                info.base_url.clone(),
                workspace,
                SecretString::from(token.clone()),
            )
            .await
            .map_err(|error| error.to_string())?;
            Ok::<_, String>((info, token, transport))
        }
        .await;
        match attempt {
            Ok(runtime) => return runtime,
            Err(error) => last_error = Some(error),
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!(
        "app server did not become ready: {}; stderr: {}",
        last_error.unwrap_or_else(|| "unknown error".to_owned()),
        fs::read_to_string(stderr).unwrap_or_else(|error| error.to_string())
    );
}

fn start_app_server(home: &Path) -> (tokio::process::Child, tempfile::NamedTempFile) {
    let stderr = tempfile::NamedTempFile::new().expect("app server stderr file");
    let child = Command::new(env!("CARGO_BIN_EXE_golutra-agent"))
        .arg("app-server")
        // Let the server keep its OS-assigned port. Reserving and releasing one
        // here races other process tests before the child binds it again.
        .arg("--addr")
        .arg("127.0.0.1:0")
        .env("GOLUTRA_AGENT_HOME", home)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(stderr.reopen().expect("app server stderr handle"))
        .kill_on_drop(true)
        .spawn()
        .expect("app server process");
    (child, stderr)
}

fn install_mock_provider(home: &Path) {
    fs::write(
        home.join("provider.json"),
        serde_json::to_vec_pretty(&json!({
            "version": 2,
            "active_profile": "mock",
            "profiles": [{
                "name": "mock",
                "protocol": "mock",
                "model_id": "mock-model",
                "enabled": true
            }]
        }))
        .expect("provider JSON"),
    )
    .expect("provider config");
}

fn write_bounded_mock_contract(home: &Path) -> std::path::PathBuf {
    // 固定 false 验收器不可能被 mock 修复；失败终态测试必须显式设置纠偏预算。
    let path = home.join("bounded-task-contract.json");
    fs::write(&path, r#"{"max_correction_rounds":1}"#).expect("bounded contract");
    path
}
