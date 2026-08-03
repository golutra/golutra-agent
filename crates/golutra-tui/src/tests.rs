use golutra_auth::{CredentialSource, OpenAiDeviceAuthorizationDescriptor};
use golutra_config::ProviderSettings;
use golutra_llm::ProviderReasoningEffort;
use golutra_protocol::{RuntimeEventType, VisibleStep};
use ratatui::backend::TestBackend;
use ratatui::layout::{Position, Rect};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::{Mutex, MutexGuard},
};

use super::*;

#[test]
fn remote_subcommand_is_an_explicit_app_server_transport() {
    let args = Args::try_parse_from([
        "golutra-tui",
        "--cwd",
        "/tmp",
        "remote",
        "--url",
        "https://runtime.example",
    ])
    .expect("remote arguments");
    let Some(TuiCommand::Remote(remote)) = args.command else {
        panic!("remote command");
    };
    assert_eq!(remote.url, "https://runtime.example");
    assert_eq!(args.cwd, Some(PathBuf::from("/tmp")));
}

#[test]
fn yolo_is_global_for_every_tui_entrypoint() {
    for arguments in [
        &["golutra-tui", "--yolo"][..],
        &["golutra-tui", "--daemon", "--yolo"][..],
        &[
            "golutra-tui",
            "--connect",
            "http://127.0.0.1:47831",
            "--yolo",
        ][..],
        &[
            "golutra-tui",
            "remote",
            "--url",
            "https://runtime.example",
            "--yolo",
        ][..],
        &["golutra-tui", "inspect", "--embedded", "--yolo"][..],
        &["golutra-tui", "driver", "--embedded", "--stdio", "--yolo"][..],
    ] {
        let args = Args::try_parse_from(arguments).expect("yolo TUI arguments");
        assert!(args.yolo, "{arguments:?}");
    }
}

#[test]
fn yolo_is_added_only_to_unrestricted_tui_prompts() {
    let unrestricted = TuiApp::new(
        ThreadId::new(),
        SessionId::new(),
        None,
        false,
        "ready (mock)".to_owned(),
        None,
    )
    .with_yolo(true);
    let guarded = TuiApp::new(
        ThreadId::new(),
        SessionId::new(),
        None,
        false,
        "ready (mock)".to_owned(),
        None,
    );

    assert_eq!(
        unrestricted.runtime_prompt_payload("modify files".to_owned())["yolo"],
        json!(true)
    );
    assert_eq!(
        unrestricted.runtime_prompt_payload("modify files".to_owned())["allow_network"],
        json!(true)
    );
    assert!(
        guarded
            .runtime_prompt_payload("inspect files".to_owned())
            .get("yolo")
            .is_none()
    );
}

#[tokio::test]
async fn remote_transport_attaches_to_the_real_app_server_and_resolves_a_session() {
    let _guard = env_lock_guard().await;
    let home = tempfile::tempdir().expect("home");
    let cwd = tempfile::tempdir().expect("cwd");
    let token = "remote-tui-test-token-000000000000000000000000000000000000";
    let address = std::net::TcpListener::bind("127.0.0.1:0")
        .expect("reserve app-server port")
        .local_addr()
        .expect("app-server address");
    let previous_home = std::env::var("GOLUTRA_HOME").ok();
    let previous_token = std::env::var("GOLUTRA_TRANSPORT_TOKEN").ok();
    unsafe {
        std::env::set_var("GOLUTRA_HOME", home.path());
        std::env::set_var("GOLUTRA_TRANSPORT_TOKEN", token);
    }
    apply_auth_mock().expect("install app-server provider");
    let server = tokio::spawn(golutra_app_server::run(address));
    let mut last_error = None;
    let mut transport = None;
    for _ in 0..200 {
        match RuntimeTransport::connect_with_token(
            format!("http://{address}"),
            cwd.path(),
            SecretString::from(token.to_owned()),
        )
        .await
        {
            Ok(connected) => {
                transport = Some(connected);
                break;
            }
            Err(error) => {
                last_error = Some(error.to_string());
                assert!(
                    !server.is_finished(),
                    "remote app server exited: {}",
                    last_error.as_deref().unwrap_or_default()
                );
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
        }
    }
    let transport = transport.unwrap_or_else(|| {
        panic!(
            "remote app server did not accept a TUI attachment: {}",
            last_error.unwrap_or_else(|| "unknown error".to_owned())
        )
    });
    let (thread_id, session_id) = initial_session(None, &transport)
        .await
        .expect("initial remote session");
    let canonical_cwd = cwd.path().canonicalize().expect("canonical cwd");
    assert_eq!(transport.cwd(), Some(canonical_cwd.as_path()));
    let RuntimeTransport::Remote(remote) = &transport else {
        panic!("remote subcommand must use the remote transport variant");
    };
    assert_eq!(remote.server_info().base_url, format!("http://{address}"));
    assert_ne!(thread_id, transport.default_thread_id());
    assert_ne!(session_id, transport.default_session_id());
    let provider_status = initial_provider_ui_status(&transport, session_id).await;
    assert_eq!(provider_status.message, "ready (mock)");
    assert_eq!(provider_status.model, "mock-model");

    let mut app = TuiApp::new(
        thread_id,
        session_id,
        None,
        false,
        provider_status.message,
        None,
    );
    app.execute_slash_command(&transport, SlashCommand::Auth(SlashAuthCommand::Setup))
        .await
        .expect("remote auth setup guard");
    assert!(app.auth_dialog.is_none());
    assert!(app.command_messages.iter().any(|item| {
        item.title == "Auth"
            && item
                .body
                .iter()
                .any(|line| line.contains("remote TUI cannot write provider credentials"))
    }));

    server.abort();
    let _ = server.await;
    match previous_home {
        Some(value) => unsafe { std::env::set_var("GOLUTRA_HOME", value) },
        None => unsafe { std::env::remove_var("GOLUTRA_HOME") },
    }
    match previous_token {
        Some(value) => unsafe { std::env::set_var("GOLUTRA_TRANSPORT_TOKEN", value) },
        None => unsafe { std::env::remove_var("GOLUTRA_TRANSPORT_TOKEN") },
    }
}

static ENV_LOCK: Mutex<()> = Mutex::const_new(());

async fn env_lock_guard() -> MutexGuard<'static, ()> {
    ENV_LOCK.lock().await
}

async fn spawn_probe_server(body: &'static str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        let mut buffer = [0_u8; 2048];
        let _ = stream.read(&mut buffer).await.expect("read request");
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        stream
            .write_all(response.as_bytes())
            .await
            .expect("write response");
    });
    format!("http://{address}/v1")
}

async fn spawn_oauth_probe_server() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        for _ in 0..4 {
            let (mut stream, _) = listener.accept().await.expect("accept");
            let mut buffer = [0_u8; 4_096];
            let read = stream.read(&mut buffer).await.expect("read request");
            let request = String::from_utf8_lossy(&buffer[..read]);
            let path = request
                .lines()
                .next()
                .and_then(|line| line.split_whitespace().nth(1))
                .expect("path");
            let body = match path {
                "/device" => serde_json::json!({
                    "device_code": "device-secret",
                    "user_code": "GOLUTRA-123",
                    "verification_uri": "https://example.com/device",
                    "expires_in": 600,
                    "interval": 1
                })
                .to_string(),
                "/openai-usercode" => serde_json::json!({
                    "device_auth_id": "openai-device-secret",
                    "user_code": "OPENAI-123",
                    "interval": "1"
                })
                .to_string(),
                "/openai-poll" => serde_json::json!({
                    "authorization_code": "openai-device-code",
                    "code_verifier": "openai-device-verifier"
                })
                .to_string(),
                "/token" => serde_json::json!({
                    "access_token": "oauth-access-token",
                    "refresh_token": "oauth-refresh-token",
                    "token_type": "Bearer",
                    "expires_in": 3600
                })
                .to_string(),
                "/v1/models" => serde_json::json!({
                    "data": [{"id": "oauth-model"}]
                })
                .to_string(),
                other => panic!("unexpected OAuth test path {other}"),
            };
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream
                .write_all(response.as_bytes())
                .await
                .expect("write response");
        }
    });
    format!("http://{address}")
}

#[tokio::test]
async fn initial_session_without_argument_starts_new_thread_and_session() {
    let transport = RuntimeTransport::in_memory().await.expect("transport");
    let (thread_id, session_id) = initial_session(None, &transport)
        .await
        .expect("initial session");

    assert_ne!(thread_id, transport.default_thread_id());
    assert_ne!(session_id, transport.default_session_id());
}

#[tokio::test]
async fn initial_session_with_argument_keeps_explicit_session() {
    let transport = RuntimeTransport::in_memory().await.expect("transport");
    let explicit_session_id = SessionId::new();
    let (thread_id, session_id) =
        initial_session(Some(&explicit_session_id.to_string()), &transport)
            .await
            .expect("initial session");

    assert_ne!(thread_id, transport.default_thread_id());
    assert_eq!(session_id, explicit_session_id);
}

#[tokio::test]
async fn initial_session_resolves_the_thread_owned_by_an_existing_session() {
    let transport = RuntimeTransport::in_memory().await.expect("transport");
    let session_id = SessionId::new();
    let thread_id = ThreadId::new();
    transport
        .send_command(session_command(
            session_id,
            SessionCommandKind::Prompt,
            json!({
                "prompt": "existing session",
                "_thread_id": thread_id.to_string(),
            }),
        ))
        .await
        .expect("prompt");

    let resolved = initial_session(Some(&session_id.to_string()), &transport)
        .await
        .expect("initial session");

    assert_eq!(resolved, (thread_id, session_id));
}

#[tokio::test]
async fn initial_auth_dialog_opens_without_provider_config() {
    let home = tempfile::tempdir().expect("home");
    let _guard = env_lock_guard().await;
    let previous_home = std::env::var("GOLUTRA_HOME").ok();
    unsafe {
        std::env::set_var("GOLUTRA_HOME", home.path());
    }
    assert!(initial_auth_dialog().is_some());
    match previous_home {
        Some(value) => unsafe {
            std::env::set_var("GOLUTRA_HOME", value);
        },
        None => unsafe {
            std::env::remove_var("GOLUTRA_HOME");
        },
    }
}

#[tokio::test]
async fn auth_dialog_mock_choice_persists_global_provider() {
    let dir = tempfile::tempdir().expect("dir");
    let home = tempfile::tempdir().expect("home");
    let _guard = env_lock_guard().await;
    let previous_home = std::env::var("GOLUTRA_HOME").ok();
    unsafe {
        std::env::set_var("GOLUTRA_HOME", home.path());
    }
    let transport = RuntimeTransport::for_cwd(dir.path())
        .await
        .expect("transport");
    let mut app = TuiApp::new(
        ThreadId::new(),
        SessionId::new(),
        None,
        false,
        provider_status_message(),
        Some(AuthDialogState::new()),
    );
    let dialog = app.auth_dialog.as_mut().expect("dialog");
    dialog.selected = 3;

    let result = advance_auth_dialog(&mut app, &transport).await;

    result.expect("advance");
    assert!(app.auth_dialog.is_none());
    assert_eq!(app.provider_message, "ready (mock)");
    assert!(initial_auth_dialog().is_none());
    let settings = ProviderSettings::load(home.path().join("provider.json")).expect("settings");
    assert_eq!(
        settings.active_profile().expect("profile").protocol,
        ProviderProtocol::Mock
    );
    assert!(!dir.path().join(".golutra/provider.json").exists());
    match previous_home {
        Some(value) => unsafe {
            std::env::set_var("GOLUTRA_HOME", value);
        },
        None => unsafe {
            std::env::remove_var("GOLUTRA_HOME");
        },
    }
}

#[tokio::test]
async fn auth_dialog_openai_flow_persists_user_key() {
    let dir = tempfile::tempdir().expect("dir");
    let home = tempfile::tempdir().expect("home");
    let base_url = spawn_probe_server(r#"{"data":[{"id":"qwen-coder"}]}"#).await;
    let _guard = env_lock_guard().await;
    let previous_home = std::env::var("GOLUTRA_HOME").ok();
    unsafe {
        std::env::set_var("GOLUTRA_HOME", home.path());
    }
    let transport = RuntimeTransport::for_cwd(dir.path())
        .await
        .expect("transport");
    let mut app = TuiApp::new(
        ThreadId::new(),
        SessionId::new(),
        None,
        false,
        provider_status_message(),
        Some(AuthDialogState::new()),
    );
    {
        let dialog = app.auth_dialog.as_mut().expect("dialog");
        dialog.provider = Some(OFFICIAL_PROVIDER_PRESET);
        dialog.step = AuthDialogStep::BaseUrl;
        dialog.base_url = base_url;
        dialog.model = "qwen-coder".to_owned();
        dialog.api_key = "test-key".to_owned();
    }

    advance_auth_dialog(&mut app, &transport)
        .await
        .expect("base url");
    advance_auth_dialog(&mut app, &transport)
        .await
        .expect("api key");
    {
        let dialog = app.auth_dialog.as_mut().expect("dialog");
        dialog.selected = dialog.custom_model_index();
    }
    advance_auth_dialog(&mut app, &transport)
        .await
        .expect("model");
    assert_eq!(
        app.auth_dialog.as_ref().map(|dialog| dialog.step),
        Some(AuthDialogStep::AdvancedConfig)
    );
    advance_auth_dialog(&mut app, &transport)
        .await
        .expect("advanced config");
    assert_eq!(
        app.auth_dialog.as_ref().map(|dialog| dialog.step),
        Some(AuthDialogStep::Review)
    );
    let reviewed_credential_id = app
        .auth_dialog
        .as_ref()
        .and_then(|dialog| dialog.review.as_ref())
        .map(|review| review.credential_ref.id.clone())
        .expect("reviewed credential");

    let result = advance_auth_dialog(&mut app, &transport).await;
    result.expect("advance");

    assert!(app.auth_dialog.is_none());
    assert_eq!(app.provider_message, "ready (golutra)");
    let settings = ProviderSettings::load(home.path().join("provider.json")).expect("settings");
    let profile = settings.active_profile().expect("profile");
    assert_eq!(profile.name, "golutra");
    assert_eq!(profile.model_id.as_deref(), Some("qwen-coder"));
    assert!(matches!(
        profile
            .credential_ref
            .as_ref()
            .map(|reference| &reference.source),
        Some(CredentialSource::Ephemeral)
    ));
    assert_eq!(
        profile
            .credential_ref
            .as_ref()
            .map(|reference| reference.id.as_str()),
        Some(reviewed_credential_id.as_str())
    );
    let persisted = std::fs::read_to_string(home.path().join("provider.json"))
        .expect("persisted provider settings");
    assert!(!persisted.contains("test-key"));
    assert!(!dir.path().join(".golutra/provider.json").exists());
    match previous_home {
        Some(value) => unsafe {
            std::env::set_var("GOLUTRA_HOME", value);
        },
        None => unsafe {
            std::env::remove_var("GOLUTRA_HOME");
        },
    }
}

#[tokio::test]
async fn auth_dialog_base_url_requires_http_scheme() {
    let transport = RuntimeTransport::in_memory().await.expect("transport");
    let mut app = TuiApp::new(
        ThreadId::new(),
        SessionId::new(),
        None,
        false,
        provider_status_message(),
        Some(AuthDialogState::new()),
    );
    {
        let dialog = app.auth_dialog.as_mut().expect("dialog");
        dialog.provider = Some(OFFICIAL_PROVIDER_PRESET);
        dialog.step = AuthDialogStep::BaseUrl;
        dialog.base_url = "api.golutra.cn".to_owned();
    }

    advance_auth_dialog(&mut app, &transport)
        .await
        .expect("advance");

    let dialog = app.auth_dialog.as_ref().expect("dialog");
    assert_eq!(dialog.step, AuthDialogStep::BaseUrl);
    assert_eq!(
        dialog.error.as_deref(),
        Some("Base URL must start with http:// or https://")
    );
}

#[tokio::test]
async fn q_key_does_not_exit_tui() {
    let transport = RuntimeTransport::in_memory().await.expect("transport");
    let mut app = TuiApp::new(
        ThreadId::new(),
        SessionId::new(),
        None,
        false,
        "ready (mock)".to_owned(),
        None,
    );

    handle_key(
        KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
        &mut app,
        &transport,
    )
    .await
    .expect("handle key");

    assert!(!app.should_quit);
    assert_eq!(app.input.text(), "q");
}

#[tokio::test]
async fn composer_keys_edit_unicode_at_the_grapheme_boundary() {
    let transport = RuntimeTransport::in_memory().await.expect("transport");
    let mut app = TuiApp::new(
        ThreadId::new(),
        SessionId::new(),
        None,
        false,
        "ready (mock)".to_owned(),
        None,
    );

    for character in ['你', '好', '👍'] {
        handle_key(
            KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE),
            &mut app,
            &transport,
        )
        .await
        .expect("insert character");
    }
    handle_key(
        KeyEvent::new(KeyCode::Left, KeyModifiers::NONE),
        &mut app,
        &transport,
    )
    .await
    .expect("move cursor");
    handle_key(
        KeyEvent::new(KeyCode::Char('啊'), KeyModifiers::NONE),
        &mut app,
        &transport,
    )
    .await
    .expect("insert in the middle");
    assert_eq!(app.input.text(), "你好啊👍");

    handle_key(
        KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
        &mut app,
        &transport,
    )
    .await
    .expect("delete grapheme");
    assert_eq!(app.input.text(), "你好👍");
}

#[test]
fn bracketed_paste_preserves_chinese_and_newlines() {
    let mut app = TuiApp::new(
        ThreadId::new(),
        SessionId::new(),
        None,
        false,
        "ready (mock)".to_owned(),
        None,
    );

    handle_paste("你好\r\n世界", &mut app);

    assert_eq!(app.input.text(), "你好\n世界");
    assert_eq!(app.input.cursor(), "你好\n世界".len());
}

#[test]
fn composer_renders_terminal_cursor_at_unicode_display_column() {
    let mut app = TuiApp::new(
        ThreadId::new(),
        SessionId::new(),
        None,
        false,
        "ready (mock)".to_owned(),
        None,
    );
    app.input.set_text("你好");
    app.input.move_left();
    let mut terminal = Terminal::new(TestBackend::new(40, 3)).expect("terminal");

    terminal
        .draw(|frame| draw_bottom_pane(frame, frame.area(), &app))
        .expect("draw");

    terminal
        .backend_mut()
        .assert_cursor_position(Position::new(4, 1));
}

#[test]
fn composer_hides_cursor_when_the_terminal_is_too_narrow_for_input() {
    let app = TuiApp::new(
        ThreadId::new(),
        SessionId::new(),
        None,
        false,
        "ready (mock)".to_owned(),
        None,
    );

    assert_eq!(composer_cursor_position(Rect::new(0, 0, 2, 3), &app), None);
}

#[test]
fn active_task_renders_ephemeral_status_above_composer() {
    let session_id = SessionId::new();
    let task_id = TaskId::new();
    let mut app = TuiApp::new(
        ThreadId::new(),
        session_id,
        None,
        false,
        "ready (mock)".to_owned(),
        None,
    );
    app.projection = Some(UserProjection {
        session_id,
        task_id: Some(task_id),
        status: golutra_core::TaskStatus::Running,
        visible_steps: Vec::new(),
        pending_approval: None,
        final_message: None,
        residual_risks: Vec::new(),
    });
    app.apply_runtime_event(transcript_event(
        1,
        session_id,
        task_id,
        RuntimeEventType::TaskCreated,
        json!({"payload": {"prompt": "work"}}),
    ));
    app.refresh_activity_snapshot();

    let mut terminal = Terminal::new(TestBackend::new(100, 6)).expect("terminal");
    terminal
        .draw(|frame| draw_bottom_pane(frame, frame.area(), &app))
        .expect("draw active status");
    let buffer = terminal.backend().buffer();
    let mut rendered = String::new();
    for row in 0..6 {
        for column in 0..100 {
            if let Some(cell) = buffer.cell((column, row)) {
                rendered.push_str(cell.symbol());
            }
        }
    }

    assert!(rendered.contains("• -- tokens/s"));
    assert!(!rendered.contains("Working"));
    assert!(rendered.contains("esc to interrupt"));
    let status_row = (0..100)
        .filter_map(|column| terminal.backend().buffer().cell((column, 0)))
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(status_row.contains("• -- tokens/s"));
    let separator_row = (0..100)
        .filter_map(|column| terminal.backend().buffer().cell((column, 1)))
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert_eq!(separator_row, "─".repeat(100));
    assert_eq!(bottom_pane_height(&app), 4);
}

#[test]
fn activity_rate_is_stable_between_status_samples() {
    let session_id = SessionId::new();
    let task_id = TaskId::new();
    let turn_id = golutra_core::TurnId::new();
    let base = chrono::Utc::now();
    let mut app = TuiApp::new(
        ThreadId::new(),
        session_id,
        None,
        false,
        "ready (mock)".to_owned(),
        None,
    );
    app.projection = Some(UserProjection {
        session_id,
        task_id: Some(task_id),
        status: golutra_core::TaskStatus::Running,
        visible_steps: Vec::new(),
        pending_approval: None,
        final_message: None,
        residual_risks: Vec::new(),
    });

    let mut created = transcript_event(
        1,
        session_id,
        task_id,
        RuntimeEventType::TaskCreated,
        json!({"payload": {"prompt": "work"}}),
    );
    created.turn_id = Some(turn_id);
    created.timestamp = base;
    app.apply_runtime_event(created);
    let mut first_delta = transcript_event(
        2,
        session_id,
        task_id,
        RuntimeEventType::ProviderStreamed,
        json!({"delta": {"kind": "text_delta", "text": "abcdefgh"}}),
    );
    first_delta.turn_id = Some(turn_id);
    first_delta.timestamp = base + chrono::Duration::milliseconds(100);
    app.apply_runtime_event(first_delta);
    app.refresh_activity_snapshot_at(base + chrono::Duration::milliseconds(1_100));
    let sampled = live_status_text(&app, 80).expect("sampled status");

    let mut burst = transcript_event(
        3,
        session_id,
        task_id,
        RuntimeEventType::ProviderStreamed,
        json!({"delta": {"kind": "text_delta", "text": "x".repeat(400)}}),
    );
    burst.turn_id = Some(turn_id);
    burst.timestamp = base + chrono::Duration::milliseconds(1_200);
    app.apply_runtime_event(burst);

    assert_eq!(
        live_status_text(&app, 80).as_deref(),
        Some(sampled.as_str())
    );
    app.refresh_activity_snapshot_at(base + chrono::Duration::milliseconds(2_100));
    assert_ne!(
        live_status_text(&app, 80).as_deref(),
        Some(sampled.as_str())
    );
}

#[tokio::test]
async fn escape_interrupts_an_active_task_without_discarding_draft_input() {
    let transport = RuntimeTransport::in_memory().await.expect("transport");
    let session_id = transport.default_session_id();
    let task_id = TaskId::new();
    let mut app = TuiApp::new(
        transport.default_thread_id(),
        session_id,
        None,
        false,
        "ready (mock)".to_owned(),
        None,
    );
    app.input.set_text("draft prompt");
    app.projection = Some(UserProjection {
        session_id,
        task_id: Some(task_id),
        status: golutra_core::TaskStatus::Running,
        visible_steps: Vec::new(),
        pending_approval: None,
        final_message: None,
        residual_risks: Vec::new(),
    });

    handle_key(
        KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
        &mut app,
        &transport,
    )
    .await
    .expect("interrupt active task");

    assert_eq!(app.input.text(), "draft prompt");
    assert!(app.last_control_ack.is_some());
    assert_ne!(app.status_message, "input cleared");
}

#[tokio::test]
async fn tab_without_slash_candidates_does_not_enable_developer_mode() {
    let transport = RuntimeTransport::in_memory().await.expect("transport");
    let mut app = TuiApp::new(
        ThreadId::new(),
        SessionId::new(),
        None,
        false,
        "ready (mock)".to_owned(),
        None,
    );

    handle_key(
        KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE),
        &mut app,
        &transport,
    )
    .await
    .expect("handle key");

    assert!(!app.debug_mode);
    assert!(app.developer_projection.is_none());
}

#[tokio::test]
async fn ctrl_c_requires_second_press_to_exit() {
    let transport = RuntimeTransport::in_memory().await.expect("transport");
    let mut app = TuiApp::new(
        ThreadId::new(),
        SessionId::new(),
        None,
        false,
        "ready (mock)".to_owned(),
        None,
    );
    let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);

    handle_key(ctrl_c, &mut app, &transport)
        .await
        .expect("first ctrl-c");
    assert!(!app.should_quit);
    assert_eq!(app.status_message, "press Ctrl+C again to quit");

    handle_key(ctrl_c, &mut app, &transport)
        .await
        .expect("second ctrl-c");
    assert!(app.should_quit);
}

#[test]
fn slash_candidates_render_below_composer_with_selection() {
    let mut app = TuiApp::new(
        ThreadId::new(),
        SessionId::new(),
        None,
        false,
        "ready (mock)".to_owned(),
        None,
    );
    app.input.set_text("/");
    app.slash_selected = 3;
    let candidates = app.slash_candidates();
    let lines = slash_candidate_lines(&app, &candidates)
        .into_iter()
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .join("\n");

    assert!(lines.contains("/new"));
    assert!(lines.contains("/resume"));
    assert!(lines.contains("› /threads"));
    assert_eq!(bottom_pane_height(&app), 8);
}

#[test]
fn footer_context_shows_model_and_home_relative_workspace_without_wrapping() {
    let workspace = Path::new("/Users/skyseek/Desktop/project/open/golutra-agent/golutra-agent");
    let label = workspace_path_label_with_home(workspace, Some(Path::new("/Users/skyseek")));

    assert_eq!(
        fit_model_and_workspace("gpt-5.6-sol ultra", &label, 120),
        "gpt-5.6-sol ultra · ~/Desktop/project/open/golutra-agent/golutra-agent"
    );

    let narrow = fit_model_and_workspace("gpt-5.6-sol ultra", &label, 30);
    assert!(display_width(&narrow) <= 30);
    assert!(narrow.contains(" · "));
    assert!(narrow.ends_with("agent"));
}

#[test]
fn footer_context_marks_yolo_mode() {
    let app = TuiApp::new(
        ThreadId::new(),
        SessionId::new(),
        None,
        false,
        "ready (custom)".to_owned(),
        None,
    )
    .with_yolo(true)
    .with_footer_context("/workspace", "gpt-5.6-sol");

    assert_eq!(
        footer_context_text(&app, 80),
        "[yolo] gpt-5.6-sol · /workspace"
    );
}

#[test]
fn bottom_pane_renders_context_instead_of_task_status() {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/home/test"));
    let workspace = home.join("Desktop/project/open/golutra-agent/golutra-agent");
    let app = TuiApp::new(
        ThreadId::new(),
        SessionId::new(),
        None,
        false,
        "ready (custom)".to_owned(),
        None,
    )
    .with_footer_context(workspace, "gpt-5.6-sol ultra");
    let mut terminal = Terminal::new(TestBackend::new(100, 3)).expect("terminal");

    terminal
        .draw(|frame| draw_bottom_pane(frame, frame.area(), &app))
        .expect("draw");

    let buffer = terminal.backend().buffer();
    let footer = (0..100)
        .filter_map(|x| buffer.cell((x, 2)))
        .map(|cell| cell.symbol())
        .collect::<String>();
    let expected = "gpt-5.6-sol ultra · ~/Desktop/project/open/golutra-agent/golutra-agent";
    assert!(footer.contains(expected));
    assert_eq!(footer.find(expected), Some(2));
    assert!(!footer.contains("ready"));
}

#[test]
fn provider_footer_adds_configured_reasoning_effort() {
    let mut profile = ProviderProfile::openai_compatible(
        "custom",
        "https://api.example.com/v1",
        "gpt-5.6-sol",
        CredentialRef::ephemeral(SecretKind::ApiKey),
    )
    .expect("profile");
    profile.generation_config = Some(ProviderGenerationConfig {
        enable_thinking: true,
        reasoning_effort: Some(ProviderReasoningEffort::Xhigh),
        context_window_size: None,
        max_tokens: None,
    });

    assert_eq!(provider_profile_footer_label(&profile), "gpt-5.6-sol xhigh");
}

#[test]
fn transcript_role_markers_follow_codex_symbols() {
    assert_eq!(role_marker(&TranscriptRole::User), "› ");
    assert_eq!(role_marker(&TranscriptRole::Assistant), "• ");
    assert_eq!(role_marker(&TranscriptRole::Status), "• ");
    assert_eq!(role_marker(&TranscriptRole::System), "• ");
}

#[tokio::test]
async fn auth_review_marks_existing_profile_updates() {
    let home = tempfile::tempdir().expect("home");
    let _guard = env_lock_guard().await;
    let previous_home = std::env::var("GOLUTRA_HOME").ok();
    unsafe {
        std::env::set_var("GOLUTRA_HOME", home.path());
    }
    let paths = provider_paths_for_tui().expect("paths");
    ProviderInstallPlan {
        scope: ProviderConfigScope::User,
        profile: ProviderProfile::openai_compatible(
            "golutra",
            "https://api.golutra.cn/v1",
            "gpt-test",
            CredentialRef::ephemeral(SecretKind::ApiKey),
        )
        .expect("profile"),
        activate: true,
        pending_secret: None,
    }
    .apply(&paths)
    .expect("install");
    let mut dialog = AuthDialogState::new();
    dialog.provider = Some(OFFICIAL_PROVIDER_PRESET);
    dialog.base_url = "https://api.golutra.cn/v1".to_owned();
    dialog.model = "qwen-coder".to_owned();
    dialog.api_key = "test-key".to_owned();

    let review = build_auth_review(&dialog).expect("review");
    match previous_home {
        Some(value) => unsafe {
            std::env::set_var("GOLUTRA_HOME", value);
        },
        None => unsafe {
            std::env::remove_var("GOLUTRA_HOME");
        },
    }

    assert!(review.updates_existing_profile);
    assert_eq!(review.credential, "ephemeral:***");
    assert!(!review.preview_json.contains("\"api_key\""));
    assert!(!review.preview_json.contains("test-key"));
}

#[tokio::test]
async fn auth_review_can_replace_an_unreadable_provider_config() {
    let home = tempfile::tempdir().expect("home");
    let _guard = env_lock_guard().await;
    let previous_home = std::env::var("GOLUTRA_HOME").ok();
    unsafe {
        std::env::set_var("GOLUTRA_HOME", home.path());
    }
    std::fs::write(
        home.path().join("provider.json"),
        r#"{
  "version": 2,
  "active_profile": "legacy",
  "profiles": [{
    "name": "legacy",
    "protocol": "openai-compatible",
    "model_id": "legacy-model",
    "base_url": "https://legacy.example.com/v1",
    "credential_ref": {
      "id": "cred_legacy",
      "source": {"kind": "removed-backend"},
      "secret_kind": "api-key",
      "revision": "rev_legacy"
    },
    "enabled": true
  }]
}
"#,
    )
    .expect("unreadable config");
    let mut dialog = AuthDialogState::new();
    dialog.provider = Some(OFFICIAL_PROVIDER_PRESET);
    dialog.base_url = "https://api.golutra.cn/v1".to_owned();
    dialog.model = "qwen-coder".to_owned();
    dialog.api_key = "test-key".to_owned();

    let review = build_auth_review(&dialog).expect("review");
    dialog.review = Some(review.clone());
    let lines = auth_review_lines(&dialog)
        .into_iter()
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .join("\n");

    match previous_home {
        Some(value) => unsafe {
            std::env::set_var("GOLUTRA_HOME", value);
        },
        None => unsafe {
            std::env::remove_var("GOLUTRA_HOME");
        },
    }
    assert!(review.replaces_unreadable_config);
    assert!(!review.updates_existing_profile);
    assert!(lines.contains("will replace unreadable provider config"));
}

#[tokio::test]
async fn auth_review_custom_provider_uses_derived_env_key() {
    let home = tempfile::tempdir().expect("home");
    let _guard = env_lock_guard().await;
    let previous_home = std::env::var("GOLUTRA_HOME").ok();
    unsafe {
        std::env::set_var("GOLUTRA_HOME", home.path());
    }
    let mut dialog = AuthDialogState::new();
    dialog.select_provider(CUSTOM_PROVIDER_PRESET);
    dialog.protocol = ProviderProtocol::OpenAiCompatible;
    dialog.base_url = "https://api.example.com/v1/".to_owned();
    dialog.model = "gpt-5.5".to_owned();
    dialog.credential_store = AuthCredentialStore::Environment;
    dialog.api_key_env = suggested_api_key_env(&dialog);

    let review = build_auth_review(&dialog).expect("review");

    match previous_home {
        Some(value) => unsafe {
            std::env::set_var("GOLUTRA_HOME", value);
        },
        None => unsafe {
            std::env::remove_var("GOLUTRA_HOME");
        },
    }
    let expected_env = generate_custom_provider_api_key_env(
        ProviderProtocol::OpenAiCompatible,
        "https://api.example.com/v1/",
    );
    assert!(review.preview_json.contains(&expected_env));
    assert!(!review.preview_json.contains("test-key"));
    assert!(
        !review
            .preview_json
            .contains("\"api_key_env\": \"GOLUTRA_PROVIDER_API_KEY\"")
    );
}

#[tokio::test]
async fn auth_review_includes_generation_config() {
    let home = tempfile::tempdir().expect("home");
    let _guard = env_lock_guard().await;
    let previous_home = std::env::var("GOLUTRA_HOME").ok();
    unsafe {
        std::env::set_var("GOLUTRA_HOME", home.path());
    }
    let mut dialog = AuthDialogState::new();
    dialog.select_provider(CUSTOM_PROVIDER_PRESET);
    dialog.protocol = ProviderProtocol::OpenAiCompatible;
    dialog.base_url = "https://api.example.com/v1".to_owned();
    dialog.model = "gpt-5.5".to_owned();
    dialog.api_key = "test-key".to_owned();
    dialog.enable_thinking = true;
    dialog.reasoning_effort = Some(ProviderReasoningEffort::High);
    dialog.context_window_size = "128000".to_owned();
    dialog.max_tokens = "512".to_owned();

    let review = build_auth_review(&dialog).expect("review");

    match previous_home {
        Some(value) => unsafe {
            std::env::set_var("GOLUTRA_HOME", value);
        },
        None => unsafe {
            std::env::remove_var("GOLUTRA_HOME");
        },
    }
    assert_eq!(
        review.advanced,
        "thinking=on, effort=high, context=128000, max_tokens=512"
    );
    assert!(review.preview_json.contains("\"enable_thinking\": true"));
    assert!(
        review
            .preview_json
            .contains("\"reasoning_effort\": \"high\"")
    );
    assert!(review.preview_json.contains("\"max_tokens\": 512"));
}

#[tokio::test]
async fn slash_auth_login_persists_native_anthropic_protocol_after_probe() {
    let dir = tempfile::tempdir().expect("dir");
    let home = tempfile::tempdir().expect("home");
    let _guard = env_lock_guard().await;
    let previous_home = std::env::var("GOLUTRA_HOME").ok();
    unsafe {
        std::env::set_var("GOLUTRA_HOME", home.path());
    }
    let transport = RuntimeTransport::for_cwd(dir.path())
        .await
        .expect("transport");
    let base_url = spawn_probe_server(
            r#"{"id":"msg-probe","type":"message","role":"assistant","model":"claude-test","content":[{"type":"text","text":"OK"}],"stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":1}}"#,
        )
        .await;
    let login = OpenAiCompatibleLogin {
        profile: "anthropic".to_owned(),
        protocol: ProviderProtocol::Anthropic,
        base_url,
        model: "claude-test".to_owned(),
        api_key_env: "GOLUTRA_PROVIDER_API_KEY".to_owned(),
        api_key: Some("test-key".to_owned()),
        credential_store: AuthCredentialStore::Ephemeral,
        credential_ref: None,
        generation_config: None,
        custom_headers: Vec::new(),
        scope: AuthConfigScope::User,
    };

    apply_auth_login(&transport, login)
        .await
        .expect("native protocol installed");
    let paths = provider_paths_for_tui().expect("paths");
    let settings = ProviderSettings::load(&paths.user_config).expect("settings");

    match previous_home {
        Some(value) => unsafe {
            std::env::set_var("GOLUTRA_HOME", value);
        },
        None => unsafe {
            std::env::remove_var("GOLUTRA_HOME");
        },
    }
    assert_eq!(
        settings.active_profile().expect("profile").protocol,
        ProviderProtocol::Anthropic
    );
}

#[tokio::test]
async fn auth_dialog_keeps_dialog_open_and_rolls_back_when_probe_fails() {
    let dir = tempfile::tempdir().expect("dir");
    let home = tempfile::tempdir().expect("home");
    let _guard = env_lock_guard().await;
    let previous_home = std::env::var("GOLUTRA_HOME").ok();
    unsafe {
        std::env::set_var("GOLUTRA_HOME", home.path());
    }
    let transport = RuntimeTransport::for_cwd(dir.path())
        .await
        .expect("transport");
    let mut app = TuiApp::new(
        ThreadId::new(),
        SessionId::new(),
        None,
        false,
        provider_status_message(),
        Some(AuthDialogState::new()),
    );
    {
        let dialog = app.auth_dialog.as_mut().expect("dialog");
        dialog.provider = Some(CUSTOM_PROVIDER_PRESET);
        dialog.protocol = ProviderProtocol::OpenAiCompatible;
        dialog.step = AuthDialogStep::Review;
        dialog.base_url = "http://127.0.0.1:9/v1".to_owned();
        dialog.model = "gpt-5.5".to_owned();
        dialog.api_key = "test-key".to_owned();
    }

    handle_auth_dialog_key(
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        &mut app,
        &transport,
    )
    .await
    .expect("enter review");

    let paths = provider_paths_for_tui().expect("paths");
    let settings = ProviderSettings::load(&paths.user_config).expect("settings");

    match previous_home {
        Some(value) => unsafe {
            std::env::set_var("GOLUTRA_HOME", value);
        },
        None => unsafe {
            std::env::remove_var("GOLUTRA_HOME");
        },
    }

    let dialog = app.auth_dialog.as_ref().expect("dialog still open");
    assert_eq!(dialog.step, AuthDialogStep::Review);
    assert!(
        dialog
            .error
            .as_deref()
            .is_some_and(|error| error.contains("provider probe failed"))
    );
    assert_eq!(app.status_message, "provider setup failed");
    assert!(settings.profiles.is_empty());
}

#[tokio::test]
async fn slash_auth_login_failure_reports_error_without_persisting_profile() {
    let dir = tempfile::tempdir().expect("dir");
    let home = tempfile::tempdir().expect("home");
    let _guard = env_lock_guard().await;
    let previous_home = std::env::var("GOLUTRA_HOME").ok();
    unsafe {
        std::env::set_var("GOLUTRA_HOME", home.path());
    }
    let transport = RuntimeTransport::for_cwd(dir.path())
        .await
        .expect("transport");
    let mut app = TuiApp::new(
        ThreadId::new(),
        SessionId::new(),
        None,
        false,
        provider_status_message(),
        None,
    );

    app.execute_auth_command(
        &transport,
        SlashAuthCommand::Login(Box::new(OpenAiCompatibleLogin {
            profile: "custom".to_owned(),
            protocol: ProviderProtocol::OpenAiCompatible,
            base_url: "http://127.0.0.1:9/v1".to_owned(),
            model: "gpt-5.5".to_owned(),
            api_key_env: "GOLUTRA_CUSTOM_PROVIDER_API_KEY_TEST".to_owned(),
            api_key: Some("test-key".to_owned()),
            credential_store: AuthCredentialStore::Ephemeral,
            credential_ref: None,
            generation_config: None,
            custom_headers: Vec::new(),
            scope: AuthConfigScope::User,
        })),
    )
    .await
    .expect("login command");

    let paths = provider_paths_for_tui().expect("paths");
    let settings = ProviderSettings::load(&paths.user_config).expect("settings");

    match previous_home {
        Some(value) => unsafe {
            std::env::set_var("GOLUTRA_HOME", value);
        },
        None => unsafe {
            std::env::remove_var("GOLUTRA_HOME");
        },
    }

    assert_eq!(app.status_message, "provider setup failed");
    assert!(settings.profiles.is_empty());
    assert!(app.command_messages.iter().any(|item| {
        item.title == "Auth failed"
            && item
                .body
                .iter()
                .any(|line| line.contains("provider probe failed"))
    }));
}

#[test]
fn auth_dialog_exposes_qwen_style_provider_groups() {
    let dialog = AuthDialogState::new();
    let group_lines = auth_group_lines(&dialog)
        .into_iter()
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .join("\n");

    assert!(group_lines.contains("Connect a Provider"));
    assert!(group_lines.contains("Golutra API"));
    assert!(group_lines.contains("Third-party Providers"));
    assert!(group_lines.contains("Custom Provider"));
}

#[test]
fn auth_dialog_exposes_third_party_provider_choices() {
    let mut dialog = AuthDialogState::new();
    dialog.step = AuthDialogStep::ThirdPartyChoice;
    let lines = auth_third_party_lines(&dialog)
        .into_iter()
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .join("\n");

    assert!(lines.contains("OpenAI"));
    assert!(lines.contains("OpenRouter"));
    assert!(lines.contains("DeepSeek"));
    assert!(lines.contains("Qwen / DashScope compatible"));
}

#[test]
fn auth_dialog_exposes_provider_specific_oauth_methods() {
    let preset = |profile: &str| {
        THIRD_PARTY_PROVIDER_PRESETS
            .iter()
            .find(|preset| preset.profile == profile)
            .copied()
            .expect("provider preset")
    };

    let mut openai = AuthDialogState::new();
    openai.select_provider(preset("openai"));
    assert_eq!(openai.step, AuthDialogStep::AuthMethod);
    assert_eq!(openai.auth_method_count(), 3);
    assert_eq!(
        openai.selected_oauth_method().map(|method| method.protocol),
        Some(ProviderProtocol::OpenAiResponses)
    );
    let openai_lines = auth_method_lines(&openai)
        .into_iter()
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(openai_lines.contains("ChatGPT Pro/Plus (browser)"));
    assert!(openai_lines.contains("ChatGPT Pro/Plus (headless)"));
    assert!(openai_lines.contains("API key"));

    let mut xai = AuthDialogState::new();
    xai.select_provider(preset("xai"));
    assert_eq!(xai.oauth_methods().len(), 2);
    assert_eq!(xai.auth_method_count(), 3);
    let xai_lines = auth_method_lines(&xai)
        .into_iter()
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(xai_lines.contains("xAI Grok OAuth (browser)"));
    assert!(xai_lines.contains("xAI Grok OAuth (headless/device)"));

    let mut copilot = AuthDialogState::new();
    copilot.select_provider(preset("github-copilot"));
    assert_eq!(copilot.auth_method_count(), 1);
    let copilot_lines = auth_method_lines(&copilot)
        .into_iter()
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(copilot_lines.contains("Login with GitHub Copilot"));
    assert!(!copilot_lines.contains("API key"));
}

#[test]
fn auth_dialog_custom_provider_exposes_protocol_step() {
    let mut dialog = AuthDialogState::new();
    dialog.select_provider(CUSTOM_PROVIDER_PRESET);
    assert_eq!(dialog.step, AuthDialogStep::Protocol);

    let lines = auth_protocol_lines(&dialog)
        .into_iter()
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .join("\n");

    assert!(lines.contains("Custom Provider · Step 1/7 · Protocol"));
    assert!(lines.contains("OpenAI-compatible"));
    assert!(lines.contains("Anthropic-compatible"));
    assert!(lines.contains("Gemini-compatible"));
}

#[tokio::test]
async fn auth_dialog_custom_provider_does_not_prefill_base_url_from_protocol() {
    let transport = RuntimeTransport::in_memory().await.expect("transport");
    let mut app = TuiApp::new(
        ThreadId::new(),
        SessionId::new(),
        None,
        false,
        provider_status_message(),
        Some(AuthDialogState::new()),
    );
    {
        let dialog = app.auth_dialog.as_mut().expect("dialog");
        dialog.select_provider(CUSTOM_PROVIDER_PRESET);
        dialog.selected = 0;
    }

    advance_auth_dialog(&mut app, &transport)
        .await
        .expect("advance");

    let dialog = app.auth_dialog.as_ref().expect("dialog");
    assert_eq!(dialog.step, AuthDialogStep::BaseUrl);
    assert!(dialog.base_url.is_empty());
}

#[tokio::test]
async fn auth_dialog_advances_for_native_custom_protocols() {
    let transport = RuntimeTransport::in_memory().await.expect("transport");
    let mut app = TuiApp::new(
        ThreadId::new(),
        SessionId::new(),
        None,
        false,
        provider_status_message(),
        Some(AuthDialogState::new()),
    );
    {
        let dialog = app.auth_dialog.as_mut().expect("dialog");
        dialog.select_provider(CUSTOM_PROVIDER_PRESET);
        dialog.protocol = ProviderProtocol::Anthropic;
        dialog.step = AuthDialogStep::Model;
        dialog.model = "claude-sonnet-4".to_owned();
        dialog.base_url = "https://api.anthropic.com/v1".to_owned();
        dialog.api_key = "test-key".to_owned();
    }

    advance_auth_dialog(&mut app, &transport)
        .await
        .expect("advance");

    let dialog = app.auth_dialog.as_ref().expect("dialog");
    assert_eq!(dialog.step, AuthDialogStep::AdvancedConfig);
    assert!(dialog.error.is_none());
}

#[test]
fn auth_dialog_exposes_recommended_models_and_custom_input() {
    let mut dialog = AuthDialogState::new();
    dialog.select_provider(OFFICIAL_PROVIDER_PRESET);
    dialog.step = AuthDialogStep::Model;
    let lines = auth_model_lines(&dialog)
        .into_iter()
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .join("\n");

    assert!(lines.contains("gpt-test"));
    assert!(lines.contains("Custom model"));
}

#[test]
fn auth_advanced_custom_headers_parse_without_persisting_literal_secrets() {
    let headers =
        parse_dialog_custom_headers("X-Client=golutra; X-Api-Key=@GOLUTRA_PROVIDER_HEADER_KEY")
            .expect("custom headers");

    assert_eq!(headers.len(), 2);
    assert!(matches!(
        headers[1].value,
        ProviderHeaderValue::Environment { ref key }
            if key == "GOLUTRA_PROVIDER_HEADER_KEY"
    ));
    assert!(parse_dialog_custom_headers("X-Api-Key=inline-secret").is_err());
}

#[test]
fn auth_advanced_header_field_accepts_full_text_input() {
    let mut dialog = AuthDialogState::new();
    dialog.step = AuthDialogStep::AdvancedConfig;
    dialog.advanced_selected = 4;
    for character in "X-Client=golutra".chars() {
        handle_auth_advanced_character(&mut dialog, character);
    }

    assert_eq!(dialog.custom_headers, "X-Client=golutra");
}

#[tokio::test]
async fn auth_model_input_accepts_numeric_custom_model_ids() {
    let transport = RuntimeTransport::in_memory().await.expect("transport");
    let mut app = TuiApp::new(
        ThreadId::new(),
        SessionId::new(),
        None,
        false,
        provider_status_message(),
        Some(AuthDialogState::new()),
    );
    {
        let dialog = app.auth_dialog.as_mut().expect("dialog");
        dialog.select_provider(OFFICIAL_PROVIDER_PRESET);
        dialog.step = AuthDialogStep::Model;
        dialog.api_key = "test-key".to_owned();
    }

    for character in "gpt-5.5".chars() {
        handle_auth_dialog_key(
            KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE),
            &mut app,
            &transport,
        )
        .await
        .expect("type model character");
    }

    let dialog = app.auth_dialog.as_ref().expect("dialog");
    assert_eq!(dialog.step, AuthDialogStep::Model);
    assert!(dialog.is_custom_model_selected());
    assert_eq!(dialog.model, "gpt-5.5");
}

#[tokio::test]
async fn auth_text_inputs_do_not_swallow_vim_key_characters() {
    let transport = RuntimeTransport::in_memory().await.expect("transport");
    let mut app = TuiApp::new(
        ThreadId::new(),
        SessionId::new(),
        None,
        false,
        provider_status_message(),
        Some(AuthDialogState::new()),
    );
    {
        let dialog = app.auth_dialog.as_mut().expect("dialog");
        dialog.select_provider(OFFICIAL_PROVIDER_PRESET);
        dialog.step = AuthDialogStep::ApiKey;
    }

    for character in "sk-key".chars() {
        handle_auth_dialog_key(
            KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE),
            &mut app,
            &transport,
        )
        .await
        .expect("type api key character");
    }
    {
        let dialog = app.auth_dialog.as_mut().expect("dialog");
        assert_eq!(dialog.api_key, "sk-key");
        dialog.step = AuthDialogStep::Model;
    }
    for character in "jkl-model".chars() {
        handle_auth_dialog_key(
            KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE),
            &mut app,
            &transport,
        )
        .await
        .expect("type model character");
    }

    let dialog = app.auth_dialog.as_ref().expect("dialog");
    assert_eq!(dialog.model, "jkl-model");
}

#[test]
fn new_idle_session_has_empty_transcript() {
    let mut app = TuiApp::new(
        ThreadId::new(),
        SessionId::new(),
        None,
        false,
        "ready (mock)".to_owned(),
        None,
    );
    app.projection = Some(UserProjection {
        session_id: app.session_id,
        task_id: None,
        status: golutra_core::TaskStatus::Idle,
        visible_steps: Vec::new(),
        pending_approval: None,
        final_message: None,
        residual_risks: Vec::new(),
    });

    assert!(transcript_items(&app).is_empty());
    assert_eq!(bottom_pane_height(&app), 3);
    assert!(provider_footer_line(&app).is_none());

    let mut terminal = Terminal::new(TestBackend::new(80, 12)).expect("terminal");
    terminal
        .draw(|frame| draw_ui(frame, &mut app))
        .expect("draw empty session");
    let separator = (0..80)
        .filter_map(|x| terminal.backend().buffer().cell((x, 1)))
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert_eq!(separator, "─".repeat(80));
}

#[test]
fn normal_transcript_keeps_only_user_visible_runtime_milestones() {
    let mut app = TuiApp::new(
        ThreadId::new(),
        SessionId::new(),
        None,
        false,
        "ready (mock)".to_owned(),
        None,
    );
    app.projection = Some(UserProjection {
        session_id: app.session_id,
        task_id: None,
        status: golutra_core::TaskStatus::Completed,
        visible_steps: vec![
            VisibleStep {
                label: "ProviderStarted".to_owned(),
                status: "Running".to_owned(),
                summary: "provider request started".to_owned(),
            },
            VisibleStep {
                label: "ToolCompleted".to_owned(),
                status: "Running".to_owned(),
                summary: "file written".to_owned(),
            },
            VisibleStep {
                label: "TaskCompleted".to_owned(),
                status: "Completed".to_owned(),
                summary: "runtime task finished".to_owned(),
            },
        ],
        pending_approval: None,
        final_message: None,
        residual_risks: Vec::new(),
    });

    let items = transcript_items(&app);

    assert_eq!(items.len(), 1);
    assert_eq!(items[0].title, "Tool Completed");
}

#[test]
fn developer_panel_exposes_governance_without_leaking_into_normal_view() {
    let session_id = SessionId::new();
    let task_id = TaskId::new();
    let events = vec![
        transcript_event(
            1,
            session_id,
            task_id,
            RuntimeEventType::ProviderStarted,
            json!({"summary": "provider request started"}),
        ),
        transcript_event(
            2,
            session_id,
            task_id,
            RuntimeEventType::ToolCompleted,
            json!({"summary": "file written"}),
        ),
        transcript_event(
            3,
            session_id,
            task_id,
            RuntimeEventType::VerificationCompleted,
            json!({"summary": "verification result: Pass"}),
        ),
        transcript_event(
            4,
            session_id,
            task_id,
            RuntimeEventType::PostTaskReviewed,
            json!({"summary": "deep post-task review outcome: pass"}),
        ),
        transcript_event(
            5,
            session_id,
            task_id,
            RuntimeEventType::EvaluationCompleted,
            json!({"summary": "task evaluation verdict: Pass"}),
        ),
        transcript_event(
            6,
            session_id,
            task_id,
            RuntimeEventType::ImprovementCandidateCreated,
            json!({"summary": "improvement candidate proposed"}),
        ),
        transcript_event(
            7,
            session_id,
            task_id,
            RuntimeEventType::RegressionCompleted,
            json!({"summary": "candidate regression completed"}),
        ),
        transcript_event(
            8,
            session_id,
            task_id,
            RuntimeEventType::PromotionDecided,
            json!({"summary": "candidate promotion decision: Approve"}),
        ),
    ];
    let verification = golutra_core::VerificationRecord {
        verification_id: golutra_core::VerificationId::new(),
        task_id,
        objective: "write a file".to_owned(),
        completion_criteria: vec!["file exists".to_owned()],
        checks: vec![golutra_core::VerificationCheck {
            kind: golutra_core::VerificationCheckKind::ObjectiveValidation,
            name: "file_exists".to_owned(),
            command: None,
            passed: true,
            evidence_refs: Vec::new(),
            message: "file exists".to_owned(),
        }],
        evidence_refs: Vec::new(),
        result: golutra_core::VerificationResult::Pass,
        policy_status: "verified".to_owned(),
        residual_risks: Vec::new(),
        plan_id: None,
        assertions: Vec::new(),
        source: Default::default(),
        independence: Default::default(),
        environment_digest: None,
    };
    let debug_projection = golutra_protocol::DebugProjection {
        session_id,
        task_id: Some(task_id),
        events: events.clone(),
        event_window: golutra_protocol::DebugEventWindow {
            start_cursor: Some(1),
            end_cursor: Some(2),
            has_more_before: false,
            limit: 512,
        },
        busy_policy_decisions: Vec::new(),
        tool_results: Vec::new(),
        artifacts: Vec::new(),
        evidence: Vec::new(),
        verification: Some(verification),
        loop_decisions: Vec::new(),
        post_task_jobs: Vec::new(),
        failure_diagnosis: None,
        failure_episodes: Vec::new(),
        diagnostic_slice: None,
        replay_execution: None,
        external_evaluations: Vec::new(),
        causal_comparisons: Vec::new(),
        trace_complete: true,
        missing_sections: Vec::new(),
        retention_losses: Vec::new(),
    };
    let diagnosis = json!({
        "diagnosis_id": "diagnosis-test",
        "source_task_id": task_id,
        "taxonomy": {"domain": "tool", "code": "tool_failed"},
        "summary": "tool failed during verification",
        "trigger_event_refs": [events[0].id],
        "causal_event_refs": [events[0].id],
        "expected_behavior": "tool succeeds",
        "actual_behavior": "tool failed",
        "counterfactual": "use a corrected invocation",
        "confidence": 90,
        "code_targets": [],
        "regression_commands": ["cargo test"],
        "analyzer_version": "test",
        "created_at": chrono::Utc::now(),
    });
    let mut debug_value = serde_json::to_value(debug_projection).expect("debug projection");
    debug_value["failure_diagnosis"] = diagnosis.clone();
    debug_value["failure_episodes"] = json!([
        {
            "episode_id": "episode-active",
            "source_task_id": task_id,
            "status": "active",
            "primary_signal": {
                "event_ref": events[0].id,
                "kind": "self_check",
                "signal_key": "self_check:verification",
                "summary": "verification failed"
            },
            "opened_at": chrono::Utc::now(),
            "updated_at": chrono::Utc::now()
        },
        {
            "episode_id": "episode-recovered",
            "source_task_id": task_id,
            "status": "recovered",
            "primary_signal": {
                "event_ref": events[0].id,
                "kind": "producer",
                "signal_key": "tool:shell",
                "summary": "shell failed"
            },
            "recovered_by": {
                "event_ref": events[1].id,
                "signal_key": "tool:shell",
                "summary": "shell recovered"
            },
            "opened_at": chrono::Utc::now(),
            "updated_at": chrono::Utc::now()
        },
        {
            "episode_id": "episode-superseded",
            "source_task_id": task_id,
            "status": "superseded",
            "primary_signal": {
                "event_ref": events[0].id,
                "kind": "producer",
                "signal_key": "provider:mock",
                "summary": "provider failed"
            },
            "superseded_by": "episode-active",
            "opened_at": chrono::Utc::now(),
            "updated_at": chrono::Utc::now()
        }
    ]);
    debug_value["diagnostic_slice"] = json!({
        "slice_id": "slice-test",
        "source_task_id": task_id,
        "diagnosis": diagnosis,
        "event_refs": [events[0].id],
        "artifact_refs": [],
        "evidence_refs": [],
        "omitted_event_count": 7,
        "complete": true,
        "generated_at": chrono::Utc::now(),
    });
    let debug_projection: golutra_protocol::DebugProjection =
        serde_json::from_value(debug_value).expect("diagnostic debug projection");

    let rows = developer_panel_rows(&debug_projection, 2);
    let row_text = rows
        .iter()
        .map(|row| format!("{row:?}"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(row_text.contains("verify Pass checks=1 evidence=0 risks=0"));
    assert!(
        row_text.contains(
            "reviews=1 evaluations=1 improvements=1 regressions=1 promotions=1 applied=0"
        )
    );
    assert!(row_text.contains("sequence_no: 7"));
    assert!(row_text.contains("RegressionCompleted/Runtime"));
    assert!(row_text.contains("sequence_no: 8"));
    assert!(row_text.contains("PromotionDecided/Runtime"));
    assert!(row_text.contains("diagnosis Tool/tool_failed confidence=90"));
    assert!(row_text.contains("failure_episodes active=1 recovered=1 superseded=1"));
    assert!(row_text.contains("slice_events=1 omitted=7 complete=true"));

    let mut app = TuiApp::new(
        ThreadId::new(),
        session_id,
        Some(task_id),
        false,
        "ready (mock)".to_owned(),
        None,
    );
    app.developer_projection = Some(debug_projection);
    let mut normal_terminal = Terminal::new(TestBackend::new(120, 24)).expect("terminal");
    normal_terminal
        .draw(|frame| draw_ui(frame, &mut app))
        .expect("draw normal view");
    let normal_text = normal_terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(!normal_text.contains("Developer runtime"));

    app.debug_mode = true;
    let mut developer_terminal = Terminal::new(TestBackend::new(120, 24)).expect("terminal");
    developer_terminal
        .draw(|frame| draw_ui(frame, &mut app))
        .expect("draw developer view");
    let developer_text = developer_terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(developer_text.contains("Developer runtime"));
    assert!(developer_text.contains("▸ facts"));
    assert!(!developer_text.contains("verify Pass"));

    let layout = app.layout;
    let developer_area = layout.developer.expect("developer area");
    assert_eq!(layout.transcript.y, developer_area.y);
    assert_eq!(layout.transcript.height, developer_area.height);
    assert_eq!(layout.transcript.width, developer_area.width);
    assert_eq!(
        developer_area.x,
        layout.transcript.x + layout.transcript.width
    );

    let toggle = developer_facts_toggle_rect(developer_area);
    assert_eq!(
        developer_terminal
            .backend()
            .buffer()
            .cell((toggle.x, toggle.y))
            .map(|cell| cell.symbol()),
        Some("▸")
    );
    handle_mouse(
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: toggle.x,
            row: toggle.y,
            modifiers: KeyModifiers::NONE,
        },
        &mut app,
    );
    assert!(app.developer_facts_expanded);
    developer_terminal
        .draw(|frame| draw_ui(frame, &mut app))
        .expect("draw expanded developer facts");
    let expanded_text = developer_terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(expanded_text.contains("▾ facts"));
    assert!(expanded_text.contains("verify Pass"));
    assert!(expanded_text.contains("diagnosis Tool/tool_failed"));
    assert_eq!(
        developer_terminal
            .backend()
            .buffer()
            .cell((toggle.x, toggle.y))
            .map(|cell| cell.symbol()),
        Some("▾")
    );

    handle_mouse(
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: toggle.x,
            row: toggle.y,
            modifiers: KeyModifiers::NONE,
        },
        &mut app,
    );
    assert!(!app.developer_facts_expanded);

    let padded_toggle = developer_facts_toggle_hit_rect(developer_area);
    assert!(padded_toggle.x < toggle.x);
    handle_mouse(
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: padded_toggle.x,
            row: padded_toggle.y,
            modifiers: KeyModifiers::NONE,
        },
        &mut app,
    );
    assert!(app.developer_facts_expanded);
}

#[test]
fn developer_event_details_toggle_between_ellipsis_and_complete_wrapped_text() {
    let session_id = SessionId::new();
    let task_id = TaskId::new();
    let tail = "developer-event-tail-marker";
    let summary = format!("{} {tail}", "complete runtime event detail ".repeat(12));
    let mut app = TuiApp::new(
        ThreadId::new(),
        session_id,
        Some(task_id),
        true,
        "ready (mock)".to_owned(),
        None,
    );
    app.developer_projection = Some(debug_projection_with_events(
        session_id,
        Some(task_id),
        vec![transcript_event(
            1,
            session_id,
            task_id,
            RuntimeEventType::StepCompleted,
            json!({"summary": summary}),
        )],
    ));
    let mut terminal = Terminal::new(TestBackend::new(120, 30)).expect("terminal");

    terminal
        .draw(|frame| draw_ui(frame, &mut app))
        .expect("draw collapsed developer event");
    let collapsed = terminal_buffer_text(&terminal);
    assert!(collapsed.contains("▸ facts"));
    assert!(collapsed.contains('…'));
    assert!(!collapsed.contains(tail));

    app.toggle_developer_facts();
    terminal
        .draw(|frame| draw_ui(frame, &mut app))
        .expect("draw expanded developer event");
    let expanded = terminal_buffer_text(&terminal);
    assert!(expanded.contains("▾ facts"));
    assert!(expanded.contains(tail));
    assert!(app.developer_event_layout.row_count > 1);

    app.toggle_developer_facts();
    terminal
        .draw(|frame| draw_ui(frame, &mut app))
        .expect("redraw collapsed developer event");
    let collapsed_again = terminal_buffer_text(&terminal);
    assert!(collapsed_again.contains("▸ facts"));
    assert!(collapsed_again.contains('…'));
    assert!(!collapsed_again.contains(tail));
}

#[test]
fn developer_detail_reflow_keeps_the_first_visible_event_as_anchor() {
    let session_id = SessionId::new();
    let task_id = TaskId::new();
    let events = (1..=30)
        .map(|sequence_no| {
            transcript_event(
                sequence_no,
                session_id,
                task_id,
                RuntimeEventType::StepCompleted,
                json!({
                    "summary": format!(
                        "event {sequence_no} {}",
                        "has complete detail that wraps at the developer pane width ".repeat(3)
                    )
                }),
            )
        })
        .collect();
    let mut app = TuiApp::new(
        ThreadId::new(),
        session_id,
        Some(task_id),
        true,
        "ready (mock)".to_owned(),
        None,
    );
    app.developer_projection = Some(debug_projection_with_events(
        session_id,
        Some(task_id),
        events,
    ));
    let mut terminal = Terminal::new(TestBackend::new(120, 24)).expect("terminal");
    terminal
        .draw(|frame| draw_ui(frame, &mut app))
        .expect("draw collapsed developer events");
    let page_rows = app.developer_event_layout.page_rows.max(1);
    for _ in 0..5 {
        app.scroll_developer(TranscriptScrollAction::LineUp, page_rows);
    }
    let collapsed_anchor = app
        .first_visible_developer_sequence()
        .expect("collapsed anchor");

    app.toggle_developer_facts();
    let expanded_anchor = app
        .first_visible_developer_sequence()
        .expect("expanded anchor");
    assert_eq!(expanded_anchor, collapsed_anchor);

    let mut narrow_terminal = Terminal::new(TestBackend::new(90, 24)).expect("narrow terminal");
    narrow_terminal
        .draw(|frame| draw_ui(frame, &mut app))
        .expect("reflow expanded developer events");
    let narrow_anchor = app
        .first_visible_developer_sequence()
        .expect("narrow anchor");
    assert_eq!(narrow_anchor, collapsed_anchor);

    app.toggle_developer_facts();
    let collapsed_again_anchor = app
        .first_visible_developer_sequence()
        .expect("collapsed-again anchor");
    assert_eq!(collapsed_again_anchor, collapsed_anchor);
}

#[test]
fn developer_detail_collapse_keeps_a_tail_event_at_the_top() {
    let session_id = SessionId::new();
    let task_id = TaskId::new();
    let events = (1..=30)
        .map(|sequence_no| {
            transcript_event(
                sequence_no,
                session_id,
                task_id,
                RuntimeEventType::StepCompleted,
                json!({
                    "summary": format!(
                        "event {sequence_no} {}",
                        "has enough detail to wrap across several visual lines ".repeat(4)
                    )
                }),
            )
        })
        .collect();
    let mut app = TuiApp::new(
        ThreadId::new(),
        session_id,
        Some(task_id),
        true,
        "ready (mock)".to_owned(),
        None,
    );
    app.developer_projection = Some(debug_projection_with_events(
        session_id,
        Some(task_id),
        events,
    ));
    let mut terminal = Terminal::new(TestBackend::new(120, 24)).expect("terminal");
    terminal
        .draw(|frame| draw_ui(frame, &mut app))
        .expect("draw collapsed developer events");
    app.toggle_developer_facts();
    let top_row = app
        .developer_event_layout
        .row_for_sequence(27)
        .expect("expanded event row");
    let layout = app.developer_event_layout.clone();
    app.set_developer_top_row(&layout, top_row);
    let tail_anchor = app.first_visible_developer_sequence().expect("tail anchor");
    assert_eq!(tail_anchor, 27);

    app.toggle_developer_facts();
    assert_eq!(app.first_visible_developer_sequence(), Some(tail_anchor));
    terminal
        .draw(|frame| draw_ui(frame, &mut app))
        .expect("redraw collapsed tail anchor");
    let developer = app.layout.developer.expect("developer area");
    let first_event_row = (developer.x..developer.x.saturating_add(developer.width))
        .filter_map(|x| {
            terminal
                .backend()
                .buffer()
                .cell((x, developer.y.saturating_add(1)))
        })
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(first_event_row.contains("#27 "), "{first_event_row}");
}

#[test]
fn expanded_developer_event_scrolls_by_visual_line() {
    let session_id = SessionId::new();
    let task_id = TaskId::new();
    let mut app = TuiApp::new(
        ThreadId::new(),
        session_id,
        Some(task_id),
        true,
        "ready (mock)".to_owned(),
        None,
    );
    app.developer_projection = Some(debug_projection_with_events(
        session_id,
        Some(task_id),
        vec![transcript_event(
            1,
            session_id,
            task_id,
            RuntimeEventType::StepCompleted,
            json!({"summary": "visual line scrolling ".repeat(80)}),
        )],
    ));
    let mut terminal = Terminal::new(TestBackend::new(120, 18)).expect("terminal");
    terminal
        .draw(|frame| draw_ui(frame, &mut app))
        .expect("draw collapsed long event");
    app.toggle_developer_facts();

    let page_rows = app.developer_event_layout.page_rows.max(1);
    assert!(app.developer_event_layout.row_count > page_rows);
    let before = app
        .developer_event_layout
        .visible_window(
            app.developer_scroll.offset_from_bottom,
            app.developer_top_row_override,
        )
        .start;
    app.scroll_developer(TranscriptScrollAction::LineDown, page_rows);
    let after = app
        .developer_event_layout
        .visible_window(
            app.developer_scroll.offset_from_bottom,
            app.developer_top_row_override,
        )
        .start;

    assert_eq!(after, before + 1);
    assert_eq!(app.first_visible_developer_sequence(), Some(1));
}

#[test]
fn transcript_wraps_long_body_lines_to_the_available_width() {
    let head_marker = "transcript-head-remains-reachable";
    let marker = "transcript-tail-remains-visible";
    let mut app = TuiApp::new(
        ThreadId::new(),
        SessionId::new(),
        None,
        false,
        "ready (mock)".to_owned(),
        None,
    );
    app.command_messages.push(TranscriptItem {
        role: TranscriptRole::Assistant,
        title: "Long response".to_owned(),
        body: vec![format!(
            "{head_marker} {} {marker}",
            "visible response content ".repeat(24)
        )],
    });
    let mut terminal = Terminal::new(TestBackend::new(64, 24)).expect("terminal");

    terminal
        .draw(|frame| draw_ui(frame, &mut app))
        .expect("draw wrapped transcript");

    assert!(terminal_buffer_text(&terminal).contains(marker));
    assert!(app.transcript_scroll.row_count > transcript_render_rows(&app).len());

    let visible_rows = app.layout.transcript.height.saturating_sub(1) as usize;
    app.scroll_transcript(TranscriptScrollAction::Top, visible_rows);
    terminal
        .draw(|frame| draw_ui(frame, &mut app))
        .expect("draw transcript from the top");
    assert!(terminal_buffer_text(&terminal).contains(head_marker));
}

#[tokio::test]
async fn developer_projection_is_only_loaded_in_explicit_debug_mode() {
    let transport = RuntimeTransport::in_memory().await.expect("transport");
    let mut app = TuiApp::new(
        ThreadId::new(),
        SessionId::new(),
        None,
        false,
        "ready (mock)".to_owned(),
        None,
    );

    app.refresh(&transport).await.expect("normal refresh");
    assert!(app.developer_projection.is_none());

    app.set_debug_mode(&transport, true)
        .await
        .expect("enable developer mode");
    assert!(app.developer_projection.is_some());
    assert!(app.developer_error.is_none());

    app.set_debug_mode(&transport, false)
        .await
        .expect("disable developer mode");
    assert!(app.developer_projection.is_none());
    assert!(app.developer_error.is_none());
}

#[tokio::test]
async fn developer_mode_observes_runtime_verification_and_evaluation_events() {
    let transport = RuntimeTransport::in_memory().await.expect("transport");
    let session_id = SessionId::new();
    let thread_id = ThreadId::new();
    transport
        .send_command(session_command(
            session_id,
            SessionCommandKind::Prompt,
            json!({
                "prompt": "hello",
                "_thread_id": thread_id.to_string(),
            }),
        ))
        .await
        .expect("start task");

    let completed = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let projection = transport
                .query(RuntimeQuery {
                    query_id: QueryId::new(),
                    session_id,
                    task_id: None,
                    kind: RuntimeQueryKind::UserProjection,
                    requester: ActorKind::Tui,
                    cursor: None,
                    timestamp: chrono::Utc::now(),
                })
                .await
                .expect("projection");
            if golutra_client::projection_status(&projection)
                == Some(golutra_core::TaskStatus::Completed)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(completed.is_ok(), "runtime task should complete");

    let mut app = TuiApp::new(
        thread_id,
        session_id,
        None,
        false,
        "ready (mock)".to_owned(),
        None,
    );
    app.set_debug_mode(&transport, true)
        .await
        .expect("enable developer mode");
    let projection = app
        .developer_projection
        .as_ref()
        .expect("developer projection");
    assert!(projection.verification.is_some());
    assert!(
        projection
            .events
            .iter()
            .any(|event| { event.event_type == RuntimeEventType::VerificationCompleted })
    );
    assert!(
        projection
            .events
            .iter()
            .any(|event| { event.event_type == RuntimeEventType::EvaluationCompleted })
    );
}

#[test]
fn failed_loop_decision_is_visible_in_transcript() {
    let mut app = TuiApp::new(
        ThreadId::new(),
        SessionId::new(),
        None,
        false,
        "ready (mock)".to_owned(),
        None,
    );
    app.projection = Some(UserProjection {
        session_id: app.session_id,
        task_id: Some(TaskId::new()),
        status: golutra_core::TaskStatus::Failed,
        visible_steps: vec![VisibleStep {
            label: "LoopDecided".to_owned(),
            status: "Running".to_owned(),
            summary: "runtime task execution failed: provider failed: model not found".to_owned(),
        }],
        pending_approval: None,
        final_message: None,
        residual_risks: Vec::new(),
    });

    let items = transcript_items(&app);

    assert_eq!(items[0].title, "Loop Decided");
    assert!(items[0].body[0].contains("model not found"));
}

#[test]
fn approval_transcript_shows_tool_resource_and_reason() {
    let event = transcript_event(
        1,
        SessionId::new(),
        TaskId::new(),
        RuntimeEventType::ApprovalRequested,
        json!({
            "summary": "approval required for shell",
            "request": {
                "tool_name": "shell",
                "resource": "cargo test --workspace",
                "reason": "process command requires explicit user approval"
            }
        }),
    );

    let items = event_transcript_items(&[event]);

    assert_eq!(items.len(), 1);
    assert_eq!(items[0].title, "Approval required");
    assert_eq!(items[0].body[0], "shell: cargo test --workspace");
    assert!(items[0].body[1].contains("explicit user approval"));
}

#[test]
fn file_changes_are_compact_in_normal_mode_and_detailed_in_developer_mode() {
    let session_id = SessionId::new();
    let task_id = TaskId::new();
    let event = transcript_event(
        1,
        session_id,
        task_id,
        RuntimeEventType::ToolCompleted,
        json!({
            "summary": "file edited",
            "file_changes": [{
                "path": "src/lib.rs",
                "kind": "modified",
                "added_lines": 2,
                "removed_lines": 1
            }],
            "turn_change_summary": {
                "files": [{
                    "path": "src/lib.rs",
                    "kind": "modified",
                    "added_lines": 2,
                    "removed_lines": 1
                }],
                "added_lines": 2,
                "removed_lines": 1,
                "stats_complete": true
            }
        }),
    );
    let mut app = TuiApp::new(
        ThreadId::new(),
        session_id,
        Some(task_id),
        false,
        "ready (mock)".to_owned(),
        None,
    );
    app.events = vec![event.clone()];
    app.rebuild_event_projections();
    app.projection = Some(UserProjection {
        session_id,
        task_id: Some(task_id),
        status: golutra_core::TaskStatus::Completed,
        visible_steps: Vec::new(),
        pending_approval: None,
        final_message: None,
        residual_risks: Vec::new(),
    });

    let mut normal_terminal = Terminal::new(TestBackend::new(160, 24)).expect("terminal");
    normal_terminal
        .draw(|frame| draw_ui(frame, &mut app))
        .expect("draw normal changes");
    let normal_text = normal_terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(normal_text.contains("Edited 1 file (+2 -1)"));
    assert!(normal_text.contains("src/lib.rs  +2 -1"));
    assert!(!normal_text.contains("changes files="));

    app.debug_mode = true;
    app.developer_facts_expanded = true;
    app.developer_projection = Some(golutra_protocol::DebugProjection {
        session_id,
        task_id: Some(task_id),
        events: vec![event],
        event_window: golutra_protocol::DebugEventWindow {
            start_cursor: Some(1),
            end_cursor: Some(1),
            has_more_before: false,
            limit: 256,
        },
        busy_policy_decisions: Vec::new(),
        tool_results: Vec::new(),
        artifacts: Vec::new(),
        evidence: Vec::new(),
        verification: None,
        loop_decisions: Vec::new(),
        post_task_jobs: Vec::new(),
        failure_diagnosis: None,
        failure_episodes: Vec::new(),
        diagnostic_slice: None,
        replay_execution: None,
        external_evaluations: Vec::new(),
        causal_comparisons: Vec::new(),
        trace_complete: true,
        missing_sections: Vec::new(),
        retention_losses: Vec::new(),
    });
    let mut developer_terminal = Terminal::new(TestBackend::new(160, 24)).expect("terminal");
    developer_terminal
        .draw(|frame| draw_ui(frame, &mut app))
        .expect("draw developer changes");
    let developer_text = developer_terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(developer_text.contains("changes files=1 +2 -1 complete=true"));
}

fn transcript_event(
    sequence_no: u64,
    session_id: SessionId,
    task_id: TaskId,
    event_type: RuntimeEventType,
    payload: serde_json::Value,
) -> RuntimeEvent {
    RuntimeEvent {
        schema_version: golutra_core::RUNTIME_EVENT_SCHEMA_VERSION,
        causal_context: Default::default(),
        causal_links: Vec::new(),
        id: golutra_core::EventId::new(),
        sequence_no,
        session_id,
        turn_id: None,
        task_id: Some(task_id),
        parent_event_id: None,
        event_type,
        timestamp: chrono::Utc::now(),
        source: golutra_protocol::RuntimeEventSource::Runtime,
        payload,
        payload_ref: None,
        durable: true,
    }
}

fn debug_projection_with_events(
    session_id: SessionId,
    task_id: Option<TaskId>,
    events: Vec<RuntimeEvent>,
) -> golutra_protocol::DebugProjection {
    golutra_protocol::DebugProjection {
        session_id,
        task_id,
        event_window: golutra_protocol::DebugEventWindow {
            start_cursor: events.first().map(|event| event.sequence_no),
            end_cursor: events.last().map(|event| event.sequence_no),
            has_more_before: false,
            limit: 256,
        },
        events,
        busy_policy_decisions: Vec::new(),
        tool_results: Vec::new(),
        artifacts: Vec::new(),
        evidence: Vec::new(),
        verification: None,
        loop_decisions: Vec::new(),
        post_task_jobs: Vec::new(),
        failure_diagnosis: None,
        failure_episodes: Vec::new(),
        diagnostic_slice: None,
        replay_execution: None,
        external_evaluations: Vec::new(),
        causal_comparisons: Vec::new(),
        trace_complete: true,
        missing_sections: Vec::new(),
        retention_losses: Vec::new(),
    }
}

fn terminal_buffer_text(terminal: &Terminal<TestBackend>) -> String {
    terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect()
}

#[tokio::test]
async fn transcript_operation_details_toggle_with_ctrl_o_and_mouse() {
    let transport = RuntimeTransport::in_memory().await.expect("transport");
    let session_id = SessionId::new();
    let task_id = TaskId::new();
    let tool_call_id = golutra_core::ToolCallId::new();
    let mut app = TuiApp::new(
        ThreadId::new(),
        session_id,
        Some(task_id),
        false,
        "ready (mock)".to_owned(),
        None,
    );
    app.events = vec![transcript_event(
        1,
        session_id,
        task_id,
        RuntimeEventType::ToolCompleted,
        json!({
            "envelope": {
                "tool_call_id": tool_call_id,
                "tool_name": "edit_file",
                "status": "ok",
                "summary": "file edited",
                "structured_facts": {}
            },
            "file_changes": [{
                "path": "src/lib.rs",
                "kind": "modified",
                "added_lines": 1,
                "removed_lines": 1
            }],
            "diff_previews": [{
                "path": "src/lib.rs",
                "lines": ["-old value", "+new value"],
                "truncated": false
            }]
        }),
    )];
    app.projection = Some(UserProjection {
        session_id,
        task_id: Some(task_id),
        status: golutra_core::TaskStatus::Completed,
        visible_steps: Vec::new(),
        pending_approval: None,
        final_message: None,
        residual_risks: Vec::new(),
    });

    assert!(
        transcript_items(&app)[0]
            .body
            .iter()
            .all(|line| line != "-old value")
    );
    handle_key(
        KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL),
        &mut app,
        &transport,
    )
    .await
    .expect("expand all transcript operations");
    assert!(
        transcript_items(&app)[0]
            .body
            .iter()
            .any(|line| line == "-old value")
    );
    handle_key(
        KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL),
        &mut app,
        &transport,
    )
    .await
    .expect("collapse all transcript operations");

    let mut terminal = Terminal::new(TestBackend::new(100, 20)).expect("terminal");
    terminal
        .draw(|frame| draw_ui(frame, &mut app))
        .expect("draw collapsed operation");
    let (_, toggle) = transcript_toggle_regions(&app, app.layout.transcript)
        .into_iter()
        .next()
        .expect("visible operation toggle");
    handle_mouse(
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: toggle.x,
            row: toggle.y,
            modifiers: KeyModifiers::NONE,
        },
        &mut app,
    );

    assert!(
        transcript_items(&app)[0]
            .body
            .iter()
            .any(|line| line == "+new value")
    );
}

#[test]
fn resumed_completed_history_renders_user_prompt_and_terminal_steps() {
    let session_id = SessionId::new();
    let task_id = TaskId::new();
    let mut app = TuiApp::new(
        ThreadId::new(),
        session_id,
        None,
        false,
        "ready (mock)".to_owned(),
        None,
    );
    app.events = vec![
        transcript_event(
            1,
            session_id,
            task_id,
            RuntimeEventType::TaskCreated,
            json!({
                "payload": {
                    "prompt": "write file chain.txt with content ok"
                },
                "summary": "runtime lane started task",
            }),
        ),
        transcript_event(
            2,
            session_id,
            task_id,
            RuntimeEventType::ToolCompleted,
            json!({"summary": "file written"}),
        ),
        transcript_event(
            3,
            session_id,
            task_id,
            RuntimeEventType::TaskCompleted,
            json!({
                "summary": "runtime task finished with Completed",
                "status": "completed",
            }),
        ),
        transcript_event(
            4,
            session_id,
            task_id,
            RuntimeEventType::AssistantMessage,
            json!({
                "summary": "Completed: file written",
                "content": "Completed: file written",
            }),
        ),
    ];
    app.projection = Some(UserProjection {
        session_id,
        task_id: Some(task_id),
        status: golutra_core::TaskStatus::Completed,
        visible_steps: vec![
            VisibleStep {
                label: "ToolCompleted".to_owned(),
                status: "Running".to_owned(),
                summary: "file written".to_owned(),
            },
            VisibleStep {
                label: "TaskCompleted".to_owned(),
                status: "Completed".to_owned(),
                summary: "runtime task finished with Completed".to_owned(),
            },
        ],
        pending_approval: None,
        final_message: Some("Completed: file written".to_owned()),
        residual_risks: Vec::new(),
    });

    let items = transcript_items(&app);
    let titles = items
        .iter()
        .map(|item| item.title.as_str())
        .collect::<Vec<_>>();
    let body = items
        .iter()
        .flat_map(|item| item.body.iter())
        .cloned()
        .collect::<Vec<_>>()
        .join("\n");

    assert_eq!(titles, vec!["You", "Tool Completed", "Golutra"]);
    assert!(body.contains("write file chain.txt with content ok"));
    assert!(body.contains("file written"));
    assert!(!body.contains("runtime task finished with Completed"));
    assert!(body.contains("Completed: file written"));
}

#[test]
fn transcript_groups_multiple_turns_from_events_top_to_bottom() {
    let session_id = SessionId::new();
    let first_task = TaskId::new();
    let second_task = TaskId::new();
    let mut app = TuiApp::new(
        ThreadId::new(),
        session_id,
        None,
        false,
        "ready (mock)".to_owned(),
        None,
    );
    app.events = vec![
        transcript_event(
            1,
            session_id,
            first_task,
            RuntimeEventType::TaskCreated,
            json!({"payload": {"prompt": "hi"}}),
        ),
        transcript_event(
            2,
            session_id,
            first_task,
            RuntimeEventType::AssistantMessage,
            json!({"content": "Hello"}),
        ),
        transcript_event(
            3,
            session_id,
            first_task,
            RuntimeEventType::TaskCompleted,
            json!({
                "summary": "runtime task finished with Completed",
                "status": "completed"
            }),
        ),
        transcript_event(
            4,
            session_id,
            second_task,
            RuntimeEventType::TaskCreated,
            json!({"payload": {"prompt": "what next"}}),
        ),
        transcript_event(
            5,
            session_id,
            second_task,
            RuntimeEventType::AssistantMessage,
            json!({"content": "Tell me what to work on."}),
        ),
        transcript_event(
            6,
            session_id,
            second_task,
            RuntimeEventType::TaskCompleted,
            json!({
                "summary": "runtime task finished with Completed",
                "status": "completed"
            }),
        ),
    ];
    app.projection = Some(UserProjection {
        session_id,
        task_id: Some(second_task),
        status: golutra_core::TaskStatus::Completed,
        visible_steps: Vec::new(),
        pending_approval: None,
        final_message: Some("stale projection final should not duplicate".to_owned()),
        residual_risks: Vec::new(),
    });

    let items = transcript_items(&app);
    let titles = items
        .iter()
        .map(|item| item.title.as_str())
        .collect::<Vec<_>>();
    let bodies = items
        .iter()
        .map(|item| item.body.join("\n"))
        .collect::<Vec<_>>();

    assert_eq!(titles, vec!["You", "Golutra", "You", "Golutra"]);
    assert_eq!(bodies[0], "hi");
    assert_eq!(bodies[1], "Hello");
    assert_eq!(bodies[2], "what next");
    assert_eq!(bodies[3], "Tell me what to work on.");
    assert!(
        bodies
            .iter()
            .all(|body| !body.contains("stale projection final"))
    );
}

#[test]
fn transcript_coalesces_provider_deltas_and_replaces_them_with_final_message() {
    let session_id = SessionId::new();
    let task_id = TaskId::new();
    let turn_id = golutra_core::TurnId::new();
    let mut first_delta = transcript_event(
        1,
        session_id,
        task_id,
        RuntimeEventType::ProviderStreamed,
        json!({"delta": {"kind": "text_delta", "text": "Hello "}}),
    );
    first_delta.turn_id = Some(turn_id);
    let mut second_delta = transcript_event(
        2,
        session_id,
        task_id,
        RuntimeEventType::ProviderStreamed,
        json!({"delta": {"kind": "text_delta", "text": "world"}}),
    );
    second_delta.turn_id = Some(turn_id);
    let mut app = TuiApp::new(
        ThreadId::new(),
        session_id,
        None,
        false,
        "ready (mock)".to_owned(),
        None,
    );
    app.events = vec![first_delta, second_delta];

    let partial = event_transcript_items(&app.events);
    assert_eq!(partial.len(), 1);
    assert_eq!(partial[0].body, vec!["Hello world"]);

    let mut completed = transcript_event(
        3,
        session_id,
        task_id,
        RuntimeEventType::AssistantMessage,
        json!({"content": "Hello world."}),
    );
    completed.turn_id = Some(turn_id);
    app.events.push(completed);
    let final_items = event_transcript_items(&app.events);

    assert_eq!(final_items.len(), 1);
    assert_eq!(final_items[0].body, vec!["Hello world."]);
}

#[test]
fn interactive_poll_interval_does_not_stall_streaming_frames() {
    assert!(
        MIN_FRAME_INTERVAL <= Duration::from_millis(16),
        "interactive runtime updates can wait {MIN_FRAME_INTERVAL:?} before rendering"
    );
}

#[test]
fn provider_deltas_skip_full_refresh_but_final_messages_reconcile() {
    let session_id = SessionId::new();
    let task_id = TaskId::new();
    let streamed = transcript_event(
        1,
        session_id,
        task_id,
        RuntimeEventType::ProviderStreamed,
        json!({"delta": {"kind": "text_delta", "text": "partial"}}),
    );
    let completed = transcript_event(
        2,
        session_id,
        task_id,
        RuntimeEventType::AssistantMessage,
        json!({"content": "authoritative"}),
    );

    assert!(!event_requires_full_refresh(&streamed));
    assert!(event_requires_full_refresh(&completed));
}

#[test]
fn transcript_visible_window_pages_from_bottom_and_round_trips() {
    assert_eq!(transcript_visible_window(50, 10, 0), 40..50);
    assert_eq!(transcript_visible_window(50, 10, 10), 30..40);
    assert_eq!(transcript_visible_window(50, 10, 1_000), 0..10);

    let mut app = TuiApp::new(
        ThreadId::new(),
        SessionId::new(),
        None,
        false,
        "ready (mock)".to_owned(),
        None,
    );
    app.transcript_scroll.row_count = 50;
    app.transcript_scroll.follow_tail = true;

    app.scroll_transcript(TranscriptScrollAction::PageUp, 10);
    assert_eq!(app.transcript_scroll.offset_from_bottom, 10);
    app.scroll_transcript(TranscriptScrollAction::PageDown, 10);
    assert_eq!(app.transcript_scroll.offset_from_bottom, 0);
    app.history_has_more_before = true;
    app.scroll_transcript(TranscriptScrollAction::Top, 10);
    assert_eq!(app.transcript_scroll.offset_from_bottom, 40);
    assert!(app.history_load_requested);
    app.scroll_transcript(TranscriptScrollAction::Bottom, 10);
    assert_eq!(app.transcript_scroll.offset_from_bottom, 0);
    assert!(!app.history_load_requested);
}

#[test]
fn mouse_wheel_routes_to_the_pane_under_the_pointer() {
    let session_id = SessionId::new();
    let mut app = TuiApp::new(
        ThreadId::new(),
        session_id,
        None,
        true,
        "ready (mock)".to_owned(),
        None,
    );
    app.command_messages = (0..40)
        .map(|index| TranscriptItem {
            role: TranscriptRole::System,
            title: format!("message-{index}"),
            body: vec!["history".to_owned()],
        })
        .collect();
    app.developer_projection = Some(golutra_protocol::DebugProjection {
        session_id,
        task_id: None,
        events: (0..40)
            .map(|sequence_no| RuntimeEvent {
                schema_version: golutra_core::RUNTIME_EVENT_SCHEMA_VERSION,
                causal_context: Default::default(),
                causal_links: Vec::new(),
                id: golutra_core::EventId::new(),
                sequence_no,
                session_id,
                turn_id: None,
                task_id: None,
                parent_event_id: None,
                event_type: RuntimeEventType::CommandAccepted,
                timestamp: chrono::Utc::now(),
                source: golutra_protocol::RuntimeEventSource::Runtime,
                payload: json!({"summary": "fact"}),
                payload_ref: None,
                durable: true,
            })
            .collect(),
        event_window: golutra_protocol::DebugEventWindow {
            start_cursor: Some(1),
            end_cursor: Some(40),
            has_more_before: false,
            limit: 256,
        },
        busy_policy_decisions: Vec::new(),
        tool_results: Vec::new(),
        artifacts: Vec::new(),
        evidence: Vec::new(),
        verification: None,
        loop_decisions: Vec::new(),
        post_task_jobs: Vec::new(),
        failure_diagnosis: None,
        failure_episodes: Vec::new(),
        diagnostic_slice: None,
        replay_execution: None,
        external_evaluations: Vec::new(),
        causal_comparisons: Vec::new(),
        trace_complete: true,
        missing_sections: Vec::new(),
        retention_losses: Vec::new(),
    });
    let mut terminal = Terminal::new(TestBackend::new(120, 30)).expect("terminal");
    terminal
        .draw(|frame| draw_ui(frame, &mut app))
        .expect("draw");
    let layout = app.layout;
    let developer_area = layout.developer.expect("developer area");
    let developer_y = developer_area.y + 1;
    let transcript_y = layout.transcript.y + 1;

    handle_mouse(
        MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: developer_area.x + 1,
            row: developer_y,
            modifiers: KeyModifiers::NONE,
        },
        &mut app,
    );
    assert!(app.developer_scroll.offset_from_bottom > 0);
    assert_eq!(app.transcript_scroll.offset_from_bottom, 0);

    handle_mouse(
        MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: layout.transcript.x + 1,
            row: transcript_y,
            modifiers: KeyModifiers::NONE,
        },
        &mut app,
    );
    assert!(app.transcript_scroll.offset_from_bottom > 0);
    assert!(app.developer_scroll.offset_from_bottom > 0);

    let event_rows = developer_event_page_rows(&app, developer_area);
    app.scroll_developer(TranscriptScrollAction::Top, event_rows);
    assert_eq!(
        transcript_visible_window(
            app.developer_scroll.row_count,
            event_rows,
            app.developer_scroll.offset_from_bottom,
        )
        .start,
        0
    );
}

#[tokio::test]
async fn clicking_developer_pane_routes_page_keys_to_developer() {
    let transport = RuntimeTransport::in_memory().await.expect("transport");
    let session_id = SessionId::new();
    let mut app = TuiApp::new(
        ThreadId::new(),
        session_id,
        None,
        true,
        "ready (mock)".to_owned(),
        None,
    );
    app.command_messages = (0..40)
        .map(|index| TranscriptItem {
            role: TranscriptRole::System,
            title: format!("message-{index}"),
            body: vec!["history".to_owned()],
        })
        .collect();
    app.developer_projection = Some(debug_projection_with_events(
        session_id,
        None,
        (1..=40)
            .map(|sequence_no| RuntimeEvent {
                schema_version: golutra_core::RUNTIME_EVENT_SCHEMA_VERSION,
                causal_context: Default::default(),
                causal_links: Vec::new(),
                id: golutra_core::EventId::new(),
                sequence_no,
                session_id,
                turn_id: None,
                task_id: None,
                parent_event_id: None,
                event_type: RuntimeEventType::CommandAccepted,
                timestamp: chrono::Utc::now(),
                source: golutra_protocol::RuntimeEventSource::Runtime,
                payload: json!({"summary": "developer paging detail ".repeat(8)}),
                payload_ref: None,
                durable: true,
            })
            .collect(),
    ));
    let mut terminal = Terminal::new(TestBackend::new(120, 24)).expect("terminal");
    terminal
        .draw(|frame| draw_ui(frame, &mut app))
        .expect("draw");
    let toggle = developer_facts_toggle_rect(app.layout.developer.expect("developer area"));

    handle_mouse(
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: toggle.x,
            row: toggle.y,
            modifiers: KeyModifiers::NONE,
        },
        &mut app,
    );
    handle_key(
        KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE),
        &mut app,
        &transport,
    )
    .await
    .expect("page developer events");

    assert!(app.developer_facts_expanded);
    assert!(app.developer_scroll.offset_from_bottom > 0);
    assert_eq!(app.transcript_scroll.offset_from_bottom, 0);
    assert!(app.status_message.starts_with("developer facts"));
}

#[tokio::test]
async fn session_pickers_support_page_boundary_and_wheel_navigation() {
    let transport = RuntimeTransport::in_memory().await.expect("transport");
    let mut app = TuiApp::new(
        ThreadId::new(),
        SessionId::new(),
        None,
        false,
        "ready (mock)".to_owned(),
        None,
    );
    let items = (0..30)
        .map(|index| ResumeThreadItem {
            thread_id: ThreadId::new(),
            session_id: SessionId::new(),
            title: format!("session-{index}"),
            preview: format!("preview-{index}"),
        })
        .collect::<Vec<_>>();
    app.resume_picker = Some(ResumePickerState {
        items: items.clone(),
        selected: 0,
    });
    let mut terminal = Terminal::new(TestBackend::new(80, 12)).expect("terminal");
    terminal
        .draw(|frame| draw_ui(frame, &mut app))
        .expect("draw picker");

    handle_key(
        KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE),
        &mut app,
        &transport,
    )
    .await
    .expect("page down");
    let page_size = resume_picker_page_size(app.layout.transcript);
    assert_eq!(
        app.resume_picker.as_ref().expect("picker").selected,
        page_size
    );

    handle_key(
        KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE),
        &mut app,
        &transport,
    )
    .await
    .expect("page up");
    assert_eq!(app.resume_picker.as_ref().expect("picker").selected, 0);

    handle_key(
        KeyEvent::new(KeyCode::End, KeyModifiers::NONE),
        &mut app,
        &transport,
    )
    .await
    .expect("picker end");
    assert_eq!(app.resume_picker.as_ref().expect("picker").selected, 29);

    handle_key(
        KeyEvent::new(KeyCode::Home, KeyModifiers::NONE),
        &mut app,
        &transport,
    )
    .await
    .expect("picker home");
    assert_eq!(app.resume_picker.as_ref().expect("picker").selected, 0);

    handle_mouse(
        MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: app.layout.transcript.x.saturating_add(1),
            row: app.layout.transcript.y.saturating_add(1),
            modifiers: KeyModifiers::NONE,
        },
        &mut app,
    );
    assert_eq!(app.resume_picker.as_ref().expect("picker").selected, 1);

    app.resume_picker = None;
    app.export_flow = Some(ExportFlowState {
        picker: ResumePickerState { items, selected: 0 },
        step: ExportFlowStep::SelectSession,
        range_input: "1".to_owned(),
        destination_input: String::new(),
        error: None,
        receipt: None,
    });
    terminal
        .draw(|frame| draw_ui(frame, &mut app))
        .expect("draw export picker");
    handle_key(
        KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE),
        &mut app,
        &transport,
    )
    .await
    .expect("export page down");
    assert_eq!(
        app.export_flow.as_ref().expect("export").picker.selected,
        resume_picker_page_size(app.layout.transcript)
    );
}

#[tokio::test]
async fn resume_thread_clears_previous_visible_transcript_state() {
    let transport = RuntimeTransport::in_memory().await.expect("transport");
    let target_thread_id = ThreadId::new();
    let target_session_id = SessionId::new();
    transport
        .send_command(session_command(
            target_session_id,
            SessionCommandKind::Prompt,
            json!({
                "prompt": "resume target",
                "_thread_id": target_thread_id.to_string(),
            }),
        ))
        .await
        .expect("create resumable thread");
    let mut completed = false;
    for _ in 0..100 {
        let projection = transport
            .query(RuntimeQuery {
                query_id: QueryId::new(),
                session_id: target_session_id,
                task_id: None,
                kind: RuntimeQueryKind::UserProjection,
                requester: ActorKind::Tui,
                cursor: None,
                timestamp: chrono::Utc::now(),
            })
            .await
            .expect("projection");
        if golutra_client::projection_status(&projection)
            == Some(golutra_core::TaskStatus::Completed)
        {
            completed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(completed, "target thread task should complete");
    let mut app = TuiApp::new(
        ThreadId::new(),
        SessionId::new(),
        None,
        false,
        "ready (mock)".to_owned(),
        None,
    );
    app.command_messages.push(TranscriptItem {
        role: TranscriptRole::System,
        title: "Old".to_owned(),
        body: vec!["old session only".to_owned()],
    });
    app.events.push(transcript_event(
        1,
        app.session_id,
        TaskId::new(),
        RuntimeEventType::TaskCreated,
        json!({}),
    ));
    app.input.set_text("/resume");
    app.slash_selected = 2;
    app.transcript_scroll.offset_from_bottom = 12;
    app.transcript_scroll.row_count = 30;
    app.transcript_scroll.follow_tail = false;

    app.resume_thread(&transport, target_thread_id)
        .await
        .expect("resume");

    assert!(app.command_messages.is_empty());
    assert!(app.events.is_empty());
    assert!(app.input.is_empty());
    assert_eq!(app.slash_selected, 0);
    assert_eq!(app.transcript_scroll.offset_from_bottom, 0);
}

#[tokio::test]
async fn export_enters_running_before_poll_and_finishes_asynchronously() {
    let transport = RuntimeTransport::in_memory().await.expect("transport");
    let session_id = SessionId::new();
    let thread_id = ThreadId::new();
    transport
        .send_command(session_command(
            session_id,
            SessionCommandKind::Prompt,
            json!({
                "prompt": "export this session",
                "_thread_id": thread_id.to_string(),
            }),
        ))
        .await
        .expect("start export fixture task");

    let completed = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let projection = transport
                .query(RuntimeQuery {
                    query_id: QueryId::new(),
                    session_id,
                    task_id: None,
                    kind: RuntimeQueryKind::UserProjection,
                    requester: ActorKind::Tui,
                    cursor: None,
                    timestamp: chrono::Utc::now(),
                })
                .await
                .expect("projection");
            if golutra_client::projection_status(&projection)
                == Some(golutra_core::TaskStatus::Completed)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(completed.is_ok(), "runtime task should complete");

    let parent = tempfile::tempdir().expect("export parent");
    let destination = parent.path().join("bundle");
    let mut app = TuiApp::new(
        thread_id,
        session_id,
        None,
        false,
        "ready (mock)".to_owned(),
        None,
    );
    app.export_flow = Some(ExportFlowState {
        picker: ResumePickerState {
            items: vec![ResumeThreadItem {
                thread_id,
                session_id,
                title: "export fixture".to_owned(),
                preview: "export this session".to_owned(),
            }],
            selected: 0,
        },
        step: ExportFlowStep::Review,
        range_input: "1".to_owned(),
        destination_input: destination.display().to_string(),
        error: None,
        receipt: None,
    });

    app.handle_export_enter(&transport)
        .await
        .expect("start export");
    assert_eq!(
        app.export_flow.as_ref().map(|flow| flow.step),
        Some(ExportFlowStep::Running)
    );
    assert!(app.export_operation.is_some());
    assert_eq!(app.status_message, "export running");

    let finished = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            app.poll_export_operation().await;
            if app
                .export_flow
                .as_ref()
                .is_some_and(|flow| flow.step == ExportFlowStep::Completed)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
    assert!(finished.is_ok(), "export operation should finish");
    assert!(app.export_operation.is_none());
    assert!(destination.join("manifest.json").is_file());
    assert_eq!(app.status_message, "export complete");
}

#[test]
fn start_new_session_resets_visible_tui_state() {
    let original_thread_id = ThreadId::new();
    let original_session_id = SessionId::new();
    let mut app = TuiApp::new(
        original_thread_id,
        original_session_id,
        Some(TaskId::new()),
        true,
        "ready (mock)".to_owned(),
        None,
    );
    app.projection = Some(UserProjection {
        session_id: original_session_id,
        task_id: Some(TaskId::new()),
        status: golutra_core::TaskStatus::Completed,
        visible_steps: Vec::new(),
        pending_approval: None,
        final_message: Some("old answer".to_owned()),
        residual_risks: Vec::new(),
    });
    app.developer_error = Some("old developer state".to_owned());
    app.debug_mode = true;
    app.command_messages.push(TranscriptItem {
        role: TranscriptRole::System,
        title: "Old".to_owned(),
        body: vec!["old session only".to_owned()],
    });
    app.events.push(transcript_event(
        1,
        app.session_id,
        TaskId::new(),
        RuntimeEventType::TaskCreated,
        json!({}),
    ));
    app.input.set_text("/new");
    app.slash_selected = 2;
    app.cursor = Some(9);
    app.resume_picker = Some(ResumePickerState {
        items: Vec::new(),
        selected: 0,
    });
    app.transcript_scroll.offset_from_bottom = 7;
    app.transcript_scroll.row_count = 20;
    app.transcript_scroll.follow_tail = false;

    app.start_new_session();

    assert_ne!(app.thread_id, original_thread_id);
    assert_ne!(app.session_id, original_session_id);
    assert!(app.task_id.is_none());
    assert!(app.projection.is_none());
    assert!(app.developer_projection.is_none());
    assert!(app.developer_error.is_none());
    assert!(app.command_messages.is_empty());
    assert!(app.events.is_empty());
    assert!(app.input.is_empty());
    assert_eq!(app.slash_selected, 0);
    assert!(app.cursor.is_none());
    assert!(app.resume_picker.is_none());
    assert!(app.debug_mode);
    assert_eq!(app.transcript_scroll.offset_from_bottom, 0);
    assert_eq!(app.status_message, "new session");
}

#[tokio::test]
async fn active_task_blocks_session_switching_commands() {
    let transport = RuntimeTransport::in_memory().await.expect("transport");
    let thread_id = ThreadId::new();
    let session_id = SessionId::new();
    let mut app = TuiApp::new(
        thread_id,
        session_id,
        Some(TaskId::new()),
        false,
        "ready (mock)".to_owned(),
        None,
    );
    app.projection = Some(UserProjection {
        session_id,
        task_id: app.task_id,
        status: golutra_core::TaskStatus::Running,
        visible_steps: Vec::new(),
        pending_approval: None,
        final_message: None,
        residual_risks: Vec::new(),
    });

    app.execute_slash_command(&transport, SlashCommand::New)
        .await
        .expect("command");

    assert_eq!(app.thread_id, thread_id);
    assert_eq!(app.session_id, session_id);
    assert_eq!(
        app.status_message,
        "interrupt the active task before switching sessions"
    );
}

#[tokio::test]
async fn auth_dialog_offers_disk_or_environment_reference() {
    let transport = RuntimeTransport::in_memory().await.expect("transport");
    let mut app = TuiApp::new(
        ThreadId::new(),
        SessionId::new(),
        None,
        false,
        "missing provider".to_owned(),
        Some(AuthDialogState::new()),
    );
    {
        let dialog = app.auth_dialog.as_mut().expect("dialog");
        dialog.select_provider(OFFICIAL_PROVIDER_PRESET);
        dialog.credential_store = AuthCredentialStore::Disk;
        dialog.step = AuthDialogStep::BaseUrl;
    }

    advance_auth_dialog(&mut app, &transport)
        .await
        .expect("base URL");
    assert_eq!(
        app.auth_dialog.as_ref().map(|dialog| dialog.step),
        Some(AuthDialogStep::CredentialStore)
    );
    {
        let dialog = app.auth_dialog.as_mut().expect("dialog");
        let lines = auth_credential_store_lines(dialog)
            .into_iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(lines.contains("Local disk"));
        assert!(lines.contains("$GOLUTRA_HOME/credentials.json"));
        assert!(lines.contains("Environment variable"));
        dialog.selected = 1;
    }
    advance_auth_dialog(&mut app, &transport)
        .await
        .expect("credential store");
    let dialog = app.auth_dialog.as_ref().expect("dialog");
    assert_eq!(dialog.step, AuthDialogStep::EnvKey);
    assert_eq!(dialog.api_key_env, "GOLUTRA_PROVIDER_API_KEY");

    let review = build_auth_review(dialog).expect("review");
    assert_eq!(review.credential, "env:GOLUTRA_PROVIDER_API_KEY");
    assert!(review.preview_json.contains("environment"));
    assert!(!review.preview_json.contains("oauth-access-token"));
}

#[tokio::test]
async fn oauth_device_task_installs_secret_ref_and_logout_removes_it() {
    let home = tempfile::tempdir().expect("home");
    let cwd = tempfile::tempdir().expect("cwd");
    let endpoint = spawn_oauth_probe_server().await;
    let descriptor = OAuthProviderDescriptor {
        provider_id: "test-oauth".to_owned(),
        client_id: "test-client".to_owned(),
        authorization_endpoint: format!("{endpoint}/authorize"),
        token_endpoint: format!("{endpoint}/token"),
        device_authorization_endpoint: Some(format!("{endpoint}/device")),
        revocation_endpoint: None,
        scopes: vec!["model.invoke".to_owned()],
        audience: None,
        browser_redirect_uri: None,
        authorization_params: std::collections::BTreeMap::new(),
        authorization_nonce: false,
        openai_device_authorization: None,
        flows: vec![OAuthFlow::DeviceCode],
    };
    let _guard = env_lock_guard().await;
    let previous_home = std::env::var("GOLUTRA_HOME").ok();
    unsafe {
        std::env::set_var("GOLUTRA_HOME", home.path());
    }
    let paths = ProviderConfigPaths::global().expect("paths");
    let (progress_tx, mut progress_rx) = mpsc::unbounded_channel();

    let outcome = run_oauth_login_task(
        paths.clone(),
        cwd.path().to_path_buf(),
        descriptor,
        OAuthLoginCommand {
            descriptor_path: "oauth.json".to_owned(),
            flow: OAuthFlow::DeviceCode,
            profile: "oauth".to_owned(),
            protocol: ProviderProtocol::OpenAiCompatible,
            base_url: format!("{endpoint}/v1"),
            model: "oauth-model".to_owned(),
            credential_store: AuthCredentialStore::Ephemeral,
            no_open_browser: true,
            generation_config: None,
        },
        CancellationToken::new(),
        progress_tx,
    )
    .await
    .expect("OAuth task");

    assert_eq!(outcome.title, "Auth updated");
    let progress = progress_rx.try_recv().expect("device instructions");
    assert!(progress.body.join(" ").contains("GOLUTRA-123"));
    let persisted = std::fs::read_to_string(&paths.user_config).expect("config");
    assert!(persisted.contains("oauth-token-set"));
    assert!(!persisted.contains("oauth-access-token"));
    assert!(!persisted.contains("oauth-refresh-token"));

    logout_provider_profile_verified(&paths, cwd.path(), "oauth")
        .await
        .expect("logout");
    let settings = ProviderSettings::load(&paths.user_config).expect("settings");
    let profile = settings
        .profiles
        .iter()
        .find(|profile| profile.name == "oauth")
        .expect("profile");
    assert!(!profile.enabled);
    assert!(profile.credential_ref.is_none());

    match previous_home {
        Some(value) => unsafe {
            std::env::set_var("GOLUTRA_HOME", value);
        },
        None => unsafe {
            std::env::remove_var("GOLUTRA_HOME");
        },
    }
}

#[tokio::test]
async fn openai_headless_task_shows_code_and_installs_secret_ref() {
    let home = tempfile::tempdir().expect("home");
    let cwd = tempfile::tempdir().expect("cwd");
    let endpoint = spawn_oauth_probe_server().await;
    let descriptor = OAuthProviderDescriptor {
        provider_id: "openai-test".to_owned(),
        client_id: "test-client".to_owned(),
        authorization_endpoint: format!("{endpoint}/authorize"),
        token_endpoint: format!("{endpoint}/token"),
        device_authorization_endpoint: None,
        revocation_endpoint: None,
        scopes: vec!["model.invoke".to_owned()],
        audience: None,
        browser_redirect_uri: None,
        authorization_params: std::collections::BTreeMap::new(),
        authorization_nonce: false,
        openai_device_authorization: Some(OpenAiDeviceAuthorizationDescriptor {
            user_code_endpoint: format!("{endpoint}/openai-usercode"),
            token_poll_endpoint: format!("{endpoint}/openai-poll"),
            verification_uri: format!("{endpoint}/verify"),
            redirect_uri: format!("{endpoint}/callback"),
        }),
        flows: vec![OAuthFlow::OpenAiDeviceAuth],
    };
    let _guard = env_lock_guard().await;
    let previous_home = std::env::var("GOLUTRA_HOME").ok();
    unsafe {
        std::env::set_var("GOLUTRA_HOME", home.path());
    }
    let paths = ProviderConfigPaths::global().expect("paths");
    let (progress_tx, mut progress_rx) = mpsc::unbounded_channel();

    let outcome = run_oauth_login_task(
        paths.clone(),
        cwd.path().to_path_buf(),
        descriptor,
        OAuthLoginCommand {
            descriptor_path: "builtin:openai-test:headless".to_owned(),
            flow: OAuthFlow::OpenAiDeviceAuth,
            profile: "openai-headless".to_owned(),
            protocol: ProviderProtocol::OpenAiCompatible,
            base_url: format!("{endpoint}/v1"),
            model: "oauth-model".to_owned(),
            credential_store: AuthCredentialStore::Ephemeral,
            no_open_browser: true,
            generation_config: None,
        },
        CancellationToken::new(),
        progress_tx,
    )
    .await
    .expect("OpenAI headless OAuth task");

    assert_eq!(outcome.title, "Auth updated");
    let progress = progress_rx.try_recv().expect("headless instructions");
    assert!(progress.body.join(" ").contains("OPENAI-123"));
    let persisted = std::fs::read_to_string(&paths.user_config).expect("config");
    assert!(persisted.contains("openai-device-auth"));
    assert!(persisted.contains("oauth-token-set"));
    assert!(!persisted.contains("oauth-access-token"));
    assert!(!persisted.contains("oauth-refresh-token"));

    match previous_home {
        Some(value) => unsafe {
            std::env::set_var("GOLUTRA_HOME", value);
        },
        None => unsafe {
            std::env::remove_var("GOLUTRA_HOME");
        },
    }
}

#[test]
fn compact_ack_reason_hides_runtime_ids() {
    assert_eq!(
        compact_ack_reason(&Some(
            "started task 00000000 in session 11111111".to_owned()
        )),
        "task started"
    );
    assert_eq!(compact_ack_reason(&None), "prompt accepted");
}

#[test]
fn resume_picker_offset_keeps_selected_item_visible() {
    assert_eq!(resume_picker_offset(0, 5, 20), 0);
    assert_eq!(resume_picker_offset(4, 5, 20), 0);
    assert_eq!(resume_picker_offset(5, 5, 20), 1);
    assert_eq!(resume_picker_offset(19, 5, 20), 15);
    assert_eq!(resume_picker_offset(3, 5, 4), 0);
}
