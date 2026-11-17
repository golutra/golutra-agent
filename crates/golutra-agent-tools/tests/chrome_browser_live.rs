//! 只对显式指定的本地测试页执行，不能对用户正在浏览的网站运行验收。
use golutra_agent_core::{SessionId, ToolCallId, ToolResultStatus};
use golutra_agent_policy::WorkspacePolicy;
use golutra_agent_tools::{
    ToolRequest, ToolRuntime,
    browser::{
        chrome::{ChromeBrowserBackend, install},
        connection::{ChromeConfig, ChromeKind},
    },
};
use serde_json::{Value, json};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

async fn call(
    runtime: &ToolRuntime,
    session: SessionId,
    name: &str,
    args: Value,
    ok: bool,
) -> golutra_agent_tools::ToolExecutionReport {
    let result = runtime
        .execute(
            ToolRequest {
                tool_call_id: ToolCallId::new(),
                provider_tool_call_id: None,
                session_id: session,
                turn_id: None,
                tool_name: name.into(),
                arguments: args,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
    std::fs::write(
        runtime
            .workspace_root()
            .join(format!("result-{}.json", result.envelope.tool_call_id)),
        serde_json::to_vec_pretty(&result.envelope).unwrap(),
    )
    .unwrap();
    assert_eq!(
        result.envelope.status == ToolResultStatus::Ok,
        ok,
        "{:?}",
        result.envelope
    );
    result
}

#[tokio::test]
#[ignore = "requires the Chrome extension connected to GOLUTRA_AGENT_CHROME_LIVE_URL localhost fixture"]
async fn chrome_native_browser_acceptance() {
    let url = std::env::var("GOLUTRA_AGENT_CHROME_LIVE_URL")
        .expect("explicit local fixture URL required");
    assert!(url.starts_with("http://127.0.0.1:"));
    let root = tempfile::Builder::new()
        .prefix("golutra-chrome-acceptance-")
        .tempdir()
        .unwrap()
        .keep();
    println!("Chrome evidence: {}", root.display());
    let make_runtime = || {
        ToolRuntime::new(WorkspacePolicy::new(&root).unwrap())
            .with_unrestricted_access(true)
            .with_external_backend(Arc::new(
                ChromeBrowserBackend::new(
                    ChromeConfig {
                        backend: ChromeKind::Chrome,
                        connection_dir: std::env::var_os(
                            "GOLUTRA_AGENT_CHROME_TEST_CONNECTION_DIR",
                        )
                        .map(std::path::PathBuf::from)
                        .unwrap_or_else(|| install::connection_dir().unwrap()),
                    },
                    &root,
                    &root.join("state"),
                    true,
                )
                .unwrap(),
            ))
            .unwrap()
    };
    let runtime = make_runtime();
    let session = SessionId::new();
    let status = call(&runtime, session, "browser_status", json!({}), true).await;
    let automatic = std::env::var_os("GOLUTRA_AGENT_CHROME_TEST_AUTOMATIC").is_some();
    if automatic {
        assert_eq!(
            status.envelope.structured_facts["browser"]["result"]["can_open_task"],
            true
        );
        call(
            &runtime,
            session,
            "browser_run",
            json!({"action":"open","url":url}),
            true,
        )
        .await;
    }
    let status = call(&runtime, session, "browser_status", json!({}), true).await;
    assert_eq!(
        status.envelope.structured_facts["browser"]["result"]["url"], url,
        "refuse to modify a non-fixture page"
    );
    call(
        &runtime,
        session,
        "browser_run",
        json!({"action":"snapshot"}),
        true,
    )
    .await;
    call(
        &runtime,
        session,
        "browser_run",
        json!({"action":"fill","selector":"#name","text":"测试 Chrome"}),
        true,
    )
    .await;
    call(
        &runtime,
        session,
        "browser_run",
        json!({"action":"click","selector":"#save"}),
        true,
    )
    .await;
    let result = call(
        &runtime,
        session,
        "browser_run",
        json!({"action":"evaluate","expression":"document.querySelector('#result').textContent"}),
        true,
    )
    .await;
    assert_eq!(
        result.envelope.structured_facts["browser"]["result"],
        "测试 Chrome"
    );
    let shot = call(
        &runtime,
        session,
        "browser_run",
        json!({"action":"screenshot"}),
        true,
    )
    .await;
    assert!(
        shot.artifacts
            .iter()
            .any(|a| a.artifact_type == "tool_image:image/png")
    );
    let artifact = shot
        .artifacts
        .iter()
        .find(|a| a.artifact_type == "tool_image:image/png")
        .unwrap();
    let image = shot
        .artifact_contents
        .iter()
        .find(|a| a.artifact_id == artifact.artifact_id)
        .unwrap();
    std::fs::write(root.join("screenshot.png"), &image.bytes).unwrap();
    drop(runtime);
    let runtime = make_runtime();
    let resumed = call(
        &runtime,
        session,
        "browser_run",
        json!({"action":"evaluate","expression":"document.querySelector('#result').textContent"}),
        true,
    )
    .await;
    assert_eq!(
        resumed.envelope.structured_facts["tab_id"],
        result.envelope.structured_facts["tab_id"]
    );
    assert_eq!(
        resumed.envelope.structured_facts["browser"]["result"],
        "测试 Chrome"
    );
    call(
        &runtime,
        SessionId::new(),
        "browser_run",
        json!({"action":"click","selector":"#save"}),
        false,
    )
    .await;
    call(
        &runtime,
        session,
        "browser_run",
        json!({"action":"click","selector":"#frame-button"}),
        true,
    )
    .await;
    let frame=call(&runtime,session,"browser_run",json!({"action":"evaluate","expression":"document.querySelector('iframe').contentDocument.querySelector('#frame-button').textContent"}),true).await;
    assert_eq!(
        frame.envelope.structured_facts["browser"]["result"],
        "Frame done"
    );
    let upload = root.join("upload.txt");
    std::fs::write(&upload, "local fixture").unwrap();
    call(
        &runtime,
        session,
        "browser_run",
        json!({"action":"upload","selector":"#upload","files":[upload]}),
        true,
    )
    .await;
    let uploaded = call(
        &runtime,
        session,
        "browser_run",
        json!({"action":"evaluate","expression":"document.querySelector('#upload').files[0].name"}),
        true,
    )
    .await;
    assert_eq!(
        uploaded.envelope.structured_facts["browser"]["result"],
        "upload.txt"
    );
    let download = call(
        &runtime,
        session,
        "browser_run",
        json!({"action":"download","url":format!("{url}download")}),
        true,
    )
    .await;
    let path = download.envelope.structured_facts["browser"]["result"]["path"]
        .as_str()
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(path).unwrap(),
        "golutra-chrome-download-ok"
    );
    call(&runtime, session, "browser_handoff", json!({}), true).await;
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
        json!({"action":"snapshot","resume_after_user":true}),
        true,
    )
    .await;
    let frames = call(
        &runtime,
        session,
        "browser_run",
        json!({"action":"frames"}),
        true,
    )
    .await;
    let cross = frames.envelope.structured_facts["browser"]["result"]
        .as_array()
        .unwrap()
        .iter()
        .find(|frame| {
            frame["url"]
                .as_str()
                .is_some_and(|url| url.contains("localhost:") && url.ends_with("/cross-frame"))
        })
        .expect("cross-site iframe must be observed")["frame_id"]
        .clone();
    call(
        &runtime,
        session,
        "browser_run",
        json!({"action":"snapshot", "frame_id":cross}),
        true,
    )
    .await;
    call(
        &runtime,
        session,
        "browser_run",
        json!({"action":"fill", "frame_id":cross,"selector":"#cross-name","text":"跨域验收"}),
        true,
    )
    .await;
    call(
        &runtime,
        session,
        "browser_run",
        json!({"action":"click", "frame_id":cross,"selector":"#cross-save"}),
        true,
    )
    .await;
    let result = call(&runtime, session, "browser_run", json!({"action":"evaluate", "frame_id":cross,"expression":"document.querySelector('#cross-result').textContent"}), true).await;
    assert_eq!(
        result.envelope.structured_facts["browser"]["result"],
        "跨域验收"
    );
    let start = std::time::Instant::now();
    call(
        &runtime,
        session,
        "browser_run",
        json!({"action":"evaluate","expression":"document.querySelector('#name').readOnly=true"}),
        true,
    )
    .await;
    call(
        &runtime,
        session,
        "browser_run",
        json!({"action":"fill","selector":"#name","text":"must not write"}),
        false,
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
    call(&runtime, session, "browser_run", json!({"action":"evaluate","expression":"document.querySelector('#name').readOnly=false; var cover=document.createElement('div');cover.id='cover';cover.style='position:fixed;inset:0;z-index:99999;background:white';document.body.append(cover)"}), true).await;
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
        json!({"action":"snapshot"}),
        true,
    )
    .await;
    call(
        &runtime,
        session,
        "browser_run",
        json!({"action":"evaluate","expression":"document.querySelector('#cover').remove()"}),
        true,
    )
    .await;
    let mut timings = Vec::new();
    for cycle in 0..50 {
        let cycle_start = std::time::Instant::now();
        let runtime = make_runtime();
        let value = format!("连续任务 {cycle}");
        call(
            &runtime,
            session,
            "browser_run",
            json!({"action":"fill","selector":"#name","text":value}),
            true,
        )
        .await;
        call(
            &runtime,
            session,
            "browser_run",
            json!({"action":"click","selector":"#save"}),
            true,
        )
        .await;
        let result = call(&runtime, session, "browser_run", json!({"action":"evaluate","expression":"document.querySelector('#result').textContent"}), true).await;
        assert_eq!(result.envelope.structured_facts["browser"]["result"], value);
        if cycle % 10 == 9 {
            call(&runtime, session, "browser_handoff", json!({}), true).await;
            call(
                &runtime,
                session,
                "browser_run",
                json!({"action":"snapshot","resume_after_user":true}),
                true,
            )
            .await;
        }
        timings.push(cycle_start.elapsed().as_millis());
    }
    std::fs::write(
        root.join("soak.json"),
        serde_json::to_vec_pretty(
            &json!({"cycles":50,"elapsed_ms":start.elapsed().as_millis(),"cycle_ms":timings}),
        )
        .unwrap(),
    )
    .unwrap();
    if let Some(handoff) = std::env::var_os("GOLUTRA_AGENT_CHROME_POPUP_HANDOFF") {
        call(
            &runtime,
            session,
            "browser_run",
            json!({"action":"click","selector":"#popup"}),
            true,
        )
        .await;
        let status = call(&runtime, session, "browser_status", json!({}), true).await;
        assert!(
            status.envelope.structured_facts["browser"]["result"]["popups"]
                .as_array()
                .unwrap()
                .iter()
                .any(|tab| tab["url"] == format!("{url}popup"))
        );
        call(&runtime, session, "browser_handoff", json!({}), true).await;
        let handoff = std::path::PathBuf::from(handoff);
        std::fs::write(&handoff, "ready").unwrap();
        let done = handoff.with_file_name(format!(
            "{}.done",
            handoff.file_name().unwrap().to_string_lossy()
        ));
        tokio::time::timeout(std::time::Duration::from_secs(30), async {
            let mut poll = tokio::time::interval(std::time::Duration::from_millis(50));
            while !done.exists() {
                poll.tick().await;
            }
        })
        .await
        .expect("fixture UI must authorize popup");
        let continued = call(
            &runtime,
            session,
            "browser_run",
            json!({"action":"snapshot","resume_after_user":true}),
            true,
        )
        .await;
        assert_eq!(
            continued.envelope.structured_facts["browser"]["result"]["url"],
            format!("{url}popup")
        );
        assert_eq!(
            continued.envelope.structured_facts["browser"]["binding"],
            result.envelope.structured_facts["browser"]["binding"]
        );
    }
    call(&runtime, session, "browser_finish", json!({}), true).await;
    if automatic {
        // 同一会话结束后及新会话均复用一次授权，但必须各自新建页面。
        let mut previous_tab = status.envelope.structured_facts["browser"]["tab_id"].clone();
        for next_session in [session, SessionId::new()] {
            let opened = call(
                &runtime,
                next_session,
                "browser_run",
                json!({"action":"open","url":url}),
                true,
            )
            .await;
            let tab = opened.envelope.structured_facts["browser"]["tab_id"].clone();
            assert_ne!(tab, previous_tab);
            previous_tab = tab;
            call(
                &runtime,
                next_session,
                "browser_run",
                json!({"action":"snapshot"}),
                true,
            )
            .await;
            call(&runtime, next_session, "browser_finish", json!({}), true).await;
        }
    }
    println!(
        "PASS Chrome: native messaging, form, screenshot, resume, competing-session refusal, upload/download, handoff/resume, finish"
    );
}
