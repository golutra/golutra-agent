//! 显式隔离 Chrome + OpenCLI 实机测试；不会使用用户的默认连接或 Profile。
use golutra_agent_core::{SessionId, ToolCallId, ToolResultStatus};
use golutra_agent_policy::WorkspacePolicy;
use golutra_agent_tools::{
    ToolRequest, ToolRuntime,
    browser::opencli::{OpenCliBrowserBackend, OpenCliConfig},
};
use serde_json::{Value, json};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

async fn call(
    runtime: &ToolRuntime,
    session: SessionId,
    tool: &str,
    args: Value,
    ok: bool,
) -> golutra_agent_tools::ToolExecutionReport {
    let report = runtime
        .execute(
            ToolRequest {
                tool_call_id: ToolCallId::new(),
                provider_tool_call_id: None,
                session_id: session,
                turn_id: None,
                tool_name: tool.into(),
                arguments: args,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
    std::fs::write(
        runtime
            .workspace_root()
            .join(format!("{}.json", report.envelope.tool_call_id)),
        serde_json::to_vec_pretty(&report.envelope).unwrap(),
    )
    .unwrap();
    assert_eq!(
        report.envelope.status == ToolResultStatus::Ok,
        ok,
        "{:?}",
        report.envelope
    );
    report
}

#[tokio::test]
#[ignore = "requires isolated OpenCLI fixture; run extensions/opencli/live-test.mjs"]
async fn chrome_via_opencli_keeps_session_identity_and_returns_real_images() {
    let executable = std::env::var_os("GOLUTRA_AGENT_OPENCLI_LIVE_EXECUTABLE")
        .expect("isolated OpenCLI required");
    let url = std::env::var("GOLUTRA_AGENT_OPENCLI_LIVE_URL").unwrap();
    assert!(url.starts_with("http://127.0.0.1:"));
    let root = tempfile::Builder::new()
        .prefix("golutra-opencli-acceptance-")
        .tempdir()
        .unwrap()
        .keep();
    println!("OpenCLI evidence: {}", root.display());
    let backend = OpenCliBrowserBackend::new(
        OpenCliConfig {
            executable: Some(executable.into()),
            ..Default::default()
        },
        &root,
        &root.join("state"),
        true,
    )
    .unwrap();
    let runtime = ToolRuntime::new(WorkspacePolicy::new(&root).unwrap())
        .with_unrestricted_access(true)
        .with_external_backend(Arc::new(backend))
        .unwrap();
    let session = SessionId::new();
    let second = SessionId::new();
    call(&runtime, session, "browser_status", json!({}), true).await;
    call(
        &runtime,
        session,
        "browser_run",
        json!({"action":"snapshot"}),
        false,
    )
    .await;
    call(
        &runtime,
        session,
        "browser_run",
        json!({"action":"open","url":url}),
        true,
    )
    .await;
    call(
        &runtime,
        session,
        "browser_run",
        json!({"action":"wait","kind":"selector","text":"#name"}),
        true,
    )
    .await;
    call(
        &runtime,
        session,
        "browser_run",
        json!({"action":"snapshot"}),
        true,
    )
    .await;
    let text = "中文 --profile other $(literal)";
    call(
        &runtime,
        session,
        "browser_run",
        json!({"action":"fill","selector":"#name","text":text}),
        true,
    )
    .await;
    let ambiguous = call(
        &runtime,
        session,
        "browser_run",
        json!({"action":"click","selector":"button"}),
        false,
    )
    .await;
    assert_eq!(
        ambiguous.envelope.structured_facts["action_dispatched"],
        false
    );
    let unchanged = call(
        &runtime,
        session,
        "browser_run",
        json!({"action":"evaluate","expression":"document.querySelector('#result').textContent"}),
        true,
    )
    .await;
    assert_eq!(unchanged.envelope.structured_facts["browser"]["data"], "");
    call(
        &runtime,
        session,
        "browser_run",
        json!({"action":"click","selector":"button","nth":0}),
        true,
    )
    .await;
    let value = call(
        &runtime,
        session,
        "browser_run",
        json!({"action":"evaluate","expression":"document.querySelector('#result').textContent"}),
        true,
    )
    .await;
    assert_eq!(value.envelope.structured_facts["browser"]["data"], text);
    let shot = call(
        &runtime,
        session,
        "browser_run",
        json!({"action":"screenshot"}),
        true,
    )
    .await;
    let artifact = shot
        .artifacts
        .iter()
        .find(|a| a.artifact_type == "tool_image:image/png")
        .expect("PNG artifact");
    println!("Screenshot artifact: {artifact:?}");
    let image = shot
        .artifact_contents
        .iter()
        .find(|image| image.artifact_id == artifact.artifact_id)
        .expect("stored PNG");
    std::fs::write(root.join("screenshot.png"), &image.bytes).unwrap();
    call(&runtime, session, "browser_handoff", json!({}), true).await;
    call(
        &runtime,
        session,
        "browser_run",
        json!({"action":"click","selector":"#save"}),
        false,
    )
    .await;
    call(
        &runtime,
        session,
        "browser_run",
        json!({"action":"snapshot","resume_after_user":true}),
        true,
    )
    .await;
    call(
        &runtime,
        second,
        "browser_run",
        json!({"action":"open","url":url}),
        true,
    )
    .await;
    let empty = call(
        &runtime,
        second,
        "browser_run",
        json!({"action":"evaluate","expression":"document.querySelector('#name').value"}),
        true,
    )
    .await;
    assert_eq!(empty.envelope.structured_facts["browser"]["data"], "");
    call(
        &runtime,
        second,
        "browser_finish",
        json!({"release":true}),
        true,
    )
    .await;
    let first = call(
        &runtime,
        session,
        "browser_run",
        json!({"action":"evaluate","expression":"document.querySelector('#result').textContent"}),
        true,
    )
    .await;
    assert_eq!(first.envelope.structured_facts["browser"]["data"], text);
    let finish = call(&runtime, session, "browser_finish", json!({}), true).await;
    assert_eq!(
        finish.envelope.structured_facts["browser"]["data"]["page_retained"],
        true
    );
    call(
        &runtime,
        session,
        "browser_run",
        json!({"action":"click","selector":"#save"}),
        false,
    )
    .await;
    let retained = call(&runtime, session, "browser_run", json!({"action":"evaluate","expression":"({text:document.querySelector('#result').textContent,value:document.querySelector('#name').value})","resume_after_user":true}), true).await;
    assert_eq!(
        retained.envelope.structured_facts["browser"]["data"],
        json!({"text":text,"value":text})
    );
    call(
        &runtime,
        session,
        "browser_finish",
        json!({"release":true}),
        true,
    )
    .await;
    println!(
        "PASS: OpenCLI + Chrome form, literal argv, snapshot, PNG, pause/resume, concurrent session isolation, finish"
    );
}
