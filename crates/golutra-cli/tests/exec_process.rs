use std::{fs, path::Path, process::Stdio, time::Duration};

use golutra_client::{AppServerInfo, RuntimeTransport};
use secrecy::SecretString;
use serde_json::{Value, json};
use tempfile::tempdir;
use tokio::{io::AsyncWriteExt, process::Command};

#[tokio::test]
async fn exec_and_exec_resume_work_across_independent_processes() {
    let home = tempdir().expect("home");
    let workspace = tempdir().expect("workspace");
    install_mock_provider(home.path());
    let address = reserve_address();
    let mut app_server = Command::new(env!("CARGO_BIN_EXE_golutra-cli"))
        .arg("app-server")
        .arg("--addr")
        .arg(address.to_string())
        .env("GOLUTRA_HOME", home.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("app server process");
    let (info, token, transport) = wait_for_runtime(home.path(), workspace.path()).await;

    let first = run_cli(
        home.path(),
        workspace.path(),
        &info,
        &token,
        &["exec", "reply with a short acknowledgement"],
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
        &["exec", "resume", &thread_id, "reply again"],
    )
    .await;
    assert!(resumed.status.success(), "exec resume failed: {resumed:?}");
    assert!(String::from_utf8_lossy(&resumed.stdout).contains("mock provider completed"));

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
    let address = reserve_address();
    let mut app_server = Command::new(env!("CARGO_BIN_EXE_golutra-cli"))
        .arg("app-server")
        .arg("--addr")
        .arg(address.to_string())
        .env("GOLUTRA_HOME", home.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("app server process");
    let (info, token, _transport) = wait_for_runtime(home.path(), workspace.path()).await;

    let mut child = Command::new(env!("CARGO_BIN_EXE_golutra-cli"))
        .arg("--cwd")
        .arg(workspace.path())
        .arg("--connect")
        .arg(&info.base_url)
        .arg("exec")
        .arg("-")
        .env("GOLUTRA_HOME", home.path())
        .env("GOLUTRA_TRANSPORT_TOKEN", &token)
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
    let output = tokio::time::timeout(Duration::from_secs(15), child.wait_with_output())
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

async fn run_cli(
    home: &Path,
    workspace: &Path,
    info: &AppServerInfo,
    token: &str,
    args: &[&str],
) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_golutra-cli"))
        .arg("--cwd")
        .arg(workspace)
        .arg("--connect")
        .arg(&info.base_url)
        .args(args)
        .env("GOLUTRA_HOME", home)
        .env("GOLUTRA_TRANSPORT_TOKEN", token)
        .output()
        .await
        .expect("CLI process output")
}

async fn wait_for_runtime(
    home: &Path,
    workspace: &Path,
) -> (AppServerInfo, String, RuntimeTransport) {
    let endpoint = home.join("app-server/app-server.json");
    let token_path = home.join("app-server/transport.token");
    let mut last_error = None;
    for _ in 0..200 {
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
        "app server did not become ready: {}",
        last_error.unwrap_or_else(|| "unknown error".to_owned())
    );
}

fn reserve_address() -> std::net::SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("reserve port");
    let address = listener.local_addr().expect("reserved address");
    drop(listener);
    address
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
