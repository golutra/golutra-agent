use std::{fs, path::Path, process::Stdio, sync::OnceLock, time::Duration};

use futures_util::{SinkExt, StreamExt};
use golutra_client::{
    APP_SERVER_ACTOR_HEADER, APP_SERVER_ATTACHMENT_HEADER, APP_SERVER_PROTOCOL_HEADER,
    AppServerInfo,
};
use golutra_protocol::RUNTIME_PROTOCOL_VERSION;
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};
use serde_json::{Value, json};
use tempfile::tempdir;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::Mutex,
};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{
        Message,
        client::IntoClientRequest,
        http::{HeaderValue as WsHeaderValue, header::AUTHORIZATION as WS_AUTHORIZATION},
    },
};

// These tests each launch a real app-server and provider runtime. Serializing them keeps
// the event-stream assertions independent of scheduler and CPU contention in CI.
static RPC_PROCESS_TEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

#[tokio::test]
async fn http_json_rpc_streams_the_same_turn_over_agent_sse() {
    let _test_lock = RPC_PROCESS_TEST_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .await;
    let home = tempdir().expect("home");
    let workspace = tempdir().expect("workspace");
    install_mock_provider(home.path());
    let _daemon = spawn_daemon(home.path());
    let (info, token) = wait_for_endpoint(home.path()).await;
    let client = reqwest::Client::new();
    let headers = rpc_headers(&token, None);

    let initialize = post_rpc(
        &client,
        &info.base_url,
        headers.clone(),
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}}),
    )
    .await;
    assert_eq!(
        initialize.pointer("/result/server").and_then(Value::as_str),
        Some("golutra-app-server")
    );

    let turn = post_rpc(
        &client,
        &info.base_url,
        headers,
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "turn/start",
            "params": {
                "cwd": workspace.path(),
                "prompt": "reply with a short acknowledgement",
                "allow_network": true,
                "yolo": true
            }
        }),
    )
    .await;
    let result = turn.get("result").expect("turn result");
    assert_eq!(result.get("accepted"), Some(&Value::Bool(true)));
    let attachment_id = result
        .get("attachment_id")
        .and_then(Value::as_str)
        .expect("attachment id");
    let session_id = result
        .pointer("/thread/session_id")
        .and_then(Value::as_str)
        .expect("session id");
    let thread_id = result
        .pointer("/thread/thread_id")
        .and_then(Value::as_str)
        .expect("thread id");
    let command_id = result
        .get("command_id")
        .and_then(Value::as_str)
        .expect("command id");
    let cursor = result.get("cursor").and_then(Value::as_u64);

    let mut request = client
        .get(format!("{}/agent/events", info.base_url))
        .headers(rpc_headers(&token, Some(attachment_id)))
        .query(&[
            ("session_id", session_id.to_owned()),
            ("thread_id", thread_id.to_owned()),
            ("command_id", command_id.to_owned()),
            (
                "cursor",
                cursor.map_or_else(String::new, |value| value.to_string()),
            ),
        ]);
    if cursor.is_none() {
        request = client
            .get(format!("{}/agent/events", info.base_url))
            .headers(rpc_headers(&token, Some(attachment_id)))
            .query(&[
                ("session_id", session_id),
                ("thread_id", thread_id),
                ("command_id", command_id),
            ]);
    }
    let response = request.send().await.expect("agent SSE response");
    assert!(response.status().is_success());
    let body = tokio::time::timeout(Duration::from_secs(15), response.text())
        .await
        .expect("agent SSE timeout")
        .expect("agent SSE body");
    let events = sse_json_events(&body);
    assert_eq!(
        events.first().and_then(|event| event["type"].as_str()),
        Some("thread.started")
    );
    assert!(events.iter().any(|event| event["type"] == "turn.started"));
    assert!(events.iter().any(|event| event["type"] == "item.completed"));
    let replay = post_rpc(
        &client,
        &info.base_url,
        rpc_headers(&token, Some(attachment_id)),
        json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "runtime/events/replay",
            "params": {"session_id": session_id, "limit": 512}
        }),
    )
    .await;
    let task_created = replay
        .pointer("/result/events")
        .and_then(Value::as_array)
        .and_then(|events| {
            events
                .iter()
                .find(|event| event["event_type"] == "task_created")
        })
        .expect("task-created runtime event");
    assert_eq!(
        task_created.pointer("/payload/execution_capabilities/network/requested"),
        Some(&Value::Bool(true)),
        "task-created event: {task_created:#}"
    );
    assert_eq!(
        task_created.pointer("/payload/execution_capabilities/network/enabled"),
        Some(&Value::Bool(false))
    );
    assert_eq!(
        task_created.pointer("/payload/execution_capabilities/policy/mode"),
        Some(&Value::String("unrestricted".to_owned()))
    );
    assert!(
        events
            .iter()
            .any(|event| { event["type"] == "turn.completed" && event["status"] == "completed" })
    );

    let terminal_cursor = events
        .iter()
        .find_map(|event| {
            (event["type"] == "turn.completed")
                .then(|| agent_event_sequence(event))
                .flatten()
        })
        .expect("terminal cursor");
    let resume_cursor = events
        .iter()
        .filter_map(agent_event_sequence)
        .find(|sequence_no| *sequence_no < terminal_cursor)
        .expect("pre-terminal cursor");
    let replay = client
        .get(format!("{}/agent/events", info.base_url))
        .headers(rpc_headers(&token, Some(attachment_id)))
        .query(&[
            ("session_id", session_id.to_owned()),
            ("thread_id", thread_id.to_owned()),
            ("command_id", command_id.to_owned()),
            ("start_cursor", cursor.unwrap_or_default().to_string()),
            ("cursor", resume_cursor.to_string()),
        ])
        .send()
        .await
        .expect("reconnected agent SSE response");
    assert!(replay.status().is_success());
    let replay = tokio::time::timeout(Duration::from_secs(15), replay.text())
        .await
        .expect("reconnected agent SSE timeout")
        .expect("reconnected agent SSE body");
    let replay = sse_json_events(&replay);
    assert_eq!(
        replay.first().and_then(|event| event["type"].as_str()),
        Some("thread.started")
    );
    assert!(
        replay
            .iter()
            .filter_map(agent_event_sequence)
            .all(|sequence_no| sequence_no > resume_cursor)
    );
    assert!(
        replay.iter().any(|event| event["type"] == "turn.completed"),
        "reconnected projector must retain command-to-turn correlation"
    );
}

#[tokio::test]
async fn json_rpc_attach_requires_a_supported_protocol_version() {
    let _test_lock = RPC_PROCESS_TEST_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .await;
    let home = tempdir().expect("home");
    let workspace = tempdir().expect("workspace");
    install_mock_provider(home.path());
    let _daemon = spawn_daemon(home.path());
    let (info, token) = wait_for_endpoint(home.path()).await;
    let client = reqwest::Client::new();

    let request = |params: Value| {
        post_rpc(
            &client,
            &info.base_url,
            rpc_headers(&token, None),
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "runtime/attach",
                "params": params,
            }),
        )
    };

    let missing = request(json!({"cwd": workspace.path()})).await;
    assert_eq!(missing.pointer("/error/code"), Some(&json!(-32602)));
    assert_eq!(
        missing.pointer("/error/message").and_then(Value::as_str),
        Some("protocol_version is required")
    );

    let invalid = request(json!({
        "cwd": workspace.path(),
        "protocol_version": "4"
    }))
    .await;
    assert_eq!(invalid.pointer("/error/code"), Some(&json!(-32602)));
    assert_eq!(
        invalid.pointer("/error/message").and_then(Value::as_str),
        Some("protocol_version is required")
    );

    let incompatible = request(json!({
        "cwd": workspace.path(),
        "protocol_version": RUNTIME_PROTOCOL_VERSION + 1
    }))
    .await;
    assert_eq!(incompatible.pointer("/error/code"), Some(&json!(-32602)));
    assert!(
        incompatible
            .pointer("/error/message")
            .and_then(Value::as_str)
            .is_some_and(|message| message.contains("incompatible"))
    );

    let attached = request(json!({
        "cwd": workspace.path(),
        "protocol_version": RUNTIME_PROTOCOL_VERSION
    }))
    .await;
    assert!(attached.pointer("/result/attachment_id").is_some());
}

#[tokio::test]
async fn http_json_rpc_notifications_return_no_content() {
    let _test_lock = RPC_PROCESS_TEST_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .await;
    let home = tempdir().expect("home");
    let _daemon = spawn_daemon(home.path());
    let (info, token) = wait_for_endpoint(home.path()).await;
    let response = reqwest::Client::new()
        .post(format!("{}/rpc", info.base_url))
        .headers(rpc_headers(&token, None))
        .json(&json!({
            "jsonrpc": "2.0",
            "method": "initialize",
            "params": {},
        }))
        .send()
        .await
        .expect("JSON-RPC notification response");
    assert_eq!(response.status().as_u16(), 204);
    assert!(
        response
            .bytes()
            .await
            .expect("notification body")
            .is_empty()
    );
}

#[tokio::test]
async fn http_json_rpc_binds_control_to_server_issued_attachments() {
    let _test_lock = RPC_PROCESS_TEST_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .await;
    let home = tempdir().expect("home");
    let workspace = tempdir().expect("workspace");
    install_mock_provider(home.path());
    let _daemon = spawn_daemon(home.path());
    let (info, token) = wait_for_endpoint(home.path()).await;
    let client = reqwest::Client::new();

    let turn = post_rpc(
        &client,
        &info.base_url,
        rpc_headers_for_actor(&token, None, "controller-a"),
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "turn/start",
            "params": {
                "cwd": workspace.path(),
                "prompt": "sleep before replying"
            }
        }),
    )
    .await;
    assert_eq!(turn.pointer("/result/accepted"), Some(&Value::Bool(true)));
    let attachment_id = turn
        .pointer("/result/attachment_id")
        .and_then(Value::as_str)
        .expect("attachment id");
    let thread_id = turn
        .pointer("/result/thread/thread_id")
        .and_then(Value::as_str)
        .expect("thread id");

    let second_attachment = post_rpc(
        &client,
        &info.base_url,
        rpc_headers_for_actor(&token, None, "spoofed-controller-a"),
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "runtime/attach",
            "params": {
                "cwd": workspace.path(),
                "protocol_version": RUNTIME_PROTOCOL_VERSION,
            }
        }),
    )
    .await;
    let second_attachment_id = second_attachment
        .pointer("/result/attachment_id")
        .and_then(Value::as_str)
        .expect("second attachment id");
    assert_ne!(attachment_id, second_attachment_id);

    let rejected = post_rpc(
        &client,
        &info.base_url,
        rpc_headers_for_actor(&token, Some(second_attachment_id), "controller-a"),
        json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "turn/interrupt",
            "params": {"thread_id": thread_id}
        }),
    )
    .await;
    assert_eq!(
        rejected.pointer("/result/ack/accepted"),
        Some(&Value::Bool(false))
    );
    assert!(
        rejected
            .pointer("/result/ack/reason")
            .and_then(Value::as_str)
            .is_some_and(|reason| reason.contains("not the active controller"))
    );

    let takeover = post_rpc(
        &client,
        &info.base_url,
        rpc_headers_for_actor(&token, Some(second_attachment_id), "controller-a"),
        json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "turn/takeover",
            "params": {"thread_id": thread_id}
        }),
    )
    .await;
    assert_eq!(
        takeover.pointer("/result/ack/accepted"),
        Some(&Value::Bool(true))
    );

    let former_controller = post_rpc(
        &client,
        &info.base_url,
        rpc_headers_for_actor(&token, Some(attachment_id), "controller-b"),
        json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "turn/interrupt",
            "params": {"thread_id": thread_id}
        }),
    )
    .await;
    assert_eq!(
        former_controller.pointer("/result/ack/accepted"),
        Some(&Value::Bool(false))
    );

    let accepted = post_rpc(
        &client,
        &info.base_url,
        rpc_headers_for_actor(&token, Some(second_attachment_id), "controller-b"),
        json!({
            "jsonrpc": "2.0",
            "id": 6,
            "method": "turn/interrupt",
            "params": {"thread_id": thread_id}
        }),
    )
    .await;
    assert_eq!(
        accepted.pointer("/result/ack/accepted"),
        Some(&Value::Bool(true))
    );
}

#[tokio::test]
async fn websocket_json_rpc_emits_incremental_agent_notifications() {
    let _test_lock = RPC_PROCESS_TEST_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .await;
    let home = tempdir().expect("home");
    let workspace = tempdir().expect("workspace");
    install_mock_provider(home.path());
    let _daemon = spawn_daemon(home.path());
    let (info, token) = wait_for_endpoint(home.path()).await;

    let websocket_url = format!("{}/rpc/ws", info.base_url.replacen("http://", "ws://", 1));
    let mut request = websocket_url
        .into_client_request()
        .expect("WebSocket request");
    request.headers_mut().insert(
        WS_AUTHORIZATION,
        WsHeaderValue::from_str(&format!("Bearer {token}")).expect("authorization"),
    );
    request.headers_mut().insert(
        APP_SERVER_PROTOCOL_HEADER,
        WsHeaderValue::from_str(&RUNTIME_PROTOCOL_VERSION.to_string()).expect("protocol version"),
    );
    let (mut socket, response) = connect_async(request).await.expect("WebSocket connection");
    assert_eq!(response.status().as_u16(), 101);

    socket
        .send(Message::Text(
            json!({
                "jsonrpc": "2.0",
                "id": 7,
                "method": "turn/start",
                "params": {
                    "cwd": workspace.path(),
                    "prompt": "reply with a short acknowledgement"
                }
            })
            .to_string()
            .into(),
        ))
        .await
        .expect("turn request");

    let mut response_thread = None;
    let mut event_types = Vec::new();
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let message = socket
                .next()
                .await
                .expect("WebSocket remains open")
                .expect("WebSocket message");
            let Message::Text(text) = message else {
                continue;
            };
            let value: Value = serde_json::from_str(&text).expect("WebSocket JSON");
            if value.get("id").and_then(Value::as_u64) == Some(7) {
                assert_eq!(value.pointer("/result/accepted"), Some(&Value::Bool(true)));
                response_thread = value
                    .pointer("/result/thread/thread_id")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned);
                continue;
            }
            if value.get("method").and_then(Value::as_str) != Some("agent/event") {
                continue;
            }
            let event_type = value
                .pointer("/params/event/type")
                .and_then(Value::as_str)
                .expect("agent event type")
                .to_owned();
            let terminal = event_type == "turn.completed";
            event_types.push(event_type);
            if terminal {
                assert_eq!(
                    value
                        .pointer("/params/event/status")
                        .and_then(Value::as_str),
                    Some("completed")
                );
                return;
            }
        }
    })
    .await
    .expect("WebSocket agent event timeout");

    assert!(response_thread.is_some());
    assert!(event_types.iter().any(|event| event == "thread.started"));
    assert!(event_types.iter().any(|event| event == "turn.started"));
    assert!(event_types.iter().any(|event| event == "item.completed"));
    socket.close(None).await.expect("WebSocket close");
}

#[tokio::test]
async fn stdio_json_rpc_uses_the_shared_dispatcher_and_resumes_threads() {
    let _test_lock = RPC_PROCESS_TEST_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .await;
    let home = tempdir().expect("home");
    let workspace = tempdir().expect("workspace");
    install_mock_provider(home.path());
    let mut child = Command::new(env!("CARGO_BIN_EXE_golutra-app-server"))
        .arg("--stdio")
        .env("GOLUTRA_HOME", home.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("stdio app server");
    let mut stdin = child.stdin.take().expect("stdin");
    let mut stdout = BufReader::new(child.stdout.take().expect("stdout"));

    send_line(
        &mut stdin,
        json!({"jsonrpc": "2.0", "method": "initialize", "params": {}}),
    )
    .await;
    send_line(
        &mut stdin,
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}}),
    )
    .await;
    let initialized = next_stdio_value(&mut stdout).await;
    assert_eq!(initialized.get("id").and_then(Value::as_u64), Some(1));
    assert_eq!(
        initialized
            .pointer("/result/server")
            .and_then(Value::as_str),
        Some("golutra-app-server")
    );

    send_line(
        &mut stdin,
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "turn/start",
            "params": {
                "cwd": workspace.path(),
                "prompt": "reply with a short acknowledgement"
            }
        }),
    )
    .await;
    let turn = response_for(&mut stdout, 2).await;
    let thread_id = turn
        .pointer("/result/thread/thread_id")
        .and_then(Value::as_str)
        .expect("thread id")
        .to_owned();
    let terminal = notification_for(&mut stdout, "turn.completed").await;
    assert_eq!(
        terminal
            .pointer("/params/event/status")
            .and_then(Value::as_str),
        Some("completed")
    );

    send_line(
        &mut stdin,
        json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "thread/resume",
            "params": {"thread_id": thread_id}
        }),
    )
    .await;
    let resumed = response_for(&mut stdout, 3).await;
    assert_eq!(
        resumed
            .pointer("/result/thread/thread_id")
            .and_then(Value::as_str),
        Some(thread_id.as_str())
    );

    drop(stdin);
    let status = tokio::time::timeout(Duration::from_secs(5), child.wait())
        .await
        .expect("stdio process exit timeout")
        .expect("stdio process status");
    assert!(status.success(), "stdio app server failed: {status}");
}

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.start_kill();
    }
}

fn spawn_daemon(home: &Path) -> ChildGuard {
    ChildGuard(
        Command::new(env!("CARGO_BIN_EXE_golutra-app-server"))
            .arg("--addr")
            .arg("127.0.0.1:0")
            .env("GOLUTRA_HOME", home)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .expect("app server daemon"),
    )
}

async fn wait_for_endpoint(home: &Path) -> (AppServerInfo, String) {
    let client = reqwest::Client::new();
    let mut last_error = None;
    for _ in 0..200 {
        let attempt = async {
            let info: AppServerInfo = serde_json::from_slice(
                &tokio::fs::read(home.join("app-server/app-server.json"))
                    .await
                    .map_err(|error| error.to_string())?,
            )
            .map_err(|error| error.to_string())?;
            let token = tokio::fs::read_to_string(home.join("app-server/transport.token"))
                .await
                .map_err(|error| error.to_string())?;
            let response = client
                .get(format!("{}/runtime/info", info.base_url))
                .bearer_auth(token.trim())
                .send()
                .await
                .map_err(|error| error.to_string())?;
            if !response.status().is_success() {
                return Err(format!("runtime info returned {}", response.status()));
            }
            Ok::<_, String>((info, token.trim().to_owned()))
        }
        .await;
        match attempt {
            Ok(endpoint) => return endpoint,
            Err(error) => last_error = Some(error),
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!(
        "app server endpoint did not become ready: {}",
        last_error.unwrap_or_else(|| "unknown error".to_owned())
    );
}

fn rpc_headers(token: &str, attachment_id: Option<&str>) -> HeaderMap {
    rpc_headers_for_actor(token, attachment_id, "rpc-test-client")
}

fn rpc_headers_for_actor(token: &str, attachment_id: Option<&str>, actor_id: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {token}")).expect("authorization"),
    );
    headers.insert(
        APP_SERVER_PROTOCOL_HEADER,
        HeaderValue::from_str(&RUNTIME_PROTOCOL_VERSION.to_string()).expect("protocol version"),
    );
    headers.insert(
        APP_SERVER_ACTOR_HEADER,
        HeaderValue::from_str(actor_id).expect("actor id"),
    );
    if let Some(attachment_id) = attachment_id {
        headers.insert(
            APP_SERVER_ATTACHMENT_HEADER,
            HeaderValue::from_str(attachment_id).expect("attachment id"),
        );
    }
    headers
}

async fn post_rpc(
    client: &reqwest::Client,
    base_url: &str,
    headers: HeaderMap,
    body: Value,
) -> Value {
    let response = client
        .post(format!("{base_url}/rpc"))
        .headers(headers)
        .json(&body)
        .send()
        .await
        .expect("JSON-RPC response");
    assert!(response.status().is_success());
    response.json().await.expect("JSON-RPC JSON")
}

fn sse_json_events(body: &str) -> Vec<Value> {
    let mut events = Vec::new();
    let mut data = Vec::new();
    for line in body.lines().chain(std::iter::once("")) {
        if line.is_empty() {
            if !data.is_empty() {
                events.push(serde_json::from_str(&data.join("\n")).expect("agent SSE event JSON"));
                data.clear();
            }
        } else if let Some(value) = line.strip_prefix("data:") {
            data.push(value.trim_start());
        }
    }
    events
}

fn agent_event_sequence(event: &Value) -> Option<u64> {
    event
        .pointer("/event/sequence_no")
        .or_else(|| event.pointer("/item/sequence_no"))
        .or_else(|| event.get("last_sequence_no"))
        .and_then(Value::as_u64)
}

async fn send_line(stdin: &mut ChildStdin, value: Value) {
    stdin
        .write_all(format!("{value}\n").as_bytes())
        .await
        .expect("write JSON-RPC request");
    stdin.flush().await.expect("flush JSON-RPC request");
}

async fn response_for(stdout: &mut BufReader<ChildStdout>, id: u64) -> Value {
    read_until(stdout, |value| {
        value.get("id").and_then(Value::as_u64) == Some(id)
    })
    .await
}

async fn next_stdio_value(stdout: &mut BufReader<ChildStdout>) -> Value {
    tokio::time::timeout(Duration::from_secs(15), async {
        let mut line = String::new();
        let read = stdout
            .read_line(&mut line)
            .await
            .expect("read JSON-RPC response");
        assert!(read > 0, "stdio app server exited before expected message");
        serde_json::from_str(&line).expect("stdio JSON-RPC JSON")
    })
    .await
    .expect("stdio JSON-RPC timeout")
}

async fn notification_for(stdout: &mut BufReader<ChildStdout>, event_type: &str) -> Value {
    read_until(stdout, |value| {
        value.get("method").and_then(Value::as_str) == Some("agent/event")
            && value.pointer("/params/event/type").and_then(Value::as_str) == Some(event_type)
    })
    .await
}

async fn read_until(
    stdout: &mut BufReader<ChildStdout>,
    predicate: impl Fn(&Value) -> bool,
) -> Value {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let mut line = String::new();
            let read = stdout
                .read_line(&mut line)
                .await
                .expect("read JSON-RPC response");
            assert!(read > 0, "stdio app server exited before expected message");
            let value: Value = serde_json::from_str(&line).expect("stdio JSON-RPC JSON");
            if predicate(&value) {
                return value;
            }
        }
    })
    .await
    .expect("stdio JSON-RPC timeout")
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
