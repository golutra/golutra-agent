use std::{fs, process::Stdio, sync::Arc};

use golutra_core::{SessionId, SideEffectType, ToolCallId, ToolResultStatus, TurnId};
use golutra_plugin::{
    McpServerManifest, PLUGIN_MANIFEST_FILE, PluginManifest, PluginPermissions, PluginStore,
    PluginToolManifest,
};
use golutra_policy::WorkspacePolicy;
use golutra_tools::{BasicToolExecutor, ToolRequest};
use serde_json::json;
use tempfile::tempdir;

use super::*;

#[tokio::test]
async fn process_only_hosts_refuse_to_execute_plugins() {
    let home = tempdir().expect("home");
    let workspace = tempdir().expect("workspace");
    let package = fixture_package();
    let store = enabled_store(home.path(), package.path());
    let backend = McpToolBackend::with_sandbox(
        store,
        workspace.path(),
        home.path().join("scratch"),
        SystemSandbox::process_only(),
    )
    .expect("backend")
    .expect("enabled plugin");
    let executor =
        BasicToolExecutor::new(WorkspacePolicy::new(workspace.path()).expect("workspace policy"))
            .with_external_backend(Arc::new(backend))
            .expect("register backend");
    let request = request("mcp__fixture__echo", json!({"text": "hello"}));
    let policy = executor.evaluate(&request).expect("policy");

    let report = executor
        .execute_with_policy(request, policy, true, CancellationToken::new())
        .await
        .expect("execution report");

    assert_eq!(report.envelope.status, ToolResultStatus::Error);
    assert!(
        report.artifact_contents[0]
            .bytes
            .windows("OS-enforced sandbox".len())
            .any(|window| window == b"OS-enforced sandbox")
    );
}

#[tokio::test]
async fn approved_stdio_plugin_is_discovered_verified_and_called() {
    if std::process::Command::new("python3")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_err()
    {
        return;
    }
    let sandbox = SystemSandbox::detect();
    if !sandbox.os_enforced() {
        return;
    }
    let home = tempdir().expect("home");
    let workspace = tempdir().expect("workspace");
    let package = fixture_package();
    let store = enabled_store(home.path(), package.path());
    let backend = McpToolBackend::with_sandbox(
        store,
        workspace.path(),
        home.path().join("scratch"),
        sandbox,
    )
    .expect("backend")
    .expect("enabled plugin");
    let executor =
        BasicToolExecutor::new(WorkspacePolicy::new(workspace.path()).expect("workspace policy"))
            .with_external_backend(Arc::new(backend))
            .expect("register backend");
    let request = request("mcp__fixture__echo", json!({"text": "hello"}));
    let policy = executor.evaluate(&request).expect("policy");

    let report = executor
        .execute_with_policy(request, policy, true, CancellationToken::new())
        .await
        .expect("execution report");

    assert_eq!(report.envelope.status, ToolResultStatus::Ok);
    assert_eq!(report.envelope.structured_facts["echo"], "hello");
    assert_eq!(
        String::from_utf8_lossy(&report.artifact_contents[0].bytes),
        "echo:hello"
    );
}

fn enabled_store(home: &Path, package: &Path) -> PluginStore {
    let store = PluginStore::new(home).expect("plugin store");
    let revision = store.stage(package).expect("stage");
    store
        .review("fixture", &revision.revision_id)
        .expect("review");
    store
        .enable("fixture", &revision.revision_id)
        .expect("enable");
    store
}

fn fixture_package() -> tempfile::TempDir {
    let package = tempdir().expect("package");
    let input_schema = json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {"text": {"type": "string"}},
        "required": ["text"]
    });
    let output_schema = json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {"echo": {"type": "string"}},
        "required": ["echo"]
    });
    let manifest = PluginManifest {
        schema_version: 1,
        id: "fixture".to_owned(),
        version: "1.0.0".to_owned(),
        display_name: Some("Fixture".to_owned()),
        description: Some("MCP fixture".to_owned()),
        server: McpServerManifest {
            command: "python3".to_owned(),
            args: vec!["-B".to_owned(), "server.py".to_owned()],
            env: Vec::new(),
        },
        permissions: PluginPermissions::default(),
        tools: vec![PluginToolManifest {
            name: "echo".to_owned(),
            description: Some("echo text".to_owned()),
            input_schema,
            output_schema: Some(output_schema),
            side_effect_type: SideEffectType::ExternalSystem,
        }],
    };
    fs::write(
        package.path().join(PLUGIN_MANIFEST_FILE),
        serde_json::to_vec_pretty(&manifest).expect("manifest JSON"),
    )
    .expect("manifest");
    fs::write(package.path().join("server.py"), fixture_server()).expect("server");
    package
}

fn fixture_server() -> &'static str {
    r#"import json
import sys

INPUT = {
    "type": "object",
    "additionalProperties": False,
    "properties": {"text": {"type": "string"}},
    "required": ["text"],
}
OUTPUT = {
    "type": "object",
    "additionalProperties": False,
    "properties": {"echo": {"type": "string"}},
    "required": ["echo"],
}

for line in sys.stdin:
    message = json.loads(line)
    method = message.get("method")
    request_id = message.get("id")
    if request_id is None:
        continue
    if method == "initialize":
        result = {
            "protocolVersion": message["params"]["protocolVersion"],
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "fixture", "version": "1.0.0"},
        }
    elif method == "tools/list":
        result = {"tools": [{
            "name": "echo",
            "description": "echo text",
            "inputSchema": INPUT,
            "outputSchema": OUTPUT,
        }]}
    elif method == "tools/call":
        text = message["params"]["arguments"]["text"]
        result = {
            "content": [{"type": "text", "text": "echo:" + text}],
            "structuredContent": {"echo": text},
            "isError": False,
        }
    else:
        response = {
            "jsonrpc": "2.0",
            "id": request_id,
            "error": {"code": -32601, "message": "method not found"},
        }
        print(json.dumps(response), flush=True)
        continue
    print(json.dumps({"jsonrpc": "2.0", "id": request_id, "result": result}), flush=True)
"#
}

fn request(tool_name: &str, arguments: Value) -> ToolRequest {
    ToolRequest {
        tool_call_id: ToolCallId::new(),
        provider_tool_call_id: None,
        session_id: SessionId::new(),
        turn_id: Some(TurnId::new()),
        tool_name: tool_name.to_owned(),
        arguments,
    }
}
