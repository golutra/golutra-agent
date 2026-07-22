use std::{fs, path::Path, process::Stdio, time::Duration};

use serde_json::{Value, json};
use tempfile::tempdir;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
};

#[tokio::test]
async fn stdio_mcp_runs_and_resumes_a_durable_thread() {
    let home = tempdir().expect("home");
    let workspace = tempdir().expect("workspace");
    install_mock_provider(home.path());
    let mut child = Command::new(env!("CARGO_BIN_EXE_golutra-cli"))
        .arg("--cwd")
        .arg(workspace.path())
        .arg("mcp-server")
        .arg("--embedded")
        .env("GOLUTRA_HOME", home.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("MCP server process");
    let mut stdin = child.stdin.take().expect("stdin");
    let mut stdout = BufReader::new(child.stdout.take().expect("stdout"));

    send(
        &mut stdin,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": {"name": "golutra-test", "version": "0.1.0"}
            }
        }),
    )
    .await;
    assert!(response(&mut stdout, 1).await.get("result").is_some());
    send(
        &mut stdin,
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
    )
    .await;

    send(
        &mut stdin,
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}}),
    )
    .await;
    let tools = response(&mut stdout, 2).await;
    let mut names = tools
        .pointer("/result/tools")
        .and_then(Value::as_array)
        .expect("tools")
        .iter()
        .filter_map(|tool| tool.get("name").and_then(Value::as_str))
        .collect::<Vec<_>>();
    names.sort_unstable();
    assert_eq!(names, vec!["golutra", "golutra-reply"]);

    send(
        &mut stdin,
        json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {
                "name": "golutra",
                "arguments": {"prompt": "reply with a short acknowledgement"}
            }
        }),
    )
    .await;
    let first = response(&mut stdout, 3).await;
    assert_eq!(first.pointer("/result/isError"), Some(&Value::Bool(false)));
    assert_eq!(
        first
            .pointer("/result/structuredContent/turn/status")
            .and_then(Value::as_str),
        Some("completed")
    );
    let thread_id = first
        .pointer("/result/structuredContent/thread/thread_id")
        .and_then(Value::as_str)
        .expect("thread id")
        .to_owned();

    send(
        &mut stdin,
        json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "tools/call",
            "params": {
                "name": "golutra-reply",
                "arguments": {
                    "thread_id": thread_id,
                    "prompt": "reply again"
                }
            }
        }),
    )
    .await;
    let reply = response(&mut stdout, 4).await;
    assert_eq!(
        reply
            .pointer("/result/structuredContent/thread/thread_id")
            .and_then(Value::as_str),
        Some(thread_id.as_str())
    );
    assert_eq!(
        reply
            .pointer("/result/structuredContent/turn/status")
            .and_then(Value::as_str),
        Some("completed"),
        "unexpected MCP reply: {reply:#}"
    );

    shutdown(stdin, &mut child).await;
}

async fn send(stdin: &mut ChildStdin, value: Value) {
    let mut bytes = serde_json::to_vec(&value).expect("JSON");
    bytes.push(b'\n');
    stdin.write_all(&bytes).await.expect("write request");
    stdin.flush().await.expect("flush request");
}

async fn response(stdout: &mut BufReader<ChildStdout>, id: u64) -> Value {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let mut line = String::new();
            let read = stdout.read_line(&mut line).await.expect("read response");
            assert!(read > 0, "MCP server exited before response {id}");
            let value: Value = serde_json::from_str(&line).expect("response JSON");
            if value.get("id").and_then(Value::as_u64) == Some(id) {
                return value;
            }
        }
    })
    .await
    .expect("MCP response timeout")
}

async fn shutdown(stdin: ChildStdin, child: &mut Child) {
    drop(stdin);
    let status = tokio::time::timeout(Duration::from_secs(5), child.wait())
        .await
        .expect("MCP process exit timeout")
        .expect("MCP process status");
    assert!(status.success(), "MCP process failed: {status}");
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
