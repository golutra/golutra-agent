use serde_json::{Value, json};
use std::{process::Stdio, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    process::Command,
};

// Exercise the actual CLI bytes through the TUI consumer without adding a
// production dependency between the two binaries or extending the public SDK.
#[path = "../../golutra-agent-tui/src/browser_presentation.rs"]
mod browser_presentation;

#[cfg(unix)]
#[tokio::test]
async fn browser_command_fits_a_one_mib_main_stack() {
    let home = tempfile::tempdir().unwrap();
    let output = tokio::time::timeout(
        Duration::from_secs(15),
        Command::new("/bin/sh")
            .arg("-c")
            .arg("ulimit -s 1024 && exec \"$1\" browser chrome install")
            .arg("browser-stack-regression")
            .arg(env!("CARGO_BIN_EXE_golutra-agent"))
            .env("GOLUTRA_AGENT_HOME", home.path())
            .env("HOME", home.path())
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .expect("browser command timeout")
    .expect("browser command output");
    assert!(
        output.status.success(),
        "browser command exceeded the Windows-sized stack: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["transport"], "opencli");
}

fn json_report(output: &std::process::Output) -> (Value, String) {
    let raw = std::str::from_utf8(&output.stdout).unwrap();
    let report = browser_presentation::BrowserReport::parse(raw, output.status.success())
        .unwrap_or_else(|error| {
            panic!(
                "invalid CLI/TUI contract: {error}\n{raw}\n{:?}",
                output.stderr
            )
        });
    (
        serde_json::from_str(raw).unwrap(),
        report.lines(true).join("\n"),
    )
}

#[tokio::test]
async fn no_browser_requires_consent_before_download_and_disconnect_persists() {
    let root = tempfile::tempdir().unwrap();
    let agent_home = root.path().join("agent");
    let run = |args: &[&str]| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_golutra-agent"));
        command
            .arg("browser")
            .args(args)
            .env("HOME", root.path())
            .env("USERPROFILE", root.path())
            .env("PATH", root.path().join("empty-path"))
            .env(
                "GOLUTRA_AGENT_BROWSER_APPLICATIONS_DIR",
                root.path().join("applications"),
            )
            .stdin(Stdio::null())
            .env("GOLUTRA_AGENT_HOME", &agent_home);
        command.env(
            "GOLUTRA_AGENT_OPENCLI_EXECUTABLE",
            root.path().join("missing-opencli"),
        );
        command
    };
    let output = run(&["status"]).output().await.unwrap();
    assert!(output.status.success());
    let status: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(status["backend"], "auto");
    assert_eq!(status["status"], "install_required");
    assert_eq!(status["browser_backend"], "auto");
    assert!(status["resolved_backend"].is_null());
    assert_eq!(status["installation_requires_confirmation"], true);
    assert!(!agent_home.join("browser.json").exists());

    let output = run(&["--json", "connect"]).output().await.unwrap();
    let (report, text) = json_report(&output);
    assert!(!output.status.success());
    assert_eq!(report["saved"], false);
    if cfg!(any(
        target_os = "macos",
        all(target_os = "windows", target_arch = "x86_64")
    )) {
        assert_eq!(report["state"], "install_confirmation_required");
        assert!(text.contains("/connect confirm-install-ego"));
    } else {
        assert_eq!(report["state"], "error");
    }
    assert!(!String::from_utf8_lossy(&output.stderr).contains("正在从 Ego 官方来源"));
    assert!(!agent_home.join("browser.json").exists());

    // Noninteractive setup cannot interpret absent input as consent or perform a download.
    let output = run(&["connect"]).output().await.unwrap();
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(
        error.contains("install_confirmation_required") || error.contains("当前平台没有已确认支持")
    );
    assert!(!error.contains("正在从 Ego 官方来源"));
    assert!(!agent_home.join("browser.json").exists());

    assert!(
        run(&["disconnect"])
            .output()
            .await
            .unwrap()
            .status
            .success()
    );
    let saved = std::fs::read(agent_home.join("browser.json")).unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&saved).unwrap(),
        json!({"backend":"disabled"})
    );
    let output = run(&["status"]).output().await.unwrap();
    assert!(output.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap()["status"],
        "disabled"
    );
    assert_eq!(
        std::fs::read(agent_home.join("browser.json")).unwrap(),
        saved
    );
    // 已禁用时不能因省略后端而自动启动 Chrome。
    let output = run(&["launch"]).output().await.unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("浏览器已禁用"));
    // 参数错误须在产生启动副作用之前拒绝。
    let output = run(&["connect", "ego", "--launch", "--profile", "invalid"])
        .output()
        .await
        .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("--profile only applies to Chrome/OpenCLI")
    );
    assert_eq!(
        std::fs::read(agent_home.join("browser.json")).unwrap(),
        saved
    );
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn automatic_discovery_prefers_ego_then_chrome_and_never_falls_back_after_onboarding_failure()
{
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let applications = root.path().join("Applications");
    let chrome = applications.join("Google Chrome.app");
    std::fs::create_dir_all(&chrome).unwrap();
    let run = |args: &[&str]| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_golutra-agent"));
        command
            .arg("browser")
            .args(args)
            .env("HOME", root.path())
            .env("USERPROFILE", root.path())
            .env("PATH", root.path().join("empty-path"))
            .env("GOLUTRA_AGENT_HOME", root.path().join("agent"))
            .env("GOLUTRA_AGENT_BROWSER_APPLICATIONS_DIR", &applications)
            .env(
                "GOLUTRA_AGENT_OPENCLI_EXECUTABLE",
                root.path().join("missing-opencli"),
            )
            .stdin(Stdio::null());
        command
    };
    let output = run(&["status"]).output().await.unwrap();
    let status: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(status["resolved_backend"], "chrome");
    assert_eq!(status["browser_backend"], "auto");
    assert!(
        status["guidance"]
            .as_str()
            .unwrap()
            .contains("停用 Ego 中的 OpenCLI")
    );
    let app = applications.join("ego lite.app");
    std::fs::create_dir_all(&app).unwrap();
    let output = run(&["status"]).output().await.unwrap();
    let status: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(status["status"], "setup_required");
    assert_eq!(status["installation_requires_confirmation"], false);
    let launcher = app
        .join("Contents/Frameworks/ego Framework.framework/Versions/Current/Helpers/ego-browser");
    std::fs::create_dir_all(launcher.parent().unwrap()).unwrap();
    std::fs::write(
        &launcher,
        "#!/bin/sh\nprintf 'Please complete the onboarding process first.\\n'\n",
    )
    .unwrap();
    std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o700)).unwrap();
    let output = run(&["status"]).output().await.unwrap();
    let status: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(status["resolved_backend"], "ego");
    assert_eq!(status["status"], "setup_required");
    let output = run(&["connect"]).output().await.unwrap();
    assert!(!output.status.success());
    assert!(!root.path().join("agent/browser.json").exists());
    let output = run(&["install-ego"]).output().await.unwrap();
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("无需重复下载"));
    assert!(output.stderr.is_empty());
    let output = run(&["connect", "auto", "--profile", "work"])
        .output()
        .await
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("explicit chrome backend"));
}

#[tokio::test]
async fn native_host_authenticates_loopback_requests_pairs_results_and_exits_on_eof() {
    let home = tempfile::tempdir().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_golutra-agent"))
        .arg("chrome-extension://eeefgmejgclnckofpckhcponhikcpdlc/")
        .env("HOME", home.path())
        .env("USERPROFILE", home.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut output = child.stdout.take().unwrap();
    send(&mut input, &json!({"type":"hello","version":1})).await;
    assert_eq!(read(&mut output).await, json!({"type":"ready","version":1}));
    let endpoint_path = home
        .path()
        .join(".golutra-agent/chrome-bridge/endpoint.json");
    let endpoint: Value = serde_json::from_slice(&std::fs::read(&endpoint_path).unwrap()).unwrap();
    let port = endpoint["port"].as_u64().unwrap() as u16;
    let mut unauthorized = TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port))
        .await
        .unwrap();
    send(
        &mut unauthorized,
        &json!({"token":"wrong","request":{"id":"never-forward"}}),
    )
    .await;
    let mut byte = [0];
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(3), unauthorized.read(&mut byte))
            .await
            .unwrap()
            .unwrap(),
        0
    );
    let mut client = TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port))
        .await
        .unwrap();
    let request = json!({"id":"paired","tool":"browser_status","session":"test","args":{}});
    send(
        &mut client,
        &json!({"token":endpoint["token"],"request":request}),
    )
    .await;
    assert_eq!(read(&mut output).await, request);
    let response = json!({"id":"paired","ok":true,"result":{"fixture":true}});
    send(&mut input, &response).await;
    assert_eq!(read(&mut client).await, response);
    drop(input);
    assert!(
        tokio::time::timeout(Duration::from_secs(5), child.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
    assert!(!endpoint_path.exists());
}

#[tokio::test]
async fn chrome_install_points_to_opencli_without_registering_legacy_host() {
    let root = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_golutra-agent"))
        .args(["browser", "chrome", "install"])
        .env("GOLUTRA_AGENT_HOME", root.path())
        .output()
        .await
        .unwrap();
    assert!(output.status.success());
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["transport"], "opencli");
    assert!(
        value["extension_url"]
            .as_str()
            .unwrap()
            .contains("ildkmabpimmkaediidaifkhjpohdnifk")
    );
    assert!(!root.path().join("browser/chrome-extension").exists());
}

#[tokio::test]
async fn opencli_connect_saves_validated_profile_and_version_failure_keeps_previous_config() {
    let root = tempfile::tempdir().unwrap();
    let scripts = root.path().join("OpenCLI scripts 测试");
    std::fs::create_dir(&scripts).unwrap();
    let executable = scripts.join("opencli.mjs");
    let fixture = include_str!("../../golutra-agent-tools/src/browser/opencli/fixture.mjs");
    std::fs::write(&executable, fixture).unwrap();
    let run = || {
        let mut command = Command::new(env!("CARGO_BIN_EXE_golutra-agent"));
        command
            .args([
                "browser",
                "connect",
                "chrome",
                "--profile",
                "profile-one",
                "--executable",
            ])
            .arg(&executable)
            .env("GOLUTRA_AGENT_HOME", root.path())
            .current_dir(root.path());
        command
    };
    let result = run().output().await.unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let saved = std::fs::read(root.path().join("browser.json")).unwrap();
    let config: Value = serde_json::from_slice(&saved).unwrap();
    assert_eq!(config["backend"], "chrome");
    assert_eq!(config["profile"], "profile-one");
    assert!(config.get("connection_dir").is_none());
    std::fs::write(&executable, fixture.replace("2.0.3", "1.7.2")).unwrap();
    let failed = run().output().await.unwrap();
    assert!(!failed.status.success());
    assert!(String::from_utf8_lossy(&failed.stderr).contains("OpenCLI 2.0.3 required"));
    assert_eq!(
        saved,
        std::fs::read(root.path().join("browser.json")).unwrap()
    );
    std::fs::write(
        &executable,
        "console.error('fixture launch failed: api_key=fixture-private-key'); process.exit(7);",
    )
    .unwrap();
    let failed = run().output().await.unwrap();
    assert!(!failed.status.success());
    let stderr = String::from_utf8_lossy(&failed.stderr);
    assert!(stderr.contains("OpenCLI version probe failed"));
    assert!(stderr.contains("fixture launch failed"));
    assert!(!stderr.contains("fixture-private-key"));
    assert!(!stderr.contains("2.0.3 required"));
    assert_eq!(
        saved,
        std::fs::read(root.path().join("browser.json")).unwrap()
    );
}

#[tokio::test]
async fn json_connect_waits_for_final_state_and_status_keeps_saved_selection() {
    let root = tempfile::tempdir().unwrap();
    let executable = root.path().join("opencli.mjs");
    let fixture = include_str!("../../golutra-agent-tools/src/browser/opencli/fixture.mjs");
    let delayed = fixture.replace(
        "reply({extensionConnected:true,connectivity:{ok:true}})",
        "(fs.existsSync('seen') ? reply({extensionConnected:true,connectivity:{ok:true}}) : (fs.writeFileSync('seen','yes'),reply({extensionConnected:false,connectivity:{ok:false}})))",
    );
    assert_ne!(delayed, fixture);
    std::fs::write(&executable, delayed).unwrap();
    let run = |args: &[&str]| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_golutra-agent"));
        command
            .args(["browser", "--json"])
            .args(args)
            .env("GOLUTRA_AGENT_HOME", root.path())
            .current_dir(root.path())
            .stdin(Stdio::null());
        command
    };
    let output = run(&["connect", "chrome", "--wait", "10", "--executable"])
        .arg(&executable)
        .output()
        .await
        .unwrap();
    let (report, summary) = json_report(&output);
    assert!(output.status.success());
    assert_eq!(report["state"], "ready");
    assert_eq!(report["backend"], "chrome");
    assert_eq!(report["selection"], "chrome");
    assert_eq!(report["saved"], true);
    assert!(summary.contains("已连接 Chrome"));
    assert!(!summary.contains("自动"));
    assert!(
        output.stderr.is_empty(),
        "startup diagnostics leaked to stderr"
    );
    let config = std::fs::read(root.path().join("browser.json")).unwrap();
    for args in [&["connect"][..], &["status"][..]] {
        let output = run(args).output().await.unwrap();
        let (report, _) = json_report(&output);
        assert_eq!(report["state"], "ready");
        assert_eq!(report["selection"], "chrome");
        assert_eq!(report["saved"], args[0] == "connect");
        assert_eq!(
            std::fs::read(root.path().join("browser.json")).unwrap(),
            config
        );
    }
    // A timed-out probe must preserve the previous selection and the reason,
    // without putting the old successful startup or tool instructions in the UI.
    std::fs::write(
        &executable,
        fixture.replace("extensionConnected:true", "extensionConnected:false"),
    )
    .unwrap();
    let output = run(&["connect", "--wait", "1"]).output().await.unwrap();
    let (report, summary) = json_report(&output);
    assert!(!output.status.success());
    assert_eq!(report["state"], "setup_required");
    assert_eq!(report["timed_out"], true);
    assert_eq!(report["saved"], false);
    assert!(summary.contains("等待超时"));
    assert!(!summary.contains("✓"));
    assert!(!summary.contains("browser_config"));
    assert_eq!(
        std::fs::read(root.path().join("browser.json")).unwrap(),
        config
    );

    let output = run(&["connect", "auto", "--profile", "work"])
        .output()
        .await
        .unwrap();
    let (report, _) = json_report(&output);
    assert_eq!(report["state"], "error");
    assert!(!output.status.success());
    assert_eq!(
        std::fs::read(root.path().join("browser.json")).unwrap(),
        config
    );
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn json_auto_selection_is_distinct_from_fixed_chrome_and_survives_status() {
    let root = tempfile::tempdir().unwrap();
    let applications = root.path().join("Applications");
    std::fs::create_dir_all(applications.join("Google Chrome.app")).unwrap();
    let executable = root.path().join("opencli.mjs");
    std::fs::write(
        &executable,
        include_str!("../../golutra-agent-tools/src/browser/opencli/fixture.mjs"),
    )
    .unwrap();
    let node = Command::new("node")
        .args(["-p", "process.execPath"])
        .output()
        .await
        .unwrap();
    assert!(node.status.success());
    let node_path = std::str::from_utf8(&node.stdout).unwrap().trim();
    let run = |args: &[&str]| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_golutra-agent"));
        command
            .args(["browser", "--json"])
            .args(args)
            .env("GOLUTRA_AGENT_HOME", root.path())
            .env("HOME", root.path())
            .env("USERPROFILE", root.path())
            .env("PATH", root.path().join("empty-path"))
            .env("GOLUTRA_AGENT_BROWSER_APPLICATIONS_DIR", &applications)
            .env("GOLUTRA_AGENT_OPENCLI_EXECUTABLE", &executable)
            .env("GOLUTRA_AGENT_OPENCLI_NODE", node_path)
            .current_dir(root.path())
            .stdin(Stdio::null());
        command
    };
    for args in [&["connect", "auto"][..], &["status"][..], &["connect"][..]] {
        let output = run(args).output().await.unwrap();
        let (report, summary) = json_report(&output);
        assert_eq!(report["state"], "ready");
        assert_eq!(report["backend"], "chrome");
        assert_eq!(report["selection"], "auto");
        assert!(summary.contains("自动（优先 Ego，其次 Chrome）"));
    }
    assert_eq!(
        serde_json::from_slice::<Value>(&std::fs::read(root.path().join("browser.json")).unwrap())
            .unwrap(),
        json!({"backend":"auto"})
    );
}

async fn send(writer: &mut (impl AsyncWriteExt + Unpin), value: &Value) {
    let bytes = serde_json::to_vec(value).unwrap();
    writer.write_u32_le(bytes.len() as u32).await.unwrap();
    writer.write_all(&bytes).await.unwrap();
    writer.flush().await.unwrap();
}
async fn read(reader: &mut (impl AsyncReadExt + Unpin)) -> Value {
    tokio::time::timeout(Duration::from_secs(5), async {
        let size = reader.read_u32_le().await.unwrap();
        assert!(size < 1024 * 1024);
        let mut bytes = vec![0; size as usize];
        reader.read_exact(&mut bytes).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    })
    .await
    .unwrap()
}

#[cfg(unix)]
#[tokio::test]
async fn failed_ego_onboarding_does_not_replace_existing_browser_config() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let executable = root.path().join("ego browser");
    std::fs::write(
        &executable,
        "#!/bin/sh\nprintf 'Please complete the onboarding process first.\\n'\n",
    )
    .unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
    let path = root.path().join("browser.json");
    let original = r#"{"executable":"/previous/ego"}"#;
    std::fs::write(&path, original).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_golutra-agent"))
        .args(["browser", "connect", "ego", "--executable"])
        .arg(executable)
        .env("GOLUTRA_AGENT_HOME", root.path())
        .output()
        .await
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("setup_required"));
    assert_eq!(std::fs::read_to_string(path).unwrap(), original);
}
