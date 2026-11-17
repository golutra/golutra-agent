//! 显式运行的真实 Ego 验收；仅访问临时本地页面，不使用用户的业务标签页。

use std::{path::PathBuf, sync::Arc};

use golutra_agent_core::{SessionId, ToolCallId, ToolResultStatus};
use golutra_agent_policy::WorkspacePolicy;
use golutra_agent_tools::{
    ToolRequest, ToolRuntime,
    browser::{EgoBrowserBackend, EgoBrowserConfig},
};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};
use tokio_util::sync::CancellationToken;

const PAGE: &str = r#"<!doctype html><meta charset="utf-8"><title>Golutra Ego acceptance</title>
<label>Name <input id="name"></label><button id="save" onclick="document.querySelector('#result').textContent=document.querySelector('#name').value">Save</button>
<output id="result"></output><input id="upload" type="file" onchange="document.querySelector('#filename').textContent=this.files[0].name"><output id="filename"></output>
<a id="popup" target="_blank" href="/popup">Popup</a><a id="download" download="receipt.txt" href="/download">Download</a>
<iframe src="/frame" title="Test frame"></iframe>
<div id="editor" role="textbox" aria-label="Rich editor" contenteditable="true"></div>
<button id="dialog" onclick="document.querySelector('#dialog-result').textContent=prompt('Test prompt','')">Prompt</button><output id="dialog-result"></output>
<div id="shadow"></div><script>const shadow=document.querySelector('#shadow').attachShadow({mode:'open'});shadow.innerHTML='<button id="shadow-button">Shadow button</button>';shadow.querySelector('button').onclick=e=>e.target.textContent='Shadow done';</script>
<p style="margin-top:2500px" id="below">Below fold acceptance marker</p>"#;

async fn call(
    runtime: &ToolRuntime,
    session: SessionId,
    name: &str,
    arguments: Value,
) -> golutra_agent_tools::ToolExecutionReport {
    let report = runtime
        .execute(
            ToolRequest {
                tool_call_id: ToolCallId::new(),
                provider_tool_call_id: None,
                session_id: session,
                turn_id: None,
                tool_name: name.to_owned(),
                arguments,
            },
            CancellationToken::new(),
        )
        .await
        .expect("browser tool executes");
    // 实机失败也保留回执与截图目录，不能让临时目录清理抹掉验收证据。
    std::fs::write(
        runtime
            .workspace_root()
            .join(format!("acceptance-{}.json", report.envelope.tool_call_id)),
        serde_json::to_vec_pretty(&report.envelope).unwrap(),
    )
    .unwrap();
    assert_eq!(
        report.envelope.status,
        ToolResultStatus::Ok,
        "{:?}",
        report.envelope
    );
    report
}

#[tokio::test]
#[ignore = "requires an installed Ego native browser; set GOLUTRA_AGENT_EGO_LIVE_BINARY explicitly"]
async fn ego_browser_live_acceptance() {
    let executable = PathBuf::from(
        std::env::var_os("GOLUTRA_AGENT_EGO_LIVE_BINARY")
            .expect("set GOLUTRA_AGENT_EGO_LIVE_BINARY to the native launcher"),
    );
    let sdk_path = std::env::var_os("GOLUTRA_AGENT_EGO_LIVE_SDK").map(PathBuf::from);
    let root = tempfile::Builder::new()
        .prefix("golutra-ego-acceptance-")
        .tempdir()
        .unwrap()
        .keep();
    println!("acceptance evidence: {}", root.display());
    let make_runtime = || {
        ToolRuntime::new(WorkspacePolicy::new(&root).unwrap())
            .with_unrestricted_access(true)
            .with_external_backend(Arc::new(
                EgoBrowserBackend::new(
                    EgoBrowserConfig {
                        executable: executable.clone(),
                        sdk_path: sdk_path.clone(),
                    },
                    &root,
                    &root.join("browser-state"),
                    true,
                )
                .unwrap(),
            ))
            .unwrap()
    };
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let stop = CancellationToken::new();
    let server_stop = stop.clone();
    let server = tokio::spawn(async move {
        loop {
            let accepted = tokio::select! {
                () = server_stop.cancelled() => break,
                accepted = listener.accept() => accepted,
            };
            let (mut stream, _) = accepted.unwrap();
            let mut buffer = Vec::new();
            let Ok(Ok(())) = tokio::time::timeout(std::time::Duration::from_secs(2), async {
                while !buffer.ends_with(b"\r\n\r\n") {
                    if buffer.len() >= 8192 {
                        return Err(std::io::Error::other(
                            "fixture request headers exceed 8 KiB",
                        ));
                    }
                    buffer.push(stream.read_u8().await?);
                }
                Ok::<_, std::io::Error>(())
            })
            .await
            else {
                continue;
            };
            let request = String::from_utf8_lossy(&buffer);
            let (content_type, body) = if request.starts_with("GET /frame ") {
                (
                    "text/html",
                    "<button id='frame-button' onclick='this.textContent=\"Frame done\"'>Frame</button>",
                )
            } else if request.starts_with("GET /download ") {
                ("application/octet-stream", "golutra-ego-download-ok")
            } else if request.starts_with("GET /popup ") {
                ("text/html", "<title>Popup ready</title><p>Popup ready</p>")
            } else {
                ("text/html", PAGE)
            };
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: {content_type}; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes()).await;
        }
    });
    let session = SessionId::new();
    let runtime = make_runtime();
    let status = call(&runtime, session, "browser_status", json!({})).await;
    println!(
        "native version: {}",
        status.envelope.structured_facts["browser"]["browser_version"]
    );
    let mut guide_args = json!({});
    loop {
        let guide = call(&runtime, session, "browser_help", guide_args).await;
        let facts = &guide.envelope.structured_facts;
        assert_eq!(facts["guide_complete"], true);
        if facts["next_offset"].is_null() {
            break;
        }
        guide_args = json!({"offset":facts["next_offset"],"guide_id":facts["guide_id"]});
    }
    let first = call(&runtime, session, "browser_run", json!({"code":format!(
        "await page.goto('http://{address}/'); await page.fill('#name', '测试 Alice'); await page.click('#save'); await page.waitForFunction(() => document.querySelector('#result').textContent === '测试 Alice'); console.log(await page.snapshot()); await browser.screenshot(page);"
    )})).await;
    assert!(
        first
            .artifacts
            .iter()
            .any(|artifact| artifact.artifact_type == "tool_image:image/png")
    );
    let space_id = first.envelope.structured_facts["space_id"].clone();
    drop(runtime);
    let runtime = make_runtime();
    let upload = root.join("upload.txt");
    std::fs::write(&upload, "upload fixture").unwrap();
    let code = format!(
        r#"
if (await page.evaluate(() => document.querySelector('#result').textContent) !== '测试 Alice') throw Error('resume lost page state');
await page.setInputFiles('#upload', [{}]);
await page.waitForFunction(() => document.querySelector('#filename').textContent === 'upload.txt');
await page.click('loc=css:#frame-button');
const popupPromise = page.waitForEvent('popup');
await page.click('#popup');
const popup = await popupPromise;
await popup.waitForLoadState();
if (await popup.title() !== 'Popup ready') throw Error('popup failed');
const downloadPromise = page.waitForEvent('download');
await page.click('#download');
const download = await downloadPromise;
const destination = browser.artifactPath('receipt.txt');
await download.saveAs(destination);
const fs = await import('node:fs');
if (fs.readFileSync(destination, 'utf8') !== 'golutra-ego-download-ok') throw Error('download failed');
await page.click('#shadow-button');
await page.waitForFunction(() => document.querySelector('#shadow').shadowRoot.querySelector('button').textContent === 'Shadow done');
await page.focus('#editor');
await page.keyboard.paste({{ text:'中文 rich text', html:'<b>中文 rich text</b>' }});
await page.waitForFunction(() => document.querySelector('#editor').textContent.includes('中文 rich text'));
const promptReceipt = await page.click('#dialog');
if (!promptReceipt.dialog) throw Error('dialog receipt missing');
await page.acceptDialog('confirmed 中文');
await page.waitForFunction(() => document.querySelector('#dialog-result').textContent === 'confirmed 中文');
console.log(await page.snapshot());
"#,
        serde_json::to_string(&upload).unwrap()
    );
    let resumed = call(&runtime, session, "browser_run", json!({"code":code})).await;
    assert_eq!(resumed.envelope.structured_facts["space_id"], space_id);
    assert!(
        resumed
            .envelope
            .model_visible_excerpt
            .as_ref()
            .unwrap()
            .contains("Frame done")
    );
    let full = call(
        &runtime,
        session,
        "browser_run",
        json!({"observation":"snapshot","scope":"full_page"}),
    )
    .await;
    assert!(
        full.envelope
            .model_visible_excerpt
            .as_ref()
            .unwrap()
            .contains("Below fold acceptance marker")
    );
    let frame_line = full
        .envelope
        .model_visible_excerpt
        .as_ref()
        .unwrap()
        .lines()
        .find(|line| line.trim_start().starts_with("iframe [ref="))
        .expect("full snapshot includes iframe ref");
    let frame_ref = frame_line
        .split("ref=")
        .nth(1)
        .unwrap()
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>();
    let subtree = call(
        &runtime,
        session,
        "browser_run",
        json!({"observation":"snapshot","scope":"subtree","root":format!("@{frame_ref}")}),
    )
    .await;
    assert!(
        subtree
            .envelope
            .model_visible_excerpt
            .as_ref()
            .unwrap()
            .contains("Frame done")
    );
    call(&runtime, session, "browser_handoff", json!({})).await;
    call(
        &runtime,
        session,
        "browser_run",
        json!({"code":"console.log(await page.snapshot());", "resume_after_user":true}),
    )
    .await;
    let finished = call(&runtime, session, "browser_finish", json!({})).await;
    assert_eq!(
        finished.envelope.structured_facts["browser"]["retention_policy"],
        "all"
    );
    assert_eq!(
        finished.envelope.structured_facts["browser"]["completion"]["closedSpace"],
        false
    );
    assert_eq!(
        finished.envelope.structured_facts["browser"]["page_retained"],
        true
    );
    assert_eq!(finished.envelope.structured_facts["phase"], "finished");
    // 完成后重建 runtime 再续接；不能只测同一活动绑定中的普通重入。
    drop(runtime);
    let runtime = make_runtime();
    let continued = call(&runtime, session, "browser_run", json!({"code":
        "if (await page.evaluate(() => document.querySelector('#result')?.textContent) !== '测试 Alice') throw Error('retained result was lost'); console.log(await page.snapshot()); await browser.screenshot(page);"
    })).await;
    assert_eq!(continued.envelope.structured_facts["space_id"], space_id);
    assert_eq!(
        continued.envelope.structured_facts["browser"]["resumed_finished"],
        true
    );
    call(&runtime, session, "browser_finish", json!({})).await;
    let status = call(&runtime, session, "browser_status", json!({})).await;
    assert!(
        status.envelope.structured_facts["browser"]["spaces"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["id"] == space_id)
    );
    stop.cancel();
    server.await.unwrap();
    println!(
        "PASS: native host, complete versioned guide, form, PNG, restart/resume, iframe subtree, popup, upload/download, shadow DOM, rich paste, dialog, full-page snapshot, handoff/takeover, finish and reclaim retained results; space={space_id}"
    );
}
