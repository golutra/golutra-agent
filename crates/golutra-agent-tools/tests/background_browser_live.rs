//! Opt-in real browser acceptance. Only the task's new local-fixture page is touched.
//! Inspect the printed artifacts before explicitly running `background_browser_live_finish`.

use std::{
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use golutra_agent_core::{SessionId, ToolCallId, ToolResultStatus, TurnId};
use golutra_agent_policy::WorkspacePolicy;
use golutra_agent_tools::{
    BackgroundToolObserver, BackgroundToolSupervisor, ToolError, ToolExecutionReport, ToolRequest,
    ToolRuntime,
    browser::{
        EgoBrowserConfig, connection::BrowserConfig, opencli::OpenCliConfig, router::BrowserRouter,
    },
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::Semaphore,
    task::JoinSet,
};
use tokio_util::sync::CancellationToken;

const PAGE: &str = r#"<!doctype html><meta charset="utf-8"><title>Golutra background acceptance</title>
<h1>Background browser acceptance</h1><button id="once">Apply once</button><output id="count">Count: 0</output>
<script>let count=0;document.querySelector('#once').onclick=async()=>{await fetch('/effect',{method:'POST'});document.querySelector('#count').textContent='Count: '+(++count);};</script>"#;

#[derive(Debug, Serialize, Deserialize)]
struct Manifest {
    backend: String,
    workspace: PathBuf,
    executable: PathBuf,
    sdk_path: Option<PathBuf>,
    profile: Option<String>,
    session_id: SessionId,
    turn_id: TurnId,
}

#[derive(Debug)]
struct Observer {
    root: PathBuf,
    completed: Mutex<Vec<ToolExecutionReport>>,
    admissions: AtomicUsize,
}

#[async_trait]
impl BackgroundToolObserver for Observer {
    async fn admit(&self, request: &ToolRequest) -> Result<(), ToolError> {
        self.admissions.fetch_add(1, Ordering::SeqCst);
        std::fs::write(self.root.join(format!("admitted-{}.json", request.tool_call_id)), serde_json::to_vec_pretty(&json!({
            "tool_call_id":request.tool_call_id,"tool":request.tool_name,"session":request.session_id,"turn":request.turn_id
        })).unwrap()).map_err(|error|ToolError::Execution(error.to_string()))
    }

    async fn complete(
        &self,
        _: &ToolRequest,
        report: &ToolExecutionReport,
    ) -> Result<(), ToolError> {
        persist(&self.root, "completed", report)
            .map_err(|error| ToolError::Execution(error.to_string()))?;
        self.completed.lock().unwrap().push(report.clone());
        Ok(())
    }

    async fn seal(&self, _: SessionId, _: Option<TurnId>) -> Result<(), ToolError> {
        Ok(())
    }
}

fn persist(root: &Path, kind: &str, report: &ToolExecutionReport) -> std::io::Result<()> {
    std::fs::write(
        root.join(format!("{kind}-{}.json", report.envelope.tool_call_id)),
        serde_json::to_vec_pretty(&json!({
            "envelope":report.envelope,"metrics":report.metrics,"artifacts":report.artifacts
        }))?,
    )?;
    for content in &report.artifact_contents {
        let png = report.artifacts.iter().any(|artifact| {
            artifact.artifact_id == content.artifact_id
                && artifact.artifact_type == "tool_image:image/png"
        });
        std::fs::write(
            root.join(format!(
                "artifact-{}.{}",
                content.artifact_id,
                if png { "png" } else { "txt" }
            )),
            &content.bytes,
        )?;
    }
    Ok(())
}

fn build_runtime(manifest: &Manifest) -> ToolRuntime {
    let runtime = ToolRuntime::new(WorkspacePolicy::new(&manifest.workspace).unwrap())
        .with_unrestricted_access(true);
    let state = manifest.workspace.join("browser-state");
    let config = match manifest.backend.as_str() {
        "ego" => BrowserConfig::Ego(EgoBrowserConfig {
            executable: manifest.executable.clone(),
            sdk_path: manifest.sdk_path.clone(),
        }),
        "chrome" => BrowserConfig::Chrome(OpenCliConfig {
            executable: Some(manifest.executable.clone()),
            profile: manifest.profile.clone(),
            ..Default::default()
        }),
        _ => panic!("unsupported manifest backend"),
    };
    let home = manifest.workspace.join("agent-home");
    config.save(&home).unwrap();
    runtime
        .with_external_backend(Arc::new(
            BrowserRouter::new(&home, &manifest.workspace, &state, true).unwrap(),
        ))
        .unwrap()
}

fn request(manifest: &Manifest, tool: &str, arguments: Value) -> ToolRequest {
    ToolRequest {
        tool_call_id: ToolCallId::new(),
        provider_tool_call_id: None,
        session_id: manifest.session_id,
        turn_id: Some(manifest.turn_id),
        tool_name: tool.into(),
        arguments,
    }
}

async fn call(
    runtime: &ToolRuntime,
    manifest: &Manifest,
    tool: &str,
    arguments: Value,
) -> ToolExecutionReport {
    let report = runtime
        .execute(request(manifest, tool, arguments), CancellationToken::new())
        .await
        .unwrap();
    persist(&manifest.workspace, "call", &report).unwrap();
    assert_eq!(
        report.envelope.status,
        ToolResultStatus::Ok,
        "{:?}",
        report.envelope
    );
    report
}

async fn completed(supervisor: &BackgroundToolSupervisor, manifest: &Manifest) {
    let mut activity = supervisor.subscribe();
    tokio::time::timeout(Duration::from_secs(120), async {
        while supervisor.has_pending(manifest.session_id, Some(manifest.turn_id)) {
            activity.changed().await.unwrap();
        }
    })
    .await
    .expect("background browser operation did not settle within120 seconds");
}

async fn acceptance(backend: &str) {
    let executable = PathBuf::from(
        std::env::var_os(if backend == "ego" {
            "GOLUTRA_AGENT_EGO_LIVE_BINARY"
        } else {
            "GOLUTRA_AGENT_OPENCLI_LIVE_EXECUTABLE"
        })
        .expect("set an explicit installed browser/OpenCLI executable"),
    );
    assert!(executable.is_absolute() && executable.is_file());
    let manifest = if let Some(path) =
        std::env::var_os("GOLUTRA_AGENT_BACKGROUND_LIVE_REUSE_MANIFEST")
    {
        let path = PathBuf::from(path);
        let mut prior: Manifest = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(
            prior.backend, backend,
            "cannot reuse another browser backend"
        );
        assert_eq!(
            prior.executable.canonicalize().unwrap(),
            executable.canonicalize().unwrap()
        );
        assert_eq!(
            path.parent().unwrap().canonicalize().unwrap(),
            prior.workspace.canonicalize().unwrap()
        );
        assert!(
            prior
                .workspace
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("golutra-background-browser-live-")
        );
        // Reuse a previously finished test goal's native Space. The backend's own
        // continuation rules reclaim its retained page and assign fresh labels.
        prior.turn_id = TurnId::new();
        prior
    } else {
        let root = tempfile::Builder::new()
            .prefix("golutra-background-browser-live-")
            .tempdir()
            .unwrap()
            .keep();
        Manifest {
            backend: backend.into(),
            workspace: root,
            executable,
            sdk_path: std::env::var_os("GOLUTRA_AGENT_EGO_LIVE_SDK").map(PathBuf::from),
            profile: std::env::var("GOLUTRA_AGENT_OPENCLI_LIVE_PROFILE").ok(),
            session_id: SessionId::new(),
            turn_id: TurnId::new(),
        }
    };
    let root = manifest.workspace.clone();
    let manifest_path = root.join("manifest.json");
    std::fs::write(
        &manifest_path,
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();
    println!("Background browser evidence: {}", root.display());
    println!(
        "Finish manifest after inspection: {}",
        manifest_path.display()
    );
    let supervisor = BackgroundToolSupervisor::new();
    let observer = Arc::new(Observer {
        root: root.clone(),
        completed: Mutex::new(Vec::new()),
        admissions: AtomicUsize::new(0),
    });
    let runtime = build_runtime(&manifest)
        .with_background_supervisor(supervisor.clone(), observer.clone())
        .unwrap();
    let selection = call(
        &runtime,
        &manifest,
        "browser_config",
        json!({"browser_backend":backend}),
    )
    .await;
    assert_eq!(
        selection.envelope.structured_facts["browser_backend"],
        backend
    );
    assert_eq!(
        selection.envelope.structured_facts["resolved_backend"],
        backend
    );
    let status = call(&runtime, &manifest, "browser_status", json!({})).await;
    assert_eq!(
        status.envelope.structured_facts["resolved_backend"],
        backend
    );
    let mut guide_args = json!({});
    let mut guide_complete = false;
    for _ in 0..64 {
        let guide = call(&runtime, &manifest, "browser_help", guide_args).await;
        if backend == "chrome" || guide.envelope.structured_facts["next_offset"].is_null() {
            if backend == "ego" {
                assert_eq!(guide.envelope.structured_facts["guide_complete"], true);
            }
            guide_complete = true;
            break;
        }
        guide_args = json!({"offset":guide.envelope.structured_facts["next_offset"],"guide_id":guide.envelope.structured_facts["guide_id"]});
    }
    assert!(guide_complete, "SDK guide exceeded bounded help pagination");

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    std::fs::write(
        root.join("fixture.json"),
        serde_json::to_vec_pretty(
            &json!({"url":format!("http://{address}/"),"gate_released":false}),
        )
        .unwrap(),
    )
    .unwrap();
    let gate = Arc::new(Semaphore::new(0));
    let effects = Arc::new(AtomicUsize::new(0));
    let visits = Arc::new(AtomicUsize::new(0));
    let stop = CancellationToken::new();
    let server_stop = stop.clone();
    let server_gate = gate.clone();
    let server_effects = effects.clone();
    let server_visits = visits.clone();
    let server_root = root.clone();
    let server = tokio::spawn(async move {
        let mut connections = JoinSet::new();
        let mut connection_id = 0;
        loop {
            let (mut stream, _) = tokio::select! {
                ()=server_stop.cancelled()=>break,
                Some(result)=connections.join_next(), if !connections.is_empty()=>{
                    result.unwrap();
                    continue;
                },
                incoming=listener.accept(), if connections.len()<16=>incoming.unwrap(),
            };
            connection_id += 1;
            let trace = server_root.join(format!("http-{connection_id}.json"));
            let stop = server_stop.clone();
            let gate = server_gate.clone();
            let visits = server_visits.clone();
            let effects = server_effects.clone();
            connections.spawn(async move {
                std::fs::write(&trace, br#"{"accepted":true}"#).unwrap();
                let mut header = Vec::new();
                let read = tokio::time::timeout(Duration::from_secs(2), async {
                    while !header.ends_with(b"\r\n\r\n") {
                        if header.len() >= 8192 {
                            return Err(std::io::Error::other("header limit"));
                        }
                        header.push(stream.read_u8().await?);
                    }
                    Ok::<_, std::io::Error>(())
                })
                .await;
                if !matches!(read, Ok(Ok(()))) {
                    std::fs::write(&trace, serde_json::to_vec(&json!({"accepted":true,"header_result":format!("{read:?}")})).unwrap()).unwrap();
                    return;
                }
                let request = String::from_utf8_lossy(&header);
                let request_line = request.lines().next().unwrap_or("");
                std::fs::write(&trace, serde_json::to_vec(&json!({"accepted":true,"request":request_line,"response_sent":false})).unwrap()).unwrap();
                let body = if request.starts_with("GET / ") {
                    visits.fetch_add(1, Ordering::SeqCst);
                    // A barrier, not a one-use token: a browser retry must be observed
                    // and fail the visit-count assertion instead of deadlocking.
                    tokio::select! { ()=stop.cancelled()=>return, permit=gate.acquire()=>drop(permit.unwrap()) };
                    PAGE
                } else if request.starts_with("POST /effect ") {
                    effects.fetch_add(1, Ordering::SeqCst);
                    "recorded"
                } else {
                    ""
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nCache-Control: no-store\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let written = tokio::time::timeout(Duration::from_secs(2), stream.write_all(response.as_bytes())).await;
                std::fs::write(&trace, serde_json::to_vec(&json!({"accepted":true,"request":request_line,"response_sent":matches!(written,Ok(Ok(()))),"write_result":format!("{written:?}")})).unwrap()).unwrap();
            });
        }
        connections.abort_all();
        while connections.join_next().await.is_some() {}
    });

    let args = if backend == "ego" {
        json!({"background":true,"code":format!(
            "await page.goto('http://{address}/'); console.log(await page.snapshot()); await page.click('#once'); await page.waitForFunction(() => document.querySelector('#count').textContent === 'Count: 1'); console.log(await page.snapshot()); await browser.screenshot(page);"
        )})
    } else {
        json!({"background":true,"action":"open","url":format!("http://{address}/")})
    };
    let operation = request(&manifest, "browser_run", args);
    let receipt = runtime
        .execute(operation.clone(), CancellationToken::new())
        .await
        .unwrap();
    persist(&root, "receipt", &receipt).unwrap();
    assert_eq!(
        receipt.envelope.structured_facts["background_task_id"],
        operation.tool_call_id.to_string()
    );
    assert_eq!(
        receipt.envelope.structured_facts["terminal"], false,
        "fixture is deliberately blocked until receipt arrives"
    );
    // Reusing the same ID while running only returns its receipt; it cannot navigate twice.
    runtime
        .execute(operation.clone(), CancellationToken::new())
        .await
        .unwrap();
    runtime
        .seal_background_batch(manifest.session_id, Some(manifest.turn_id))
        .await
        .unwrap();
    gate.add_permits(1);
    std::fs::write(
        root.join("fixture.json"),
        serde_json::to_vec_pretty(
            &json!({"url":format!("http://{address}/"),"gate_released":true}),
        )
        .unwrap(),
    )
    .unwrap();
    completed(&supervisor, &manifest).await;
    if backend == "chrome" {
        let snapshot = call(
            &runtime,
            &manifest,
            "browser_run",
            json!({"action":"snapshot"}),
        )
        .await;
        assert!(
            snapshot
                .envelope
                .model_visible_excerpt
                .as_deref()
                .unwrap_or("")
                .contains("Apply once")
        );
        let click = request(
            &manifest,
            "browser_run",
            json!({"action":"click","selector":"#once","background":true}),
        );
        runtime
            .execute(click.clone(), CancellationToken::new())
            .await
            .unwrap();
        runtime
            .seal_background_batch(manifest.session_id, Some(manifest.turn_id))
            .await
            .unwrap();
        completed(&supervisor, &manifest).await;
        runtime
            .execute(click, CancellationToken::new())
            .await
            .unwrap();
        call(
            &runtime,
            &manifest,
            "browser_run",
            json!({"action":"wait","kind":"text","text":"Count: 1"}),
        )
        .await;
        call(
            &runtime,
            &manifest,
            "browser_run",
            json!({"action":"screenshot"}),
        )
        .await;
    }
    assert_eq!(
        visits.load(Ordering::SeqCst),
        1,
        "duplicate call replayed navigation"
    );
    assert_eq!(
        effects.load(Ordering::SeqCst),
        1,
        "button effect must occur exactly once"
    );
    let space_id = {
        let records = observer.completed.lock().unwrap();
        assert!(!records.is_empty());
        for record in records.iter() {
            assert_eq!(
                record.envelope.status,
                ToolResultStatus::Ok,
                "{:?}",
                record.envelope
            );
        }
        records[0].envelope.structured_facts["space_id"].clone()
    };
    let summary = json!({"backend":backend,"manifest":manifest_path,"session_id":manifest.session_id,"space_id":space_id,
        "navigation_count":visits.load(Ordering::SeqCst),"effect_count":effects.load(Ordering::SeqCst),"admissions":observer.admissions.load(Ordering::SeqCst),
        "background_receipt_before_navigation_completed":true,"browser_finish_pending":true});
    std::fs::write(
        root.join("summary.json"),
        serde_json::to_vec_pretty(&summary).unwrap(),
    )
    .unwrap();
    println!("{summary}");
    // Leave the page and task ownership intact for the parent to inspect. A separate
    // explicit finish test below ends this exact task without closing user pages.
    stop.cancel();
    server.await.unwrap();
    supervisor.shutdown_and_wait().await;
}

#[tokio::test]
#[ignore = "real Ego: set GOLUTRA_AGENT_EGO_LIVE_BINARY; retains task for explicit finish after inspection"]
async fn ego_background_browser_live() {
    acceptance("ego").await;
}

#[tokio::test]
#[ignore = "real Chrome: set GOLUTRA_AGENT_OPENCLI_LIVE_EXECUTABLE; retains task for explicit finish after inspection"]
async fn chrome_background_browser_live() {
    acceptance("chrome").await;
}

#[tokio::test]
#[ignore = "finish only the task in GOLUTRA_AGENT_BACKGROUND_LIVE_MANIFEST after inspecting its artifacts"]
async fn background_browser_live_finish() {
    let path = PathBuf::from(
        std::env::var_os("GOLUTRA_AGENT_BACKGROUND_LIVE_MANIFEST")
            .expect("explicit acceptance manifest required"),
    );
    let manifest: Manifest = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(
        path.parent().unwrap().canonicalize().unwrap(),
        manifest.workspace.canonicalize().unwrap()
    );
    assert!(
        manifest
            .workspace
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("golutra-background-browser-live-")
    );
    let runtime = build_runtime(&manifest);
    let arguments = if std::env::var("GOLUTRA_AGENT_BACKGROUND_LIVE_RELEASE").as_deref() == Ok("1")
    {
        json!({"release":true})
    } else {
        json!({})
    };
    let report = call(&runtime, &manifest, "browser_finish", arguments).await;
    println!(
        "Finished acceptance task: {}",
        report.envelope.structured_facts
    );
}
