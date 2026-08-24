#![cfg(unix)]

use std::{
    ffi::OsString,
    fs,
    os::unix::fs::{FileTypeExt, PermissionsExt, symlink},
    path::Path,
    process::Stdio,
    sync::{Arc, Mutex as StdMutex},
    time::Duration,
};

use golutra_auth::{CredentialRef, SecretKind};
use golutra_client::{RuntimeClient, RuntimeExecutionOptions, RuntimeTransport};
use golutra_config::{ProviderConfigPaths, ProviderProfile, ProviderSettings};
use golutra_core::{ActorKind, QueryId, RedactionStatus, SessionId, TaskStatus, TokenUsageRecord};
use golutra_llm::{ProviderGenerationConfig, ProviderProtocol};
use golutra_protocol::{EventFilter, RuntimeQuery, RuntimeQueryKind, TuiFrame, UserProjection};
use serde_json::{Value, json};
use tempfile::tempdir;
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, Lines},
    net::{UnixStream, unix::OwnedReadHalf, unix::OwnedWriteHalf},
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::{Mutex, MutexGuard},
};

static ENV_LOCK: Mutex<()> = Mutex::const_new(());

struct HomeEnv {
    previous: Option<OsString>,
}

impl HomeEnv {
    fn set(path: &Path) -> Self {
        let previous = std::env::var_os("GOLUTRA_HOME");
        // These process tests serialize environment changes with ENV_LOCK.
        unsafe { std::env::set_var("GOLUTRA_HOME", path) };
        Self { previous }
    }
}

impl Drop for HomeEnv {
    fn drop(&mut self) {
        match self.previous.take() {
            Some(previous) => unsafe { std::env::set_var("GOLUTRA_HOME", previous) },
            None => unsafe { std::env::remove_var("GOLUTRA_HOME") },
        }
    }
}

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.start_kill();
    }
}

struct StdioDriver {
    child: ChildGuard,
    input: ChildStdin,
    output: Lines<BufReader<ChildStdout>>,
    stderr: Arc<StdMutex<String>>,
}

impl StdioDriver {
    async fn spawn(home: &Path, cwd: &Path, extra: &[&str]) -> Self {
        let mut command = tui_command(home, cwd);
        command.arg("driver").args(extra);
        Self::spawn_command(command).await
    }

    async fn spawn_with_task(home: &Path, cwd: &Path, task_id: &str, extra: &[&str]) -> Self {
        let mut command = tui_command(home, cwd);
        command
            .arg("--task-id")
            .arg(task_id)
            .arg("driver")
            .args(extra);
        Self::spawn_command(command).await
    }

    async fn spawn_command(mut command: Command) -> Self {
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command.spawn().expect("spawn stdio driver");
        let input = child.stdin.take().expect("driver stdin");
        let output = BufReader::new(child.stdout.take().expect("driver stdout")).lines();
        let stderr = Arc::new(StdMutex::new(String::new()));
        let stderr_sink = Arc::clone(&stderr);
        let mut stderr_reader = child.stderr.take().expect("driver stderr");
        tokio::spawn(async move {
            let mut text = String::new();
            let _ = stderr_reader.read_to_string(&mut text).await;
            if let Ok(mut sink) = stderr_sink.lock() {
                sink.push_str(&text);
            }
        });
        Self {
            child: ChildGuard(child),
            input,
            output,
            stderr,
        }
    }

    async fn send(&mut self, value: Value) {
        let mut bytes = serde_json::to_vec(&value).expect("request JSON");
        bytes.push(b'\n');
        self.input.write_all(&bytes).await.expect("write request");
        self.input.flush().await.expect("flush request");
    }

    async fn receive(&mut self, request_id: &str) -> Value {
        let stderr = Arc::clone(&self.stderr);
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let line = self
                    .output
                    .next_line()
                    .await
                    .expect("read driver output")
                    .unwrap_or_else(|| {
                        let stderr = stderr.lock().map(|value| value.clone()).unwrap_or_default();
                        panic!("driver stdout closed before {request_id}; stderr: {stderr}")
                    });
                let value: Value = serde_json::from_str(&line)
                    .unwrap_or_else(|error| panic!("stdout was not NDJSON: {error}: {line}"));
                if value["request_id"] == request_id {
                    return value;
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {request_id}"))
    }

    async fn receive_event_kind(&mut self, kind: &str) -> Value {
        let stderr = Arc::clone(&self.stderr);
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let line = self
                    .output
                    .next_line()
                    .await
                    .expect("read driver event")
                    .unwrap_or_else(|| {
                        let stderr = stderr.lock().map(|value| value.clone()).unwrap_or_default();
                        panic!("driver stdout closed before event {kind}; stderr: {stderr}")
                    });
                let value: Value = serde_json::from_str(&line)
                    .unwrap_or_else(|error| panic!("stdout was not NDJSON: {error}: {line}"));
                if value["type"] == "event" && value["event"]["kind"] == kind {
                    return value;
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for event {kind}"))
    }

    async fn wait_for_exit(&mut self) {
        let status = tokio::time::timeout(Duration::from_secs(10), self.child.0.wait())
            .await
            .expect("driver exit timeout")
            .expect("driver exit");
        assert!(status.success(), "driver exited with {status}");
    }
}

struct SocketConnection {
    input: OwnedWriteHalf,
    output: Lines<BufReader<OwnedReadHalf>>,
}

impl SocketConnection {
    async fn connect(path: &Path) -> Self {
        let mut last_error = None;
        for _ in 0..1200 {
            match UnixStream::connect(path).await {
                Ok(stream) => {
                    let (read, write) = stream.into_split();
                    return Self {
                        input: write,
                        output: BufReader::new(read).lines(),
                    };
                }
                Err(error) => last_error = Some(error),
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!(
            "driver socket {} did not become ready: {:?}",
            path.display(),
            last_error
        );
    }

    async fn send(&mut self, value: Value) {
        let mut bytes = serde_json::to_vec(&value).expect("socket request JSON");
        bytes.push(b'\n');
        self.input
            .write_all(&bytes)
            .await
            .expect("write socket request");
        self.input.flush().await.expect("flush socket request");
    }

    async fn receive(&mut self, request_id: &str) -> Value {
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let line = self
                    .output
                    .next_line()
                    .await
                    .expect("read socket response")
                    .unwrap_or_else(|| panic!("socket closed before {request_id}"));
                let value: Value = serde_json::from_str(&line).expect("socket NDJSON");
                if value["request_id"] == request_id {
                    return value;
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for socket response {request_id}"))
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn inspect_and_stdio_driver_execute_real_offscreen_tui() {
    let (_lock, home, workspace, _env) = process_test_context().await;

    let secret = "sk-process-secret-value";
    let output = tui_command(home.path(), workspace.path())
        .arg("inspect")
        .args([
            "--embedded",
            "--yolo",
            "--session",
            "new",
            "--prompt",
            &format!("hello Authorization: Bearer {secret}"),
            "--timeout-ms",
            "15000",
            "--width",
            "120",
            "--height",
            "30",
            "--view",
            "response+developer",
            "--format",
            "json",
        ])
        .output()
        .await
        .expect("run inspect");
    assert_command_success(&output, "inspect");
    let frame: TuiFrame = serde_json::from_slice(&output.stdout).expect("inspect frame");
    let inspect_json = String::from_utf8(output.stdout).expect("inspect UTF-8");
    assert!(frame.complete, "missing: {:?}", frame.missing_sections);
    assert_eq!(frame.redaction_status, RedactionStatus::Redacted);
    assert!(!inspect_json.contains(secret));
    assert!(inspect_json.contains("redacted-secret"));
    assert!(
        frame
            .lines
            .iter()
            .any(|line| line.text.contains('#') && line.text.contains("/Runtime"))
    );
    assert!(
        frame
            .lines
            .iter()
            .all(|line| !line.text.contains("Developer runtime"))
    );
    assert!(
        frame
            .lines
            .iter()
            .any(|line| line.text.contains("mock provider completed"))
    );

    let mut driver = StdioDriver::spawn(
        home.path(),
        workspace.path(),
        &[
            "--embedded",
            "--stdio",
            "--session",
            "new",
            "--debug",
            "--width",
            "120",
            "--height",
            "30",
            "--heartbeat-secs",
            "0",
            "--idle-timeout-secs",
            "0",
        ],
    )
    .await;
    let ready = driver.receive("ready").await;
    assert_eq!(ready["type"], "ready");
    let instance_id = ready["instance_id"]
        .as_str()
        .expect("instance id")
        .to_owned();

    driver
        .send(json!({
            "request_id": "hello",
            "type": "hello",
            "protocol_version": 1
        }))
        .await;
    let hello = driver.receive("hello").await;
    assert_eq!(hello["instance_id"], instance_id);

    let stdio_secret = "sk-stdio-secret-value";
    submit_prompt(
        &mut driver,
        "prompt-1",
        &format!("first turn Authorization: Bearer {stdio_secret}"),
    )
    .await;
    let first = wait_for(&mut driver, "wait-1", "task_terminal").await;
    let first_task = first["state"]["task_id"]
        .as_str()
        .expect("first task")
        .to_owned();

    driver
        .send(json!({
            "request_id": "paste-2",
            "type": "input_paste",
            "text": "你好 second turn"
        }))
        .await;
    assert_eq!(driver.receive("paste-2").await["type"], "accepted");
    driver
        .send(json!({
            "request_id": "enter-2",
            "type": "input_key",
            "key": "enter"
        }))
        .await;
    assert_eq!(driver.receive("enter-2").await["type"], "accepted");
    driver
        .send(json!({
            "request_id": "evaluation-2",
            "type": "wait",
            "until": {"kind": "evaluation_terminal"},
            "timeout_ms": 15000
        }))
        .await;
    let second = wait_for(&mut driver, "wait-2", "task_terminal").await;
    let second_task = second["state"]["task_id"]
        .as_str()
        .expect("second task")
        .to_owned();
    assert_eq!(second["state"]["status"], "completed");
    assert_ne!(
        second_task, first_task,
        "wait reused the previous terminal task"
    );
    let second_evaluation = driver.receive("evaluation-2").await;
    assert_eq!(
        second_evaluation["type"], "wait_result",
        "evaluation wait: {second_evaluation}"
    );
    assert_eq!(
        second_evaluation["state"]["task_id"], second_task,
        "evaluation wait reused the previous task"
    );

    driver
        .send(json!({
            "request_id": "frame-1",
            "type": "snapshot",
            "scope": "current_turn",
            "panes": "response_and_developer",
            "width": 120,
            "height": 30,
            "rows": {"start": 1, "end": 10},
            "detail": "cells"
        }))
        .await;
    let first_page = driver.receive("frame-1").await;
    assert_eq!(first_page["type"], "snapshot");
    assert_eq!(first_page["complete"], true);
    assert_eq!(first_page["redaction_status"], "redacted");
    assert!(
        first_page["cells"]
            .as_array()
            .is_some_and(|cells| { cells.iter().any(|cell| cell["symbol"] == "你") })
    );
    let frame_id = first_page["frame_id"]
        .as_str()
        .expect("frame id")
        .to_owned();
    let next_range = first_page["next_range"].clone();
    assert!(next_range.is_object());

    driver
        .send(json!({
            "request_id": "frame-2",
            "type": "snapshot",
            "scope": "current_turn",
            "panes": "response_and_developer",
            "width": 120,
            "height": 30,
            "rows": next_range,
            "frame_id": frame_id,
            "detail": "cells"
        }))
        .await;
    let second_page = driver.receive("frame-2").await;
    assert_eq!(second_page["frame_id"], first_page["frame_id"]);
    assert_eq!(
        second_page["event_high_watermark"],
        first_page["event_high_watermark"]
    );

    driver
        .send(json!({
            "request_id": "open-export-redaction",
            "type": "input_slash",
            "text": "/export"
        }))
        .await;
    assert_eq!(
        driver.receive("open-export-redaction").await["type"],
        "accepted"
    );
    driver
        .send(json!({
            "request_id": "export-transcript-frame",
            "type": "snapshot",
            "scope": "screen",
            "panes": "transcript",
            "width": 120,
            "height": 30,
            "detail": "cells"
        }))
        .await;
    let export_frame = driver.receive("export-transcript-frame").await;
    let export_json = serde_json::to_string(&export_frame).expect("export frame JSON");
    assert_eq!(export_frame["type"], "snapshot");
    assert_eq!(export_frame["redaction_status"], "redacted");
    assert!(!export_json.contains(stdio_secret));
    driver
        .send(json!({
            "request_id": "close-export-redaction",
            "type": "input_key",
            "key": "escape"
        }))
        .await;
    assert_eq!(
        driver.receive("close-export-redaction").await["type"],
        "accepted"
    );

    submit_prompt(&mut driver, "prompt-3", "sleep").await;
    let started_3 = wait_for(&mut driver, "started-3", "approval_required").await;
    let task_3 = started_3["state"]["task_id"]
        .as_str()
        .expect("third task")
        .to_owned();
    driver
        .send(json!({
            "request_id": "terminal-3",
            "type": "wait",
            "until": {"kind": "task_terminal"},
            "timeout_ms": 15000
        }))
        .await;
    driver
        .send(json!({"request_id": "ping-during-wait", "type": "ping"}))
        .await;
    let pong = tokio::time::timeout(Duration::from_secs(2), driver.receive("ping-during-wait"))
        .await
        .expect("pending wait blocked ping");
    assert_eq!(pong["type"], "pong");
    submit_prompt(
        &mut driver,
        "prompt-3-queued",
        "what happened after the pending approval",
    )
    .await;
    driver
        .send(json!({
            "request_id": "deny-during-wait",
            "type": "input_slash",
            "text": "/deny"
        }))
        .await;
    let denied = tokio::time::timeout(Duration::from_secs(2), driver.receive("deny-during-wait"))
        .await
        .expect("pending wait blocked control");
    assert_eq!(denied["type"], "accepted");
    let terminal = driver.receive("terminal-3").await;
    assert_eq!(terminal["type"], "wait_result", "terminal wait: {terminal}");
    assert_eq!(
        terminal["state"]["task_id"], task_3,
        "task wait was incorrectly constrained to the first turn"
    );

    submit_prompt(&mut driver, "prompt-4", "sleep").await;
    wait_for(&mut driver, "approval-4", "approval_required").await;
    driver
        .send(json!({
            "request_id": "terminal-4",
            "type": "wait",
            "until": {"kind": "task_terminal"},
            "timeout_ms": 15000
        }))
        .await;
    driver
        .send(json!({"request_id": "abort-during-wait", "type": "abort"}))
        .await;
    let aborted = tokio::time::timeout(Duration::from_secs(2), driver.receive("abort-during-wait"))
        .await
        .expect("pending wait blocked abort");
    assert_eq!(aborted["type"], "accepted");
    let terminal_4 = driver.receive("terminal-4").await;
    assert_eq!(
        terminal_4["type"], "wait_result",
        "abort wait: {terminal_4}"
    );

    driver
        .send(json!({
            "request_id": "resize",
            "type": "resize",
            "width": 100,
            "height": 24
        }))
        .await;
    assert_eq!(driver.receive("resize").await["type"], "accepted");
    driver
        .send(json!({
            "request_id": "old-viewport",
            "type": "snapshot",
            "scope": "screen",
            "panes": "full_screen",
            "width": 120,
            "height": 30
        }))
        .await;
    let mismatch = driver.receive("old-viewport").await;
    assert_eq!(mismatch["code"], "viewport_mismatch");

    driver
        .send(json!({
            "request_id": "mouse-invalid",
            "type": "input_mouse",
            "event": {"kind": "scroll_down", "column": 10, "row": 24}
        }))
        .await;
    assert_eq!(
        driver.receive("mouse-invalid").await["code"],
        "invalid_mouse_position"
    );
    let composer_secret = "sk-unsent-composer-secret";
    driver
        .send(json!({
            "request_id": "composer-secret",
            "type": "input_paste",
            "text": composer_secret
        }))
        .await;
    assert_eq!(driver.receive("composer-secret").await["type"], "accepted");
    driver
        .send(json!({
            "request_id": "screen-with-composer",
            "type": "snapshot",
            "scope": "screen",
            "panes": "full_screen",
            "width": 100,
            "height": 24,
            "detail": "cells"
        }))
        .await;
    let screen = driver.receive("screen-with-composer").await;
    assert_eq!(screen["width"], 100);
    assert_eq!(screen["height"], 24);
    assert_eq!(screen["redaction_status"], "redacted");
    let screen_json = serde_json::to_string(&screen).expect("screen JSON");
    assert!(!screen_json.contains(stdio_secret));
    assert!(!screen_json.contains(composer_secret));
    assert!(screen_json.contains("redacted-secret"));
    let has_debug_event = screen["lines"].as_array().is_some_and(|lines| {
        lines.iter().any(|line| {
            line["text"].as_str().is_some_and(|text| {
                text.trim_start()
                    .strip_prefix('#')
                    .and_then(|event| event.split_once(' '))
                    .is_some_and(|(sequence, event)| {
                        sequence.parse::<u64>().is_ok() && event.contains('/')
                    })
            })
        })
    });
    assert!(
        has_debug_event,
        "debug events missing from full-screen snapshot: {screen}"
    );
    assert!(
        !screen_json.contains("Developer runtime"),
        "removed developer title returned: {screen}"
    );
    assert!(
        !screen_json.contains("▸ facts") && !screen_json.contains("▾ facts"),
        "removed facts control returned: {screen}"
    );
    assert!(
        screen["hit_regions"].as_array().is_some_and(|regions| {
            regions
                .iter()
                .all(|region| region["id"] != "developer_facts_toggle")
        }),
        "removed facts hit region returned: {screen}"
    );
    driver
        .send(json!({
            "request_id": "developer-before-page",
            "type": "snapshot",
            "scope": "screen",
            "panes": "full_screen",
            "width": 100,
            "height": 24
        }))
        .await;
    let before_page = driver.receive("developer-before-page").await;
    let before_page_json = serde_json::to_string(&before_page).expect("developer JSON");
    assert!(!before_page_json.contains("▸ facts"));
    assert!(!before_page_json.contains("▾ facts"));
    driver
        .send(json!({
            "request_id": "page-developer-observations",
            "type": "input_key",
            "key": "page_up"
        }))
        .await;
    assert_eq!(
        driver.receive("page-developer-observations").await["type"],
        "accepted"
    );
    driver
        .send(json!({
            "request_id": "expanded-developer-after-page",
            "type": "snapshot",
            "scope": "screen",
            "panes": "full_screen",
            "width": 100,
            "height": 24
        }))
        .await;
    let after_page = driver.receive("expanded-developer-after-page").await;
    let before_watermark = before_page["event_high_watermark"]
        .as_u64()
        .expect("before PageUp event watermark");
    let after_watermark = after_page["event_high_watermark"]
        .as_u64()
        .expect("after PageUp event watermark");
    assert!(
        after_watermark >= before_watermark,
        "PageUp moved the debug view behind the runtime event tail"
    );
    let after_page_json = serde_json::to_string(&after_page).expect("after PageUp JSON");
    assert!(!after_page_json.contains("▸ facts"));
    assert!(!after_page_json.contains("▾ facts"));
    if after_watermark == before_watermark {
        assert_eq!(
            after_page["lines"], before_page["lines"],
            "PageUp changed the fixed debug view without any new runtime events"
        );
    }
    driver
        .send(json!({
            "request_id": "clear-composer",
            "type": "input_key",
            "key": "escape"
        }))
        .await;
    assert_eq!(driver.receive("clear-composer").await["type"], "accepted");

    for (request_id, prefix) in [("candidate-new", "/n"), ("candidate-resume", "/r")] {
        let paste_id = format!("{request_id}-paste");
        driver
            .send(json!({
                "request_id": paste_id,
                "type": "input_paste",
                "text": prefix
            }))
            .await;
        assert_eq!(driver.receive(&paste_id).await["type"], "accepted");
        driver
            .send(json!({
                "request_id": request_id,
                "type": "input_key",
                "key": "enter"
            }))
            .await;
        assert_eq!(
            driver.receive(request_id).await["code"],
            "session_binding_immutable"
        );
        let clear_id = format!("{request_id}-clear");
        driver
            .send(json!({
                "request_id": clear_id,
                "type": "input_key",
                "key": "escape"
            }))
            .await;
        assert_eq!(driver.receive(&clear_id).await["type"], "accepted");
    }

    driver
        .send(json!({
            "request_id": "slash-new",
            "type": "input_slash",
            "text": "/new"
        }))
        .await;
    assert_eq!(
        driver.receive("slash-new").await["code"],
        "session_binding_immutable"
    );
    driver
        .send(json!({
            "request_id": "type-new",
            "type": "input_paste",
            "text": "/new"
        }))
        .await;
    assert_eq!(driver.receive("type-new").await["type"], "accepted");
    driver
        .send(json!({
            "request_id": "enter-new",
            "type": "input_key",
            "key": "enter"
        }))
        .await;
    assert_eq!(
        driver.receive("enter-new").await["code"],
        "session_binding_immutable"
    );
    driver
        .send(json!({
            "request_id": "clear-new",
            "type": "input_key",
            "key": "escape"
        }))
        .await;
    assert_eq!(driver.receive("clear-new").await["type"], "accepted");

    driver
        .send(json!({
            "request_id": "invalid-slash",
            "type": "input_slash",
            "text": "/does-not-exist"
        }))
        .await;
    assert_eq!(
        driver.receive("invalid-slash").await["code"],
        "invalid_slash_command"
    );
    driver
        .send(json!({
            "request_id": "invalid-slash-paste",
            "type": "input_paste",
            "text": "/does-not-exist"
        }))
        .await;
    assert_eq!(
        driver.receive("invalid-slash-paste").await["type"],
        "accepted"
    );
    driver
        .send(json!({
            "request_id": "invalid-slash-enter",
            "type": "input_key",
            "key": "enter"
        }))
        .await;
    assert_eq!(
        driver.receive("invalid-slash-enter").await["code"],
        "invalid_slash_command"
    );
    driver
        .send(json!({
            "request_id": "invalid-slash-clear",
            "type": "input_key",
            "key": "escape"
        }))
        .await;
    assert_eq!(
        driver.receive("invalid-slash-clear").await["type"],
        "accepted"
    );

    driver
        .send(json!({
            "request_id": "fill-fork-paste",
            "type": "input_paste",
            "text": "/fork 019f79f6-c084-7210-a891-a12832a20f14"
        }))
        .await;
    assert_eq!(driver.receive("fill-fork-paste").await["type"], "accepted");
    driver
        .send(json!({
            "request_id": "fill-fork-enter",
            "type": "input_key",
            "key": "enter"
        }))
        .await;
    assert_eq!(
        driver.receive("fill-fork-enter").await["code"],
        "session_binding_immutable"
    );
    driver
        .send(json!({
            "request_id": "fill-fork-clear",
            "type": "input_key",
            "key": "escape"
        }))
        .await;
    assert_eq!(driver.receive("fill-fork-clear").await["type"], "accepted");

    driver
        .send(json!({
            "request_id": "composer-limit",
            "type": "input_paste",
            "text": "x".repeat(256 * 1024)
        }))
        .await;
    assert_eq!(driver.receive("composer-limit").await["type"], "accepted");
    driver
        .send(json!({
            "request_id": "composer-overflow",
            "type": "input_paste",
            "text": "x"
        }))
        .await;
    assert_eq!(
        driver.receive("composer-overflow").await["code"],
        "input_too_large"
    );
    driver
        .send(json!({
            "request_id": "composer-limit-clear",
            "type": "input_key",
            "key": "escape"
        }))
        .await;
    assert_eq!(
        driver.receive("composer-limit-clear").await["type"],
        "accepted"
    );

    for index in 0..=64 {
        driver
            .send(json!({
                "request_id": format!("bounded-wait-{index}"),
                "type": "wait",
                "until": {"kind": "event", "event_type": "never_happens"},
                "timeout_ms": 15000
            }))
            .await;
    }
    assert_eq!(
        driver.receive("bounded-wait-64").await["code"],
        "too_many_pending_waits"
    );
    driver
        .send(json!({"request_id": "bounded-wait-0", "type": "ping"}))
        .await;
    assert_eq!(
        driver.receive("bounded-wait-0").await["code"],
        "duplicate_request_id"
    );
    driver
        .send(json!({
            "request_id": "quit-slash",
            "type": "input_slash",
            "text": "/quit"
        }))
        .await;
    assert_eq!(driver.receive("quit-slash").await["type"], "closed");
    driver.wait_for_exit().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inspect_accepts_yolo_and_marks_the_full_screen() {
    let (_lock, home, workspace, _env) = process_test_context().await;
    let output = tui_command(home.path(), workspace.path())
        .arg("inspect")
        .args([
            "--embedded",
            "--session",
            "new",
            "--yolo",
            "--view",
            "screen",
            "--format",
            "json",
        ])
        .output()
        .await
        .expect("run yolo inspect");
    assert_command_success(&output, "yolo inspect");
    let inspect_json = String::from_utf8(output.stdout).expect("inspect UTF-8");

    assert!(inspect_json.contains("[unrestricted]"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unix_socket_is_owner_only_locked_and_reconnectable() {
    let (_lock, home, workspace, _env) = process_test_context().await;
    let socket_dir = home.path().join("tui-driver");
    fs::create_dir(&socket_dir).expect("socket dir");
    fs::set_permissions(&socket_dir, fs::Permissions::from_mode(0o700)).expect("socket dir mode");
    let socket = socket_dir.join("driver.sock");

    let mut child = spawn_socket_driver(home.path(), workspace.path(), &socket, true, "new");
    wait_for_socket(&socket).await;
    let socket_metadata = fs::symlink_metadata(&socket).expect("socket metadata");
    assert!(socket_metadata.file_type().is_socket());
    assert_eq!(socket_metadata.permissions().mode() & 0o777, 0o600);
    let lock = socket.with_file_name("driver.sock.lock");
    assert_eq!(
        fs::metadata(&lock)
            .expect("lock metadata")
            .permissions()
            .mode()
            & 0o777,
        0o600
    );

    let mut first = SocketConnection::connect(&socket).await;
    let ready = first.receive("ready").await;
    let instance_id = ready["instance_id"]
        .as_str()
        .expect("instance id")
        .to_owned();
    first
        .send(json!({"request_id": "state-1", "type": "state"}))
        .await;
    assert_eq!(first.receive("state-1").await["instance_id"], instance_id);
    drop(first);

    let mut second = SocketConnection::connect(&socket).await;
    assert_eq!(second.receive("ready").await["instance_id"], instance_id);

    let contender = tokio::time::timeout(
        Duration::from_secs(10),
        socket_driver_command(home.path(), workspace.path(), &socket, true, "new").output(),
    )
    .await
    .expect("lock contender timeout")
    .expect("lock contender");
    assert!(!contender.status.success());
    assert!(String::from_utf8_lossy(&contender.stderr).contains("socket_in_use"));

    second
        .send(json!({
            "request_id": "close",
            "type": "close",
            "abort_active_task": false
        }))
        .await;
    assert_eq!(second.receive("close").await["type"], "closed");
    let status = tokio::time::timeout(Duration::from_secs(10), child.0.wait())
        .await
        .expect("socket driver exit timeout")
        .expect("socket driver exit");
    assert!(status.success());
    assert!(!socket.exists());

    let symlink_target = socket_dir.join("target");
    fs::write(&symlink_target, b"not a socket").expect("symlink target");
    let symlink_socket = socket_dir.join("linked.sock");
    symlink(&symlink_target, &symlink_socket).expect("socket symlink");
    let linked = socket_driver_command(home.path(), workspace.path(), &symlink_socket, true, "new")
        .output()
        .await
        .expect("symlink socket driver");
    assert!(!linked.status.success());
    assert!(String::from_utf8_lossy(&linked.stderr).contains("invalid_socket"));

    let lock_symlink_socket = socket_dir.join("lock-linked.sock");
    symlink(&symlink_target, socket_dir.join("lock-linked.sock.lock")).expect("lock symlink");
    let linked_lock = socket_driver_command(
        home.path(),
        workspace.path(),
        &lock_symlink_socket,
        true,
        "new",
    )
    .output()
    .await
    .expect("symlink lock driver");
    assert!(!linked_lock.status.success());
    assert!(String::from_utf8_lossy(&linked_lock.stderr).contains("lock path"));

    let insecure_dir = home.path().join("insecure-driver");
    fs::create_dir(&insecure_dir).expect("insecure dir");
    fs::set_permissions(&insecure_dir, fs::Permissions::from_mode(0o755)).expect("insecure mode");
    let insecure = socket_driver_command(
        home.path(),
        workspace.path(),
        &insecure_dir.join("driver.sock"),
        true,
        "new",
    )
    .output()
    .await
    .expect("insecure socket driver");
    assert!(!insecure.status.success());
    assert!(String::from_utf8_lossy(&insecure.stderr).contains("owner-only"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn driver_metrics_are_redacted_and_survive_socket_reconnect() {
    let (_lock, home, workspace, _env) = process_test_context().await;
    let socket_dir = home.path().join("tui-driver-metrics");
    fs::create_dir(&socket_dir).expect("metrics socket dir");
    fs::set_permissions(&socket_dir, fs::Permissions::from_mode(0o700))
        .expect("metrics socket dir mode");
    let socket = socket_dir.join("driver.sock");
    let mut child = spawn_socket_driver(home.path(), workspace.path(), &socket, true, "new");
    wait_for_socket(&socket).await;

    let mut first = SocketConnection::connect(&socket).await;
    assert_eq!(first.receive("ready").await["type"], "ready");
    first
        .send(json!({"request_id": "metrics-1", "type": "metrics"}))
        .await;
    let initial = first.receive("metrics-1").await;
    assert_eq!(initial["type"], "metrics");
    assert_eq!(initial["metrics"]["connections"], 1);
    assert_eq!(initial["metrics"]["reconnects"], 0);

    first
        .send(json!({"request_id": "capabilities", "type": "capabilities"}))
        .await;
    let capabilities = first.receive("capabilities").await;
    assert!(
        capabilities["capabilities"]
            .as_array()
            .is_some_and(|values| values.iter().any(|value| value == "diagnostics.metrics"))
    );

    first
        .send(json!({
            "request_id": "metrics-prompt",
            "type": "input_prompt",
            "text": "metrics secret prompt"
        }))
        .await;
    assert_eq!(first.receive("metrics-prompt").await["type"], "accepted");
    first
        .send(json!({
            "request_id": "metrics-terminal",
            "type": "wait",
            "until": {"kind": "task_terminal"},
            "timeout_ms": 15000
        }))
        .await;
    assert_eq!(
        first.receive("metrics-terminal").await["type"],
        "wait_result"
    );
    first
        .send(json!({
            "request_id": "metrics-timeout",
            "type": "wait",
            "until": {"kind": "event", "event_type": "never_happens"},
            "timeout_ms": 0
        }))
        .await;
    assert_eq!(
        first.receive("metrics-timeout").await["type"],
        "wait_timeout"
    );
    first
        .send(json!({
            "request_id": "metrics-error",
            "type": "input_slash",
            "text": "/does-not-exist"
        }))
        .await;
    assert_eq!(first.receive("metrics-error").await["type"], "error");

    first
        .send(json!({
            "request_id": "snapshot-1",
            "type": "snapshot",
            "scope": "session",
            "panes": "transcript",
            "width": 160,
            "height": 40
        }))
        .await;
    let snapshot = first.receive("snapshot-1").await;
    assert_eq!(snapshot["type"], "snapshot");
    let frame_id = snapshot["frame_id"].as_str().expect("frame id");
    first
        .send(json!({
            "request_id": "snapshot-frozen",
            "type": "snapshot",
            "scope": "session",
            "panes": "transcript",
            "width": 160,
            "height": 40,
            "frame_id": frame_id
        }))
        .await;
    assert_eq!(first.receive("snapshot-frozen").await["type"], "snapshot");
    first
        .send(json!({
            "request_id": "snapshot-miss",
            "type": "snapshot",
            "scope": "session",
            "panes": "transcript",
            "width": 160,
            "height": 40,
            "frame_id": "missing-frame"
        }))
        .await;
    assert_eq!(
        first.receive("snapshot-miss").await["code"],
        "frame_expired"
    );
    first
        .send(json!({
            "request_id": "pending-wait",
            "type": "wait",
            "until": {"kind": "event", "event_type": "never_happens"},
            "timeout_ms": 15000
        }))
        .await;
    first
        .send(json!({"request_id": "metrics-pending", "type": "metrics"}))
        .await;
    let pending_metrics = first.receive("metrics-pending").await;
    assert_eq!(pending_metrics["metrics"]["pending_waits"], 1);
    drop(first);

    let mut second = SocketConnection::connect(&socket).await;
    assert_eq!(second.receive("ready").await["type"], "ready");
    second
        .send(json!({"request_id": "metrics-2", "type": "metrics"}))
        .await;
    let metrics = second.receive("metrics-2").await;
    assert_eq!(metrics["metrics"]["connections"], 2);
    assert_eq!(metrics["metrics"]["reconnects"], 1);
    assert_eq!(metrics["metrics"]["snapshot_requests"], 3);
    assert_eq!(metrics["metrics"]["snapshot_renders"], 1);
    assert_eq!(metrics["metrics"]["frozen_frame_hits"], 1);
    assert_eq!(metrics["metrics"]["frozen_frame_misses"], 1);
    assert_eq!(metrics["metrics"]["frame_cache_entries"], 1);
    assert_eq!(metrics["metrics"]["wait_requests"], 3);
    assert_eq!(metrics["metrics"]["wait_results"], 1);
    assert_eq!(metrics["metrics"]["wait_timeouts"], 1);
    assert_eq!(metrics["metrics"]["wait_cancelled"], 1);
    assert_eq!(metrics["metrics"]["pending_waits"], 0);
    assert!(
        metrics["metrics"]["request_errors"]
            .as_u64()
            .is_some_and(|errors| errors >= 1)
    );
    assert!(
        metrics["metrics"]["snapshot_latency"]["samples"]
            .as_u64()
            .is_some_and(|samples| samples >= 3)
    );
    assert!(
        metrics["metrics"]["sync_latency"]["samples"]
            .as_u64()
            .is_some_and(|samples| samples >= 1)
    );
    assert!(
        metrics["metrics"]["render"]["redraws"]
            .as_u64()
            .is_some_and(|redraws| redraws >= 1)
    );
    assert!(
        metrics["metrics"]["render"]["delta_events"]
            .as_u64()
            .is_some_and(|deltas| deltas >= 1)
    );
    assert!(
        metrics["metrics"]["render"]["first_token_latency"]["samples"]
            .as_u64()
            .is_some_and(|samples| samples >= 1)
    );
    let metrics_json = serde_json::to_string(&metrics).expect("metrics JSON");
    assert!(!metrics_json.contains("workspace_path"));
    assert!(!metrics_json.contains("prompt"));
    assert!(!metrics_json.contains("secret"));

    close_socket_driver(&mut second, &mut child, "close-metrics").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stdio_driver_emits_heartbeat_and_honors_idle_timeout() {
    let (_lock, home, workspace, _env) = process_test_context().await;
    let mut driver = StdioDriver::spawn(
        home.path(),
        workspace.path(),
        &[
            "--embedded",
            "--stdio",
            "--session",
            "new",
            "--heartbeat-secs",
            "1",
            "--idle-timeout-secs",
            "2",
        ],
    )
    .await;
    assert_eq!(driver.receive("ready").await["type"], "ready");
    driver
        .send(json!({
            "request_id": "pending-during-heartbeat",
            "type": "wait",
            "until": {"kind": "event", "event_type": "never_happens"},
            "timeout_ms": 10000
        }))
        .await;
    let heartbeat = driver.receive_event_kind("heartbeat").await;
    assert_eq!(heartbeat["type"], "event");
    assert_eq!(driver.receive("idle-timeout").await["type"], "closed");
    driver.wait_for_exit().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn direct_quit_closes_an_initial_provider_setup_modal() {
    let lock = ENV_LOCK.lock().await;
    let home = tempdir().expect("home");
    let workspace = tempdir().expect("workspace");
    fs::set_permissions(home.path(), fs::Permissions::from_mode(0o700)).expect("home mode");
    let env = HomeEnv::set(home.path());
    let mut driver = StdioDriver::spawn(
        home.path(),
        workspace.path(),
        &[
            "--embedded",
            "--stdio",
            "--session",
            "new",
            "--heartbeat-secs",
            "0",
            "--idle-timeout-secs",
            "0",
        ],
    )
    .await;
    assert_eq!(driver.receive("ready").await["type"], "ready");
    driver
        .send(json!({
            "request_id": "mutate-provider-setup",
            "type": "input_slash",
            "text": "/auth mock"
        }))
        .await;
    assert_eq!(
        driver.receive("mutate-provider-setup").await["code"],
        "ui_modal_active"
    );
    driver
        .send(json!({
            "request_id": "quit-provider-setup",
            "type": "input_slash",
            "text": "/quit"
        }))
        .await;
    assert_eq!(
        driver.receive("quit-provider-setup").await["type"],
        "closed"
    );
    driver.wait_for_exit().await;
    drop(env);
    drop(lock);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires explicit GOLUTRA_TUI_DRIVER_LIVE provider credentials"]
async fn live_provider_driver_smoke_is_isolated_and_opt_in() {
    const ENABLE_ENV: &str = "GOLUTRA_TUI_DRIVER_LIVE";
    const API_KEY_ENV: &str = "GOLUTRA_TUI_DRIVER_LIVE_API_KEY";
    const BASE_URL_ENV: &str = "GOLUTRA_TUI_DRIVER_LIVE_BASE_URL";
    const MODEL_ENV: &str = "GOLUTRA_TUI_DRIVER_LIVE_MODEL";
    const PROTOCOL_ENV: &str = "GOLUTRA_TUI_DRIVER_LIVE_PROTOCOL";
    const MAX_CALLS_ENV: &str = "GOLUTRA_TUI_DRIVER_LIVE_MAX_CALLS";
    const MAX_OUTPUT_TOKENS_ENV: &str = "GOLUTRA_TUI_DRIVER_LIVE_MAX_OUTPUT_TOKENS";
    const EXPECTED_RESPONSE: &str = "GOLUTRA_DRIVER_LIVE_OK";
    const EXPECTED_CJK: &str = "中文验收通过";
    const SYNTHETIC_SECRET: &str = "sk-golutra-live-redaction-marker-1234567890";

    if std::env::var(ENABLE_ENV).as_deref() != Ok("1") {
        eprintln!("skipping live TUI Driver smoke: set {ENABLE_ENV}=1 to opt in");
        return;
    }
    let api_key = std::env::var(API_KEY_ENV)
        .unwrap_or_else(|_| panic!("{API_KEY_ENV} must be set when live smoke is enabled"));
    let base_url = std::env::var(BASE_URL_ENV)
        .unwrap_or_else(|_| panic!("{BASE_URL_ENV} must be set when live smoke is enabled"));
    let model = std::env::var(MODEL_ENV)
        .unwrap_or_else(|_| panic!("{MODEL_ENV} must be set when live smoke is enabled"));
    let max_calls = std::env::var(MAX_CALLS_ENV)
        .unwrap_or_else(|_| panic!("{MAX_CALLS_ENV} must be set when live smoke is enabled"))
        .parse::<u32>()
        .unwrap_or_else(|_| panic!("{MAX_CALLS_ENV} must be an integer"));
    let max_output_tokens = std::env::var(MAX_OUTPUT_TOKENS_ENV)
        .unwrap_or_else(|_| {
            panic!("{MAX_OUTPUT_TOKENS_ENV} must be set when live smoke is enabled")
        })
        .parse::<u64>()
        .unwrap_or_else(|_| panic!("{MAX_OUTPUT_TOKENS_ENV} must be an integer"));
    assert!(
        (1..=2).contains(&max_calls),
        "{MAX_CALLS_ENV} must be 1 or 2"
    );
    assert!(
        (1..=512).contains(&max_output_tokens),
        "{MAX_OUTPUT_TOKENS_ENV} must be between 1 and 512"
    );
    assert!(
        api_key.trim().len() >= 16,
        "{API_KEY_ENV} must contain a real dedicated test credential"
    );
    assert!(!base_url.trim().is_empty(), "{BASE_URL_ENV} is empty");
    assert!(!model.trim().is_empty(), "{MODEL_ENV} is empty");
    let protocol_value = std::env::var(PROTOCOL_ENV)
        .unwrap_or_else(|_| ProviderProtocol::OpenAiCompatible.id().to_owned());
    let protocol = ProviderProtocol::from_config_value(&protocol_value).unwrap_or_else(|| {
        panic!("{PROTOCOL_ENV} contains unsupported protocol `{protocol_value}`")
    });
    assert_ne!(
        protocol,
        ProviderProtocol::Mock,
        "{PROTOCOL_ENV} must be live"
    );

    let home = tempdir().expect("isolated live home");
    let workspace = tempdir().expect("isolated live workspace");
    fs::set_permissions(home.path(), fs::Permissions::from_mode(0o700)).expect("live home mode");
    let credential =
        CredentialRef::environment(API_KEY_ENV, SecretKind::ApiKey).expect("live credential ref");
    let mut profile =
        ProviderProfile::live_profile("tui-driver-live", protocol, base_url, model, credential)
            .expect("live provider profile");
    profile.generation_config = Some(ProviderGenerationConfig {
        max_tokens: Some(max_output_tokens),
        ..ProviderGenerationConfig::default()
    });
    let mut settings = ProviderSettings::default();
    settings.upsert_profile(profile, true);
    let paths = ProviderConfigPaths::from_home(home.path()).expect("live provider paths");
    settings
        .save(&paths.user_config)
        .expect("write isolated provider profile");

    let mut command = tui_command(home.path(), workspace.path());
    command
        .env(API_KEY_ENV, &api_key)
        .arg("--debug")
        .arg("driver")
        .args([
            "--embedded",
            "--stdio",
            "--session",
            "new",
            "--width",
            "160",
            "--height",
            "40",
            "--heartbeat-secs",
            "0",
            "--idle-timeout-secs",
            "0",
        ]);
    let mut driver = StdioDriver::spawn_command(command).await;
    let ready = driver.receive("ready").await;
    assert_eq!(ready["type"], "ready");
    let session_id = ready["session_id"]
        .as_str()
        .expect("live session id")
        .parse::<SessionId>()
        .expect("valid live session id");
    driver
        .send(json!({
            "request_id": "live-prompt",
            "type": "input_prompt",
            "text": format!(
                "Reply with exactly {EXPECTED_RESPONSE} {EXPECTED_CJK}. Do not repeat this synthetic credential: Authorization: Bearer {SYNTHETIC_SECRET}"
            )
        }))
        .await;
    assert_eq!(driver.receive("live-prompt").await["type"], "accepted");
    driver
        .send(json!({
            "request_id": "live-evaluation",
            "type": "wait",
            "until": {"kind": "evaluation_terminal"},
            "timeout_ms": 180000
        }))
        .await;
    let terminal = driver.receive("live-evaluation").await;
    assert_eq!(
        terminal["type"], "wait_result",
        "live provider did not reach evaluation terminal: {terminal}"
    );
    assert!(
        matches!(
            terminal["state"]["status"].as_str(),
            Some("completed" | "partial")
        ),
        "unexpected live terminal state: {terminal}"
    );

    for (request_id, kind) in [
        ("live-task-terminal", "task_terminal"),
        ("live-turn-terminal", "turn_terminal"),
    ] {
        driver
            .send(json!({
                "request_id": request_id,
                "type": "wait",
                "until": {"kind": kind},
                "timeout_ms": 1000
            }))
            .await;
        let terminal = driver.receive(request_id).await;
        assert_eq!(
            terminal["type"], "wait_result",
            "live provider did not reach {kind}: {terminal}"
        );
    }

    driver
        .send(json!({
            "request_id": "live-frame",
            "type": "snapshot",
            "scope": "current_turn",
            "panes": "response_and_developer",
            "width": 160,
            "height": 40,
            "detail": "text"
        }))
        .await;
    let frame = driver.receive("live-frame").await;
    assert_eq!(frame["type"], "snapshot");
    assert_eq!(frame["complete"], true, "incomplete live frame: {frame}");
    let frame_json = serde_json::to_string(&frame).expect("live frame JSON");
    assert!(
        frame_json.contains(EXPECTED_RESPONSE),
        "provider reply missing: {frame}"
    );
    assert!(
        frame_json.contains(EXPECTED_CJK),
        "CJK reply missing: {frame}"
    );
    assert!(
        frame_json.contains("ProviderStreamed"),
        "stream delta event missing: {frame}"
    );
    assert!(
        frame_json.contains("token_records="),
        "normalized usage projection missing: {frame}"
    );
    assert!(
        frame_json.contains("/Runtime"),
        "debug events missing from response+developer frame: {frame}"
    );
    assert!(
        !frame_json.contains("Developer runtime"),
        "removed developer title returned: {frame}"
    );
    assert!(
        !frame_json.contains(&api_key),
        "live API key leaked into frame"
    );
    assert!(
        !frame_json.contains(SYNTHETIC_SECRET),
        "synthetic credential leaked into frame"
    );
    assert!(
        frame_json.contains("redacted-secret"),
        "synthetic credential was not visibly redacted"
    );

    driver
        .send(json!({"request_id": "live-metrics", "type": "metrics"}))
        .await;
    let metrics = driver.receive("live-metrics").await;
    assert_eq!(metrics["type"], "metrics");
    for path in [
        ["delta_events", ""],
        ["first_deltas", ""],
        ["first_token_latency", "samples"],
        ["final_frame_latency", "samples"],
    ] {
        let value = if path[1].is_empty() {
            &metrics["metrics"]["render"][path[0]]
        } else {
            &metrics["metrics"]["render"][path[0]][path[1]]
        };
        assert!(
            value.as_u64().is_some_and(|count| count >= 1),
            "render metric {}.{} missing: {metrics}",
            path[0],
            path[1]
        );
    }

    let transport = RuntimeTransport::for_cwd_with_options(
        workspace.path(),
        RuntimeExecutionOptions::default(),
    )
    .await
    .expect("open live event transport");
    let events = transport
        .replay_events(EventFilter {
            session_id,
            task_id: None,
            after_sequence_no: None,
        })
        .await
        .expect("read live events");
    let provider_started_count = events
        .iter()
        .filter(|event| event.get("event_type").and_then(Value::as_str) == Some("provider_started"))
        .count();
    assert!(
        provider_started_count >= 1,
        "live provider did not start: {events:?}"
    );
    assert!(
        u32::try_from(provider_started_count).unwrap_or(u32::MAX) <= max_calls,
        "live provider call budget exceeded: started={provider_started_count}, max={max_calls}"
    );
    let usage = events
        .iter()
        .find(|event| {
            event.get("event_type").and_then(Value::as_str) == Some("token_usage_recorded")
        })
        .and_then(|event| event.get("payload"))
        .and_then(|payload| payload.get("record"))
        .cloned()
        .map(|value| serde_json::from_value::<TokenUsageRecord>(value).expect("usage record"))
        .expect("live provider usage record");
    assert!(usage.usage().input_tokens_total.is_some());
    assert!(usage.usage().output_tokens.is_some());
    assert!(usage.usage().usage_complete);
    assert!(
        serde_json::to_value(&usage)
            .expect("usage JSON")
            .get("cache_read_tokens")
            .is_some()
    );
    transport.close().await.expect("close live event transport");

    driver
        .send(json!({
            "request_id": "live-close",
            "type": "close",
            "abort_active_task": false
        }))
        .await;
    assert_eq!(driver.receive("live-close").await["type"], "closed");
    driver.wait_for_exit().await;
    let stderr = driver
        .stderr
        .lock()
        .map(|value| value.clone())
        .unwrap_or_default();
    assert!(
        !stderr.contains(&api_key),
        "live API key leaked into stderr"
    );
    assert!(
        !home.path().join("credentials.json").exists(),
        "live smoke copied its environment credential to disk"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn daemon_driver_enforces_binding_and_survives_disconnect_and_restart() {
    let (_lock, home, workspace, _env) = process_test_context().await;
    let other_workspace = tempdir().expect("other workspace");
    let mut daemon = start_daemon();
    let transport = wait_for_daemon(workspace.path()).await;

    let socket_dir = home.path().join("daemon-drivers");
    fs::create_dir(&socket_dir).expect("daemon socket dir");
    fs::set_permissions(&socket_dir, fs::Permissions::from_mode(0o700))
        .expect("daemon socket mode");
    let session_id = SessionId::new();
    let session_spec = format!("new:{session_id}");
    let socket_a = socket_dir.join("a.sock");
    let mut child_a = spawn_socket_driver(
        home.path(),
        workspace.path(),
        &socket_a,
        false,
        &session_spec,
    );
    let mut driver_a = SocketConnection::connect(&socket_a).await;
    let ready_a = driver_a.receive("ready").await;
    assert_eq!(ready_a["session_id"], session_id.to_string());
    assert_eq!(ready_a["controller_mode"], "controller");

    driver_a
        .send(json!({
            "request_id": "sleep",
            "type": "input_prompt",
            "text": "sleep"
        }))
        .await;
    let sleep = driver_a.receive("sleep").await;
    assert_eq!(sleep["type"], "accepted", "initial prompt: {sleep}");
    driver_a
        .send(json!({
            "request_id": "started",
            "type": "wait",
            "until": {"kind": "task_started"},
            "timeout_ms": 5000
        }))
        .await;
    let started = driver_a.receive("started").await;
    assert_eq!(started["type"], "wait_result");
    let task_id = started["state"]["task_id"]
        .as_str()
        .expect("daemon task id")
        .to_owned();
    driver_a
        .send(json!({
            "request_id": "approval-before-takeover",
            "type": "wait",
            "until": {"kind": "approval_required"},
            "timeout_ms": 15000
        }))
        .await;
    let approval = driver_a.receive("approval-before-takeover").await;
    if approval["type"] != "wait_result" {
        let projection = transport
            .query(RuntimeQuery {
                query_id: QueryId::new(),
                session_id,
                task_id: None,
                kind: RuntimeQueryKind::UserProjection,
                requester: ActorKind::Sdk,
                cursor: None,
                timestamp: chrono::Utc::now(),
            })
            .await
            .expect("query approval projection");
        let events = transport
            .replay_events(EventFilter {
                session_id,
                task_id: None,
                after_sequence_no: None,
            })
            .await
            .expect("query approval events")
            .into_iter()
            .filter_map(|event| {
                event
                    .get("event_type")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .collect::<Vec<_>>();
        panic!("approval wait failed: {approval}; projection: {projection}; events: {events:?}");
    }
    assert_eq!(approval["type"], "wait_result", "approval wait: {approval}");

    let waiting_inspect = inspect_command(home.path(), workspace.path(), &session_id.to_string())
        .args([
            "--wait",
            "approval-required",
            "--timeout-ms",
            "5000",
            "--view",
            "task",
        ])
        .output()
        .await
        .expect("wait-only inspect");
    assert_command_success(&waiting_inspect, "wait-only inspect");

    let socket_b = socket_dir.join("b.sock");
    let mut child_b = spawn_socket_driver(
        home.path(),
        workspace.path(),
        &socket_b,
        false,
        &session_id.to_string(),
    );
    let mut driver_b = SocketConnection::connect(&socket_b).await;
    assert_eq!(
        driver_b.receive("ready").await["controller_mode"],
        "observer"
    );
    driver_b
        .send(json!({
            "request_id": "observer-prompt",
            "type": "input_prompt",
            "text": "observer must not append"
        }))
        .await;
    assert_eq!(
        driver_b.receive("observer-prompt").await["code"],
        "command_rejected"
    );
    driver_b
        .send(json!({"request_id": "takeover", "type": "takeover"}))
        .await;
    assert_eq!(driver_b.receive("takeover").await["type"], "accepted");
    driver_b
        .send(json!({"request_id": "state-b", "type": "state"}))
        .await;
    assert_eq!(
        driver_b.receive("state-b").await["controller_mode"],
        "controller"
    );
    driver_a
        .send(json!({"request_id": "observer-abort", "type": "abort"}))
        .await;
    assert_eq!(
        driver_a.receive("observer-abort").await["code"],
        "command_rejected"
    );
    driver_a
        .send(json!({
            "request_id": "observer-slash-abort",
            "type": "input_slash",
            "text": "/abort"
        }))
        .await;
    assert_eq!(
        driver_a.receive("observer-slash-abort").await["code"],
        "command_rejected"
    );
    driver_a
        .send(json!({
            "request_id": "observer-close-abort",
            "type": "close",
            "abort_active_task": true
        }))
        .await;
    assert_eq!(
        driver_a.receive("observer-close-abort").await["code"],
        "command_rejected"
    );
    driver_a
        .send(json!({"request_id": "observer-still-open", "type": "state"}))
        .await;
    assert_eq!(
        driver_a.receive("observer-still-open").await["type"],
        "state"
    );
    driver_b
        .send(json!({
            "request_id": "approve",
            "type": "input_slash",
            "text": "/approve"
        }))
        .await;
    assert_eq!(driver_b.receive("approve").await["type"], "accepted");

    close_socket_driver(&mut driver_b, &mut child_b, "close-b").await;
    close_socket_driver(&mut driver_a, &mut child_a, "close-a").await;
    let terminal = wait_for_terminal_projection(&transport, session_id).await;
    assert!(
        matches!(terminal.status, TaskStatus::Completed | TaskStatus::Partial),
        "unexpected terminal projection after approval: {terminal:?}"
    );
    let terminal_status = terminal.status;
    assert_eq!(
        terminal.task_id.map(|id| id.to_string()),
        Some(task_id.clone())
    );

    fs::remove_file(home.path().join("provider.json")).expect("remove provider config");
    let mut task_bound = StdioDriver::spawn_with_task(
        home.path(),
        workspace.path(),
        &task_id,
        &[
            "--stdio",
            "--session",
            &session_id.to_string(),
            "--heartbeat-secs",
            "0",
            "--idle-timeout-secs",
            "0",
        ],
    )
    .await;
    assert_eq!(task_bound.receive("ready").await["type"], "ready");
    task_bound
        .send(json!({
            "request_id": "task-bound-screen",
            "type": "snapshot",
            "scope": "screen",
            "panes": "full_screen",
            "width": 160,
            "height": 40
        }))
        .await;
    let task_bound_screen = task_bound.receive("task-bound-screen").await;
    assert_eq!(task_bound_screen["type"], "snapshot");
    assert!(
        !serde_json::to_string(&task_bound_screen)
            .expect("task-bound screen JSON")
            .contains("Provider setup"),
        "task-bound Driver opened a mutable provider setup flow"
    );
    task_bound
        .send(json!({"request_id": "task-bound-state", "type": "state"}))
        .await;
    assert_eq!(
        task_bound.receive("task-bound-state").await["task_id"],
        task_id
    );
    task_bound
        .send(json!({
            "request_id": "task-bound-prompt",
            "type": "input_prompt",
            "text": "must remain read only"
        }))
        .await;
    assert_eq!(
        task_bound.receive("task-bound-prompt").await["code"],
        "task_binding_read_only"
    );
    task_bound
        .send(json!({
            "request_id": "task-bound-auth-paste",
            "type": "input_paste",
            "text": "/auth logout mock"
        }))
        .await;
    assert_eq!(
        task_bound.receive("task-bound-auth-paste").await["type"],
        "accepted"
    );
    task_bound
        .send(json!({
            "request_id": "task-bound-auth-enter",
            "type": "input_key",
            "key": "enter"
        }))
        .await;
    assert_eq!(
        task_bound.receive("task-bound-auth-enter").await["code"],
        "task_binding_read_only"
    );
    task_bound
        .send(json!({
            "request_id": "task-bound-auth-clear",
            "type": "input_key",
            "key": "escape"
        }))
        .await;
    assert_eq!(
        task_bound.receive("task-bound-auth-clear").await["type"],
        "accepted"
    );
    for (request_id, request) in [
        (
            "task-bound-takeover",
            json!({"request_id": "task-bound-takeover", "type": "takeover"}),
        ),
        (
            "task-bound-abort",
            json!({"request_id": "task-bound-abort", "type": "abort"}),
        ),
        (
            "task-bound-slash-abort",
            json!({
                "request_id": "task-bound-slash-abort",
                "type": "input_slash",
                "text": "/abort"
            }),
        ),
        (
            "task-bound-ctrl-c",
            json!({
                "request_id": "task-bound-ctrl-c",
                "type": "input_key",
                "key": "ctrl_c"
            }),
        ),
        (
            "task-bound-close-abort",
            json!({
                "request_id": "task-bound-close-abort",
                "type": "close",
                "abort_active_task": true
            }),
        ),
    ] {
        task_bound.send(request).await;
        assert_eq!(
            task_bound.receive(request_id).await["code"],
            "task_binding_read_only",
            "request {request_id} mutated a task-bound Driver"
        );
    }
    task_bound
        .send(json!({"request_id": "task-bound-open", "type": "state"}))
        .await;
    assert_eq!(task_bound.receive("task-bound-open").await["type"], "state");
    task_bound
        .send(json!({
            "request_id": "close-task-bound",
            "type": "close",
            "abort_active_task": false
        }))
        .await;
    assert_eq!(
        task_bound.receive("close-task-bound").await["type"],
        "closed"
    );
    task_bound.wait_for_exit().await;
    install_mock_provider(home.path());

    let duplicate = tui_command(home.path(), workspace.path())
        .arg("inspect")
        .args(["--session", &session_spec, "--view", "session"])
        .output()
        .await
        .expect("duplicate explicit session");
    assert!(!duplicate.status.success());
    assert!(String::from_utf8_lossy(&duplicate.stderr).contains("session_exists"));

    let existing = inspect_command(home.path(), workspace.path(), &session_id.to_string())
        .args(["--view", "task"])
        .output()
        .await
        .expect("existing session inspect");
    assert_command_success(&existing, "existing session inspect");
    let existing_frame: TuiFrame =
        serde_json::from_slice(&existing.stdout).expect("existing frame");
    assert_eq!(existing_frame.session_id, session_id.to_string());
    assert_eq!(existing_frame.task_id.as_deref(), Some(task_id.as_str()));

    let current = inspect_command(home.path(), workspace.path(), "current")
        .args(["--view", "session"])
        .output()
        .await
        .expect("current session inspect");
    assert_command_success(&current, "current session inspect");
    let current_frame: TuiFrame = serde_json::from_slice(&current.stdout).expect("current frame");
    assert_eq!(current_frame.session_id, session_id.to_string());

    let second = inspect_command(home.path(), workspace.path(), "new")
        .args([
            "--prompt",
            "second session",
            "--wait",
            "task-terminal",
            "--timeout-ms",
            "10000",
            "--view",
            "response",
        ])
        .output()
        .await
        .expect("second session inspect");
    assert_command_success(&second, "second session inspect");
    let second_frame: TuiFrame = serde_json::from_slice(&second.stdout).expect("second frame");
    let second_task = second_frame.task_id.expect("second task");

    let mismatch = tui_command(home.path(), workspace.path())
        .arg("--task-id")
        .arg(&second_task)
        .arg("inspect")
        .args(["--session", &session_id.to_string(), "--view", "task"])
        .output()
        .await
        .expect("task mismatch inspect");
    assert!(!mismatch.status.success());
    assert!(String::from_utf8_lossy(&mismatch.stderr).contains("task_not_found"));

    let workspace_mismatch =
        inspect_command(home.path(), other_workspace.path(), &session_id.to_string())
            .args(["--view", "session"])
            .output()
            .await
            .expect("workspace mismatch inspect");
    assert!(!workspace_mismatch.status.success());
    assert!(String::from_utf8_lossy(&workspace_mismatch.stderr).contains("workspace"));

    let socket_c = socket_dir.join("c.sock");
    let mut child_c = spawn_socket_driver(
        home.path(),
        workspace.path(),
        &socket_c,
        false,
        &session_id.to_string(),
    );
    let mut before_restart = SocketConnection::connect(&socket_c).await;
    let ready_before = before_restart.receive("ready").await;
    let instance_id = ready_before["instance_id"]
        .as_str()
        .expect("restart instance")
        .to_owned();
    before_restart
        .send(json!({
            "request_id": "frame-before-restart",
            "type": "snapshot",
            "scope": "session",
            "panes": "transcript",
            "width": 160,
            "height": 40
        }))
        .await;
    let frame_before_restart = before_restart.receive("frame-before-restart").await;
    let frozen_frame_id = frame_before_restart["frame_id"]
        .as_str()
        .expect("frozen frame id")
        .to_owned();
    drop(before_restart);

    stop_daemon(&mut daemon, home.path()).await;
    let mut during_outage = SocketConnection::connect(&socket_c).await;
    let outage_ready = during_outage.receive("ready").await;
    assert_eq!(outage_ready["instance_id"], instance_id);
    during_outage
        .send(json!({
            "request_id": "state-during-outage",
            "type": "state"
        }))
        .await;
    let outage_state = during_outage.receive("state-during-outage").await;
    assert_eq!(outage_state["type"], "state");
    assert_eq!(outage_state["instance_id"], instance_id);
    during_outage
        .send(json!({
            "request_id": "frame-during-outage",
            "type": "snapshot",
            "scope": "session",
            "panes": "transcript",
            "width": 160,
            "height": 40,
            "frame_id": frozen_frame_id
        }))
        .await;
    let frame_during_outage = during_outage.receive("frame-during-outage").await;
    assert_eq!(frame_during_outage["type"], "snapshot");
    assert_eq!(
        frame_during_outage["frame_id"],
        frame_before_restart["frame_id"]
    );
    drop(during_outage);

    daemon = start_daemon();
    let restarted_transport = wait_for_daemon(workspace.path()).await;
    assert_eq!(
        wait_for_terminal_projection(&restarted_transport, session_id)
            .await
            .status,
        terminal_status
    );

    let mut after_restart = SocketConnection::connect(&socket_c).await;
    let ready_after = after_restart.receive("ready").await;
    assert_eq!(ready_after["instance_id"], instance_id);
    after_restart
        .send(json!({"request_id": "state-after", "type": "state"}))
        .await;
    assert_eq!(after_restart.receive("state-after").await["type"], "state");
    after_restart
        .send(json!({
            "request_id": "prompt-after",
            "type": "input_prompt",
            "text": "after daemon restart"
        }))
        .await;
    let prompt_after = after_restart.receive("prompt-after").await;
    assert_eq!(
        prompt_after["type"], "accepted",
        "prompt after daemon restart: {prompt_after}"
    );
    after_restart
        .send(json!({
            "request_id": "terminal-after",
            "type": "wait",
            "until": {"kind": "task_terminal"},
            "timeout_ms": 15000
        }))
        .await;
    let terminal_after = after_restart.receive("terminal-after").await;
    assert_eq!(
        terminal_after["type"], "wait_result",
        "restart wait: {terminal_after}"
    );
    assert_ne!(terminal_after["state"]["task_id"], task_id);
    close_socket_driver(&mut after_restart, &mut child_c, "close-c").await;
    stop_daemon(&mut daemon, home.path()).await;
}

async fn process_test_context() -> (
    MutexGuard<'static, ()>,
    tempfile::TempDir,
    tempfile::TempDir,
    HomeEnv,
) {
    let lock = ENV_LOCK.lock().await;
    let home = tempdir().expect("home");
    let workspace = tempdir().expect("workspace");
    install_mock_provider(home.path());
    let env = HomeEnv::set(home.path());
    (lock, home, workspace, env)
}

fn install_mock_provider(home: &Path) {
    fs::set_permissions(home, fs::Permissions::from_mode(0o700)).expect("home mode");
    let bytes = serde_json::to_vec_pretty(&json!({
        "version": 2,
        "active_profile": "mock",
        "profiles": [{
            "name": "mock",
            "protocol": "mock",
            "model_id": "mock-model",
            "enabled": true
        }]
    }))
    .expect("provider JSON");
    let path = home.join("provider.json");
    fs::write(&path, bytes).expect("provider config");
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).expect("provider mode");
}

fn tui_command(home: &Path, cwd: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_golutra-tui"));
    command
        .env("GOLUTRA_HOME", home)
        .arg("--cwd")
        .arg(cwd)
        .current_dir(cwd);
    command
}

fn inspect_command(home: &Path, cwd: &Path, session: &str) -> Command {
    let mut command = tui_command(home, cwd);
    command.arg("inspect").arg("--session").arg(session);
    command
}

fn socket_driver_command(
    home: &Path,
    cwd: &Path,
    socket: &Path,
    embedded: bool,
    session: &str,
) -> Command {
    let mut command = tui_command(home, cwd);
    command
        .arg("driver")
        .arg("--socket")
        .arg(socket)
        .arg("--session")
        .arg(session)
        .args(["--heartbeat-secs", "0", "--idle-timeout-secs", "0"]);
    if embedded {
        command.arg("--embedded");
    }
    command
}

fn spawn_socket_driver(
    home: &Path,
    cwd: &Path,
    socket: &Path,
    embedded: bool,
    session: &str,
) -> ChildGuard {
    let mut command = socket_driver_command(home, cwd, socket, embedded, session);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    ChildGuard(command.spawn().expect("spawn socket driver"))
}

async fn wait_for_socket(path: &Path) {
    for _ in 0..200 {
        if fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_socket()) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("socket did not appear: {}", path.display());
}

fn start_daemon() -> tokio::task::JoinHandle<miette::Result<()>> {
    tokio::spawn(golutra_app_server::run(
        "127.0.0.1:0".parse().expect("daemon address"),
    ))
}

async fn wait_for_daemon(cwd: &Path) -> RuntimeTransport {
    let mut last_error = None;
    for _ in 0..240 {
        match RuntimeTransport::local_daemon(cwd).await {
            Ok(transport) => return transport,
            Err(error) => last_error = Some(error.to_string()),
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("daemon did not become ready: {last_error:?}");
}

async fn stop_daemon(daemon: &mut tokio::task::JoinHandle<miette::Result<()>>, home: &Path) {
    daemon.abort();
    let _ = daemon.await;
    let endpoint = home.join("app-server/app-server.json");
    let socket = home.join("app-server/app-server.sock");
    for _ in 0..200 {
        if !endpoint.exists() && !socket.exists() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("daemon endpoint or socket remained after shutdown");
}

async fn wait_for_terminal_projection(
    transport: &RuntimeTransport,
    session_id: SessionId,
) -> UserProjection {
    let mut last = None;
    for _ in 0..500 {
        let value = transport
            .query(RuntimeQuery {
                query_id: QueryId::new(),
                session_id,
                task_id: None,
                kind: RuntimeQueryKind::UserProjection,
                requester: ActorKind::Sdk,
                cursor: None,
                timestamp: chrono::Utc::now(),
            })
            .await
            .expect("query terminal projection");
        let projection: UserProjection = serde_json::from_value(value).expect("user projection");
        if matches!(
            projection.status,
            TaskStatus::Completed
                | TaskStatus::Partial
                | TaskStatus::Failed
                | TaskStatus::Blocked
                | TaskStatus::Cancelled
                | TaskStatus::Interrupted
                | TaskStatus::Uncertain
        ) {
            return projection;
        }
        last = Some(projection.status);
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("task did not reach terminal state; last status: {last:?}");
}

async fn close_socket_driver(
    connection: &mut SocketConnection,
    child: &mut ChildGuard,
    request_id: &str,
) {
    connection
        .send(json!({
            "request_id": request_id,
            "type": "close",
            "abort_active_task": false
        }))
        .await;
    assert_eq!(connection.receive(request_id).await["type"], "closed");
    let status = tokio::time::timeout(Duration::from_secs(10), child.0.wait())
        .await
        .expect("socket driver exit timeout")
        .expect("socket driver exit");
    assert!(status.success(), "socket driver exited with {status}");
}

async fn submit_prompt(driver: &mut StdioDriver, request_id: &str, text: &str) {
    driver
        .send(json!({
            "request_id": request_id,
            "type": "input_prompt",
            "text": text
        }))
        .await;
    assert_eq!(driver.receive(request_id).await["type"], "accepted");
}

async fn wait_for(driver: &mut StdioDriver, request_id: &str, kind: &str) -> Value {
    driver
        .send(json!({
            "request_id": request_id,
            "type": "wait",
            "until": {"kind": kind},
            "timeout_ms": 15000
        }))
        .await;
    let response = driver.receive(request_id).await;
    assert_eq!(response["type"], "wait_result", "wait response: {response}");
    response
}

fn assert_command_success(output: &std::process::Output, label: &str) {
    assert!(
        output.status.success(),
        "{label} failed with {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
}
